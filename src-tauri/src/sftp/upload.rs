use super::errors::is_permanent_sftp_error;
use super::transfer_common::{
    align_down, bounded_read_len, current_retry_count, effective_verified_bytes, ensure_not_cancelled,
    increment_retry_count, sha256_bytes, sha256_local_file, sha256_remote_file,
    should_skip_full_file_hash, sleep_backoff, TransferCheckpoint, TransferError, CHUNK_SIZE,
    HASH_READ_BUFFER_SIZE, hex_encode,
};
use super::client::{
    connect_session, is_sftp_not_found, hurry_failed_session, is_responsive, keepalive_while_idle, AbortHandles, AbortUnlessDone, ConnectError,
    ConnectionConfig,
};
use super::plan::{
    effective_connections, watch_for_stalls, Claim, PlanStatus, RangeSet, TransferPlan, DELTA_MIN_BYTES,
    PIECE_BYTES,
};
use super::remote_hash::{remote_block_hashes, remote_copy, remote_file_sha256, BlockHasher};
use super::browse::join_remote_path;
use super::pool;
use super::preflight::shell_quote;
pub use super::transfer_common::{is_staging_file, staging_path};
use crate::checkpoint::{save_checkpoint, CheckpointStatus, UploadCheckpoint};
use crate::retry::{whole_seconds, Backoff};
use sha2::{Digest, Sha256};
use ssh2::{File as RemoteFile, FileStat, OpenFlags, OpenType, Session, Sftp};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 写入分片大小的上下限。libssh2 会把单次写入的数据流水线发送，但每次写入都要等这一批全部被确认
/// 才返回，也就是每个分片之间连接会空转一个往返；分片越大，空转占的比例越小：
/// 实测 100 ms 往返延迟下 5 MB 上传，64 KB 分片 12.5 s、256 KB 6.0 s、1 MB 4.2 s。
/// 但进度只在每个分片写完时刷新，慢速链路上大分片会让进度条长时间不动、被误判为无响应，
/// 所以按实测速度自适应：让每个分片大约 TARGET_SLICE_SECS 秒写完。
/// 上限是一整段任务：快而延迟高的链路上（跨境专线等）若仍以 1 MB 为上限，每传 1 MB 就要
/// 空等一个往返写确认、再等一个往返校验，实际速度会被压到带宽的几分之一。
const MIN_WRITE_SLICE: usize = 16 * 1024;
const MAX_WRITE_SLICE: usize = PIECE_BYTES as usize;
/// 分片每轮最多增长到上一轮写入量的几倍。
const SLICE_GROWTH_LIMIT: usize = 2;
/// 出错重连后把分片缩小到原来的几分之一：链路可能刚变差，沿用之前的大分片第一片就可能超时。
const SLICE_SHRINK_ON_ERROR: usize = 4;

/// 一轮连续写入多少字节：大约一个分片的量，凑成整块（断点按块记录），至少一块，不超过这一段剩下的。
/// 一轮写完才校验、才记入断点，所以它也是一次断线最多白传的量——约 TARGET_SLICE_SECS 秒的数据。
fn run_length(write_slice: usize, remaining: u64) -> u64 {
    let blocks = (write_slice as u64).div_ceil(CHUNK_SIZE).max(1);
    (blocks * CHUNK_SIZE).min(remaining)
}
/// 第一块还没有测速数据，取中间值。
const INITIAL_WRITE_SLICE: usize = 256 * 1024;
const TARGET_SLICE_SECS: f64 = 2.0;
/// 每一轮写完都要空等约两个往返（等写确认、等校验结果）。往返延迟高时把一轮拉长到延迟的这么多倍，
/// 让空等只占百分之几；延迟低时仍是 TARGET_SLICE_SECS。
const ROUND_RTT_MULTIPLE: f64 = 40.0;
/// 一轮最长多少秒：再长的话进度刷新太稀，断线时白传的也多。
const MAX_ROUND_SECS: f64 = 4.0;

