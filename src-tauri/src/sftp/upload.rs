use super::client::{connect_session, ConnectError, ConnectionConfig};
use crate::checkpoint::{save_checkpoint, CheckpointStatus, UploadCheckpoint};
use crate::retry::{next_backoff_ms, INITIAL_BACKOFF_MS};
use sha2::{Digest, Sha256};
use ssh2::{FileStat, OpenFlags, OpenType, Session, Sftp};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;

/// 固定块大小：续传时回退到块边界，避免“半块”脏数据（你遇到的 33556504 正是此类问题）。
const CHUNK_SIZE: u64 = 1024 * 1024;
const HASH_READ_BUFFER_SIZE: usize = 256 * 1024;
/// 写入时更细粒度的进度回调（仅 UI 速度用，不改变断点块大小）。
const PROGRESS_SLICE_SIZE: usize = 64 * 1024;

pub struct UploadCallbacks {
    pub on_progress: Box<dyn Fn(u64, u64, &str, u32) + Send + Sync>,
    pub on_retry: Box<dyn Fn(u32, u64, &str) + Send + Sync>,
}

#[derive(Debug, Error)]
pub enum UploadError {
    #[error("用户取消了上传")]
    Cancelled,
    #[error("远端路径无效: {0}")]
    InvalidRemotePath(String),
    #[error("远端文件大于本地文件，无法续传")]
    RemoteLargerThanLocal { remote: u64, local: u64 },
    #[error("续传边界校验失败（offset={offset}），请清除断点后重新上传")]
    ResumeBoundaryMismatch { offset: u64 },
    #[error("上传完成后远端大小不一致（本地={local}，远端={remote}）")]
    SizeMismatch { local: u64, remote: u64 },
    #[error("上传完成后校验不一致（SHA-256 不匹配）")]
    HashMismatch,
    #[error("块校验失败（offset={offset}，len={len}），将重传该块")]
    ChunkVerifyMismatch { offset: u64, len: u64 },
    #[error("本地文件读取失败: {0}")]
    LocalIo(String),
    #[error("连接失败: {0}")]
    Connect(#[from] ConnectError),
    #[error("SFTP 错误: {0}")]
    Sftp(String),
}

/// 远端填目录时自动拼接本地文件名；连接后若发现路径是已存在目录，也会自动拼接。
pub fn resolve_remote_path(remote_path: &str, local_path: &Path) -> Result<String, UploadError> {
    let trimmed = remote_path.trim();
    if trimmed.is_empty() {
        return Err(UploadError::InvalidRemotePath("远端路径不能为空".into()));
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
        .ok_or_else(|| UploadError::InvalidRemotePath("无法读取本地文件名".into()))?;
    Ok(format!("{dir_path}{file_name}"))
}

fn join_remote_path(base: &str, file_name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{file_name}")
    } else {
        format!("{base}/{file_name}")
    }
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
        return Err(UploadError::InvalidRemotePath("远端路径不能为空".into()));
    }

    if trimmed.ends_with('/') {
        return append_local_filename(trimmed, local_path);
    }

    let path = Path::new(trimmed);
    if is_remote_directory(sftp, path) {
        let file_name = local_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| UploadError::InvalidRemotePath("无法读取本地文件名".into()))?;
        let resolved = join_remote_path(trimmed, file_name);
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
            return format!(
                "远端路径错误：{remote_path}\n上级目录「{p}」在服务器上不存在，请核对用户名与目录名（本地文件不受影响）"
            );
        }
        return format!(
            "远端路径错误：{remote_path}\n该路径在服务器上不存在（本地文件不受影响）"
        );
    }
    if lower.contains("permission denied") || lower.contains("access denied") {
        return format!("远端路径错误：{remote_path}\n没有写入权限，请检查该目录权限");
    }
    if lower.contains("sftp(4)") || lower.contains("failure") {
        return format!("远端路径错误：{remote_path}\n写入失败：{message}");
    }
    format!("远端路径「{remote_path}」操作失败：{message}")
}

