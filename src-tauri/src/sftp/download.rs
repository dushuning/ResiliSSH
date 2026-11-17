use super::errors::is_permanent_sftp_error;
use super::pool;
use super::transfer_common::{
    staging_path,
    align_down, bounded_read_len, current_retry_count, effective_verified_bytes, ensure_not_cancelled,
    increment_retry_count, sha256_bytes, sha256_local_file, sha256_remote_file,
    should_skip_full_file_hash, sleep_backoff, TransferCheckpoint, TransferError, CHUNK_SIZE,
    HASH_READ_BUFFER_SIZE,
};
use super::client::{
    connect_session, is_sftp_not_found, hurry_failed_session, keepalive_while_idle, AbortHandles, AbortUnlessDone, ConnectError,
    ConnectionConfig,
};
use super::plan::{
    effective_connections, Claim, PlanStatus, RangeSet, TransferPlan, DELTA_MIN_BYTES, MAX_CONNECTIONS,
};
use super::remote_hash::{remote_block_hashes, remote_file_sha256, BlockHasher};
use crate::checkpoint::CheckpointStatus;
use crate::download_checkpoint::{save_download_checkpoint, DownloadCheckpoint};
use crate::retry::Backoff;
use sha2::{Digest, Sha256};
use ssh2::{File as RemoteFile, OpenFlags, OpenType, Session, Sftp};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub struct DownloadCallbacks {
    pub on_progress: Box<dyn Fn(u64, u64, &str, u32) + Send + Sync>,
    pub on_retry: Box<dyn Fn(u32, u64, &str) + Send + Sync>,
    /// 块读回校验等长时间无字节进度时刷新 stall 心跳，避免误判断连
    pub on_activity: Box<dyn Fn() + Send + Sync>,
}

