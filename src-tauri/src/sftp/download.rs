use super::client::{connect_session, ConnectError, ConnectionConfig};
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
use thiserror::Error;

const CHUNK_SIZE: u64 = 1024 * 1024;
const HASH_READ_BUFFER_SIZE: usize = 256 * 1024;
const PROGRESS_SLICE_SIZE: usize = 64 * 1024;

pub struct DownloadCallbacks {
    pub on_progress: Box<dyn Fn(u64, u64, &str, u32) + Send + Sync>,
    pub on_retry: Box<dyn Fn(u32, u64, &str) + Send + Sync>,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("用户取消了下载")]
    Cancelled,
    #[error("远端路径无效: {0}")]
    InvalidRemotePath(String),
    #[error("本地路径无效: {0}")]
    InvalidLocalPath(String),
    #[error("远端不是文件: {0}")]
    RemoteNotAFile(String),
    #[error("本地文件大于远端文件，无法续传")]
    LocalLargerThanRemote { local: u64, remote: u64 },
    #[error("续传边界校验失败（offset={offset}），请清除断点后重新下载")]
    ResumeBoundaryMismatch { offset: u64 },
    #[error("下载完成后大小不一致（远端={remote}，本地={local}）")]
    SizeMismatch { remote: u64, local: u64 },
    #[error("下载完成后校验不一致（SHA-256 不匹配）")]
    HashMismatch,
    #[error("块校验失败（offset={offset}，len={len}），将重下该块")]
    ChunkVerifyMismatch { offset: u64, len: u64 },
    #[error("本地文件写入失败: {0}")]
    LocalIo(String),
    #[error("连接失败: {0}")]
    Connect(#[from] ConnectError),
    #[error("SFTP 错误: {0}")]
    Sftp(String),
}

