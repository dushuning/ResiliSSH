use super::errors::is_permanent_sftp_error;
use super::transfer_common::{
    align_down, current_retry_count, effective_verified_bytes, ensure_not_cancelled,
    increment_retry_count, sha256_bytes, sha256_local_file, sha256_remote_file,
    should_skip_full_file_hash, sleep_backoff, TransferCheckpoint, TransferError, CHUNK_SIZE,
    HASH_READ_BUFFER_SIZE, hex_encode,
};
use super::client::{
    connect_session, is_sftp_not_found, keepalive_while_idle, AbortHandles, ConnectError, ConnectionConfig,
};
use super::plan::{effective_connections, Claim, PlanStatus, RangeSet, TransferPlan, DELTA_MIN_BYTES};
use super::remote_hash::{remote_block_hashes, remote_copy, remote_file_sha256, BlockHasher};
use super::browse::join_remote_path;
use super::pool;
use super::preflight::shell_quote;
pub use super::transfer_common::{is_staging_file, staging_path};
use crate::checkpoint::{save_checkpoint, CheckpointStatus, UploadCheckpoint};
use crate::retry::Backoff;
use sha2::{Digest, Sha256};
use ssh2::{File as RemoteFile, FileStat, OpenFlags, OpenType, Session, Sftp};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 写入分片大小的上下限。libssh2 会把单次写入的数据流水线发送，分片越大，每个往返在途的数据越多：
/// 实测 100 ms 往返延迟下 5 MB 上传，64 KB 分片 12.5 s、256 KB 6.0 s、1 MB 4.2 s。
/// 但进度只在每个分片写完时刷新，慢速链路上大分片会让进度条长时间不动、被误判为无响应，
/// 所以按实测速度自适应：让每个分片大约 TARGET_SLICE_SECS 秒写完。
const MIN_WRITE_SLICE: usize = 64 * 1024;
const MAX_WRITE_SLICE: usize = CHUNK_SIZE as usize;
/// 第一块还没有测速数据，取中间值。
const INITIAL_WRITE_SLICE: usize = 256 * 1024;
const TARGET_SLICE_SECS: f64 = 2.0;

pub struct UploadCallbacks {
    pub on_progress: Box<dyn Fn(u64, u64, &str, u32) + Send + Sync>,
    pub on_retry: Box<dyn Fn(u32, u64, &str) + Send + Sync>,
    /// 块读回校验等长时间无字节进度时刷新 stall 心跳，避免误判断连
    pub on_activity: Box<dyn Fn() + Send + Sync>,
}

/// 不用 thiserror 的固定 `#[error]` 文案：错误会原样展示给用户，必须在展示时按界面语言选择。
/// 注意 `is_permanent_sftp_error` 依赖这里的部分措辞（续传边界、SHA-256），改文案时同步关键字表。
#[derive(Debug)]
pub enum UploadError {
    Cancelled,
    InvalidRemotePath(String),
    RemoteLargerThanLocal { remote: u64, local: u64 },
    ResumeBoundaryMismatch { offset: u64 },
    SizeMismatch { local: u64, remote: u64 },
    HashMismatch,
    ChunkVerifyMismatch { offset: u64, len: u64 },
    LocalIo(String),
    /// 本地文件在传输途中被改写：已传的部分和后传的部分来自不同版本，结果不可信。
    LocalFileChanged,
    Connect(ConnectError),
    Sftp(String),
}