/// 不用 thiserror 的固定 `#[error]` 文案：错误会原样展示给用户，必须在展示时按界面语言选择。
/// 注意 `is_permanent_sftp_error` 依赖这里的部分措辞（远端不是文件、续传边界、SHA-256），改文案时同步关键字表。
#[derive(Debug)]
pub enum DownloadError {
    Cancelled,
    InvalidRemotePath(String),
    InvalidLocalPath(String),
    RemoteNotAFile(String),
    /// 字段只在 Debug 诊断输出里使用（用户文案不展示具体大小），先前由 thiserror 的展开隐式读取。
    #[allow(dead_code)]
    LocalLargerThanRemote { local: u64, remote: u64 },
    ResumeBoundaryMismatch { offset: u64 },
    SizeMismatch { remote: u64, local: u64 },
    HashMismatch,
    ChunkVerifyMismatch { offset: u64, len: u64 },
    LocalIo(String),
    /// 远端文件在下载途中（或上次中断之后）被改写：已下载的部分属于旧版本，接着下只会拼出一个坏文件。
    RemoteFileChanged,
    Connect(ConnectError),
    Sftp(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            DownloadError::Cancelled => crate::tr!("用户取消了下载", "Download cancelled by user").to_string(),
            DownloadError::InvalidRemotePath(e) => {
                crate::tr!(format!("远端路径无效: {e}"), format!("Invalid remote path: {e}"))
            }
            DownloadError::InvalidLocalPath(e) => {
                crate::tr!(format!("本地路径无效: {e}"), format!("Invalid local path: {e}"))
            }
            DownloadError::RemoteNotAFile(e) => {
                crate::tr!(format!("远端不是文件: {e}"), format!("Remote path is not a file: {e}"))
            }
            DownloadError::LocalLargerThanRemote { .. } => crate::tr!(
                "本地文件大于远端文件，无法续传",
                "Local file is larger than the remote file; cannot resume"
            )
            .to_string(),
            DownloadError::ResumeBoundaryMismatch { offset } => crate::tr!(
                format!("续传边界校验失败（offset={offset}），请清除断点后重新下载"),
                format!("Resume boundary verification failed (offset={offset}); clear the checkpoint and download again")
            ),
            DownloadError::SizeMismatch { remote, local } => crate::tr!(
                format!("下载完成后大小不一致（远端={remote}，本地={local}）"),
                format!("Size mismatch after download (remote={remote}, local={local})")
            ),
            DownloadError::HashMismatch => crate::tr!(
                "下载完成后校验不一致（SHA-256 不匹配）",
                "Verification failed after download (SHA-256 mismatch)"
            )
            .to_string(),
            DownloadError::ChunkVerifyMismatch { offset, len } => crate::tr!(
                format!("块校验失败（offset={offset}，len={len}），将重下该块"),
                format!("Chunk verification failed (offset={offset}, len={len}); downloading the chunk again")
            ),
            DownloadError::LocalIo(e) => {
                crate::tr!(format!("本地文件写入失败: {e}"), format!("Failed to write local file: {e}"))
            }
            DownloadError::RemoteFileChanged => crate::tr!(
                "远端文件已被修改（大小或修改时间变化），已下载的部分不能继续使用，请清除断点后重新下载",
                "The remote file was modified (size or modification time changed), so the downloaded part cannot be reused; clear the checkpoint and download again"
            )
            .to_string(),
            DownloadError::Connect(e) => crate::tr!(format!("连接失败: {e}"), format!("Connection failed: {e}")),
            DownloadError::Sftp(e) => crate::tr!(format!("SFTP 错误: {e}"), format!("SFTP error: {e}")),
        };
        f.write_str(&text)
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DownloadError::Connect(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ConnectError> for DownloadError {
    fn from(err: ConnectError) -> Self {
        DownloadError::Connect(err)
    }
}

impl TransferError for DownloadError {
    fn cancelled() -> Self {
        DownloadError::Cancelled
    }
    fn sftp(detail: String) -> Self {
        DownloadError::Sftp(detail)
    }
    fn local_io(detail: String) -> Self {
        DownloadError::LocalIo(detail)
    }
}

impl TransferCheckpoint for DownloadCheckpoint {
    type Error = DownloadError;
    fn transferred_bytes(&self) -> u64 {
        self.downloaded_bytes
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

pub fn resolve_local_path(local_path: &str, remote_file_name: &str) -> Result<String, DownloadError> {
    let trimmed = local_path.trim();
    if trimmed.is_empty() {
        return Err(DownloadError::InvalidLocalPath(
            crate::tr!("本地路径不能为空", "Local path cannot be empty").into(),
        ));
    }
    if trimmed.ends_with('/') || trimmed.ends_with('\\') {
        let base = trimmed.trim_end_matches(['/', '\\']);
        Ok(format!("{base}/{remote_file_name}"))
    } else {
        Ok(trimmed.to_string())
    }
}

pub fn remote_file_name(remote_path: &str) -> Result<String, DownloadError> {
    let trimmed = remote_path.trim().trim_end_matches('/');
    let name = Path::new(trimmed)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| DownloadError::InvalidRemotePath(
                crate::tr!("无法读取远端文件名", "Cannot read the remote file name").into(),
            ))?;
    Ok(name.to_string())
}

/// 没有任务可领、但别的连接还在传时，多久再看一眼队列。
const PLAN_POLL: Duration = Duration::from_millis(100);

/// 准备阶段确定的路径与大小。
#[derive(Clone)]
struct DownloadTarget {
    remote_path: String,
    /// 实际写入的本地文件：新下载为临时文件，旧版断点（staged=false）是正式文件本身。
    write_path: String,
    final_local: String,
    size: u64,
}

/// 一次下载中各连接共享的状态。
struct DownloadShared<'a> {
    cancel: &'a AtomicBool,
    strict: &'a AtomicBool,
    checkpoint: &'a Mutex<DownloadCheckpoint>,
    callbacks: &'a DownloadCallbacks,
    aborts: &'a AbortHandles,
    plan: TransferPlan<DownloadError>,
    target: Mutex<Option<DownloadTarget>>,
}

impl DownloadShared<'_> {
    /// 汇报进度：已完成的块加上各连接正在传的那一块里已传的部分。
    fn report(&self, status: &str) {
        let (done, total, retries) = self
            .checkpoint
            .lock()
            .map(|cp| (cp.downloaded_bytes, cp.file_size, cp.retry_count))
            .unwrap_or((0, 0, 0));
        let transferred = (done + self.plan.partial_bytes()).min(total);
        (self.callbacks.on_progress)(transferred, total, status, retries);
    }

    /// 一块已写入本地并落盘（并按当前模式校验过）：记入断点。
    fn complete_block(&self, offset: u64, len: u64, read_back: bool) -> Result<(), DownloadError> {
        let mut cp = self.checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
        cp.done_ranges.insert(offset, offset + len);
        let total = cp.done_ranges.total();
        cp.downloaded_bytes = total;
        cp.verified_bytes = total;
        cp.all_chunks_verified &= read_back;
        save_download_checkpoint(&cp).map_err(checkpoint_error)
    }

    fn transferred(&self) -> u64 {
        self.checkpoint.lock().map(|cp| cp.downloaded_bytes).unwrap_or(0)
    }
}

/// 断点写不进本地磁盘：本机的问题，重连解决不了，按本地错误停下来（理由同上传侧）。
fn checkpoint_error(detail: String) -> DownloadError {
    DownloadError::LocalIo(crate::tr!(
        format!("无法保存断点文件（{detail}）"),
        format!("cannot save the checkpoint file ({detail})")
    ))
}

/// 重连也不会好的错误：停止重试，把断点留给用户处理。
fn is_permanent(err: &DownloadError) -> bool {
    match err {
        DownloadError::Cancelled | DownloadError::ChunkVerifyMismatch { .. } => false,
        // 判定用变体里的原始内容，原因同上传侧：Display 加的「SFTP 错误: 」前缀会让路径外壳识别失效。
        DownloadError::Sftp(raw) => is_permanent_sftp_error(raw),
        DownloadError::Connect(e) => e.is_permanent(),
        DownloadError::InvalidRemotePath(_)
        | DownloadError::InvalidLocalPath(_)
        | DownloadError::RemoteNotAFile(_)
        | DownloadError::LocalLargerThanRemote { .. }
        | DownloadError::ResumeBoundaryMismatch { .. }
        | DownloadError::SizeMismatch { .. }
        | DownloadError::HashMismatch
        | DownloadError::RemoteFileChanged
        | DownloadError::LocalIo(_) => true,
    }
}

/// 断点里记录的已完成区间。旧版断点只有「已校验到第几字节」一个数，换算成文件开头的一段。
fn checkpoint_ranges(cp: &DownloadCheckpoint) -> RangeSet {
    if cp.done_ranges.is_empty() {
        RangeSet::prefix(effective_verified_bytes(cp))
    } else {
        cp.done_ranges.clone()
    }
}

#[allow(clippy::too_many_arguments)] // 与上传侧对称的入口，参数逐个对应调用方持有的状态。
pub fn download_with_resume(
    config: ConnectionConfig,
    remote_path: &str,
    local_path: &str,
    cancel_flag: Arc<AtomicBool>,
    force_overwrite: Arc<AtomicBool>,
    strict_chunk_verify: Arc<AtomicBool>,
    aborts: Arc<AbortHandles>,
    checkpoint: Arc<Mutex<DownloadCheckpoint>>,
    callbacks: DownloadCallbacks,
    connections: usize,
) -> Result<(), DownloadError> {
    let shared = DownloadShared {
        cancel: &cancel_flag,
        strict: &strict_chunk_verify,
        checkpoint: &checkpoint,
        callbacks: &callbacks,
        aborts: &aborts,
        plan: TransferPlan::new(),
        target: Mutex::new(None),
    };
    // 文件大小要连上之后才知道，所以辅助线程先按请求的数量起好；
    // 它们只在有任务可领时才真正建立连接，小文件不会多出任何连接。
    let helpers = connections.clamp(1, MAX_CONNECTIONS) - 1;

    thread::scope(|scope| {
        for index in 1..=helpers {
            let (config, shared) = (&config, &shared);
            let spawned = thread::Builder::new()
                .name(format!("download-conn-{index}"))
                .spawn_scoped(scope, move || helper_loop(config, shared, index));
            if let Err(e) = spawned {
                // 起不了线程只是少一条并行连接，主连接照常下完。
                log::warn!("cannot start parallel download connection {index}: {e}");
            }
        }
        let result = lead_loop(&config, &shared, remote_path, local_path, &force_overwrite);
        shared.plan.finish();
        // 无论成败都打断还登记着的连接（成功时主连接已交还连接池、不在其中，剩下的只有辅助连接），
        // 理由同上传侧：别让卡在死连接上的收尾拖住一个已经完成的任务。
        aborts.abort_all();
        result
    })
}

/// 主连接：负责准备、对用户可见的重连提示、最终校验与换成正式文件；传输阶段与辅助连接一样从队列领任务。
fn lead_loop(
    config: &ConnectionConfig,
    shared: &DownloadShared<'_>,
    remote_path: &str,
    local_path: &str,
    force_overwrite: &AtomicBool,
) -> Result<(), DownloadError> {
    let (checkpoint, callbacks) = (shared.checkpoint, shared.callbacks);
    let mut backoff = Backoff::new();
    let mut session: Option<(Session, u64)> = None;
    // 第一次连接优先接手预检刚用过的会话，省掉一次完整握手；之后的重连一律新建。
    let mut first_connect = true;

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
                        return Err(DownloadError::Cancelled);
                    }
                    if err.is_permanent() {
                        log::error!("connect failed permanently, stop retrying: {err}");
                        return Err(DownloadError::Connect(err));
                    }
                    let retry_count = increment_retry_count(checkpoint)?;
                    let backoff_ms = backoff.next_delay_ms(shared.transferred());
                    let message = crate::tr!(
                        format!("连接失败，{backoff_ms}ms 后重试: {err}"),
                        format!("Connection failed, retrying in {backoff_ms}ms: {err}")
                    );
                    log::warn!("download retry #{retry_count}: {message}");
                    (callbacks.on_retry)(retry_count, backoff_ms, &message);
                    sleep_backoff::<DownloadError>(shared.cancel, backoff_ms)?;
                    continue;
                }
            }
        }

        let (active_session, connection) = session.as_ref().unwrap();
        // 传输、校验、换成正式文件放在同一个结果里：校验阶段的网络错误与传输中的一样走下面的
        // 重试分支，重连后再校验，而不是让一个已经下完的文件以失败告终。
        let transfer_result = lead_transfer(active_session, shared, remote_path, local_path, force_overwrite, *connection)
            .and_then(|target| {
                if !should_skip_full_file_hash(shared.strict, checkpoint, target.size)? {
                    (callbacks.on_progress)(target.size, target.size, "verifying", current_retry_count(checkpoint));
                    verify_download_integrity(
                        active_session,
                        Path::new(&target.write_path),
                        &target.remote_path,
                        target.size,
                        shared.cancel,
                        checkpoint,
                    )?;
                }
                if target.write_path != target.final_local {
                    promote_local_file(&target.write_path, &target.final_local, target.size)?;
                }
                Ok(target.size)
            });

        match transfer_result {
            Ok(remote_size) => {
                let retry_count = current_retry_count(checkpoint);
                {
                    let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
                    cp.downloaded_bytes = remote_size;
                    cp.status = CheckpointStatus::Completed;
                }
                (callbacks.on_progress)(remote_size, remote_size, "completed", retry_count);
                // 会话还完好：留给紧接着的下一步（批量中的下一个文件）复用。
                if let Some((session, abort_id)) = session.take() {
                    if let Some(abort) = shared.aborts.remove(abort_id) {
                        pool::release(config, session, abort);
                    }
                }
                return Ok(());
            }
            Err(DownloadError::Cancelled) => {
                checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?.status = CheckpointStatus::Failed;
                return Err(DownloadError::Cancelled);
            }
            Err(err) => {
                if shared.cancel.load(Ordering::Relaxed) {
                    return Err(DownloadError::Cancelled);
                }
                if is_permanent(&err) {
                    checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?.status =
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
                let backoff_ms = backoff.next_delay_ms(shared.transferred());
                let message = crate::tr!(
                    format!("传输中断（{err}），{backoff_ms}ms 后重连续传"),
                    format!("Transfer interrupted ({err}), reconnecting to resume in {backoff_ms}ms")
                );
                log::warn!("download retry #{retry_count}: {message}");
                (callbacks.on_retry)(retry_count, backoff_ms, &message);
                sleep_backoff::<DownloadError>(shared.cancel, backoff_ms)?;
            }
        }
    }
}