/// 按实测往返延迟决定一轮写入大约持续多久。
fn round_secs_for(rtt: Duration) -> f64 {
    (rtt.as_secs_f64() * ROUND_RTT_MULTIPLE).clamp(TARGET_SLICE_SECS, MAX_ROUND_SECS)
}

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
/// 连接空等超过这么久，再用它之前要先探活。
const IDLE_PROBE_AFTER: Duration = Duration::from_secs(5);

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
    /// 一轮写入的目标时长（毫秒），由准备阶段按实测往返延迟设定，各连接共用。
    round_millis: std::sync::atomic::AtomicU64,
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

    /// 一轮数据已写入（并按当前模式校验过）：记入断点并落盘。
    fn complete_range(&self, offset: u64, len: u64, read_back: bool) -> Result<(), UploadError> {
        let mut cp = self.checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
        cp.done_ranges.insert(offset, offset + len);
        let total = cp.done_ranges.total();
        cp.uploaded_bytes = total;
        cp.verified_bytes = total;
        cp.all_chunks_verified &= read_back;
        save_checkpoint(&cp).map_err(checkpoint_error)
    }

    fn transferred(&self) -> u64 {
        self.checkpoint.lock().map(|cp| cp.uploaded_bytes).unwrap_or(0)
    }
}