impl std::fmt::Display for UploadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            UploadError::Cancelled => crate::tr!("用户取消了上传", "Upload cancelled by user").to_string(),
            UploadError::InvalidRemotePath(e) => {
                crate::tr!(format!("远端路径无效: {e}"), format!("Invalid remote path: {e}"))
            }
            UploadError::RemoteLargerThanLocal { .. } => crate::tr!(
                "远端文件大于本地文件，无法续传",
                "Remote file is larger than the local file; cannot resume"
            )
            .to_string(),
            UploadError::ResumeBoundaryMismatch { offset } => crate::tr!(
                format!("续传边界校验失败（offset={offset}），请清除断点后重新上传"),
                format!("Resume boundary verification failed (offset={offset}); clear the checkpoint and upload again")
            ),
            UploadError::SizeMismatch { local, remote } => crate::tr!(
                format!("上传完成后远端大小不一致（本地={local}，远端={remote}）"),
                format!("Remote size mismatch after upload (local={local}, remote={remote})")
            ),
            UploadError::HashMismatch => crate::tr!(
                "上传完成后校验不一致（SHA-256 不匹配）",
                "Verification failed after upload (SHA-256 mismatch)"
            )
            .to_string(),
            UploadError::ChunkVerifyMismatch { offset, len } => crate::tr!(
                format!("块校验失败（offset={offset}，len={len}），将重传该块"),
                format!("Chunk verification failed (offset={offset}, len={len}); resending the chunk")
            ),
            UploadError::LocalIo(e) => {
                crate::tr!(format!("本地文件读取失败: {e}"), format!("Failed to read local file: {e}"))
            }
            UploadError::LocalFileChanged => crate::tr!(
                "本地文件在上传过程中被修改，已停止。请等文件不再变化后重新上传",
                "The local file was modified during the upload, so the upload was stopped. Upload again once the file is no longer changing"
            )
            .to_string(),
            UploadError::Connect(e) => crate::tr!(format!("连接失败: {e}"), format!("Connection failed: {e}")),
            UploadError::Sftp(e) => crate::tr!(format!("SFTP 错误: {e}"), format!("SFTP error: {e}")),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for UploadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            UploadError::Connect(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ConnectError> for UploadError {
    fn from(err: ConnectError) -> Self {
        UploadError::Connect(err)
    }
}

impl TransferError for UploadError {
    fn cancelled() -> Self {
        UploadError::Cancelled
    }
    fn sftp(detail: String) -> Self {
        UploadError::Sftp(detail)
    }
    fn local_io(detail: String) -> Self {
        UploadError::LocalIo(detail)
    }
}

impl TransferCheckpoint for UploadCheckpoint {
    type Error = UploadError;
    fn transferred_bytes(&self) -> u64 {
        self.uploaded_bytes
    }
    fn verified_bytes(&self) -> u64 {
        self.verified_bytes
    }
    fn retry_count(&self) -> u32 {
        self.retry_count
    }
    fn set_retry_count(&mut self, count: u32) {
        self.retry_count = count;
    }
    fn mark_failed(&mut self) {
        self.status = CheckpointStatus::Failed;
    }
    fn all_chunks_verified(&self) -> bool {
        self.all_chunks_verified
    }
}

/// 本地文件的大小与修改时间，用来发现「传输途中文件被改写」。
fn local_fingerprint(path: &Path) -> Result<(u64, Option<std::time::SystemTime>), UploadError> {
    let meta = std::fs::metadata(path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    Ok((meta.len(), meta.modified().ok()))
}

/// 远端填目录时自动拼接本地文件名；连接后若发现路径是已存在目录，也会自动拼接。
pub fn resolve_remote_path(remote_path: &str, local_path: &Path) -> Result<String, UploadError> {
    let trimmed = remote_path.trim();
    if trimmed.is_empty() {
        return Err(UploadError::InvalidRemotePath(
            crate::tr!("远端路径不能为空", "Remote path cannot be empty").into(),
        ));
    }

    if trimmed.ends_with('/') {
        return append_local_filename(trimmed, local_path);
    }

    Ok(trimmed.to_string())
}

fn append_local_filename(dir_path: &str, local_path: &Path) -> Result<String, UploadError> {
    let file_name = local_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| UploadError::InvalidRemotePath(
                crate::tr!("无法读取本地文件名", "Cannot read the local file name").into(),
            ))?;
    Ok(format!("{dir_path}{file_name}"))
}

fn is_remote_directory(sftp: &Sftp, path: &Path) -> bool {
    if let Ok(stat) = sftp.stat(path) {
        if stat.is_dir() {
            return true;
        }
        if stat.is_file() {
            return false;
        }
    }
    sftp.opendir(path).is_ok()
}

/// 连接 SFTP 后解析真实上传目标：若远端路径是目录则自动拼接文件名。
pub fn resolve_upload_target(
    sftp: &Sftp,
    remote_path: &str,
    local_path: &Path,
) -> Result<String, UploadError> {
    let trimmed = remote_path.trim();
    if trimmed.is_empty() {
        return Err(UploadError::InvalidRemotePath(
            crate::tr!("远端路径不能为空", "Remote path cannot be empty").into(),
        ));
    }

    if trimmed.ends_with('/') {
        return append_local_filename(trimmed, local_path);
    }

    let path = Path::new(trimmed);
    if is_remote_directory(sftp, path) {
        let resolved = append_local_filename(&join_remote_path(trimmed, ""), local_path)?;
        log::info!("remote path is directory, upload target: {resolved}");
        return Ok(resolved);
    }

    Ok(trimmed.to_string())
}

fn remote_parent_path(remote_file: &str) -> Option<String> {
    Path::new(remote_file.trim())
        .parent()
        .and_then(|p| p.to_str())
        .filter(|s| !s.is_empty() && *s != "/")
        .map(|s| s.to_string())
}

/// 将 SFTP 原始错误转成带远端路径的明确提示。
pub fn format_remote_sftp_error(message: &str, remote_path: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let parent = remote_parent_path(remote_path);

    if lower.contains("no such file") || lower.contains("sftp(2)") {
        if let Some(p) = parent {
            return crate::tr!(
                format!(
                    "远端路径错误：{remote_path}\n上级目录「{p}」在服务器上不存在，请核对用户名与目录名（本地文件不受影响）"
                ),
                format!(
                    "Remote path error: {remote_path}\nParent directory \"{p}\" does not exist on the server; check the username and directory name (local file is unaffected)"
                )
            );
        }
        return crate::tr!(
            format!("远端路径错误：{remote_path}\n该路径在服务器上不存在（本地文件不受影响）"),
            format!(
                "Remote path error: {remote_path}\nThe path does not exist on the server (local file is unaffected)"
            )
        );
    }
    if lower.contains("permission denied") || lower.contains("access denied") {
        return crate::tr!(
            format!("远端路径错误：{remote_path}\n没有写入权限，请检查该目录权限"),
            format!("Remote path error: {remote_path}\nNo write permission; check the directory permissions")
        );
    }
    if lower.contains("sftp(4)") || lower.contains("failure") {
        return crate::tr!(
            format!("远端路径错误：{remote_path}\n写入失败：{message}"),
            format!("Remote path error: {remote_path}\nWrite failed: {message}")
        );
    }
    crate::tr!(
        format!("远端路径「{remote_path}」操作失败：{message}"),
        format!("Remote path \"{remote_path}\" failed: {message}")
    )
}

/// 递归创建远端父目录（类似 mkdir -p），仅创建路径中缺失的目录。
fn ensure_remote_parent_directories(sftp: &Sftp, remote_file: &str) -> Result<(), UploadError> {
    let path = Path::new(remote_file.trim());
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let parent_str = parent.to_str().unwrap_or("/");
    if parent_str.is_empty() || parent_str == "/" {
        return Ok(());
    }

    // 相对路径（相对 SFTP 起始目录，通常是家目录）必须保持相对：一律补上开头的 `/`
    // 会去根目录下建目录，普通用户直接得到「权限不足」，root 则会建出多余的目录。
    let absolute = parent_str.starts_with('/');
    let parts: Vec<&str> = parent_str.split('/').filter(|p| !p.is_empty()).collect();
    let mut current = String::new();
    for part in parts {
        current = if !current.is_empty() {
            format!("{current}/{part}")
        } else if absolute {
            format!("/{part}")
        } else {
            part.to_string()
        };
        let dir = Path::new(&current);
        if sftp.stat(dir).is_ok() {
            continue;
        }
        sftp.mkdir(dir, 0o755).map_err(|e| {
            UploadError::Sftp(format_remote_sftp_error(
                &crate::tr!(
                    format!("创建远端目录「{current}」失败: {e}"),
                    format!("Failed to create remote directory \"{current}\": {e}")
                ),
                remote_file,
            ))
        })?;
    }
    Ok(())
}

/// 检查远端文件的父目录是否存在，返回给用户的前置提示。
pub fn remote_parent_status(sftp: &Sftp, remote_file: &str) -> Option<String> {
    let path = Path::new(remote_file.trim());
    let parent = path.parent()?;
    let parent_str = parent.to_str().filter(|s| !s.is_empty() && *s != "/")?;
    if sftp.stat(Path::new(parent_str)).is_ok() {
        return None;
    }
    Some(crate::tr!(
        format!("远端路径可能有问题：上级目录「{parent_str}」在服务器上不存在"),
        format!("Remote path may be wrong: parent directory \"{parent_str}\" does not exist on the server")
    ))
}


/// 权限位（含 setuid/setgid/sticky），不含文件类型位。
const PERMISSION_BITS: u32 = 0o7777;

/// 临时文件已不在、目标已是完整大小：说明上次的改名其实成功了，只是回复在断线中丢失。
fn already_promoted(sftp: &Sftp, final_path: &str, size: u64) -> bool {
    sftp.stat(Path::new(final_path)).ok().and_then(|st| st.size) == Some(size)
}

/// 校验通过后把临时文件换成正式文件。可重复调用：弱网下改名的回复可能丢失，重连后再调一次
/// 必须能识别「其实已经换好了」，绝不能因此误判而从头重传或删掉新文件。
pub(crate) fn promote_staged_file(
    session: &Session,
    part: &str,
    final_path: &str,
    size: u64,
) -> Result<(), UploadError> {
    let sftp = session.sftp().map_err(|e| UploadError::Sftp(e.to_string()))?;
    match sftp.stat(Path::new(part)) {
        Ok(_) => {}
        Err(e) if is_sftp_not_found(&e) => {
            if already_promoted(&sftp, final_path, size) {
                log::info!("staged file already promoted to {final_path}");
                return Ok(());
            }
            // 归为永久错误：临时文件被外部删掉了，无限重试也找不回来。
            return Err(UploadError::InvalidRemotePath(crate::tr!(
                format!("校验通过的临时文件 {part} 不见了（可能被其他程序删除），请重新上传"),
                format!("The verified temporary file {part} has disappeared (possibly deleted by another program); upload again")
            )));
        }
        Err(e) => return Err(UploadError::Sftp(e.to_string())),
    }

    let existing = sftp.stat(Path::new(final_path)).ok();
    // 目标是目录时 `mv` 会把临时文件挪进目录里并返回成功，文件就此「消失」在一个隐藏文件名下。
    // 正常流程在连接后已把目录解析成「目录/文件名」，这里是最后一道防线。
    if existing.as_ref().is_some_and(|st| st.is_dir()) {
        log::error!("refusing to replace directory {final_path} with staged file {part}");
        return Err(UploadError::InvalidRemotePath(crate::tr!(
            format!("{final_path} 是一个目录，无法用上传的文件替换它"),
            format!("{final_path} is a directory and cannot be replaced by the uploaded file")
        )));
    }
    // 覆盖已有文件时沿用它的权限（如脚本的可执行位），否则替换后会变成新建文件的默认权限。
    if let Some(mode) = existing.as_ref().and_then(|st| st.perm).map(|perm| perm & PERMISSION_BITS) {
        let attrs = FileStat { size: None, uid: None, gid: None, perm: Some(mode), atime: None, mtime: None };
        if let Err(e) = sftp.setstat(Path::new(part), attrs) {
            log::warn!("cannot copy permissions {mode:o} from {final_path} to staged file: {e}");
        }
    }
    let rename = |sftp: &Sftp| {
        sftp.rename(Path::new(part), Path::new(final_path), None)
            .map_err(|e| UploadError::Sftp(format_remote_sftp_error(&e.to_string(), final_path)))
    };
    // 目标不存在时 SFTP 改名本身就是原子的。
    if existing.is_none() {
        return rename(&sftp);
    }
    // OpenSSH 的 SFTP 改名不会覆盖已有文件；在服务器上执行 mv（即 rename(2)）可原子替换，
    // 读这个文件的进程要么看到旧版本、要么看到新版本，不会看到半截。
    if replace_with_mv(session, part, final_path) {
        return Ok(());
    }
    // mv 失败也可能是「其实已执行成功、只是结果没传回来」：此时再删目标就会删掉新文件。
    match sftp.stat(Path::new(part)) {
        Ok(_) => {}
        Err(e) if is_sftp_not_found(&e) && already_promoted(&sftp, final_path, size) => return Ok(()),
        Err(e) => return Err(UploadError::Sftp(e.to_string())),
    }
    // 只开放 SFTP、不能执行命令的服务器：先删后改名。中间极短时间目标不存在，但仍不会出现半截文件。
    log::warn!("atomic replace unavailable, falling back to unlink + rename for {final_path}");
    sftp.unlink(Path::new(final_path))
        .map_err(|e| UploadError::Sftp(format_remote_sftp_error(&e.to_string(), final_path)))?;
    rename(&sftp)
}

fn replace_with_mv(session: &Session, part: &str, final_path: &str) -> bool {
    // 合并 stderr：失败原因（如 Permission denied）只出现在 stderr 上，不合并日志里就是空的。
    let command = format!("mv -f -- {} {} 2>&1", shell_quote(part), shell_quote(final_path));
    let Ok(mut channel) = session.channel_session() else { return false };
    if channel.exec(&command).is_err() {
        return false;
    }
    let mut output = String::new();
    // 没收到退出码时 libssh2 的 exit_status 默认是 0：连接在结果返回前断开会被误当成成功。
    // 所以必须读完输出并等到通道正常关闭，退出码才可信；否则交给调用方重新 stat 判断。
    let finished = channel.read_to_string(&mut output).is_ok() && channel.wait_close().is_ok();
    let ok = finished && channel.exit_status().is_ok_and(|code| code == 0);
    if !ok {
        log::warn!("remote mv failed for {final_path}: {}", output.trim());
    }
    ok
}

/// 没有任务可领、但别的连接还在传时，多久再看一眼队列。
const PLAN_POLL: Duration = Duration::from_millis(100);

/// 一次上传中各连接共享的状态。
struct UploadShared<'a> {
    local_path: &'a Path,
    local_size: u64,
    cancel: &'a AtomicBool,
    strict: &'a AtomicBool,
    checkpoint: &'a Mutex<UploadCheckpoint>,
    callbacks: &'a UploadCallbacks,
    aborts: &'a AbortHandles,
    plan: TransferPlan<UploadError>,
    /// 准备阶段确定的（最终目标, 实际写入的文件）；主连接重连后不必重新确定。
    target: Mutex<Option<(String, String)>>,
}