/// 本次下载实际写入的本地文件：新下载为临时文件，旧版断点（staged=false）仍是正式文件本身。
fn local_write_path(final_local: &str, checkpoint: &Mutex<DownloadCheckpoint>) -> String {
    let staged = checkpoint.lock().map(|cp| cp.staged).unwrap_or(false);
    if staged { staging_path(final_local) } else { final_local.to_string() }
}

/// 校验通过后把临时文件换成正式文件。本地 rename(2) 是原子的，已有同名文件会被直接替换，
/// 正在打开旧文件的程序不受影响。
fn promote_local_file(part: &str, final_path: &str, size: u64) -> Result<(), DownloadError> {
    if let Err(e) = fs::metadata(part) {
        // 已经换好（例如上一轮在改名后、更新断点前被中断）：正式文件已是完整大小即视为完成。
        if e.kind() == std::io::ErrorKind::NotFound
            && fs::metadata(final_path).is_ok_and(|m| m.len() == size)
        {
            return Ok(());
        }
        return Err(DownloadError::LocalIo(e.to_string()));
    }
    fs::rename(part, final_path).map_err(|e| {
        DownloadError::LocalIo(crate::tr!(
            format!("无法把下载好的文件改名为 {final_path}：{e}"),
            format!("Could not rename the downloaded file to {final_path}: {e}")
        ))
    })?;
    log::info!("download verified and moved into place: {final_path}");
    Ok(())
}

