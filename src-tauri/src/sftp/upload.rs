use super::errors::is_permanent_sftp_error;
use super::client::{connect_session, is_sftp_not_found, ConnectError, ConnectionConfig};
use crate::checkpoint::{save_checkpoint, CheckpointStatus, UploadCheckpoint};
use crate::retry::{next_backoff_ms, INITIAL_BACKOFF_MS};
use sha2::{Digest, Sha256};
use ssh2::{File as RemoteFile, FileStat, OpenFlags, OpenType, Session, Sftp};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 固定块大小：续传时回退到块边界，避免“半块”脏数据（你遇到的 33556504 正是此类问题）。
const CHUNK_SIZE: u64 = 1024 * 1024;
const HASH_READ_BUFFER_SIZE: usize = 256 * 1024;
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
        return Err(UploadError::InvalidRemotePath(
            crate::tr!("远端路径不能为空", "Remote path cannot be empty").into(),
        ));
    }

    if trimmed.ends_with('/') {
        return append_local_filename(trimmed, local_path);
    }

    let path = Path::new(trimmed);
    if is_remote_directory(sftp, path) {
        let file_name = local_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| UploadError::InvalidRemotePath(
                crate::tr!("无法读取本地文件名", "Cannot read the local file name").into(),
            ))?;
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
                    if err.is_permanent() {
                        log::error!("connect failed permanently, stop retrying: {err}");
                        return Err(UploadError::Connect(err));
                    }
                    let retry_count = increment_retry_count(&checkpoint)?;
                    let uploaded_bytes = checkpoint
                        .lock()
                        .map(|cp| cp.uploaded_bytes)
                        .unwrap_or(0);
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
                if is_permanent_sftp_error(&detail) {
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
                let message = crate::tr!(
                    format!("传输中断（{detail}），{backoff_ms}ms 后重连续传"),
                    format!("Transfer interrupted ({detail}), reconnecting to resume in {backoff_ms}ms")
                );
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

    let local_hash = sha256_local_file(session, local_path, cancel_flag, checkpoint)?;
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
            return Err(UploadError::Sftp(crate::tr!(
                format!(
                    "传完后校验时读取远端失败（弱网常见），数据块已全部校验通过，可点「开始上传」仅重试校验: {detail}"
                ),
                format!(
                    "Failed to read the remote file for final verification (common on weak networks); all chunks already verified, click \"Start upload\" to retry verification only: {detail}"
                )
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

/// 哈希大文件可能要几十秒，期间 SSH 连接上没有任何流量；顺带发 keepalive，
/// 防止 NAT/防火墙或服务器 ClientAlive 把空闲连接掐掉，导致随后读远端时又得重连。
fn sha256_local_file(
    session: &Session,
    path: &Path,
    cancel_flag: &Arc<AtomicBool>,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
) -> Result<[u8; 32], UploadError> {
    let mut file = File::open(path).map_err(|e| UploadError::LocalIo(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    loop {
        ensure_not_cancelled(cancel_flag, checkpoint)?;
        // keepalive_send 内部按 set_keepalive 的间隔节流，每轮调用并不会真的每次发包。
        if let Err(e) = session.keepalive_send() {
            log::debug!("keepalive during local hash failed: {e}");
        }
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
            return Err(UploadError::Sftp(
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

/// 给定断点可信续传位置与远端实际字节数，返回安全的续传偏移。
/// 远端不少于可信位置时直接用可信位置；否则夹紧到远端块对齐位置——
/// 否则续传边界校验会 read_exact 读过远端 EOF，抛出非永久错误而无限重连死循环。
fn safe_resume_offset(trusted: u64, remote_size: u64) -> u64 {
    if remote_size >= trusted {
        trusted
    } else {
        align_down(remote_size)
    }
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

/// 直接对内存字节求 SHA-256。上传时本地块内容已在缓冲区里（刚从本地文件读入），
/// 无需再打开+读一遍本地文件即可与远端读回结果比对。
fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
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

/// 每写完一块立即读回比对 SHA-256，失败则截断远端并拒绝推进 verified 断点。
fn verify_written_chunk(
    sftp: &Sftp,
    remote_file: &mut RemoteFile,
    local_bytes: &[u8],
    remote_path: &str,
    offset: u64,
    len: u64,
    on_activity: &dyn Fn(),
) -> Result<(), UploadError> {
    if len == 0 {
        return Ok(());
    }

    // 本地块内容已在内存缓冲，直接哈希；远端侧必须真读回，这正是要验证的目标。
    let local_hash = sha256_bytes(local_bytes);
    on_activity();
    let remote_hash = sha256_remote_handle_range(remote_file, offset, len, Some(on_activity))?;

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
        // 远端多于可信位置：多出的部分未经校验，先截断回可信位置。
        if remote_stat_size > trusted {
            log::warn!(
                "remote has {remote_stat_size} bytes but only {trusted} verified, truncate"
            );
            truncate_remote_file(sftp, remote_path, trusted)?;
        }
        let safe = safe_resume_offset(trusted, remote_stat_size);
        if safe < trusted {
            // 远端字节比断点声称的可信位置还少（文件被外部删改/替换、断点过期），
            // 回退到远端真实拥有的块对齐位置，让边界校验重新确认这一块。
            log::warn!(
                "remote has only {remote_stat_size} bytes but checkpoint trusts {trusted}; \
                 checkpoint stale, resume from block-aligned {safe}"
            );
        }
        return Ok(safe);
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

/// 一次连接只打开一次远端文件，所有块复用同一个句柄：此前每块都要 stat + open + close，
/// 读回校验再 open + close，每 MB 多出约 5 次往返，高延迟链路上 1 GB 文件要多花二十多分钟。
/// 写入与读回走同一个句柄：同一 SFTP 通道上的请求由服务器按序处理，读到的一定是刚写入的数据。
/// 从 0 开始传时截断（覆盖远端旧文件）；续传时保留已有内容。
fn open_remote_for_upload(sftp: &Sftp, remote_path: &str, fresh: bool) -> Result<RemoteFile, UploadError> {
    let flags = if fresh {
        OpenFlags::WRITE | OpenFlags::READ | OpenFlags::CREATE | OpenFlags::TRUNCATE
    } else {
        OpenFlags::WRITE | OpenFlags::READ | OpenFlags::CREATE
    };
    sftp.open_mode(Path::new(remote_path), flags, 0o644, OpenType::File)
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

    // 只有「不存在」才视为 0 字节；其他 stat 失败交给重试循环重连，
    // 否则一次超时就会让 resolve_resume_offset 把断点归零并截断远端文件。
    let remote_stat_size = match sftp.stat(Path::new(&actual_remote_path)) {
        Ok(stat) => stat.size.unwrap_or(0),
        Err(e) if is_sftp_not_found(&e) => 0,
        Err(e) => {
            log::warn!("stat remote failed (will retry): path={actual_remote_path} err={e}");
            return Err(UploadError::Sftp(e.to_string()));
        }
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
    log_resume_point(offset, local_size);

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
    let mut remote_file = open_remote_for_upload(&sftp, &actual_remote_path, offset == 0)?;
    let mut write_slice = INITIAL_WRITE_SLICE;

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

        let write_started = std::time::Instant::now();
        write_remote_chunk(
            &mut remote_file,
            offset,
            &chunk_buffer[..chunk_len],
            write_slice,
            |absolute| {
                (callbacks.on_progress)(absolute, local_size, "uploading", retry_count);
            },
        )?;
        write_slice = next_write_slice(chunk_len, write_started.elapsed());

        if strict_chunk_verify.load(Ordering::Relaxed) {
            (callbacks.on_progress)(
                chunk_start,
                local_size,
                "chunk_verifying",
                retry_count,
            );
            verify_written_chunk(
                &sftp,
                &mut remote_file,
                &chunk_buffer[..chunk_len],
                &actual_remote_path,
                chunk_start,
                chunk_len as u64,
                &callbacks.on_activity,
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

    // 关闭句柄让服务器完成所有写入，再用路径 stat 核对最终大小。
    remote_file
        .close()
        .map_err(|e| UploadError::Sftp(e.to_string()))?;
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

    // 回归：远端字节 < 断点可信位置时必须夹紧到远端块对齐位置，
    // 否则边界校验读过 EOF 会无限重连死循环。
    #[test]
    fn safe_resume_offset_clamps_when_remote_shorter_than_checkpoint() {
        // 远端足够：用可信位置
        assert_eq!(safe_resume_offset(2 * CHUNK_SIZE, 5 * CHUNK_SIZE), 2 * CHUNK_SIZE);
        assert_eq!(safe_resume_offset(2 * CHUNK_SIZE, 2 * CHUNK_SIZE), 2 * CHUNK_SIZE);
        // 远端更短：夹紧到远端块对齐位置
        assert_eq!(
            safe_resume_offset(5 * CHUNK_SIZE, 2 * CHUNK_SIZE + 123),
            2 * CHUNK_SIZE
        );
        // 远端只剩不足一块：从头开始
        assert_eq!(safe_resume_offset(5 * CHUNK_SIZE, 42), 0);
    }
}