impl UploadShared<'_> {
    /// 汇报进度：已完成的块加上各连接正在传的那一块里已传的部分。
    fn report(&self, status: &str) {
        let (done, retries) = self
            .checkpoint
            .lock()
            .map(|cp| (cp.uploaded_bytes, cp.retry_count))
            .unwrap_or((0, 0));
        let transferred = (done + self.plan.partial_bytes()).min(self.local_size);
        (self.callbacks.on_progress)(transferred, self.local_size, status, retries);
    }

    /// 一块已写入（并按当前模式校验过）：记入断点并落盘。
    fn complete_block(&self, offset: u64, len: u64, read_back: bool) -> Result<(), UploadError> {
        let mut cp = self.checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
        cp.done_ranges.insert(offset, offset + len);
        let total = cp.done_ranges.total();
        cp.uploaded_bytes = total;
        cp.verified_bytes = total;
        cp.all_chunks_verified &= read_back;
        save_checkpoint(&cp).map_err(UploadError::Sftp)
    }

    fn transferred(&self) -> u64 {
        self.checkpoint.lock().map(|cp| cp.uploaded_bytes).unwrap_or(0)
    }
}

/// 重连也不会好的错误：停止重试，把断点留给用户处理。
fn is_permanent(err: &UploadError) -> bool {
    match err {
        UploadError::Cancelled | UploadError::ChunkVerifyMismatch { .. } => false,
        // 判定用变体里的原始内容：Display 会再套一层「SFTP 错误: 」前缀，
        // 套上之后 is_permanent_sftp_error 就认不出路径外壳，路径里带 auth 之类的字样会被误判成永久错误。
        UploadError::Sftp(raw) => is_permanent_sftp_error(raw),
        UploadError::Connect(e) => e.is_permanent(),
        UploadError::InvalidRemotePath(_)
        | UploadError::RemoteLargerThanLocal { .. }
        | UploadError::ResumeBoundaryMismatch { .. }
        | UploadError::SizeMismatch { .. }
        | UploadError::HashMismatch
        | UploadError::LocalFileChanged
        | UploadError::LocalIo(_) => true,
    }
}