/// 主连接一轮的工作：必要时做准备，然后传到所有块完成为止。
fn lead_transfer(
    session: &Session,
    shared: &DownloadShared<'_>,
    remote_path: &str,
    local_path: &str,
    force_overwrite: &AtomicBool,
    connection: u64,
) -> Result<DownloadTarget, DownloadError> {
    let sftp = session.sftp().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    // 在 sftp 之后声明：出错时先打断连接，sftp 的析构才不会在一条死连接上空等。
    let teardown = AbortUnlessDone::new(shared.aborts, session, connection);
    let lock_target = || shared.target.lock().map_err(|e| DownloadError::Sftp(e.to_string()));
    let existing = lock_target()?.clone();
    let target = match existing {
        Some(target) => target,
        None => {
            let target = prepare_download(session, &sftp, shared, remote_path, local_path, force_overwrite)?;
            *lock_target()? = Some(target.clone());
            target
        }
    };

    pump_pieces(session, &sftp, &target, shared, connection)?;
    if let Some(fatal) = shared.plan.take_fatal() {
        return Err(fatal);
    }

    let final_size = fs::metadata(&target.write_path).map(|m| m.len()).unwrap_or(0);
    if final_size != target.size {
        return Err(DownloadError::SizeMismatch { remote: target.size, local: final_size });
    }
    // 下载期间远端被改写时，各块来自不同版本、逐块校验却都能通过；结束前再核对一次。
    let end_stat = sftp
        .stat(Path::new(&target.remote_path))
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    let cp = shared.checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    if remote_changed(&cp, &end_stat) {
        log::error!("remote file changed during download: {}", target.remote_path);
        return Err(DownloadError::RemoteFileChanged);
    }
    // cp 借用着 checkpoint 的锁，先放掉它再结束。
    drop(cp);
    teardown.done();
    Ok(target)
}