/// 断点写不进本地磁盘（磁盘满、目录不可写）。这是本机的问题，重连服务器解决不了；
/// 而断点落不了盘就无法保证续传，所以按本地错误停下来，而不是伪装成一个会被无限重试的「SFTP 错误」。
fn checkpoint_error(detail: String) -> UploadError {
    UploadError::LocalIo(crate::tr!(
        format!("无法保存断点文件（{detail}）"),
        format!("cannot save the checkpoint file ({detail})")
    ))
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
        round_millis: std::sync::atomic::AtomicU64::new((TARGET_SLICE_SECS * 1000.0) as u64),
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
        // 后台盯着卡死的连接；发送端在本作用域结束时被丢弃，它随即退出。
        let (stop_watch, watch_stopped) = std::sync::mpsc::channel::<()>();
        {
            let shared = &shared;
            scope.spawn(move || watch_for_stalls(&shared.plan, shared.aborts, watch_stopped));
        }
        let result =
            lead_loop(&config, &shared, &resolved_remote_path, staged, started_fingerprint, &force_overwrite);
        drop(stop_watch);
        shared.plan.finish();
        // 无论成败都打断还登记着的连接（成功时主连接已交还连接池、不在其中，剩下的只有辅助连接）：
        // 辅助连接若正卡在一条已经死掉的连接上做收尾，会让一个早已传完并校验通过的任务迟迟不结束。
        aborts.abort_all();
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
                    let wait = whole_seconds(backoff_ms);
                    let message = if uploaded_bytes > 0 {
                        crate::tr!(
                            format!(
                                "连接失败（已传 {uploaded_bytes} 字节），{wait} 秒后重连续传: {err}"
                            ),
                            format!(
                                "Connection failed ({uploaded_bytes} bytes sent), reconnecting to resume in {wait} s: {err}"
                            )
                        )
                    } else {
                        crate::tr!(
                            format!("连接失败，{wait} 秒后重试: {err}"),
                            format!("Connection failed, retrying in {wait} s: {err}")
                        )
                    };
                    log::warn!("upload retry #{retry_count}: {message}");
                    (callbacks.on_retry)(retry_count, backoff_ms, &message);
                    sleep_backoff::<UploadError>(shared.cancel, backoff_ms)?;
                    continue;
                }
            }
        }

        let (active_session, connection) = session.as_ref().unwrap();
        let transfer_result = if let Some(final_path) = &verified_target {
            promote_staged_file(active_session, &write_path_for(final_path), final_path, local_size)
                .map(|()| final_path.clone())
        } else {
            lead_transfer(active_session, shared, resolved_remote_path, staged, force_overwrite, &mut write_slice, *connection)
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
                if let Some((failed_session, abort_id)) = session.take() {
                    // 先打断再丢弃：会话析构时还要向服务器道别，在一条死连接上会空等一次超时。
                    shared.aborts.abort(abort_id);
                    hurry_failed_session(&failed_session);
                    drop(failed_session);
                    shared.aborts.remove(abort_id);
                }
                write_slice = (write_slice / SLICE_SHRINK_ON_ERROR).max(MIN_WRITE_SLICE);
                let backoff_ms = backoff.next_delay_ms(shared.transferred());
                let wait = whole_seconds(backoff_ms);
                let message = crate::tr!(
                    format!("传输中断（{err}），{wait} 秒后重连续传"),
                    format!("Transfer interrupted ({err}), reconnecting to resume in {wait} s")
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
    connection: u64,
) -> Result<String, UploadError> {
    let sftp = session.sftp().map_err(|e| UploadError::Sftp(e.to_string()))?;
    // 在 sftp 之后声明：出错时先打断连接，sftp 的析构才不会在一条死连接上空等。
    let teardown = AbortUnlessDone::new(shared.aborts, session, connection);
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

    pump_pieces(session, &sftp, &write_path, shared, write_slice, connection)?;
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
    teardown.done();
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
        save_checkpoint(&cp).map_err(checkpoint_error)?;
        log::info!("resolved upload target: {final_path}");
    }
    ensure_remote_parent_directories(sftp, &final_path)?;
    let write_path = if staged { staging_path(&final_path) } else { final_path.clone() };

    // 只有「不存在」才视为没有数据；其他 stat 失败交给重试循环重连，
    // 否则一次超时就会把断点归零、把已传的数据清掉。
    // 顺便给这次 stat 计时，当作链路的往返延迟（它只有一问一答）。
    let asked = Instant::now();
    let stat_result = sftp.stat(Path::new(&write_path));
    let round_secs = round_secs_for(asked.elapsed());
    shared.round_millis.store((round_secs * 1000.0) as u64, Ordering::Relaxed);
    let remote_size = match stat_result {
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
        save_checkpoint(&cp).map_err(checkpoint_error)?;
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

/// 每写完一轮立即逐块核对远端内容；有一块不一致就报错，这一轮会被重新传。
/// 优先让服务器一次算出这一轮所有块的哈希（请求连发、回复连收，总共约一个往返），
/// 这条路走不通时逐块读回来算。
fn verify_written_run(
    remote_file: &mut RemoteFile,
    hasher: &mut Option<BlockHasher>,
    local_bytes: &[u8],
    offset: u64,
    shared: &UploadShared<'_>,
    on_activity: &dyn Fn(),
) -> Result<(), UploadError> {
    let blocks: Vec<(u64, &[u8])> = local_bytes
        .chunks(CHUNK_SIZE as usize)
        .enumerate()
        .map(|(index, bytes)| (offset + index as u64 * CHUNK_SIZE, bytes))
        .collect();
    on_activity();
    let from_server = hasher.as_mut().and_then(|active| {
        let ranges: Vec<(u64, u64)> = blocks.iter().map(|(start, bytes)| (*start, bytes.len() as u64)).collect();
        match active.hash_many(&ranges, shared.cancel) {
            Ok(hashes) => Some(hashes),
            Err(e) => {
                log::info!("remote block hasher unavailable ({e}), reading blocks back instead");
                None
            }
        }
    });
    if from_server.is_none() {
        *hasher = None;
    }

    for (index, (start, bytes)) in blocks.iter().enumerate() {
        let len = bytes.len() as u64;
        let local_hash = sha256_bytes(bytes);
        let mut remote_hash = match &from_server {
            Some(hashes) => hashes[index],
            None => sha256_remote_handle_range(remote_file, *start, len, Some(on_activity))?,
        };
        if local_hash != remote_hash && from_server.is_some() {
            // 服务器端算出来的不一致：下结论前先按老办法读回来确认一次，免得因为那条捷径自身的
            // 问题（例如工具输出异常）把好数据判成坏的；确认是它的问题就不再用它。
            remote_hash = sha256_remote_handle_range(remote_file, *start, len, Some(on_activity))?;
            if local_hash == remote_hash {
                log::warn!("remote block hasher disagreed with read-back at offset {start}; disabling it");
                *hasher = None;
            }
        }
        if local_hash != remote_hash {
            log::error!(
                "chunk hash mismatch offset={start} len={len} local={} remote={}",
                hex_encode(&local_hash),
                hex_encode(&remote_hash)
            );
            return Err(UploadError::ChunkVerifyMismatch { offset: *start, len });
        }
        log::debug!("chunk verified offset={start} len={len} hash={}", hex_encode(&local_hash));
    }
    Ok(())
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
    connection: u64,
) -> Result<(), UploadError> {
    let mut local_file = File::open(shared.local_path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    let mut remote_file: Option<RemoteFile> = None;
    // 服务器端哈希进程按需启动（只有逐块校验才用得到），每个连接一个。
    let mut hasher: Option<BlockHasher> = None;
    let mut hasher_started = false;
    // 在文件句柄和哈希通道之后声明，出错时先于它们执行（见 AbortUnlessDone）。
    let teardown = AbortUnlessDone::new(shared.aborts, session, connection);
    let mut idle_since: Option<Instant> = None;
    // 一轮最多写一整段任务，缓冲区按这个上限分配一次。
    let mut run_buffer = vec![0_u8; PIECE_BYTES as usize];

    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return Err(UploadError::Cancelled);
        }
        // 空等了一阵之后、要接新任务或收尾之前，先确认这条连接还通（原因见 is_responsive）。
        // 不探的话，最后一段任务会被一条条早已卡死的空闲连接轮流抢走，每条各白等一次操作超时。
        if idle_since.is_some_and(|since| since.elapsed() >= IDLE_PROBE_AFTER)
            && shared.plan.status() != PlanStatus::Wait
        {
            if !is_responsive(session) {
                return Err(UploadError::Sftp(
                    crate::tr!("连接在空闲期间已经失效", "The connection was lost while it was idle").into(),
                ));
            }
            idle_since = None;
        }
        let mut piece = match shared.plan.claim(connection) {
            Claim::Piece(piece) => piece,
            Claim::Wait => {
                idle_since.get_or_insert_with(Instant::now);
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
            let run_len = run_length(*write_slice, piece.end - offset) as usize;
            log::debug!("connection {connection}: writing {run_len} bytes at {offset} (slice {})", *write_slice);
            let run = &mut run_buffer[..run_len];
            local_file.seek(SeekFrom::Start(offset)).map_err(|e| UploadError::LocalIo(e.to_string()))?;
            local_file.read_exact(run).map_err(|e| UploadError::LocalIo(e.to_string()))?;

            let write_started = Instant::now();
            write_remote_chunk(remote, offset, run, *write_slice, |absolute| {
                piece.set_partial(absolute - offset);
                shared.report("uploading");
            })?;
            // 每轮最多比上一轮翻一倍：一次测速偏高（刚连上、缓冲区还空）就直接跳到上限的话，
            // 下一轮可能一口气写出好几秒的数据，取消要等它写完才生效，断线时白传的也多。
            let round_secs = shared.round_millis.load(Ordering::Relaxed) as f64 / 1000.0;
            *write_slice =
                next_write_slice(run_len, write_started.elapsed(), round_secs).min(run_len * SLICE_GROWTH_LIMIT);

            let read_back = shared.strict.load(Ordering::Relaxed);
            if read_back {
                if !std::mem::replace(&mut hasher_started, true) {
                    hasher = BlockHasher::start(session, write_path);
                }
                shared.report("chunk_verifying");
                // 读回校验不产生字节进度：除了给界面的无响应检测报平安，也要让调度知道这条连接还活着。
                let alive = || {
                    (shared.callbacks.on_activity)();
                    piece.touch();
                };
                verify_written_run(remote, &mut hasher, run, offset, shared, &alive)?;
            }

            log::debug!("connection {connection}: {run_len} bytes at {offset} written and checked");
            shared.complete_range(offset, run_len as u64, read_back)?;
            piece.advance(run_len as u64);
            shared.report("uploading");
        }
    }

    log::debug!("connection {connection}: no more pieces, closing the remote file");
    // 关闭句柄让服务器完成所有写入，之后才能用路径 stat 核对最终大小。
    if let Some(mut file) = remote_file {
        file.close().map_err(|e| UploadError::Sftp(e.to_string()))?;
    }
    teardown.done();
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
            log::debug!("parallel upload thread {index} now uses connection {abort_id}");
            let result = session.sftp().map_err(|e| UploadError::Sftp(e.to_string())).and_then(|sftp| {
                let result = pump_pieces(&session, &sftp, &write_path, shared, &mut write_slice, abort_id);
                log::debug!("connection {abort_id}: pump returned (ok={})", result.is_ok());
                result
            });
            log::debug!("connection {abort_id}: sftp channel released");
            shared.aborts.remove(abort_id);
            drop(session);
            log::debug!("connection {abort_id}: session released");
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
                write_slice = (write_slice / SLICE_SHRINK_ON_ERROR).max(MIN_WRITE_SLICE);
                log::warn!("parallel upload thread {index} interrupted ({err}), retrying in {delay}ms");
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
        // 读取大小随剩余量收紧：读回校验的范围之外的预读数据用不上，换位置时会被整批作废。
        let to_read = bounded_read_len((buffer.len() as u64).min(remaining) as usize, remaining);
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

/// 根据上一轮的实测写入速度决定下一个分片的大小（取 2 的幂，避免尺寸频繁抖动）。
/// 向上取整：实测速度里含有「等确认」的空转时间，总是略低于链路的真实速度；向下取整的话，
/// 分片会卡在比目标小一档的地方上不去（例如 1 MB/s 的链路永远停在 1 MB，每块都要空等一次）。
fn next_write_slice(bytes: usize, elapsed: std::time::Duration, round_secs: f64) -> usize {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        return MAX_WRITE_SLICE;
    }
    let target = (bytes as f64 / secs * round_secs) as usize;
    target.next_power_of_two().clamp(MIN_WRITE_SLICE, MAX_WRITE_SLICE)
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
        assert_eq!(next_write_slice(1 << 20, Duration::from_millis(100), TARGET_SLICE_SECS), MAX_WRITE_SLICE);
        // 约 300 KB/s：2 秒约 600 KB → 向上取 2 的幂，1 MB。
        assert_eq!(next_write_slice(1 << 20, Duration::from_millis(3400), TARGET_SLICE_SECS), 1 << 20);
        // 弱网 20 KB/s：2 秒约 40 KB → 64 KB，进度仍然几秒刷新一次。
        assert_eq!(next_write_slice(1 << 20, Duration::from_secs(50), TARGET_SLICE_SECS), 64 * 1024);
        // 每秒只有 1 KB 左右：退到下限，一个分片也只要十几秒，不会被当成卡死。
        assert_eq!(next_write_slice(64 * 1024, Duration::from_secs(60), TARGET_SLICE_SECS), MIN_WRITE_SLICE);
        assert_eq!(next_write_slice(1 << 20, Duration::ZERO, TARGET_SLICE_SECS), MAX_WRITE_SLICE);
    }

    #[test]
    fn rounds_stretch_with_latency_within_limits() {
        use std::time::Duration;
        assert_eq!(round_secs_for(Duration::from_millis(2)), TARGET_SLICE_SECS);
        assert_eq!(round_secs_for(Duration::from_millis(75)), 3.0);
        assert_eq!(round_secs_for(Duration::from_millis(600)), MAX_ROUND_SECS);
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