/// 断点里记录的已完成区间。旧版断点只有「已校验到第几字节」一个数，换算成文件开头的一段。
fn checkpoint_ranges(cp: &UploadCheckpoint) -> RangeSet {
    if cp.done_ranges.is_empty() {
        RangeSet::prefix(effective_verified_bytes(cp))
    } else {
        cp.done_ranges.clone()
    }
}

#[allow(clippy::too_many_arguments)] // 与下载侧对称的入口，参数逐个对应调用方持有的状态。
pub fn upload_with_resume(
    config: ConnectionConfig,
    local_path: &Path,
    remote_path: &str,
    cancel_flag: Arc<AtomicBool>,
    force_overwrite: Arc<AtomicBool>,
    strict_chunk_verify: Arc<AtomicBool>,
    aborts: Arc<AbortHandles>,
    checkpoint: Arc<Mutex<UploadCheckpoint>>,
    callbacks: UploadCallbacks,
    connections: usize,
) -> Result<(), UploadError> {
    let resolved_remote_path = resolve_remote_path(remote_path, local_path)?;
    let started_fingerprint = local_fingerprint(local_path)?;
    let local_size = started_fingerprint.0;

    let staged = {
        let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
        cp.file_size = local_size;
        cp.remote_path = resolved_remote_path.clone();
        cp.status = CheckpointStatus::InProgress;
        // 数据写到哪里：新传输写临时文件，校验通过后再替换目标；旧版断点继续直接写目标。
        cp.staged
    };

    let shared = UploadShared {
        local_path,
        local_size,
        cancel: &cancel_flag,
        strict: &strict_chunk_verify,
        checkpoint: &checkpoint,
        callbacks: &callbacks,
        aborts: &aborts,
        plan: TransferPlan::new(),
        target: Mutex::new(None),
    };
    let helpers = effective_connections(connections, local_size) - 1;

    thread::scope(|scope| {
        for index in 1..=helpers {
            let (config, shared) = (&config, &shared);
            let spawned = thread::Builder::new()
                .name(format!("upload-conn-{index}"))
                .spawn_scoped(scope, move || helper_loop(config, shared, index));
            if let Err(e) = spawned {
                // 起不了线程只是少一条并行连接，主连接照常传完。
                log::warn!("cannot start parallel upload connection {index}: {e}");
            }
        }
        let result =
            lead_loop(&config, &shared, &resolved_remote_path, staged, started_fingerprint, &force_overwrite);
        shared.plan.finish();
        if result.is_err() {
            // 失败或取消：别让辅助连接在阻塞的网络 IO 里再等上几十秒才发现该退出了。
            aborts.abort_all();
        }
        result
    })
}

/// 主连接：负责准备、对用户可见的重连提示、最终校验与替换；传输阶段与辅助连接一样从队列领任务。
fn lead_loop(
    config: &ConnectionConfig,
    shared: &UploadShared<'_>,
    resolved_remote_path: &str,
    staged: bool,
    started_fingerprint: (u64, Option<std::time::SystemTime>),
    force_overwrite: &AtomicBool,
) -> Result<(), UploadError> {
    let (checkpoint, callbacks, local_size) = (shared.checkpoint, shared.callbacks, shared.local_size);
    // 临时文件路径必须由「连接后解析出的最终目标」推出，不能用用户填的路径算：
    // 用户填的若是已存在的目录，临时文件会建到上一级，替换时被 mv 挪进目录里却报告成功。
    let write_path_for = |final_path: &str| if staged { staging_path(final_path) } else { final_path.to_string() };
    // 已校验通过、只差替换这一步时记下最终目标。替换失败（多为断网）时只重试替换，不重传数据。
    let mut verified_target: Option<String> = None;
    let mut backoff = Backoff::new();
    let mut session: Option<(Session, u64)> = None;
    // 第一次连接优先接手预检刚用过的会话，省掉一次完整握手；之后的重连一律新建。
    let mut first_connect = true;
    // 分片大小跨重连保留：慢速链路上每次重连都从默认值重新试探，第一片就可能超时。
    let mut write_slice = INITIAL_WRITE_SLICE;

    loop {
        ensure_not_cancelled(shared.cancel, checkpoint)?;

        if session.is_none() {
            let connected = if std::mem::take(&mut first_connect) {
                pool::acquire(config)
            } else {
                connect_session(config)
            };
            match connected {
                Ok((s, abort_socket)) => session = Some((s, shared.aborts.register(abort_socket))),
                Err(err) => {
                    if shared.cancel.load(Ordering::Relaxed) {
                        return Err(UploadError::Cancelled);
                    }
                    if err.is_permanent() {
                        log::error!("connect failed permanently, stop retrying: {err}");
                        return Err(UploadError::Connect(err));
                    }
                    let retry_count = increment_retry_count(checkpoint)?;
                    let uploaded_bytes = shared.transferred();
                    let backoff_ms = backoff.next_delay_ms(uploaded_bytes);
                    let message = if uploaded_bytes > 0 {
                        crate::tr!(
                            format!(
                                "连接失败（已传 {uploaded_bytes} 字节），{backoff_ms}ms 后重连续传: {err}"
                            ),
                            format!(
                                "Connection failed ({uploaded_bytes} bytes sent), reconnecting to resume in {backoff_ms}ms: {err}"
                            )
                        )
                    } else {
                        crate::tr!(
                            format!("连接失败，{backoff_ms}ms 后重试: {err}"),
                            format!("Connection failed, retrying in {backoff_ms}ms: {err}")
                        )
                    };
                    log::warn!("upload retry #{retry_count}: {message}");
                    (callbacks.on_retry)(retry_count, backoff_ms, &message);
                    sleep_backoff::<UploadError>(shared.cancel, backoff_ms)?;
                    continue;
                }
            }
        }

        let (active_session, _) = session.as_ref().unwrap();
        let transfer_result = if let Some(final_path) = &verified_target {
            promote_staged_file(active_session, &write_path_for(final_path), final_path, local_size)
                .map(|()| final_path.clone())
        } else {
            lead_transfer(active_session, shared, resolved_remote_path, staged, force_overwrite, &mut write_slice)
                // 校验与传输放在同一个结果里：校验阶段的网络错误和传输中的一样，交给下面的重试分支
                // 重连后再校验一次，而不是让一个已经传完的文件以失败告终。
                .and_then(|final_path| {
                    // 逐块校验只能证明「写进去的等于当时读到的」。文件若在途中被改写，各块来自不同版本，
                    // 每一块都能通过校验，拼起来却不是任何一个版本；所以必须先确认源文件没变。
                    if local_fingerprint(shared.local_path)? != started_fingerprint {
                        log::error!("local file changed during upload: {}", shared.local_path.display());
                        return Err(UploadError::LocalFileChanged);
                    }
                    if should_skip_full_file_hash(shared.strict, checkpoint, local_size)? {
                        log::info!("all {local_size} bytes chunk-verified, skip full-file SHA-256");
                    } else {
                        (callbacks.on_progress)(local_size, local_size, "verifying", current_retry_count(checkpoint));
                        verify_upload_integrity(
                            active_session,
                            shared.local_path,
                            &write_path_for(&final_path),
                            local_size,
                            shared.cancel,
                            checkpoint,
                        )?;
                    }
                    Ok(final_path)
                })
        };

        match transfer_result {
            // 数据已校验、但还在临时文件里：下一轮只做替换。
            Ok(final_path) if staged && verified_target.is_none() => {
                log::info!("staged upload verified, replacing {final_path}");
                verified_target = Some(final_path);
            }
            Ok(_) => {
                finish_upload(checkpoint, callbacks, local_size, current_retry_count(checkpoint))?;
                // 会话还完好：留给紧接着的下一步（批量中的下一个文件、传完后命令）复用。
                if let Some((session, abort_id)) = session.take() {
                    if let Some(abort) = shared.aborts.remove(abort_id) {
                        pool::release(config, session, abort);
                    }
                }
                return Ok(());
            }
            Err(UploadError::Cancelled) => {
                checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?.status = CheckpointStatus::Failed;
                return Err(UploadError::Cancelled);
            }
            Err(err) => {
                if shared.cancel.load(Ordering::Relaxed) {
                    return Err(UploadError::Cancelled);
                }
                if is_permanent(&err) {
                    checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?.status =
                        CheckpointStatus::Failed;
                    return Err(err);
                }

                let retry_count = increment_retry_count(checkpoint)?;
                if let Some((_, abort_id)) = session.take() {
                    shared.aborts.remove(abort_id);
                }
                let backoff_ms = backoff.next_delay_ms(shared.transferred());
                let message = crate::tr!(
                    format!("传输中断（{err}），{backoff_ms}ms 后重连续传"),
                    format!("Transfer interrupted ({err}), reconnecting to resume in {backoff_ms}ms")
                );
                log::warn!("upload retry #{retry_count}: {message}");
                (callbacks.on_retry)(retry_count, backoff_ms, &message);
                sleep_backoff::<UploadError>(shared.cancel, backoff_ms)?;
            }
        }
    }
}