/// 传输开始前的一次性准备：确认远端文件、核对断点声称的进度是否还成立、把剩余部分排进队列。
/// 只在一次下载的第一次成功连接时做；之后主连接重连时，内存里的计划就是准确的进度。
fn prepare_download(
    session: &Session,
    sftp: &Sftp,
    shared: &DownloadShared<'_>,
    remote_path: &str,
    local_path: &str,
    force_overwrite: &AtomicBool,
) -> Result<DownloadTarget, DownloadError> {
    let checkpoint = shared.checkpoint;
    let lock = || checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()));

    let actual_remote_path = resolve_remote_file(sftp, remote_path)?;
    let remote_stat = sftp
        .stat(Path::new(&actual_remote_path))
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    if !remote_stat.is_file() {
        return Err(DownloadError::RemoteNotAFile(actual_remote_path.clone()));
    }
    let remote_size = remote_stat.size.unwrap_or(0);

    let file_name = remote_file_name(&actual_remote_path)?;
    let resolved_local_path = resolve_local_path(local_path, &file_name)?;
    if let Some(parent) = Path::new(&resolved_local_path).parent() {
        fs::create_dir_all(parent).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    }

    let force = force_overwrite.swap(false, Ordering::Relaxed);
    let (mut done, staged) = {
        let mut cp = lock()?;
        let done = if force { RangeSet::default() } else { checkpoint_ranges(&cp) };
        // 已有可续传的进度时，先确认远端还是同一个文件，再用新值覆盖断点里的记录——
        // 覆盖之后就再也看不出它变过了。
        if !done.is_empty() && remote_changed(&cp, &remote_stat) {
            log::error!(
                "remote file changed since checkpoint: path={actual_remote_path} size {} -> {remote_size}",
                cp.file_size
            );
            cp.status = CheckpointStatus::Failed;
            return Err(DownloadError::RemoteFileChanged);
        }
        cp.file_size = remote_size;
        cp.remote_path = actual_remote_path.clone();
        cp.local_path = resolved_local_path.clone();
        cp.remote_mtime = remote_stat.mtime;
        save_download_checkpoint(&cp).map_err(checkpoint_error)?;
        (done, cp.staged)
    };

    // 之后的本地读写都针对实际写入的文件：新下载写同目录的临时文件，校验通过后才换成正式文件，
    // 下载途中「下载」文件夹里不会出现一个看起来完整、其实只有一半的文件。
    let write_path = local_write_path(&resolved_local_path, checkpoint);
    // 与上传侧对称：只有文件不存在才算没有数据，其他读元数据失败不能被当成「从头下载」。
    let local_size = match fs::metadata(&write_path) {
        Ok(meta) => Some(meta.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(DownloadError::LocalIo(e.to_string())),
    };
    // 旧版断点直接写正式文件：它比远端还大，说明不是这次下载留下的，不能在上面接着写。
    if !staged && !force && local_size.is_some_and(|size| size > remote_size) {
        return Err(DownloadError::LocalLargerThanRemote { local: local_size.unwrap_or(0), remote: remote_size });
    }
    // 本地实际没有那么多数据（文件被删改、断点过期）：超出的进度作废。
    done.truncate(local_size.unwrap_or(0));

    let fresh = done.is_empty();
    if fresh && staged && !force && remote_size >= DELTA_MIN_BYTES {
        done = seed_from_existing_local(session, shared, &actual_remote_path, &resolved_local_path, &write_path, remote_size);
    }

    if done.is_empty() {
        // 从头开始：建一个空文件，顺带清掉这个路径上可能残留的旧数据（空的远端文件也因此有了对应的本地文件）。
        File::create(&write_path).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    } else {
        if local_size.is_some_and(|size| size > remote_size) {
            truncate_local_file(Path::new(&write_path), remote_size)?;
        }
        verify_resume_boundaries(session, sftp, shared, &actual_remote_path, Path::new(&write_path), &done)?;
    }

    {
        let mut cp = lock()?;
        let total = done.total();
        cp.downloaded_bytes = total;
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
        save_download_checkpoint(&cp).map_err(checkpoint_error)?;
    }

    super::upload::log_resume_point(done.total(), remote_size);
    // 小文件只用主连接：辅助连接看到没有任务（或任务很快被领完）就不会建立连接。
    let gaps = done.gaps(remote_size);
    if effective_connections(MAX_CONNECTIONS, remote_size) == 1 {
        shared.plan.start_exclusive(&gaps);
    } else {
        shared.plan.start(&gaps);
    }
    shared.report("downloading");
    Ok(DownloadTarget { remote_path: actual_remote_path, write_path, final_local: resolved_local_path, size: remote_size })
}

/// 本地已有旧版本时只下载发生变化的块，与上传侧的 seed_from_existing_target 对称：
/// 让服务器算出远端每一块的哈希，把本地旧文件复制成临时文件后逐块比对，相同的块直接算作已完成。
/// 任何一步走不通都返回空集合，调用方按从头下载处理。
fn seed_from_existing_local(
    session: &Session,
    shared: &DownloadShared<'_>,
    remote_path: &str,
    final_local: &str,
    write_path: &str,
    remote_size: u64,
) -> RangeSet {
    use crate::activity::{format_bytes, info, Category};
    let nothing = RangeSet::default();
    let on_activity = &shared.callbacks.on_activity;
    let Some(existing_size) = fs::metadata(final_local)
        .ok()
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .filter(|size| *size >= CHUNK_SIZE)
    else {
        return nothing;
    };
    let blocks = existing_size.min(remote_size) / CHUNK_SIZE;
    info(
        Category::Transfer,
        crate::tr!(
            format!("本地已有旧版本，正在比对哪些部分没有变化（{}）", format_bytes(blocks * CHUNK_SIZE)),
            format!("An older local version exists; checking which parts are unchanged ({})", format_bytes(blocks * CHUNK_SIZE))
        ),
    );
    // 先问服务器：它算不了哈希的话就不必白白复制一份本地文件。
    let Some(remote_hashes) = remote_block_hashes(session, remote_path, blocks, shared.cancel, on_activity) else {
        return nothing;
    };
    if fs::copy(final_local, write_path).is_err() {
        return nothing;
    }
    // 临时文件不能比远端文件长：多出来的尾巴是旧版本的内容。
    if existing_size > remote_size && truncate_local_file(Path::new(write_path), remote_size).is_err() {
        return nothing;
    }

    let Ok(mut local_file) = File::open(write_path) else { return nothing };
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
            format!("{} 与本地旧版本相同，无需重新下载", format_bytes(same.total())),
            format!("{} unchanged from the older local version; no need to download it again", format_bytes(same.total()))
        ),
    );
    same
}