pub fn resolve_local_path(local_path: &str, remote_file_name: &str) -> Result<String, DownloadError> {
    let trimmed = local_path.trim();
    if trimmed.is_empty() {
        return Err(DownloadError::InvalidLocalPath("本地路径不能为空".into()));
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
        .ok_or_else(|| DownloadError::InvalidRemotePath("无法读取远端文件名".into()))?;
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
                    let retry_count = increment_retry_count(&checkpoint)?;
                    let message = format!("连接失败，{backoff_ms}ms 后重试: {err}");
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
                let downloaded_bytes = checkpoint
                    .lock()
                    .map(|cp| cp.downloaded_bytes)
                    .unwrap_or(0);
                if is_permanent_sftp_error(&detail, downloaded_bytes) {
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
                let message = format!("传输中断（{detail}），{backoff_ms}ms 后重连续传");
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
        return Err(DownloadError::InvalidRemotePath("远端文件为空".into()));
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

    let local_stat_size = fs::metadata(&resolved_local_path)
        .map(|m| m.len())
        .unwrap_or(0);

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
            verify_written_chunk(
                &sftp,
                Path::new(&resolved_local_path),
                &actual_remote_path,
                chunk_start,
                chunk_len as u64,
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
        return Err(DownloadError::InvalidRemotePath("远端路径不能为空".into()));
    }
    let path = Path::new(trimmed);
    match sftp.stat(path) {
        Ok(stat) if stat.is_dir() => Err(DownloadError::RemoteNotAFile(
            "请填写远端文件路径，不支持下载整个目录".into(),
        )),
        Ok(_) => Ok(trimmed.to_string()),
        Err(_) => Err(DownloadError::InvalidRemotePath(format!(
            "远端文件不存在: {trimmed}"
        ))),
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
            return Err(DownloadError::Sftp("读取远端数据时意外结束".into()));
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
    file.flush()
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
    let local_hash = sha256_local_file(local_path, cancel_flag, checkpoint)?;
    let sftp = session.sftp().map_err(|e| DownloadError::Sftp(e.to_string()))?;
    let remote_hash = match sha256_remote_file(&sftp, remote_path, remote_size, cancel_flag, checkpoint) {
        Ok(hash) => hash,
        Err(DownloadError::Sftp(detail)) => {
            let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
            cp.failure_reason = Some("verify_read_failed".into());
            cp.status = CheckpointStatus::Failed;
            let _ = save_download_checkpoint(&cp);
            return Err(DownloadError::Sftp(format!(
                "传完后校验时读取远端失败，可点「开始下载」重试校验: {detail}"
            )));
        }
        Err(err) => return Err(err),
    };

    if local_hash != remote_hash {
        let _ = fs::remove_file(local_path);
        let mut cp = checkpoint.lock().map_err(|e| DownloadError::Sftp(e.to_string()))?;
        cp.downloaded_bytes = 0;
        cp.verified_bytes = 0;
        cp.failure_reason = Some("hash_mismatch".into());
        cp.status = CheckpointStatus::Failed;
        let _ = save_download_checkpoint(&cp);
        return Err(DownloadError::HashMismatch);
    }
    Ok(current_retry_count(checkpoint))
}

fn verify_written_chunk(
    sftp: &Sftp,
    local_path: &Path,
    remote_path: &str,
    offset: u64,
    len: u64,
) -> Result<(), DownloadError> {
    if len == 0 {
        return Ok(());
    }
    let local_hash = sha256_local_range(local_path, offset, len)?;
    let remote_hash = sha256_remote_range(sftp, remote_path, offset, len)?;
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
    _remote_size: u64,
    force_overwrite: &AtomicBool,
    checkpoint: &DownloadCheckpoint,
) -> Result<u64, DownloadError> {
    if force_overwrite.load(Ordering::Relaxed) {
        return prepare_local_resume_offset(local_path, local_stat_size, force_overwrite);
    }
    let trusted = effective_verified_bytes(checkpoint);
    if trusted > 0 {
        if local_stat_size > trusted {
            truncate_local_file(local_path, trusted)?;
        }
        return Ok(trusted);
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

fn sha256_local_file(
    path: &Path,
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
) -> Result<[u8; 32], DownloadError> {
    let mut file = File::open(path).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    loop {
        ensure_not_cancelled(cancel_flag, checkpoint)?;
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
            return Err(DownloadError::Sftp("读取远端文件用于校验时意外结束".into()));
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes as u64;
    }
    Ok(hasher.finalize().into())
}

fn sha256_local_range(path: &Path, offset: u64, len: u64) -> Result<[u8; 32], DownloadError> {
    let mut file = File::open(path).map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    while remaining > 0 {
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        file.read_exact(&mut buffer[..to_read])
            .map_err(|e| DownloadError::LocalIo(e.to_string()))?;
        hasher.update(&buffer[..to_read]);
        remaining -= to_read as u64;
    }
    Ok(hasher.finalize().into())
}

fn sha256_remote_range(
    sftp: &Sftp,
    remote_path: &str,
    offset: u64,
    len: u64,
) -> Result<[u8; 32], DownloadError> {
    let mut remote_file = sftp
        .open_mode(Path::new(remote_path), OpenFlags::READ, 0, OpenType::File)
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    remote_file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| DownloadError::Sftp(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    while remaining > 0 {
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        let read_bytes = remote_file
            .read(&mut buffer[..to_read])
            .map_err(|e| DownloadError::Sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(DownloadError::Sftp("读取远端块用于校验时意外结束".into()));
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes as u64;
    }
    Ok(hasher.finalize().into())
}

fn align_down(offset: u64) -> u64 {
    offset - (offset % CHUNK_SIZE)
}

fn strip_sftp_wrapper(message: &str) -> &str {
    if let Some(pos) = message.find('」') {
        if message.starts_with("远端路径「") {
            let rest = message[pos + '」'.len_utf8()..].trim_start();
            return rest.strip_prefix("操作失败：").unwrap_or(rest);
        }
    }
    message
}

fn is_permanent_sftp_error(message: &str, _downloaded_bytes: u64) -> bool {
    let raw = strip_sftp_wrapper(message);
    let lower = raw.to_ascii_lowercase();
    [
        "permission denied",
        "no such file",
        "not a directory",
        "is a directory",
        "cannot create",
        "access denied",
        "auth",
        "续传边界校验失败",
        "sha-256",
        "远端路径错误",
        "远端不是文件",
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
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
}