/// 主连接一轮的工作：必要时做准备，然后传到所有块完成为止，返回最终目标路径。
fn lead_transfer(
    session: &Session,
    shared: &UploadShared<'_>,
    resolved_remote_path: &str,
    staged: bool,
    force_overwrite: &AtomicBool,
    write_slice: &mut usize,
) -> Result<String, UploadError> {
    let sftp = session.sftp().map_err(|e| UploadError::Sftp(e.to_string()))?;
    let lock_target = || shared.target.lock().map_err(|e| UploadError::Sftp(e.to_string()));
    let existing = lock_target()?.clone();
    let (final_path, write_path) = match existing {
        Some(target) => target,
        None => {
            let target = prepare_upload(session, &sftp, shared, resolved_remote_path, staged, force_overwrite)?;
            *lock_target()? = Some(target.clone());
            target
        }
    };

    pump_pieces(session, &sftp, &write_path, shared, write_slice)?;
    if let Some(fatal) = shared.plan.take_fatal() {
        return Err(fatal);
    }

    // 所有块都已完成：用路径 stat 核对最终大小。
    let final_size = sftp
        .stat(Path::new(&write_path))
        .map_err(|e| UploadError::Sftp(e.to_string()))?
        .size
        .unwrap_or(0);
    if final_size != shared.local_size {
        return Err(UploadError::SizeMismatch { local: shared.local_size, remote: final_size });
    }
    Ok(final_path)
}

/// 传输开始前的一次性准备：确定最终目标、核对断点声称的进度是否还成立、把剩余部分排进队列。
/// 只在一次上传的第一次成功连接时做；之后主连接重连时，内存里的计划就是准确的进度。
fn prepare_upload(
    session: &Session,
    sftp: &Sftp,
    shared: &UploadShared<'_>,
    resolved_remote_path: &str,
    staged: bool,
    force_overwrite: &AtomicBool,
) -> Result<(String, String), UploadError> {
    let (checkpoint, local_size) = (shared.checkpoint, shared.local_size);
    let lock = || checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()));

    let final_path = resolve_upload_target(sftp, resolved_remote_path, shared.local_path)?;
    if final_path != resolved_remote_path {
        let mut cp = lock()?;
        cp.remote_path = final_path.clone();
        save_checkpoint(&cp).map_err(UploadError::Sftp)?;
        log::info!("resolved upload target: {final_path}");
    }
    ensure_remote_parent_directories(sftp, &final_path)?;
    let write_path = if staged { staging_path(&final_path) } else { final_path.clone() };

    // 只有「不存在」才视为没有数据；其他 stat 失败交给重试循环重连，
    // 否则一次超时就会把断点归零、把已传的数据清掉。
    let remote_size = match sftp.stat(Path::new(&write_path)) {
        Ok(stat) => Some(stat.size.unwrap_or(0)),
        Err(e) if is_sftp_not_found(&e) => None,
        Err(e) => {
            log::warn!("stat remote failed (will retry): path={write_path} err={e}");
            return Err(UploadError::Sftp(e.to_string()));
        }
    };

    let force = force_overwrite.swap(false, Ordering::Relaxed);
    let mut done = if force { RangeSet::default() } else { checkpoint_ranges(&*lock()?) };
    // 旧版断点直接写目标文件：目标比本地还大，说明它不是这次上传留下的，不能在上面接着写。
    if !staged && !force && remote_size.is_some_and(|size| size > local_size) {
        return Err(UploadError::RemoteLargerThanLocal { remote: remote_size.unwrap_or(0), local: local_size });
    }
    // 远端实际没有那么多数据（文件被外部删改、断点过期）：超出的进度作废。
    done.truncate(remote_size.unwrap_or(0));

    let fresh = done.is_empty();
    if fresh && staged && !force && local_size >= DELTA_MIN_BYTES {
        done = seed_from_existing_target(session, sftp, shared, &final_path, &write_path);
    }

    if done.is_empty() {
        if force {
            log::info!("overwriting existing remote file: {write_path}");
        }
        // 从头开始：建一个空文件，顺带清掉这个路径上可能残留的旧数据。
        sftp.open_mode(
            Path::new(&write_path),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
            NEW_FILE_MODE,
            OpenType::File,
        )
        .map_err(|e| UploadError::Sftp(format_remote_sftp_error(&e.to_string(), &write_path)))?;
    } else {
        if remote_size.is_some_and(|size| size > local_size) {
            truncate_remote_file(sftp, &write_path, local_size)?;
        }
        verify_resume_boundaries(session, sftp, shared, &write_path, &done)?;
    }

    {
        let mut cp = lock()?;
        let total = done.total();
        cp.uploaded_bytes = total;
        cp.verified_bytes = total;
        // 重新开始干活：上一轮（如校验读取失败）留下的失败标记不再成立。
        cp.status = CheckpointStatus::InProgress;
        cp.failure_reason = None;
        if fresh {
            // 从头开始时还没有任何未经校验的数据（从旧版本沿用的块是按哈希逐块比对过的）；
            // 之后只要有一块没校验就会被置回 false。
            cp.all_chunks_verified = true;
        }
        cp.done_ranges = done.clone();
        save_checkpoint(&cp).map_err(UploadError::Sftp)?;
    }

    // 每次开始都记录从哪里接着传：用户能在日志里看到断点续传确实生效、没有从头再来。
    log_resume_point(done.total(), local_size);
    shared.plan.start(&done.gaps(local_size));
    shared.report("uploading");
    Ok((final_path, write_path))
}