/// 续传前抽查：每个已完成区间的最后一块，本地内容必须与远端一致。
/// 断点可能已经过期（本地临时文件被改动），在其上接着下只会得到一个坏文件。
fn verify_resume_boundaries(
    session: &Session,
    sftp: &Sftp,
    shared: &DownloadShared<'_>,
    remote_path: &str,
    write_path: &Path,
    done: &RangeSet,
) -> Result<(), DownloadError> {
    let on_activity = &shared.callbacks.on_activity;
    let mut remote_file = open_remote_for_download(sftp, remote_path)?;
    // 优先让服务器自己算这一块的哈希；走不通再把这一块读回来。
    let mut hasher = BlockHasher::start(session, remote_path);
    let mut buffer = vec![0_u8; CHUNK_SIZE as usize];
    for &(_, end) in done.ranges() {
        let start = align_down(end - 1);
        let len = end - start;
        let local_hash = sha256_local_range(write_path, start, len, Some(on_activity))?;
        on_activity();
        let hashed = hasher.as_mut().map(|active| active.hash(start, len, shared.cancel));
        let remote_hash = match hashed {
            Some(Ok(hash)) => hash,
            other => {
                if let Some(Err(e)) = other {
                    log::info!("remote block hasher unavailable ({e}), reading blocks instead");
                    hasher = None;
                }
                // 每读到一段就刷新心跳：慢速链路上读 1 MB 可能超过无响应阈值。
                read_remote_chunk(&mut remote_file, &mut None, start, &mut buffer[..len as usize], end, |_| {
                    on_activity()
                })?;
                sha256_bytes(&buffer[..len as usize])
            }
        };
        if local_hash != remote_hash {
            return Err(DownloadError::ResumeBoundaryMismatch { offset: end });
        }
    }
    Ok(())
}

/// 从队列里不断领任务并下载，直到全部完成。主连接和辅助连接共用。
/// 一个连接只打开一次远端文件和本地文件，所有块复用同一对句柄。
fn pump_pieces(
    session: &Session,
    sftp: &Sftp,
    target: &DownloadTarget,
    shared: &DownloadShared<'_>,
    connection: u64,
) -> Result<(), DownloadError> {
    let write_path = Path::new(&target.write_path);
    let mut remote_file: Option<RemoteFile> = None;
    let mut local_file: Option<File> = None;
    // 远端句柄当前读到的位置：同一段任务里各块是连续的，据此避免无谓的重新定位。
    let mut remote_position: Option<u64> = None;
    let mut chunk_buffer = vec![0_u8; CHUNK_SIZE as usize];
    // 在文件句柄之后声明，出错时先于它们执行（见 AbortUnlessDone）。
    let teardown = AbortUnlessDone::new(shared.aborts, session, connection);

    loop {
        if shared.cancel.load(Ordering::Relaxed) {
            return Err(DownloadError::Cancelled);
        }
        let mut piece = match shared.plan.claim(connection) {
            Claim::Piece(piece) => piece,
            Claim::Wait => {
                // 自己没活干、在等别的连接：顺便看看有没有哪条已经卡死，有就打断它，
                // 它手里的那段会退回队列由我接手，不必等它自己超时。
                for stalled in shared.plan.take_stalled() {
                    log::warn!("connection {stalled} made no progress for too long while others wait; aborting it");
                    shared.aborts.abort(stalled);
                }
                keepalive_while_idle(session);
                thread::sleep(PLAN_POLL);
                continue;
            }
            Claim::Done => {
                teardown.done();
                return Ok(());
            }
        };
        if remote_file.is_none() {
            remote_file = Some(open_remote_for_download(sftp, &target.remote_path)?);
            // 文件已由准备阶段建好；各连接往各自的位置写，所以不能带截断。
            let file = OpenOptions::new()
                .write(true)
                .open(write_path)
                .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
            local_file = Some(file);
        }
        let (remote, local) = (remote_file.as_mut().unwrap(), local_file.as_mut().unwrap());

        while piece.next < piece.end {
            if shared.cancel.load(Ordering::Relaxed) {
                return Err(DownloadError::Cancelled);
            }
            let offset = piece.next;
            let chunk_len = CHUNK_SIZE.min(piece.end - offset) as usize;
            let chunk = &mut chunk_buffer[..chunk_len];

            read_remote_chunk(remote, &mut remote_position, offset, chunk, piece.end, |absolute| {
                piece.set_partial(absolute - offset);
                shared.report("downloading");
            })?;
            write_local_chunk(local, offset, chunk)?;

            let read_back = shared.strict.load(Ordering::Relaxed);
            if read_back {
                shared.report("chunk_verifying");
                verify_written_chunk(write_path, chunk, offset, &shared.callbacks.on_activity)?;
            }

            shared.complete_block(offset, chunk_len as u64, read_back)?;
            piece.advance(chunk_len as u64);
            shared.report("downloading");
        }
    }
}