fn enhance_sftp_error(message: &str, remote_path: &str) -> String {
    format_remote_sftp_error(message, remote_path)
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

    let parts: Vec<&str> = parent_str.split('/').filter(|p| !p.is_empty()).collect();
    let mut current = String::new();
    for part in parts {
        current = if current.is_empty() {
            format!("/{part}")
        } else {
            format!("{current}/{part}")
        };
        let dir = Path::new(&current);
        if sftp.stat(dir).is_ok() {
            continue;
        }
        sftp.mkdir(dir, 0o755).map_err(|e| {
            UploadError::Sftp(enhance_sftp_error(
                &format!("创建远端目录「{current}」失败: {e}"),
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
    Some(format!(
        "远端路径可能有问题：上级目录「{parent_str}」在服务器上不存在"
    ))
}


pub fn upload_with_resume(
    config: ConnectionConfig,
    local_path: &Path,
    remote_path: &str,
    cancel_flag: Arc<AtomicBool>,
    force_overwrite: Arc<AtomicBool>,
    strict_chunk_verify: Arc<AtomicBool>,
    abort_socket_holder: Arc<Mutex<Option<TcpStream>>>,
    checkpoint: Arc<Mutex<UploadCheckpoint>>,
    callbacks: UploadCallbacks,
) -> Result<(), UploadError> {
    let resolved_remote_path = resolve_remote_path(remote_path, local_path)?;
    let local_size = std::fs::metadata(local_path)
        .map_err(|e| UploadError::LocalIo(e.to_string()))?
        .len();

    {
        let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
        cp.file_size = local_size;
        cp.remote_path = resolved_remote_path.clone();
        cp.status = CheckpointStatus::InProgress;
    }

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
                        .map_err(|e| UploadError::Sftp(e.to_string()))? = Some(abort_socket);
                    session = Some(s);
                }
                Err(err) => {
                    if cancel_flag.load(Ordering::Relaxed) {
                        return Err(UploadError::Cancelled);
                    }
                    let retry_count = increment_retry_count(&checkpoint)?;
                    let uploaded_bytes = checkpoint
                        .lock()
                        .map(|cp| cp.uploaded_bytes)
                        .unwrap_or(0);
                    let message = if uploaded_bytes > 0 {
                        format!(
                            "连接失败（已传 {uploaded_bytes} 字节），{backoff_ms}ms 后重连续传: {err}"
                        )
                    } else {
                        format!("连接失败，{backoff_ms}ms 后重试: {err}")
                    };
                    log::warn!("upload retry #{retry_count}: {message}");
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
            local_path,
            &resolved_remote_path,
            local_size,
            &cancel_flag,
            &force_overwrite,
            &strict_chunk_verify,
            &checkpoint,
            &callbacks,
        );

        match transfer_result {
            Ok(done) => {
                if done {
                    let retry_count = if should_skip_full_file_hash(
                        &strict_chunk_verify,
                        &checkpoint,
                        local_size,
                    )? {
                        log::info!(
                            "all {local_size} bytes chunk-verified, skip full-file SHA-256 read"
                        );
                        current_retry_count(&checkpoint)
                    } else {
                        (callbacks.on_progress)(
                            local_size,
                            local_size,
                            "verifying",
                            current_retry_count(&checkpoint),
                        );
                        verify_upload_integrity(
                            active_session,
                            local_path,
                            &resolved_remote_path,
                            local_size,
                            &cancel_flag,
                            &checkpoint,
                        )?
                    };

                    {
                        let mut cp = checkpoint
                            .lock()
                            .map_err(|e| UploadError::Sftp(e.to_string()))?;
                        cp.uploaded_bytes = local_size;
                        cp.status = CheckpointStatus::Completed;
                    }
                    (callbacks.on_progress)(local_size, local_size, "completed", retry_count);
                    log::info!("upload completed and verified, retries={retry_count}");
                    return Ok(());
                }
                backoff_ms = INITIAL_BACKOFF_MS;
            }
            Err(UploadError::Cancelled) => {
                checkpoint
                    .lock()
                    .map_err(|e| UploadError::Sftp(e.to_string()))?
                    .status = CheckpointStatus::Failed;
                return Err(UploadError::Cancelled);
            }
            Err(
                UploadError::InvalidRemotePath(_)
                | UploadError::RemoteLargerThanLocal { .. }
                | UploadError::ResumeBoundaryMismatch { .. }
                | UploadError::SizeMismatch { .. }
                | UploadError::HashMismatch
                | UploadError::LocalIo(_),
            ) => {
                checkpoint
                    .lock()
                    .map_err(|e| UploadError::Sftp(e.to_string()))?
                    .status = CheckpointStatus::Failed;
                return Err(transfer_result.err().unwrap());
            }
            Err(UploadError::Connect(_) | UploadError::Sftp(_) | UploadError::ChunkVerifyMismatch { .. }) => {
                if cancel_flag.load(Ordering::Relaxed) {
                    return Err(UploadError::Cancelled);
                }

                let detail = transfer_result.as_ref().err().map(|e| e.to_string()).unwrap_or_default();
                let uploaded_bytes = checkpoint
                    .lock()
                    .map(|cp| cp.uploaded_bytes)
                    .unwrap_or(0);
                if is_permanent_sftp_error(&detail, uploaded_bytes) {
                    checkpoint
                        .lock()
                        .map_err(|e| UploadError::Sftp(e.to_string()))?
                        .status = CheckpointStatus::Failed;
                    return Err(transfer_result.err().unwrap());
                }

                let retry_count = increment_retry_count(&checkpoint)?;
                session = None;
                *abort_socket_holder
                    .lock()
                    .map_err(|e| UploadError::Sftp(e.to_string()))? = None;
                let message = format!("传输中断（{detail}），{backoff_ms}ms 后重连续传");
                log::warn!("upload retry #{retry_count}: {message}");
                (callbacks.on_retry)(retry_count, backoff_ms, &message);
                sleep_backoff(&cancel_flag, backoff_ms)?;
                backoff_ms = next_backoff_ms(backoff_ms);
            }
        }
    }
}

fn verify_upload_integrity(
    session: &mut Session,
    local_path: &Path,
    remote_path: &str,
    local_size: u64,
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
) -> Result<u32, UploadError> {
    ensure_not_cancelled(cancel_flag, checkpoint)?;

    let local_hash = sha256_local_file(local_path, cancel_flag, checkpoint)?;
    let sftp = session.sftp().map_err(|e| UploadError::Sftp(e.to_string()))?;
    let remote_hash = match sha256_remote_file(
        &sftp,
        remote_path,
        local_size,
        cancel_flag,
        checkpoint,
    ) {
        Ok(hash) => hash,
        Err(UploadError::Sftp(detail)) => {
            log::warn!("full-file remote read failed during verify: {detail}");
            let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
            cp.failure_reason = Some("verify_read_failed".into());
            cp.status = CheckpointStatus::Failed;
            let _ = save_checkpoint(&cp);
            return Err(UploadError::Sftp(format!(
                "传完后校验时读取远端失败（弱网常见），数据块已全部校验通过，可点「开始上传」仅重试校验: {detail}"
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

fn sha256_local_file(
    path: &Path,
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
) -> Result<[u8; 32], UploadError> {
    let mut file = File::open(path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    loop {
        ensure_not_cancelled(cancel_flag, checkpoint)?;
        let read_bytes = file
            .read(&mut buffer)
            .map_err(|e| UploadError::LocalIo(e.to_string()))?;
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
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
) -> Result<[u8; 32], UploadError> {
    let mut remote_file = sftp
        .open_mode(
            Path::new(remote_path),
            OpenFlags::READ,
            0,
            OpenType::File,
        )
        .map_err(|e| UploadError::Sftp(e.to_string()))?;

    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    let mut remaining = size;

    while remaining > 0 {
        ensure_not_cancelled(cancel_flag, checkpoint)?;

        let to_read = (buffer.len() as u64).min(remaining) as usize;
        let read_bytes = remote_file
            .read(&mut buffer[..to_read])
            .map_err(|e| UploadError::Sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(UploadError::Sftp("读取远端文件用于校验时意外结束".into()));
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes as u64;
    }

    Ok(hasher.finalize().into())
}

fn should_skip_full_file_hash(
    strict_chunk_verify: &AtomicBool,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
    local_size: u64,
) -> Result<bool, UploadError> {
    if !strict_chunk_verify.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let cp = checkpoint
        .lock()
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
    Ok(cp.verified_bytes == local_size && cp.uploaded_bytes >= local_size)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ensure_not_cancelled(
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
) -> Result<(), UploadError> {
    if cancel_flag.load(Ordering::Relaxed) {
        checkpoint
            .lock()
            .map_err(|e| UploadError::Sftp(e.to_string()))?
            .status = CheckpointStatus::Failed;
        return Err(UploadError::Cancelled);
    }
    Ok(())
}

fn increment_retry_count(checkpoint: &Arc<Mutex<UploadCheckpoint>>) -> Result<u32, UploadError> {
    let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
    cp.retry_count = cp.retry_count.saturating_add(1);
    Ok(cp.retry_count)
}

fn current_retry_count(checkpoint: &Arc<Mutex<UploadCheckpoint>>) -> u32 {
    checkpoint
        .lock()
        .map(|cp| cp.retry_count)
        .unwrap_or(0)
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

fn is_permanent_sftp_error(message: &str, _uploaded_bytes: u64) -> bool {
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
    ]
    .iter()
    .any(|keyword| lower.contains(keyword))
}

fn sleep_backoff(cancel_flag: &Arc<AtomicBool>, backoff_ms: u64) -> Result<(), UploadError> {
    let step_ms = 200_u64;
    let mut waited = 0_u64;
    while waited < backoff_ms {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(UploadError::Cancelled);
        }
        thread::sleep(Duration::from_millis(step_ms));
        waited += step_ms;
    }
    Ok(())
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

fn prepare_resume_offset(
    sftp: &Sftp,
    remote_path: &str,
    remote_stat_size: u64,
    force_overwrite: &AtomicBool,
) -> Result<u64, UploadError> {
    if force_overwrite.swap(false, Ordering::Relaxed) {
        log::info!("overwriting existing remote file: {remote_path}");
        if remote_stat_size > 0 {
            if let Ok(stat) = sftp.stat(Path::new(remote_path)) {
                if stat.is_file() {
                    let _ = sftp.unlink(Path::new(remote_path));
                }
            }
        }
        return Ok(0);
    }

    let resume_offset = align_down(remote_stat_size);
    if resume_offset != remote_stat_size {
        log::warn!(
            "remote size {remote_stat_size} is not chunk-aligned, rewind to {resume_offset}"
        );
        truncate_remote_file(sftp, remote_path, resume_offset)?;
    }

    Ok(resume_offset)
}

fn verify_resume_boundary(
    sftp: &Sftp,
    local_path: &Path,
    remote_path: &str,
    resume_offset: u64,
) -> Result<(), UploadError> {
    if resume_offset == 0 {
        return Ok(());
    }

    let verify_len = CHUNK_SIZE.min(resume_offset);
    let local_start = resume_offset - verify_len;

    let mut local_buf = vec![0_u8; verify_len as usize];
    let mut local_file = File::open(local_path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    local_file
        .seek(SeekFrom::Start(local_start))
        .map_err(|e| UploadError::LocalIo(e.to_string()))?;
    local_file
        .read_exact(&mut local_buf)
        .map_err(|e| UploadError::LocalIo(e.to_string()))?;

    let mut remote_file = sftp
        .open_mode(
            Path::new(remote_path),
            OpenFlags::READ,
            0,
            OpenType::File,
        )
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
    let mut remote_buf = vec![0_u8; verify_len as usize];
    remote_file
        .seek(SeekFrom::Start(local_start))
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
    remote_file
        .read_exact(&mut remote_buf)
        .map_err(|e| UploadError::Sftp(e.to_string()))?;

    if local_buf != remote_buf {
        return Err(UploadError::ResumeBoundaryMismatch {
            offset: resume_offset,
        });
    }

    log::info!("resume boundary verified at offset {resume_offset}");
    Ok(())
}

fn effective_verified_bytes(cp: &UploadCheckpoint) -> u64 {
    if cp.verified_bytes > 0 {
        return cp.verified_bytes;
    }
    if cp.uploaded_bytes > 0 {
        return align_down(cp.uploaded_bytes);
    }
    0
}

fn sha256_local_range(path: &Path, offset: u64, len: u64) -> Result<[u8; 32], UploadError> {
    let mut file = File::open(path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| UploadError::LocalIo(e.to_string()))?;

    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];

    while remaining > 0 {
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        file.read_exact(&mut buffer[..to_read])
            .map_err(|e| UploadError::LocalIo(e.to_string()))?;
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
) -> Result<[u8; 32], UploadError> {
    let mut remote_file = sftp
        .open_mode(
            Path::new(remote_path),
            OpenFlags::READ,
            0,
            OpenType::File,
        )
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
    remote_file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| UploadError::Sftp(e.to_string()))?;

    let mut hasher = Sha256::new();
    let mut remaining = len;
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];

    while remaining > 0 {
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        let read_bytes = remote_file
            .read(&mut buffer[..to_read])
            .map_err(|e| UploadError::Sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(UploadError::Sftp("读取远端块用于校验时意外结束".into()));
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes as u64;
    }

    Ok(hasher.finalize().into())
}

/// 每写完一块立即读回比对 SHA-256，失败则截断远端并拒绝推进 verified 断点。
fn verify_written_chunk(
    sftp: &Sftp,
    local_path: &Path,
    remote_path: &str,
    offset: u64,
    len: u64,
) -> Result<(), UploadError> {
    if len == 0 {
        return Ok(());
    }

    let local_hash = sha256_local_range(local_path, offset, len)?;
    let remote_hash = sha256_remote_range(sftp, remote_path, offset, len)?;

    if local_hash == remote_hash {
        log::info!(
            "chunk verified offset={offset} len={len} hash={}",
            hex_encode(&local_hash)
        );
        return Ok(());
    }

    log::error!(
        "chunk hash mismatch offset={offset} len={len} local={} remote={}",
        hex_encode(&local_hash),
        hex_encode(&remote_hash)
    );
    truncate_remote_file(sftp, remote_path, offset)?;
    Err(UploadError::ChunkVerifyMismatch { offset, len })
}

fn resolve_resume_offset(
    sftp: &Sftp,
    remote_path: &str,
    remote_stat_size: u64,
    force_overwrite: &AtomicBool,
    checkpoint: &UploadCheckpoint,
) -> Result<u64, UploadError> {
    if force_overwrite.load(Ordering::Relaxed) {
        return prepare_resume_offset(sftp, remote_path, remote_stat_size, force_overwrite);
    }

    let trusted = effective_verified_bytes(checkpoint);
    if trusted > 0 && !force_overwrite.load(Ordering::Relaxed) {
        if remote_stat_size > trusted {
            log::warn!(
                "remote has {remote_stat_size} bytes but only {trusted} verified, truncate"
            );
            truncate_remote_file(sftp, remote_path, trusted)?;
        }
        if trusted > 0 {
            return Ok(trusted);
        }
    }

    if remote_stat_size > 0 {
        log::info!(
            "checkpoint has no verified progress but remote has {remote_stat_size} bytes, remove remote file"
        );
        if let Ok(stat) = sftp.stat(Path::new(remote_path)) {
            if stat.is_file() {
                let _ = sftp.unlink(Path::new(remote_path));
            }
        }
        return Ok(0);
    }

    prepare_resume_offset(sftp, remote_path, remote_stat_size, force_overwrite)
}

fn write_remote_chunk<F: FnMut(u64)>(
    sftp: &Sftp,
    remote_path: &str,
    offset: u64,
    data: &[u8],
    truncate_first: bool,
    mut on_partial_progress: F,
) -> Result<(), UploadError> {
    let path = Path::new(remote_path);
    let remote_exists = sftp
        .stat(path)
        .ok()
        .is_some_and(|stat| stat.is_file());

    let flags = if offset == 0 {
        if truncate_first && remote_exists {
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE
        } else {
            OpenFlags::WRITE | OpenFlags::CREATE
        }
    } else {
        OpenFlags::WRITE | OpenFlags::READ
    };

    let mut remote_file = sftp
        .open_mode(path, flags, 0o644, OpenType::File)
        .map_err(|e| UploadError::Sftp(e.to_string()))?;

    if offset > 0 {
        remote_file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| UploadError::Sftp(e.to_string()))?;
    }

    let mut written = 0usize;
    while written < data.len() {
        let end = (written + PROGRESS_SLICE_SIZE).min(data.len());
        remote_file
            .write_all(&data[written..end])
            .map_err(|e| UploadError::Sftp(e.to_string()))?;
        written = end;
        on_partial_progress(offset + written as u64);
    }

    remote_file
        .close()
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
    Ok(())
}

fn transfer_chunks(
    session: &mut Session,
    local_path: &Path,
    remote_path: &str,
    local_size: u64,
    cancel_flag: &Arc<AtomicBool>,
    force_overwrite: &Arc<AtomicBool>,
    strict_chunk_verify: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
    callbacks: &UploadCallbacks,
) -> Result<bool, UploadError> {
    let sftp = session
        .sftp()
        .map_err(|e| UploadError::Sftp(e.to_string()))?;

    let actual_remote_path = resolve_upload_target(&sftp, remote_path, local_path)?;
    if actual_remote_path != remote_path {
        let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
        cp.remote_path = actual_remote_path.clone();
        save_checkpoint(&cp).map_err(|e| UploadError::Sftp(e))?;
        log::info!("resolved upload target: {actual_remote_path}");
    }

    ensure_remote_parent_directories(&sftp, &actual_remote_path)?;

    let remote_stat_size = match sftp.stat(Path::new(&actual_remote_path)) {
        Ok(stat) => stat.size.unwrap_or(0),
        Err(_) => 0,
    };

    if remote_stat_size > local_size && !force_overwrite.load(Ordering::Relaxed) {
        return Err(UploadError::RemoteLargerThanLocal {
            remote: remote_stat_size,
            local: local_size,
        });
    }

    let checkpoint_snapshot = checkpoint
        .lock()
        .map_err(|e| UploadError::Sftp(e.to_string()))?
        .clone();

    let mut offset = resolve_resume_offset(
        &sftp,
        &actual_remote_path,
        remote_stat_size,
        force_overwrite,
        &checkpoint_snapshot,
    )?;

    if offset > 0 {
        verify_resume_boundary(&sftp, local_path, &actual_remote_path, offset)?;
    }

    {
        let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
        cp.uploaded_bytes = offset;
        cp.verified_bytes = offset;
    }

    let retry_count = current_retry_count(checkpoint);
    (callbacks.on_progress)(offset, local_size, "uploading", retry_count);

    let mut local_file = File::open(local_path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    local_file
        .seek(SeekFrom::Start(offset))
        .map_err(|e| UploadError::LocalIo(e.to_string()))?;

    let mut chunk_buffer = vec![0_u8; CHUNK_SIZE as usize];
    let mut truncate_first = offset == 0;

    while offset < local_size {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(UploadError::Cancelled);
        }

        let remaining = local_size - offset;
        let chunk_len = (CHUNK_SIZE as u64).min(remaining) as usize;

        local_file
            .read_exact(&mut chunk_buffer[..chunk_len])
            .map_err(|e| UploadError::LocalIo(e.to_string()))?;

        let chunk_start = offset;

        write_remote_chunk(
            &sftp,
            &actual_remote_path,
            offset,
            &chunk_buffer[..chunk_len],
            truncate_first,
            |absolute| {
                (callbacks.on_progress)(absolute, local_size, "uploading", retry_count);
            },
        )?;
        truncate_first = false;

        if strict_chunk_verify.load(Ordering::Relaxed) {
            verify_written_chunk(
                &sftp,
                local_path,
                &actual_remote_path,
                chunk_start,
                chunk_len as u64,
            )?;
        }

        offset += chunk_len as u64;
        {
            let mut cp = checkpoint.lock().map_err(|e| UploadError::Sftp(e.to_string()))?;
            cp.uploaded_bytes = offset;
            cp.verified_bytes = offset;
            save_checkpoint(&cp).map_err(|e| UploadError::Sftp(e))?;
        }
        (callbacks.on_progress)(offset, local_size, "uploading", retry_count);
    }

    let final_size = sftp
        .stat(Path::new(&actual_remote_path))
        .map_err(|e| UploadError::Sftp(e.to_string()))?
        .size
        .unwrap_or(0);

    if final_size != local_size {
        return Err(UploadError::SizeMismatch {
            local: local_size,
            remote: final_size,
        });
    }

    Ok(true)
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
    fn aligns_offset_down_to_chunk_boundary() {
        assert_eq!(align_down(0), 0);
        assert_eq!(align_down(CHUNK_SIZE), CHUNK_SIZE);
        assert_eq!(align_down(CHUNK_SIZE + 2072), CHUNK_SIZE);
        assert_eq!(align_down(33_556_504), 33_554_432);
    }

    #[test]
    fn socket_timeout_after_progress_is_retryable() {
        let wrapped = format_remote_sftp_error("Timed out waiting on socket", "/tmp/a.jar");
        assert!(!is_permanent_sftp_error(&wrapped, 19_922_944));
        assert!(!is_permanent_sftp_error("Timed out waiting on socket", 19_922_944));
    }
}