/// 目标位置已有旧版本时，只传发生变化的块：在服务器上把旧文件复制成临时文件，让服务器算出每一块的
/// 哈希，与本地逐块比对，相同的块直接算作已完成。对「同一个大文件改了一部分再传」的场景
/// （镜像、数据库导出、追加写的归档）能省下大部分流量；内容整体移位或重新压缩过的文件则基本没有相同的块，
/// 那时只是多花了服务器本地的一次复制。任何一步走不通都返回空集合，调用方按从头上传处理。
fn seed_from_existing_target(
    session: &Session,
    sftp: &Sftp,
    shared: &UploadShared<'_>,
    final_path: &str,
    write_path: &str,
) -> RangeSet {
    use crate::activity::{format_bytes, info, Category};
    let nothing = RangeSet::default();
    let on_activity = &shared.callbacks.on_activity;
    let Some(existing_size) = sftp
        .stat(Path::new(final_path))
        .ok()
        .filter(|stat| stat.is_file())
        .and_then(|stat| stat.size)
        .filter(|size| *size >= CHUNK_SIZE)
    else {
        return nothing;
    };
    let blocks = existing_size.min(shared.local_size) / CHUNK_SIZE;
    info(
        Category::Transfer,
        crate::tr!(
            format!("目标位置已有旧版本，正在比对哪些部分没有变化（{}）", format_bytes(blocks * CHUNK_SIZE)),
            format!("An older version exists at the target; checking which parts are unchanged ({})", format_bytes(blocks * CHUNK_SIZE))
        ),
    );
    if !remote_copy(session, final_path, write_path, existing_size, shared.cancel, on_activity) {
        log::info!("server-side copy unavailable, uploading {final_path} in full");
        return nothing;
    }
    let Some(remote_hashes) = remote_block_hashes(session, write_path, blocks, shared.cancel, on_activity) else {
        return nothing;
    };
    // 临时文件不能比新文件长：多出来的尾巴是旧版本的内容。
    if existing_size > shared.local_size && truncate_remote_file(sftp, write_path, shared.local_size).is_err() {
        return nothing;
    }

    let Ok(mut local_file) = File::open(shared.local_path) else { return nothing };
    let mut buffer = vec![0_u8; CHUNK_SIZE as usize];
    let mut same = RangeSet::default();
    for (index, remote_hash) in remote_hashes.iter().enumerate() {
        if shared.cancel.load(Ordering::Relaxed) || local_file.read_exact(&mut buffer).is_err() {
            return nothing;
        }
        on_activity();
        if sha256_bytes(&buffer) == *remote_hash {
            let start = index as u64 * CHUNK_SIZE;
            same.insert(start, start + CHUNK_SIZE);
        }
    }
    info(
        Category::Transfer,
        crate::tr!(
            format!("{} 与旧版本相同，无需重传", format_bytes(same.total())),
            format!("{} unchanged from the older version; no need to send it again", format_bytes(same.total()))
        ),
    );
    same
}

/// 续传前抽查：每个已完成区间的最后一块，远端内容必须与本地一致。
/// 断点可能已经过期（远端文件被别的程序改写、本地文件换了内容），在其上接着传只会得到一个坏文件。
fn verify_resume_boundaries(
    session: &Session,
    sftp: &Sftp,
    shared: &UploadShared<'_>,
    write_path: &str,
    done: &RangeSet,
) -> Result<(), UploadError> {
    let mut local_file = File::open(shared.local_path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    let mut remote_file = sftp
        .open_mode(Path::new(write_path), OpenFlags::READ, 0, OpenType::File)
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
    let mut hasher = BlockHasher::start(session, write_path);
    let mut buffer = vec![0_u8; CHUNK_SIZE as usize];
    for &(_, end) in done.ranges() {
        let start = align_down(end - 1);
        let len = (end - start) as usize;
        local_file.seek(SeekFrom::Start(start)).map_err(|e| UploadError::LocalIo(e.to_string()))?;
        local_file.read_exact(&mut buffer[..len]).map_err(|e| UploadError::LocalIo(e.to_string()))?;
        let remote_hash = remote_block_hash(&mut remote_file, &mut hasher, start, len as u64, shared)?;
        if sha256_bytes(&buffer[..len]) != remote_hash {
            return Err(UploadError::ResumeBoundaryMismatch { offset: end });
        }
        log::info!("resume boundary verified at offset {end}");
    }
    Ok(())
}

/// 远端 `[offset, offset+len)` 的 SHA-256：优先让服务器自己算（几乎不占带宽），
/// 这条路走不通时把这一块读回来算。
fn remote_block_hash(
    remote_file: &mut RemoteFile,
    hasher: &mut Option<BlockHasher>,
    offset: u64,
    len: u64,
    shared: &UploadShared<'_>,
) -> Result<[u8; 32], UploadError> {
    let on_activity = &shared.callbacks.on_activity;
    on_activity();
    if let Some(active) = hasher.as_mut() {
        match active.hash(offset, len, shared.cancel) {
            Ok(hash) => return Ok(hash),
            Err(e) => {
                log::info!("remote block hasher unavailable ({e}), reading blocks back instead");
                *hasher = None;
            }
        }
    }
    sha256_remote_handle_range(remote_file, offset, len, Some(on_activity))
}

/// 每写完一块立即核对远端内容；不一致则报错，这一块会被重新传。
fn verify_written_block(
    remote_file: &mut RemoteFile,
    hasher: &mut Option<BlockHasher>,
    local_bytes: &[u8],
    offset: u64,
    shared: &UploadShared<'_>,
) -> Result<(), UploadError> {
    let len = local_bytes.len() as u64;
    let local_hash = sha256_bytes(local_bytes);
    let used_remote_hasher = hasher.is_some();
    let mut remote_hash = remote_block_hash(remote_file, hasher, offset, len, shared)?;
    if local_hash != remote_hash && used_remote_hasher && hasher.is_some() {
        // 服务器端算出来的不一致：下结论前先按老办法读回来确认一次，
        // 免得因为那条捷径自身的问题（例如工具输出异常）把好数据判成坏的；确认是它的问题就不再用它。
        remote_hash = sha256_remote_handle_range(remote_file, offset, len, Some(&shared.callbacks.on_activity))?;
        if local_hash == remote_hash {
            log::warn!("remote block hasher disagreed with read-back at offset {offset}; disabling it");
            *hasher = None;
        }
    }
    if local_hash == remote_hash {
        log::info!("chunk verified offset={offset} len={len} hash={}", hex_encode(&local_hash));
        return Ok(());
    }
    log::error!(
        "chunk hash mismatch offset={offset} len={len} local={} remote={}",
        hex_encode(&local_hash),
        hex_encode(&remote_hash)
    );
    Err(UploadError::ChunkVerifyMismatch { offset, len })
}

/// 从队列里不断领任务并传输，直到全部完成。主连接和辅助连接共用。
/// 一个连接只打开一次远端文件，所有块复用同一个句柄：此前每块都要 stat + open + close，
/// 读回校验再 open + close，每 MB 多出约 5 次往返，高延迟链路上 1 GB 文件要多花二十多分钟。
fn pump_pieces(
    session: &Session,
    sftp: &Sftp,
    write_path: &str,
    shared: &UploadShared<'_>,
    write_slice: &mut usize,
) -> Result<(), UploadError> {
    let mut local_file = File::open(shared.local_path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    let mut remote_file: Option<RemoteFile> = None;
    // 服务器端哈希进程按需启动（只有逐块校验才用得到），每个连接一个。
    let mut hasher: Option<BlockHasher> = None;
    let mut hasher_started = false;
    let mut chunk_buffer = vec![0_u8; CHUNK_SIZE as usize];

    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return Err(UploadError::Cancelled);
        }
        let mut piece = match shared.plan.claim() {
            Claim::Piece(piece) => piece,
            Claim::Wait => {
                keepalive_while_idle(session);
                thread::sleep(PLAN_POLL);
                continue;
            }
            Claim::Done => break,
        };
        if remote_file.is_none() {
            remote_file = Some(open_remote_for_upload(sftp, write_path)?);
        }
        let remote = remote_file.as_mut().unwrap();

        while piece.next < piece.end {
            if shared.cancel.load(Ordering::Relaxed) {
                return Err(UploadError::Cancelled);
            }
            let offset = piece.next;
            let chunk_len = CHUNK_SIZE.min(piece.end - offset) as usize;
            let chunk = &mut chunk_buffer[..chunk_len];
            local_file.seek(SeekFrom::Start(offset)).map_err(|e| UploadError::LocalIo(e.to_string()))?;
            local_file.read_exact(chunk).map_err(|e| UploadError::LocalIo(e.to_string()))?;

            let write_started = Instant::now();
            write_remote_chunk(remote, offset, chunk, *write_slice, |absolute| {
                piece.set_partial(absolute - offset);
                shared.report("uploading");
            })?;
            *write_slice = next_write_slice(chunk_len, write_started.elapsed());

            let read_back = shared.strict.load(Ordering::Relaxed);
            if read_back {
                if !std::mem::replace(&mut hasher_started, true) {
                    hasher = BlockHasher::start(session, write_path);
                }
                shared.report("chunk_verifying");
                verify_written_block(remote, &mut hasher, chunk, offset, shared)?;
            }

            shared.complete_block(offset, chunk_len as u64, read_back)?;
            piece.advance(chunk_len as u64);
            shared.report("uploading");
        }
    }

    // 关闭句柄让服务器完成所有写入，之后才能用路径 stat 核对最终大小。
    if let Some(mut file) = remote_file {
        file.close().map_err(|e| UploadError::Sftp(e.to_string()))?;
    }
    Ok(())
}