/// 辅助连接：只在有任务可领时才建立连接，默默地传、默默地重连；它的失败不打扰用户
/// （主连接还在工作，进度仍在前进），只有「重连也不会好」的错误才交给主连接上报。
fn helper_loop(config: &ConnectionConfig, shared: &DownloadShared<'_>, index: usize) {
    let mut backoff = Backoff::new();
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
        let Some(target) = shared.target.lock().ok().and_then(|target| target.clone()) else {
            return;
        };

        let result = connect_session(config).map_err(DownloadError::Connect).and_then(|(session, abort)| {
            let abort_id = shared.aborts.register(abort);
            let result = session
                .sftp()
                .map_err(|e| DownloadError::Sftp(e.to_string()))
                .and_then(|sftp| pump_pieces(&session, &sftp, &target, shared, abort_id));
            shared.aborts.remove(abort_id);
            result
        });
        match result {
            Ok(()) | Err(DownloadError::Cancelled) => return,
            Err(err) if is_permanent(&err) => {
                log::error!("parallel download connection {index} failed permanently: {err}");
                // 连不上只是少一条并行连接；传输过程中的永久错误对整个任务成立，交给主连接上报。
                if !matches!(err, DownloadError::Connect(_)) {
                    shared.plan.fail(err);
                }
                return;
            }
            Err(err) => {
                let delay = backoff.next_delay_ms(shared.transferred());
                log::warn!("parallel download connection {index} interrupted ({err}), retrying in {delay}ms");
                if sleep_backoff::<DownloadError>(shared.cancel, delay).is_err() {
                    return;
                }
            }
        }
    }
}

/// 远端文件是否已不是断点记录的那一个（大小或修改时间不同）。旧断点没记修改时间时只比大小。
fn remote_changed(checkpoint: &DownloadCheckpoint, stat: &ssh2::FileStat) -> bool {
    stat.size.unwrap_or(0) != checkpoint.file_size
        || checkpoint.remote_mtime.is_some_and(|saved| stat.mtime != Some(saved))
}

fn resolve_remote_file(sftp: &Sftp, remote_path: &str) -> Result<String, DownloadError> {
    let trimmed = remote_path.trim();
    if trimmed.is_empty() {
        return Err(DownloadError::InvalidRemotePath(
            crate::tr!("远端路径不能为空", "Remote path cannot be empty").into(),
        ));
    }
    let path = Path::new(trimmed);
    match sftp.stat(path) {
        Ok(stat) if stat.is_dir() => Err(DownloadError::RemoteNotAFile(
            crate::tr!(
                "请填写远端文件路径，不支持下载整个目录",
                "Please enter a remote file path; downloading a whole directory is not supported"
            )
            .into(),
        )),
        Ok(_) => Ok(trimmed.to_string()),
        Err(e) if is_sftp_not_found(&e) => Err(DownloadError::InvalidRemotePath(crate::tr!(
            format!("远端文件不存在: {trimmed}"),
            format!("Remote file does not exist: {trimmed}")
        ))),
        // InvalidRemotePath 是永久错误；超时等抖动必须走可重试的 Sftp 分支，不能直接放弃下载。
        Err(e) => {
            log::warn!("stat remote failed (will retry): path={trimmed} err={e}");
            Err(DownloadError::Sftp(e.to_string()))
        }
    }
}

/// 一次连接只打开一次远端文件，所有块复用：此前每块 open + close，每 MB 多两次网络往返。
fn open_remote_for_download(sftp: &Sftp, remote_path: &str) -> Result<RemoteFile, DownloadError> {
    sftp.open_mode(Path::new(remote_path), OpenFlags::READ, 0, OpenType::File)
        .map_err(|e| DownloadError::Sftp(e.to_string()))
}

/// 读取远端 `[offset, offset + buffer.len())`。
/// `position` 是这个句柄当前所在的位置（None 表示未知）：顺序读时不重新定位，因为 libssh2 的 seek
/// 会把已经发出的预读请求全部作废。`boundary` 是这条连接当前任务段的终点，读取大小据此收紧，
/// 让预读不越过它（见 bounded_read_len）。
fn read_remote_chunk<F: FnMut(u64)>(
    remote_file: &mut RemoteFile,
    position: &mut Option<u64>,
    offset: u64,
    buffer: &mut [u8],
    boundary: u64,
    mut on_partial_progress: F,
) -> Result<(), DownloadError> {
    if *position != Some(offset) {
        remote_file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    }
    // 出错提前返回时位置不再可信，下次必须重新定位。
    *position = None;

    let mut read_total = 0usize;
    while read_total < buffer.len() {
        let at = offset + read_total as u64;
        let want = bounded_read_len(buffer.len() - read_total, boundary.saturating_sub(at));
        let read_bytes = remote_file
            .read(&mut buffer[read_total..read_total + want])
            .map_err(|e| DownloadError::Sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(DownloadError::Sftp(
                crate::tr!("读取远端数据时意外结束", "Remote data ended unexpectedly while reading").into(),
            ));
        }
        read_total += read_bytes;
        on_partial_progress(offset + read_total as u64);
    }
    *position = Some(offset + buffer.len() as u64);
    Ok(())
}

