use super::errors::is_permanent_sftp_error;
use super::client::{connect_session, is_sftp_not_found, ConnectError, ConnectionConfig};
use crate::checkpoint::CheckpointStatus;
use crate::download_checkpoint::{save_download_checkpoint, DownloadCheckpoint};
use crate::retry::{next_backoff_ms, INITIAL_BACKOFF_MS};
use sha2::{Digest, Sha256};
use ssh2::{OpenFlags, OpenType, Session, Sftp};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CHUNK_SIZE: u64 = 1024 * 1024;
const HASH_READ_BUFFER_SIZE: usize = 256 * 1024;
const PROGRESS_SLICE_SIZE: usize = 64 * 1024;

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

pub fn resolve_local_path(local_path: &str, remote_file_name: &str) -> Result<String, DownloadError> {
    let trimmed = local_path.trim();
    if trimmed.is_empty() {
        return Err(DownloadError::InvalidLocalPath(
            crate::tr!("本地路径不能为空", "Local path cannot be empty").into(),
        ));
    }
    if trimmed.ends_with('/') || trimmed.ends_with('\\') {
        let base = trimmed.trim_end_matches(|c| c == '/' || c == '\\');
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

pub fn download_with_resume(
    config: ConnectionConfig,
    remote_path: &str,
    local_path: &str,
    cancel_flag: Arc<AtomicBool>,
    force_overwrite: Arc<AtomicBool>,
    strict_chunk_verify: Arc<AtomicBool>,
    abort_socket_holder: Arc<Mutex<Option<TcpStream>>>,
    checkpoint: Arc<Mutex<DownloadCheckpoint>>,
    callbacks: DownloadCallbacks,
) -> Result<(), DownloadError> {
    let mut backoff_ms = INITIAL_BACKOFF_MS;
    let mut session: Option<Session> = None;

    loop {
        ensure_not_cancelled(&cancel_flag, &checkpoint)?;

        if session.is_none() {
            match connect_session(&config) {
                Ok((s, abort_socket)) => {
                    backoff_ms = INITIAL_BACKOFF_MS;
                    *abort_socket_holder
                        .lock()
                        .map_err(|e| DownloadError::Sftp(e.to_string()))? = Some(abort_socket);
                    session = Some(s);
                }
                Err(err) => {
                    if cancel_flag.load(Ordering::Relaxed) {
                        return Err(DownloadError::Cancelled);
                    }
                    if err.is_permanent() {
                        log::error!("connect failed permanently, stop retrying: {err}");
                        return Err(DownloadError::Connect(err));
                    }
                    let retry_count = increment_retry_count(&checkpoint)?;
                    let message = crate::tr!(
                        format!("连接失败，{backoff_ms}ms 后重试: {err}"),
                        format!("Connection failed, retrying in {backoff_ms}ms: {err}")
                    );
                    (callbacks.on_retry)(retry_count, backoff_ms, &message);
                    sleep_backoff(&cancel_flag, backoff_ms)?;
                    backoff_ms = next_backoff_ms(backoff_ms);
                    continue;
                }
            }
        }

        let active_session = session.as_mut().unwrap();
        let transfer_result = transfer_chunks(
            active_session,
            remote_path,
            local_path,
            &cancel_flag,
            &force_overwrite,
            &strict_chunk_verify,
            &checkpoint,
            &callbacks,
        );

        match transfer_result {
            Ok(done) => {
                if done {
                    let remote_size = checkpoint
                        .lock()
                        .map(|cp| cp.file_size)
                        .unwrap_or(0);
                    let retry_count = if should_skip_full_file_hash(
                        &strict_chunk_verify,
                        &checkpoint,
                        remote_size,
                    )? {
                        current_retry_count(&checkpoint)
                    } else {
                        let resolved_local = checkpoint
                            .lock()
                            .map(|cp| cp.local_path.clone())
                            .unwrap_or_default();
                        let resolved_remote = checkpoint
                            .lock()
                            .map(|cp| cp.remote_path.clone())
                            .unwrap_or_default();
                        (callbacks.on_progress)(
                            remote_size,
                            remote_size,
                            "verifying",
                            current_retry_count(&checkpoint),
                        );
                        verify_download_integrity(
                            active_session,
                            Path::new(&resolved_local),
                            &resolved_remote,
                            remote_size,
                            &cancel_flag,
                            &checkpoint,
                        )?
                    };

                    {
                        let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
                        cp.downloaded_bytes = remote_size;
                        cp.status = CheckpointStatus::Completed;
                    }
                    (callbacks.on_progress)(remote_size, remote_size, "completed", retry_count);
                    return Ok(());
                }
                backoff_ms = INITIAL_BACKOFF_MS;
            }
            Err(DownloadError::Cancelled) => {
                checkpoint
                    .lock()
                    .map_err(|e| DownloadError::Sftp(e.to_string()))?
                    .status = CheckpointStatus::Failed;
                return Err(DownloadError::Cancelled);
            }
            Err(
                DownloadError::InvalidRemotePath(_)
                | DownloadError::InvalidLocalPath(_)
                | DownloadError::RemoteNotAFile(_)
                | DownloadError::LocalLargerThanRemote { .. }
                | DownloadError::ResumeBoundaryMismatch { .. }
                | DownloadError::SizeMismatch { .. }
                | DownloadError::HashMismatch
                | DownloadError::LocalIo(_),
            ) => {
                checkpoint
                    .lock()
                    .map_err(|e| DownloadError::Sftp(e.to_string()))?
                    .status = CheckpointStatus::Failed;
                return Err(transfer_result.err().unwrap());
            }
            Err(DownloadError::Connect(_) | DownloadError::Sftp(_) | DownloadError::ChunkVerifyMismatch { .. }) => {
                if cancel_flag.load(Ordering::Relaxed) {
                    return Err(DownloadError::Cancelled);
                }
                let detail = transfer_result.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
                if is_permanent_sftp_error(&detail) {
                    checkpoint
                        .lock()
                        .map_err(|e| DownloadError::Sftp(e.to_string()))?
                        .status = CheckpointStatus::Failed;
                    return Err(transfer_result.err().unwrap());
                }
                let retry_count = increment_retry_count(&checkpoint)?;
                session = None;
                *abort_socket_holder
                    .lock()
                    .map_err(|e| DownloadError::Sftp(e.to_string()))? = None;
                let message = crate::tr!(
                    format!("传输中断（{detail}），{backoff_ms}ms 后重连续传"),
                    format!("Transfer interrupted ({detail}), reconnecting to resume in {backoff_ms}ms")
                );
                (callbacks.on_retry)(retry_count, backoff_ms, &message);
                sleep_backoff(&cancel_flag, backoff_ms)?;
                backoff_ms = next_backoff_ms(backoff_ms);
            }
        }
    }
}

fn transfer_chunks(
    session: &mut Session,
    remote_path: &str,
    local_path: &str,
    cancel_flag: &Arc<AtomicBool>,
    force_overwrite: &Arc<AtomicBool>,
    strict_chunk_verify: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
    callbacks: &DownloadCallbacks,
) -> Result<bool, DownloadError> {
    let sftp = session.sftp().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    let actual_remote_path = resolve_remote_file(&sftp, remote_path)?;
    let remote_stat = sftp
        .stat(Path::new(&actual_remote_path))
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    if !remote_stat.is_file() {
        return Err(DownloadError::RemoteNotAFile(actual_remote_path.clone()));
    }
    let remote_size = remote_stat.size.unwrap_or(0);
    if remote_size == 0 {
        return Err(DownloadError::InvalidRemotePath(
            crate::tr!("远端文件为空", "Remote file is empty").into(),
        ));
    }

    let file_name = remote_file_name(&actual_remote_path)?;
    let resolved_local_path = resolve_local_path(local_path, &file_name)?;

    if let Some(parent) = Path::new(&resolved_local_path).parent() {
        fs::create_dir_all(parent).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    }

    {
        let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
        cp.file_size = remote_size;
        cp.remote_path = actual_remote_path.clone();
        cp.local_path = resolved_local_path.clone();
        cp.remote_mtime = remote_stat.mtime;
        save_download_checkpoint(&cp).map_err(|e| DownloadError::Sftp(e))?;
    }

    // 与上传侧对称：只有文件不存在才算 0 字节，其他读元数据失败不能被当成「从头下载」。
    let local_stat_size = match fs::metadata(&resolved_local_path) {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(DownloadError::LocalIo(e.to_string())),
    };

    if local_stat_size > remote_size && !force_overwrite.load(Ordering::Relaxed) {
        return Err(DownloadError::LocalLargerThanRemote {
            local: local_stat_size,
            remote: remote_size,
        });
    }

    let checkpoint_snapshot = checkpoint
        .lock()
        .map_err(|e| DownloadError::Sftp(e.to_string()))?
        .clone();

    let mut offset = resolve_download_offset(
        Path::new(&resolved_local_path),
        local_stat_size,
        remote_size,
        force_overwrite,
        &checkpoint_snapshot,
    )?;

    if offset > 0 {
        verify_resume_boundary(
            &sftp,
            Path::new(&resolved_local_path),
            &actual_remote_path,
            offset,
        )?;
    }
    super::upload::log_resume_point(offset, remote_size);

    {
        let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
        cp.downloaded_bytes = offset;
        cp.verified_bytes = offset;
    }

    let retry_count = current_retry_count(checkpoint);
    (callbacks.on_progress)(offset, remote_size, "downloading", retry_count);

    let mut chunk_buffer = vec![0_u8; CHUNK_SIZE as usize];
    let mut truncate_first = offset == 0 && !Path::new(&resolved_local_path).exists();

    while offset < remote_size {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(DownloadError::Cancelled);
        }

        let remaining = remote_size - offset;
        let chunk_len = (CHUNK_SIZE as u64).min(remaining) as usize;
        let chunk_start = offset;

        read_remote_chunk(
            &sftp,
            &actual_remote_path,
            offset,
            &mut chunk_buffer[..chunk_len],
            |absolute| {
                (callbacks.on_progress)(absolute, remote_size, "downloading", retry_count);
            },
        )?;

        write_local_chunk(
            Path::new(&resolved_local_path),
            offset,
            &chunk_buffer[..chunk_len],
            truncate_first,
            |absolute| {
                (callbacks.on_progress)(absolute, remote_size, "downloading", retry_count);
            },
        )?;
        truncate_first = false;

        if strict_chunk_verify.load(Ordering::Relaxed) {
            (callbacks.on_progress)(
                chunk_start,
                remote_size,
                "chunk_verifying",
                retry_count,
            );
            verify_written_chunk(
                Path::new(&resolved_local_path),
                &chunk_buffer[..chunk_len],
                chunk_start,
                chunk_len as u64,
                &callbacks.on_activity,
            )?;
        }

        offset += chunk_len as u64;
        {
            let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
            cp.downloaded_bytes = offset;
            cp.verified_bytes = offset;
            save_download_checkpoint(&cp).map_err(|e| DownloadError::Sftp(e))?;
        }
        (callbacks.on_progress)(offset, remote_size, "downloading", retry_count);
    }

    let final_size = fs::metadata(&resolved_local_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if final_size != remote_size {
        return Err(DownloadError::SizeMismatch {
            remote: remote_size,
            local: final_size,
        });
    }

    Ok(true)
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

fn read_remote_chunk<F: FnMut(u64)>(
    sftp: &Sftp,
    remote_path: &str,
    offset: u64,
    buffer: &mut [u8],
    mut on_partial_progress: F,
) -> Result<(), DownloadError> {
    let mut remote_file = sftp
        .open_mode(
            Path::new(remote_path),
            OpenFlags::READ,
            0,
            OpenType::File,
        )
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    remote_file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;

    let mut read_total = 0usize;
    while read_total < buffer.len() {
        let read_bytes = remote_file
            .read(&mut buffer[read_total..])
            .map_err(|e| DownloadError::Sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(DownloadError::Sftp(
                crate::tr!("读取远端数据时意外结束", "Remote data ended unexpectedly while reading").into(),
            ));
        }
        read_total += read_bytes;
        on_partial_progress(offset + read_total as u64);
    }
    remote_file
        .close()
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    Ok(())
}

fn write_local_chunk<F: FnMut(u64)>(
    local_path: &Path,
    offset: u64,
    data: &[u8],
    create_new: bool,
    mut on_partial_progress: F,
) -> Result<(), DownloadError> {
    let mut file = if offset == 0 && create_new {
        File::create(local_path).map_err(|e| DownloadError::LocalIo(e.to_string()))?
    } else {
        OpenOptions::new()
            .write(true)
            .create(true)
            .open(local_path)
            .map_err(|e| DownloadError::LocalIo(e.to_string()))?
    };

    file.seek(SeekFrom::Start(offset))
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;

    let mut written = 0usize;
    while written < data.len() {
        let end = (written + PROGRESS_SLICE_SIZE).min(data.len());
        file.write_all(&data[written..end])
            .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
        written = end;
        on_partial_progress(offset + written as u64);
    }
    // 必须 sync_all（fsync）而非 flush：flush 对 std::fs::File 是空操作，
    // 数据仍停留在 OS page cache。断点随后会把这些字节标记为 verified，
    // 若此时掉电/崩溃，断点会指向尚未落盘的数据。逐块 fsync 相对网络传输开销可忽略，
    // 换取「断点声称已下载即真正落盘」的持久性保证。
    file.sync_all()
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    Ok(())
}

fn verify_download_integrity(
    session: &mut Session,
    local_path: &Path,
    remote_path: &str,
    remote_size: u64,
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
) -> Result<u32, DownloadError> {
    ensure_not_cancelled(cancel_flag, checkpoint)?;
    let local_hash = sha256_local_file(session, local_path, cancel_flag, checkpoint)?;
    let sftp = session.sftp().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    let remote_hash = match sha256_remote_file(&sftp, remote_path, remote_size, cancel_flag, checkpoint) {
        Ok(hash) => hash,
        Err(DownloadError::Sftp(detail)) => {
            log::warn!("full-file remote read failed during download verify: {detail}");
            let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
            cp.failure_reason = Some("verify_read_failed".into());
            cp.status = CheckpointStatus::Failed;
            let _ = save_download_checkpoint(&cp);
            return Err(DownloadError::Sftp(crate::tr!(
                format!("传完后校验时读取远端失败，可点「开始下载」重试校验: {detail}"),
                format!(
                    "Failed to read the remote file for final verification; click \"Start download\" to retry verification: {detail}"
                )
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
        cp.failure_reason = Some("hash_mismatch".into());
        cp.status = CheckpointStatus::Failed;
        let _ = save_download_checkpoint(&cp);
        return Err(DownloadError::HashMismatch);
    }
    log::info!("download sha256 verified for {remote_path}");
    Ok(current_retry_count(checkpoint))
}

/// 每写完一块立即读回比对 SHA-256，失败则截断本地并拒绝推进 verified 断点。
/// 本地侧真读回磁盘（写入目标，必须验证）；远端侧直接哈希内存缓冲（刚下载的字节），
/// 省去一次远端往返——弱网下这是每块一次的显著开销。
fn verify_written_chunk(
    local_path: &Path,
    remote_bytes: &[u8],
    offset: u64,
    len: u64,
    on_activity: &dyn Fn(),
) -> Result<(), DownloadError> {
    if len == 0 {
        return Ok(());
    }
    on_activity();
    let local_hash = sha256_local_range(local_path, offset, len, Some(on_activity))?;
    let remote_hash = sha256_bytes(remote_bytes);
    if local_hash == remote_hash {
        return Ok(());
    }
    truncate_local_file(local_path, offset)?;
    Err(DownloadError::ChunkVerifyMismatch { offset, len })
}

fn verify_resume_boundary(
    sftp: &Sftp,
    local_path: &Path,
    remote_path: &str,
    resume_offset: u64,
) -> Result<(), DownloadError> {
    if resume_offset == 0 {
        return Ok(());
    }
    let verify_len = CHUNK_SIZE.min(resume_offset);
    let start = resume_offset - verify_len;
    let mut local_buf = vec![0_u8; verify_len as usize];
    let mut local_file = File::open(local_path).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    local_file.seek(SeekFrom::Start(start)).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    local_file
        .read_exact(&mut local_buf)
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;

    let mut remote_file = sftp
        .open_mode(Path::new(remote_path), OpenFlags::READ, 0, OpenType::File)
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    let mut remote_buf = vec![0_u8; verify_len as usize];
    remote_file.seek(SeekFrom::Start(start)).map_err(|e| DownloadError::Sftp(e.to_string()))?;
    remote_file
        .read_exact(&mut remote_buf)
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;

    if local_buf != remote_buf {
        return Err(DownloadError::ResumeBoundaryMismatch {
            offset: resume_offset,
        });
    }
    Ok(())
}

fn resolve_download_offset(
    local_path: &Path,
    local_stat_size: u64,
    remote_size: u64,
    force_overwrite: &AtomicBool,
    checkpoint: &DownloadCheckpoint,
) -> Result<u64, DownloadError> {
    if force_overwrite.load(Ordering::Relaxed) {
        return prepare_local_resume_offset(local_path, local_stat_size, force_overwrite);
    }
    let trusted = effective_verified_bytes(checkpoint);
    if trusted > 0 {
        // 续传位置不能超过本地实际已有字节，也不能超过远端实际字节，否则
        // verify_resume_boundary 会读过 EOF：本地不足 → LocalIo（永久失败）；
        // 远端不足 → Sftp "failed to fill whole buffer"（非永久）会无限重连死循环。
        let safe = safe_resume_offset(trusted, local_stat_size, remote_size);
        if safe < trusted {
            log::warn!(
                "checkpoint trusts {trusted} but local={local_stat_size} remote={remote_size}; \
                 checkpoint stale, resume from block-aligned {safe}"
            );
        }
        if local_stat_size > safe {
            truncate_local_file(local_path, safe)?;
        }
        if safe > 0 {
            return Ok(safe);
        }
        // safe == 0：本地/远端没有可信共同前缀，落到下面从头开始的逻辑。
    }
    if local_stat_size > 0 {
        log::info!(
            "checkpoint has no verified progress but local has {local_stat_size} bytes, remove local file"
        );
        if local_path.exists() {
            let _ = fs::remove_file(local_path);
        }
        return Ok(0);
    }
    prepare_local_resume_offset(local_path, local_stat_size, force_overwrite)
}

fn prepare_local_resume_offset(
    local_path: &Path,
    local_stat_size: u64,
    force_overwrite: &AtomicBool,
) -> Result<u64, DownloadError> {
    if force_overwrite.swap(false, Ordering::Relaxed) {
        if local_path.exists() {
            let _ = fs::remove_file(local_path);
        }
        return Ok(0);
    }
    if !local_path.exists() {
        return Ok(0);
    }
    let resume_offset = align_down(local_stat_size);
    if resume_offset != local_stat_size {
        truncate_local_file(local_path, resume_offset)?;
    }
    Ok(resume_offset)
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

fn effective_verified_bytes(cp: &DownloadCheckpoint) -> u64 {
    if cp.verified_bytes > 0 {
        return cp.verified_bytes;
    }
    if cp.downloaded_bytes > 0 {
        return align_down(cp.downloaded_bytes);
    }
    0
}

fn should_skip_full_file_hash(
    strict_chunk_verify: &AtomicBool,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
    file_size: u64,
) -> Result<bool, DownloadError> {
    if !strict_chunk_verify.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    Ok(cp.verified_bytes == file_size && cp.downloaded_bytes >= file_size)
}

/// 哈希大文件可能要几十秒，期间 SSH 连接上没有任何流量；顺带发 keepalive，
/// 防止 NAT/防火墙或服务器 ClientAlive 把空闲连接掐掉，导致随后读远端时又得重连。
fn sha256_local_file(
    session: &Session,
    path: &Path,
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
) -> Result<[u8; 32], DownloadError> {
    let mut file = File::open(path).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    loop {
        ensure_not_cancelled(cancel_flag, checkpoint)?;
        // keepalive_send 内部按 set_keepalive 的间隔节流，每轮调用并不会真的每次发包。
        if let Err(e) = session.keepalive_send() {
            log::debug!("keepalive during local hash failed: {e}");
        }
        let read_bytes = file.read(&mut buffer).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
        if read_bytes == 0 {
            break;
        }
        hasher.update(&buffer[..read_bytes]);
    }
    Ok(hasher.finalize().into())
}

fn sha256_remote_file(
    sftp: &Sftp,
    remote_path: &str,
    size: u64,
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
) -> Result<[u8; 32], DownloadError> {
    let mut remote_file = sftp
        .open_mode(Path::new(remote_path), OpenFlags::READ, 0, OpenType::File)
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        ensure_not_cancelled(cancel_flag, checkpoint)?;
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        let read_bytes = remote_file
            .read(&mut buffer[..to_read])
            .map_err(|e| DownloadError::Sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(DownloadError::Sftp(
                crate::tr!(
                    "读取远端文件用于校验时意外结束",
                    "Remote file ended unexpectedly while reading for verification"
                )
                .into(),
            ));
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes as u64;
    }
    Ok(hasher.finalize().into())
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

/// 直接对内存字节求 SHA-256。下载时远端块内容已在缓冲区里（刚下载的字节），
/// 无需再向远端读一遍即可与本地磁盘读回结果比对——省去一次远端往返（弱网关键）。
fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

fn align_down(offset: u64) -> u64 {
    offset - (offset % CHUNK_SIZE)
}

/// 给定断点可信续传位置、本地实际字节、远端实际字节，返回安全的续传偏移。
/// 下载同时依赖本地已写字节与远端可读字节，任一侧不足都会让边界校验读过 EOF：
/// 本地不足 → LocalIo（永久失败）；远端不足 → 非永久错误会无限重连死循环。
/// 因此夹紧到两侧都满足的块对齐位置。
fn safe_resume_offset(trusted: u64, local_size: u64, remote_size: u64) -> u64 {
    let mut safe = trusted;
    if local_size < safe {
        safe = align_down(local_size);
    }
    if remote_size < safe {
        safe = align_down(remote_size);
    }
    safe
}



fn sleep_backoff(cancel_flag: &Arc<AtomicBool>, backoff_ms: u64) -> Result<(), DownloadError> {
    let step_ms = 200_u64;
    let mut waited = 0_u64;
    while waited < backoff_ms {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(DownloadError::Cancelled);
        }
        thread::sleep(Duration::from_millis(step_ms));
        waited += step_ms;
    }
    Ok(())
}

fn ensure_not_cancelled(
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
) -> Result<(), DownloadError> {
    if cancel_flag.load(Ordering::Relaxed) {
        checkpoint
            .lock()
            .map_err(|e| DownloadError::Sftp(e.to_string()))?
            .status = CheckpointStatus::Failed;
        return Err(DownloadError::Cancelled);
    }
    Ok(())
}

fn increment_retry_count(checkpoint: &Arc<Mutex<DownloadCheckpoint>>) -> Result<u32, DownloadError> {
    let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    cp.retry_count = cp.retry_count.saturating_add(1);
    Ok(cp.retry_count)
}

fn current_retry_count(checkpoint: &Arc<Mutex<DownloadCheckpoint>>) -> u32 {
    checkpoint.lock().map(|cp| cp.retry_count).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_local_directory_path() {
        let path = resolve_local_path("/tmp/downloads/", "demo.jar").unwrap();
        assert_eq!(path, "/tmp/downloads/demo.jar");
    }

    // 回归：本地或远端字节 < 断点可信位置时须夹紧到两侧都满足的块对齐位置，
    // 否则边界校验读过 EOF（远端不足会无限重连死循环）。
    #[test]
    fn safe_resume_offset_clamps_to_shorter_side() {
        // 两侧都足够：用可信位置
        assert_eq!(
            safe_resume_offset(2 * CHUNK_SIZE, 5 * CHUNK_SIZE, 5 * CHUNK_SIZE),
            2 * CHUNK_SIZE
        );
        // 远端更短：夹到远端块对齐位置
        assert_eq!(
            safe_resume_offset(5 * CHUNK_SIZE, 5 * CHUNK_SIZE, 3 * CHUNK_SIZE + 9),
            3 * CHUNK_SIZE
        );
        // 本地更短：夹到本地块对齐位置
        assert_eq!(
            safe_resume_offset(5 * CHUNK_SIZE, 2 * CHUNK_SIZE + 7, 5 * CHUNK_SIZE),
            2 * CHUNK_SIZE
        );
        // 取两侧更小者
        assert_eq!(
            safe_resume_offset(5 * CHUNK_SIZE, 4 * CHUNK_SIZE, 2 * CHUNK_SIZE),
            2 * CHUNK_SIZE
        );
    }
}