/// 辅助连接：只在有任务可领时才建立连接，默默地传、默默地重连；它的失败不打扰用户
/// （主连接还在工作，进度仍在前进），只有「重连也不会好」的错误才交给主连接上报。
fn helper_loop(config: &ConnectionConfig, shared: &UploadShared<'_>, index: usize) {
    let mut backoff = Backoff::new();
    let mut write_slice = INITIAL_WRITE_SLICE;
    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return;
        }
        match shared.plan.status() {
            PlanStatus::Done => return,
            PlanStatus::Wait => {
                thread::sleep(PLAN_POLL);
                continue;
            }
            PlanStatus::Work => {}
        }
        let Some((_, write_path)) = shared.target.lock().ok().and_then(|target| target.clone()) else {
            return;
        };

        let result = connect_session(config).map_err(UploadError::Connect).and_then(|(session, abort)| {
            let abort_id = shared.aborts.register(abort);
            let result = session
                .sftp()
                .map_err(|e| UploadError::Sftp(e.to_string()))
                .and_then(|sftp| pump_pieces(&session, &sftp, &write_path, shared, &mut write_slice));
            shared.aborts.remove(abort_id);
            result
        });
        match result {
            Ok(()) | Err(UploadError::Cancelled) => return,
            Err(err) if is_permanent(&err) => {
                log::error!("parallel upload connection {index} failed permanently: {err}");
                // 连不上（如服务器限制了同一来源的连接数）只是少一条并行连接；
                // 传输过程中的永久错误（权限、路径）则对整个任务成立，交给主连接上报。
                if !matches!(err, UploadError::Connect(_)) {
                    shared.plan.fail(err);
                }
                return;
            }
            Err(err) => {
                let delay = backoff.next_delay_ms(shared.transferred());
                log::warn!("parallel upload connection {index} interrupted ({err}), retrying in {delay}ms");
                if sleep_backoff::<UploadError>(shared.cancel, delay).is_err() {
                    return;
                }
            }
        }
    }
}

fn finish_upload(
    checkpoint: &Mutex<UploadCheckpoint>,
    callbacks: &UploadCallbacks,
    local_size: u64,
    retry_count: u32,
) -> Result<(), UploadError> {
    {
        let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
        cp.uploaded_bytes = local_size;
        cp.status = CheckpointStatus::Completed;
    }
    (callbacks.on_progress)(local_size, local_size, "completed", retry_count);
    log::info!("upload completed and verified, retries={retry_count}");
    Ok(())
}

fn verify_upload_integrity(
    session: &Session,
    local_path: &Path,
    remote_path: &str,
    local_size: u64,
    cancel_flag: &AtomicBool,
    checkpoint: &Mutex<UploadCheckpoint>,
) -> Result<u32, UploadError> {
    ensure_not_cancelled(cancel_flag, checkpoint)?;

    let local_hash = sha256_local_file(session, local_path, cancel_flag, checkpoint)?;
    let sftp = session.sftp().map_err(|e| UploadError::Sftp(e.to_string()))?;
    // 优先让服务器自己算：整个文件读回来再算等于把流量翻倍。走不通（不能执行命令等）才读回。
    let remote_result = match remote_file_sha256(session, remote_path, local_size, cancel_flag) {
        Some(hash) => Ok(hash),
        None => sha256_remote_file(&sftp, remote_path, local_size, cancel_flag, checkpoint),
    };
    let remote_hash = match remote_result {
        Ok(hash) => hash,
        Err(UploadError::Sftp(detail)) => {
            log::warn!("full-file remote read failed during verify: {detail}");
            let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
            cp.failure_reason = Some("verify_read_failed".into());
            cp.status = CheckpointStatus::Failed;
            let _ = save_checkpoint(&cp);
            // 这个错误会进入重试循环自动重连再校验；只有用户取消或退出应用时才会停在这里。
            return Err(UploadError::Sftp(crate::tr!(
                format!("数据已传完，最终校验时读取远端失败: {detail}"),
                format!("All data was sent, but reading the remote file for final verification failed: {detail}")
            )));
        }
        Err(err) => return Err(err),
    };

    if local_hash != remote_hash {
        log::error!(
            "hash mismatch local={} remote={}",
            hex_encode(&local_hash),
            hex_encode(&remote_hash)
        );
        let _ = sftp.unlink(Path::new(remote_path));
        {
            let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
            cp.uploaded_bytes = 0;
            cp.verified_bytes = 0;
            cp.done_ranges = RangeSet::default();
            cp.failure_reason = Some("hash_mismatch".into());
            cp.status = CheckpointStatus::Failed;
            let _ = save_checkpoint(&cp);
        }
        log::warn!("remote file deleted after hash mismatch: {remote_path}");
        return Err(UploadError::HashMismatch);
    }

    log::info!("sha256 verified: {}", hex_encode(&local_hash));
    Ok(current_retry_count(checkpoint))
}