/// 把一块写到本地文件的指定位置并落盘。
fn write_local_chunk(file: &mut File, offset: u64, data: &[u8]) -> Result<(), DownloadError> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    file.write_all(data)
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    // 必须 sync_all（fsync）而非 flush：flush 对 std::fs::File 是空操作，
    // 数据仍停留在 OS page cache。断点随后会把这些字节标记为已完成，
    // 若此时掉电/崩溃，断点会指向尚未落盘的数据。逐块 fsync 相对网络传输开销可忽略，
    // 换取「断点声称已下载即真正落盘」的持久性保证。
    file.sync_all()
        .map_err(|e| DownloadError::LocalIo(e.to_string()))
}

fn verify_download_integrity(
    session: &Session,
    local_path: &Path,
    remote_path: &str,
    remote_size: u64,
    cancel_flag: &AtomicBool,
    checkpoint: &Mutex<DownloadCheckpoint>,
) -> Result<u32, DownloadError> {
    ensure_not_cancelled(cancel_flag, checkpoint)?;
    let local_hash = sha256_local_file(session, local_path, cancel_flag, checkpoint)?;
    let sftp = session.sftp().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    // 优先让服务器自己算：把整个文件再读一遍等于流量翻倍。走不通（不能执行命令等）才读。
    let remote_result = match remote_file_sha256(session, remote_path, remote_size, cancel_flag) {
        Some(hash) => Ok(hash),
        None => sha256_remote_file(&sftp, remote_path, remote_size, cancel_flag, checkpoint),
    };
    let remote_hash = match remote_result {
        Ok(hash) => hash,
        Err(DownloadError::Sftp(detail)) => {
            log::warn!("full-file remote read failed during download verify: {detail}");
            let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
            cp.failure_reason = Some("verify_read_failed".into());
            cp.status = CheckpointStatus::Failed;
            let _ = save_download_checkpoint(&cp);
            // 这个错误会进入重试循环自动重连再校验；只有用户取消或退出应用时才会停在这里。
            return Err(DownloadError::Sftp(crate::tr!(
                format!("数据已下完，最终校验时读取远端失败: {detail}"),
                format!("All data was received, but reading the remote file for final verification failed: {detail}")
            )));
        }
        Err(err) => return Err(err),
    };

    if local_hash != remote_hash {
        log::error!(
            "download sha256 mismatch for {remote_path} (size={remote_size}), deleting local file {}",
            local_path.display()
        );
        let _ = fs::remove_file(local_path);
        let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
        cp.downloaded_bytes = 0;
        cp.verified_bytes = 0;
        cp.done_ranges = RangeSet::default();
        cp.failure_reason = Some("hash_mismatch".into());
        cp.status = CheckpointStatus::Failed;
        let _ = save_download_checkpoint(&cp);
        return Err(DownloadError::HashMismatch);
    }
    log::info!("download sha256 verified for {remote_path}");
    Ok(current_retry_count(checkpoint))
}

/// 每写完一块立即读回比对 SHA-256；不一致则报错，这一块会被重新下载。
/// 本地侧真读回磁盘（写入目标，必须验证）；远端侧直接哈希内存缓冲（刚下载的字节），不需要再走网络。
fn verify_written_chunk(
    local_path: &Path,
    remote_bytes: &[u8],
    offset: u64,
    on_activity: &dyn Fn(),
) -> Result<(), DownloadError> {
    let len = remote_bytes.len() as u64;
    on_activity();
    let local_hash = sha256_local_range(local_path, offset, len, Some(on_activity))?;
    if local_hash == sha256_bytes(remote_bytes) {
        return Ok(());
    }
    Err(DownloadError::ChunkVerifyMismatch { offset, len })
}

fn truncate_local_file(path: &Path, size: u64) -> Result<(), DownloadError> {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    file.set_len(size)
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    Ok(())
}

fn sha256_local_range(
    path: &Path,
    offset: u64,
    len: u64,
    on_read: Option<&dyn Fn()>,
) -> Result<[u8; 32], DownloadError> {
    let mut file = File::open(path).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    while remaining > 0 {
        if let Some(defer) = on_read {
            defer();
        }
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        file.read_exact(&mut buffer[..to_read])
            .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
        hasher.update(&buffer[..to_read]);
        remaining -= to_read as u64;
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_local_directory_path() {
        let path = resolve_local_path("/tmp/downloads/", "demo.jar").unwrap();
        assert_eq!(path, "/tmp/downloads/demo.jar");
    }
}