fn truncate_remote_file(sftp: &Sftp, remote_path: &str, size: u64) -> Result<(), UploadError> {
    let stat = FileStat {
        size: Some(size),
        uid: None,
        gid: None,
        perm: None,
        atime: None,
        mtime: None,
    };
    sftp.setstat(Path::new(remote_path), stat)
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
    Ok(())
}

/// 在已打开的句柄上读回 [offset, offset+len) 并计算 SHA-256（不重新打开文件）。
fn sha256_remote_handle_range(
    remote_file: &mut RemoteFile,
    offset: u64,
    len: u64,
    on_read: Option<&dyn Fn()>,
) -> Result<[u8; 32], UploadError> {
    remote_file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| UploadError::Sftp(e.to_string()))?;

    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];

    while remaining > 0 {
        if let Some(defer) = on_read {
            defer();
        }
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        let read_bytes = remote_file
            .read(&mut buffer[..to_read])
            .map_err(|e| UploadError::Sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(UploadError::Sftp(
                crate::tr!(
                    "读取远端块用于校验时意外结束",
                    "Remote chunk ended unexpectedly while reading for verification"
                )
                .into(),
            ));
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes as u64;
    }

    Ok(hasher.finalize().into())
}

/// 每次（重）连接后记录从哪里开始：用户能在日志里看到断点续传确实生效、没有从头再来。
pub(super) fn log_resume_point(offset: u64, total: u64) {
    use crate::activity::{format_bytes, info, Category};
    if offset > 0 {
        info(
            Category::Transfer,
            crate::tr!(
                format!("从 {} 处续传（共 {}）", format_bytes(offset), format_bytes(total)),
                format!("Resuming from {} (of {})", format_bytes(offset), format_bytes(total))
            ),
        );
    } else {
        info(Category::Transfer, crate::tr!(
                format!("从头开始传输，共 {}", format_bytes(total)),
                format!("Starting transfer from the beginning, {} total", format_bytes(total))
            ));
    }
}

/// 新建文件的权限（实际结果还会被服务器的 umask 收紧；替换已有文件时会改用旧文件的权限）。
const NEW_FILE_MODE: i32 = 0o644;

/// 打开实际写入的文件。写入与读回走同一个句柄：同一 SFTP 通道上的请求由服务器按序处理，
/// 读到的一定是刚写入的数据。不带 TRUNCATE：文件由准备阶段建好，各连接往各自的位置写。
fn open_remote_for_upload(sftp: &Sftp, remote_path: &str) -> Result<RemoteFile, UploadError> {
    sftp.open_mode(
        Path::new(remote_path),
        OpenFlags::WRITE | OpenFlags::READ | OpenFlags::CREATE,
        NEW_FILE_MODE,
        OpenType::File,
    )
    .map_err(|e| UploadError::Sftp(e.to_string()))
}

/// 根据上一块的实测写入速度决定下一块的分片大小（取 2 的幂，避免尺寸频繁抖动）。
fn next_write_slice(bytes: usize, elapsed: std::time::Duration) -> usize {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return MAX_WRITE_SLICE;
    }
    let target = (bytes as f64 / secs * TARGET_SLICE_SECS) as usize;
    let power_of_two = if target == 0 {
        MIN_WRITE_SLICE
    } else {
        1 << (usize::BITS - 1 - target.leading_zeros())
    };
    power_of_two.clamp(MIN_WRITE_SLICE, MAX_WRITE_SLICE)
}

fn write_remote_chunk<F: FnMut(u64)>(
    remote_file: &mut RemoteFile,
    offset: u64,
    data: &[u8],
    slice: usize,
    mut on_partial_progress: F,
) -> Result<(), UploadError> {
    // seek 只改 libssh2 本地记录的写入位置，不产生网络往返。
    remote_file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| UploadError::Sftp(e.to_string()))?;

    let mut written = 0usize;
    while written < data.len() {
        let end = (written + slice).min(data.len());
        remote_file
            .write_all(&data[written..end])
            .map_err(|e| UploadError::Sftp(e.to_string()))?;
        written = end;
        on_partial_progress(offset + written as u64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn appends_filename_for_directory_remote_path() {
        let local = PathBuf::from("/tmp/demo.jar");
        let resolved = resolve_remote_path("/home/user/dsn/", &local).unwrap();
        assert_eq!(resolved, "/home/user/dsn/demo.jar");
    }

    #[test]
    fn write_slice_adapts_to_measured_speed() {
        use std::time::Duration;
        // 快速链路：1 MB 只用 0.1 s → 用满上限，尽量多的数据在途。
        assert_eq!(next_write_slice(1 << 20, Duration::from_millis(100)), MAX_WRITE_SLICE);
        // 约 300 KB/s：2 秒约 600 KB → 取 2 的幂 512 KB。
        assert_eq!(next_write_slice(1 << 20, Duration::from_millis(3400)), 512 * 1024);
        // 弱网 20 KB/s：退到下限，保证进度仍然频繁刷新。
        assert_eq!(next_write_slice(1 << 20, Duration::from_secs(50)), MIN_WRITE_SLICE);
        assert_eq!(next_write_slice(1 << 20, Duration::ZERO), MAX_WRITE_SLICE);
    }

    #[test]
    fn aligns_offset_down_to_chunk_boundary() {
        assert_eq!(align_down(0), 0);
        assert_eq!(align_down(CHUNK_SIZE), CHUNK_SIZE);
        assert_eq!(align_down(CHUNK_SIZE + 2072), CHUNK_SIZE);
        assert_eq!(align_down(33_556_504), 33_554_432);
    }

    #[test]
    fn socket_timeout_after_progress_is_retryable() {
        let wrapped = format_remote_sftp_error("Timed out waiting on socket", "/tmp/a.jar");
        assert!(!is_permanent_sftp_error(&wrapped));
        assert!(!is_permanent_sftp_error("Timed out waiting on socket"));
    }
}
