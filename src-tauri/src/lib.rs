mod checkpoint;
mod download_checkpoint;
mod history;
mod profiles;
mod retry;
mod sftp;
mod ssh_config;
mod stall;
mod storage;

use checkpoint::{
    clear_checkpoint, file_fingerprint, load_checkpoint, save_checkpoint, AuthKind, CheckpointStatus,
    UploadCheckpoint,
};
use history::{list_history, record_download_result, record_upload_result};
use download_checkpoint::{
    clear_download_checkpoint, load_download_checkpoint, save_download_checkpoint, DownloadCheckpoint,
};
use profiles::{delete_profile, list_profiles, save_profile, ConnectionProfile, SaveProfileRequest};
use sftp::{
    abort_socket, connect_session, create_remote_directory, download_with_resume,
    format_remote_sftp_error, list_remote_directory, remote_file_name, remote_parent_status,
    resolve_local_path, resolve_remote_path, resolve_upload_target,
    test_connection as test_sftp_connection, upload_with_resume, AuthMethod, ConnectionConfig,
    DownloadCallbacks, DownloadError, RemoteDirListing, UploadCallbacks, UploadError,
};

use serde::{Deserialize, Serialize};
use ssh_config::{list_ssh_hosts, resolve_host, SshHostEntry};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use stall::{spawn_stall_watchdog, ProgressHeartbeat};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_notification::NotificationExt;

const UPLOAD_STALL_TIMEOUT_SECS: u64 = 45;

/// 按传输阶段同步 stall 心跳：全文件校验关闭检测，块校验仅 defer，正常传输 touch
fn sync_transfer_heartbeat(heartbeat: &ProgressHeartbeat, bytes: u64, status: &str) {
    match status {
        "verifying" => heartbeat.set_active(false),
        "chunk_verifying" => {
            heartbeat.set_active(true);
            heartbeat.defer_next_check();
        }
        _ => {
            heartbeat.set_active(true);
            heartbeat.touch(bytes);
        }
    }
}

#[derive(Default)]
struct UploadRuntime {
    cancel_flag: Option<Arc<AtomicBool>>,
    abort_socket: Option<Arc<Mutex<Option<TcpStream>>>>,
    is_running: bool,
    /// 传输 worker 线程句柄。reset 时须 join 旧线程，确保它不再写断点文件，
    /// 否则新传输可能与旧线程并发写同一断点文件导致内容交错损坏。
    worker: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct DownloadRuntime {
    cancel_flag: Option<Arc<AtomicBool>>,
    abort_socket: Option<Arc<Mutex<Option<TcpStream>>>>,
    is_running: bool,
    /// 同 UploadRuntime::worker。
    worker: Option<thread::JoinHandle<()>>,
}

const DOWNLOAD_STALL_TIMEOUT_SECS: u64 = 45;

#[derive(Default)]
struct CloseGate(AtomicBool);

#[derive(Clone, Serialize)]
struct TransferRunningState {
    upload_running: bool,
    download_running: bool,
}

#[derive(Debug, Deserialize)]
struct ConnectionRequest {
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    password: Option<String>,
    key_path: Option<String>,
    key_passphrase: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StartUploadRequest {
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    password: Option<String>,
    key_path: Option<String>,
    key_passphrase: Option<String>,
    local_path: String,
    remote_path: String,
    #[serde(default)]
    strict_chunk_verify: Option<bool>,
    #[serde(default)]
    force_overwrite: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ProbeUploadRequest {
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    password: Option<String>,
    key_path: Option<String>,
    key_passphrase: Option<String>,
    local_path: String,
    remote_path: String,
}

#[derive(Clone, Serialize)]
struct RemoteUploadProbe {
    resolved_remote_path: String,
    remote_exists: bool,
    remote_size: u64,
    local_size: u64,
    verified_bytes: u64,
    action: String,
    message: String,
}

#[derive(Clone, Serialize)]
struct UploadProgressEvent {
    uploaded_bytes: u64,
    total_bytes: u64,
    status: String,
    message: String,
    retry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify_summary: Option<String>,
}

#[derive(Clone, Serialize)]
struct UploadRetryEvent {
    delay_ms: u64,
    message: String,
    retry_count: u32,
}

#[derive(Debug, Deserialize)]
struct StartDownloadRequest {
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    password: Option<String>,
    key_path: Option<String>,
    key_passphrase: Option<String>,
    local_path: String,
    remote_path: String,
    #[serde(default)]
    strict_chunk_verify: Option<bool>,
    #[serde(default)]
    force_overwrite: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ProbeDownloadRequest {
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    password: Option<String>,
    key_path: Option<String>,
    key_passphrase: Option<String>,
    local_path: String,
    remote_path: String,
}

#[derive(Clone, Serialize)]
struct RemoteDownloadProbe {
    resolved_local_path: String,
    resolved_remote_path: String,
    remote_exists: bool,
    remote_size: u64,
    local_size: u64,
    verified_bytes: u64,
    action: String,
    message: String,
}

#[derive(Clone, Serialize)]
struct DownloadProgressEvent {
    downloaded_bytes: u64,
    total_bytes: u64,
    status: String,
    message: String,
    retry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify_summary: Option<String>,
}

#[derive(Clone, Serialize)]
struct DownloadRetryEvent {
    delay_ms: u64,
    message: String,
    retry_count: u32,
}

fn build_auth(request: &ConnectionRequest) -> Result<AuthMethod, String> {
    match request.auth_type.as_str() {
        "password" => {
            let password = request
                .password
                .clone()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "请填写密码".to_string())?;
            Ok(AuthMethod::Password(password))
        }
        "key" => {
            let key_path = request
                .key_path
                .clone()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "请选择私钥文件".to_string())?;
            Ok(AuthMethod::PrivateKey {
                key_path,
                passphrase: request.key_passphrase.clone().filter(|p| !p.is_empty()),
            })
        }
        _ => Err("未知认证方式".into()),
    }
}

fn build_connection_config(request: &ConnectionRequest) -> Result<ConnectionConfig, String> {
    Ok(ConnectionConfig {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth: build_auth(request)?,
    })
}

fn remote_paths_compatible(
    checkpoint_remote: &str,
    resolved_remote: &str,
    user_remote: &str,
) -> bool {
    let cp = checkpoint_remote.trim();
    let resolved = resolved_remote.trim();
    let user = user_remote.trim();

    if cp == resolved || cp == user {
        return true;
    }

    let base = user.trim_end_matches('/');
    cp == base || cp.starts_with(&format!("{base}/"))
}

fn local_paths_compatible(
    checkpoint_local: &str,
    resolved_local: &str,
    user_local: &str,
) -> bool {
    let cp = checkpoint_local.trim();
    let resolved = resolved_local.trim();
    let user = user_local.trim();

    if cp == resolved || cp == user {
        return true;
    }

    let base = user.trim_end_matches(|c| c == '/' || c == '\\');
    cp == base || cp.starts_with(&format!("{base}/")) || cp.starts_with(&format!("{base}\\"))
}

fn send_desktop_notification(app: &AppHandle, title: &str, body: &str) {
    let _ = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show();
}

fn format_upload_failure(err: &UploadError, remote_path: &str, retry_count: u32) -> String {
    let body = match err {
        UploadError::HashMismatch => {
            "SHA-256 校验未通过，远端文件已删除，请重新上传".to_string()
        }
        UploadError::InvalidRemotePath(msg) => {
            format!("远端路径错误：{remote_path} — {msg}")
        }
        UploadError::RemoteLargerThanLocal { remote, local } => {
            format!(
                "远端路径错误：{remote_path} — 远端文件（{remote} 字节）大于本地文件（{local} 字节）"
            )
        }
        UploadError::ResumeBoundaryMismatch { offset } => {
            format!(
                "远端路径错误：{remote_path} — 续传边界校验失败（offset={offset}），建议清除断点后重试"
            )
        }
        UploadError::Sftp(detail) => {
            if detail.contains("远端路径错误") || detail.starts_with("远端路径「") {
                detail.clone()
            } else {
                format_remote_sftp_error(detail, remote_path)
            }
        }
        _ => err.to_string(),
    };
    format!("{body}（已累计自动重试 {retry_count} 次）")
}

fn format_download_failure(err: &DownloadError, remote_path: &str, retry_count: u32) -> String {
    let body = match err {
        DownloadError::HashMismatch => {
            "SHA-256 校验未通过，本地文件已删除，请重新下载".to_string()
        }
        DownloadError::InvalidRemotePath(msg) | DownloadError::RemoteNotAFile(msg) => {
            format!("远端路径错误：{remote_path} — {msg}")
        }
        DownloadError::Sftp(detail) => {
            if detail.contains("远端路径错误") || detail.starts_with("远端路径「") {
                detail.clone()
            } else {
                format_remote_sftp_error(detail, remote_path)
            }
        }
        _ => err.to_string(),
    };
    format!("{body}（已累计自动重试 {retry_count} 次）")
}

fn completed_verify_summary(strict_chunk_verify: bool) -> String {
    if strict_chunk_verify {
        "校验通过 · 逐块校验 + SHA-256 一致".to_string()
    } else {
        "校验通过 · SHA-256 一致".to_string()
    }
}

#[derive(Debug, Deserialize)]
struct ListRemoteDirRequest {
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    password: Option<String>,
    key_path: Option<String>,
    key_passphrase: Option<String>,
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateRemoteDirRequest {
    host: String,
    port: u16,
    username: String,
    auth_type: String,
    password: Option<String>,
    key_path: Option<String>,
    key_passphrase: Option<String>,
    parent_path: String,
    dir_name: String,
}

#[tauri::command]
fn create_remote_dir(request: CreateRemoteDirRequest) -> Result<RemoteDirListing, String> {
    if request.host.trim().is_empty() {
        return Err("请填写主机".into());
    }
    if request.parent_path.trim().is_empty() {
        return Err("请先进入要创建目录的父路径".into());
    }

    let conn = ConnectionRequest {
        host: request.host,
        port: request.port,
        username: request.username,
        auth_type: request.auth_type,
        password: request.password,
        key_path: request.key_path,
        key_passphrase: request.key_passphrase,
    };
    let config = build_connection_config(&conn)?;
    create_remote_directory(&config, &request.parent_path, &request.dir_name)
}

#[tauri::command]
fn list_remote_dir(request: ListRemoteDirRequest) -> Result<RemoteDirListing, String> {
    if request.host.trim().is_empty() {
        return Err("请填写主机".into());
    }
    if request.username.trim().is_empty() {
        return Err("请填写用户名".into());
    }

    let conn = ConnectionRequest {
        host: request.host,
        port: request.port,
        username: request.username,
        auth_type: request.auth_type,
        password: request.password,
        key_path: request.key_path,
        key_passphrase: request.key_passphrase,
    };
    let config = build_connection_config(&conn)?;
    list_remote_directory(&config, request.path.as_deref())
}

#[tauri::command]
fn get_transfer_running(
    upload_runtime: State<'_, Mutex<UploadRuntime>>,
    download_runtime: State<'_, Mutex<DownloadRuntime>>,
) -> Result<TransferRunningState, String> {
    let upload_running = upload_runtime
        .lock()
        .map(|s| s.is_running)
        .map_err(|e| e.to_string())?;
    let download_running = download_runtime
        .lock()
        .map(|s| s.is_running)
        .map_err(|e| e.to_string())?;
    Ok(TransferRunningState {
        upload_running,
        download_running,
    })
}

#[tauri::command]
fn prepare_app_close(
    app: AppHandle,
    gate: State<'_, CloseGate>,
    upload_runtime: State<'_, Mutex<UploadRuntime>>,
    download_runtime: State<'_, Mutex<DownloadRuntime>>,
) -> Result<(), String> {
    gate.0.store(true, Ordering::Relaxed);

    let upload_running = upload_runtime
        .lock()
        .map(|s| s.is_running)
        .map_err(|e| e.to_string())?;
    if upload_running {
        let _ = cancel_upload(app.clone(), upload_runtime);
    }

    let download_running = download_runtime
        .lock()
        .map(|s| s.is_running)
        .map_err(|e| e.to_string())?;
    if download_running {
        let _ = cancel_download(app, download_runtime);
    }

    Ok(())
}

fn should_block_app_close(
    gate: &CloseGate,
    upload_runtime: &Mutex<UploadRuntime>,
    download_runtime: &Mutex<DownloadRuntime>,
) -> bool {
    if gate.0.load(Ordering::Relaxed) {
        return false;
    }
    let upload_running = upload_runtime.lock().map(|s| s.is_running).unwrap_or(false);
    let download_running = download_runtime.lock().map(|s| s.is_running).unwrap_or(false);
    upload_running || download_running
}

#[tauri::command]
fn list_hosts() -> Result<Vec<SshHostEntry>, String> {
    list_ssh_hosts()
}

#[tauri::command]
fn get_host(alias: String) -> Result<Option<SshHostEntry>, String> {
    resolve_host(&alias)
}

#[tauri::command]
fn get_saved_checkpoint() -> Result<Option<UploadCheckpoint>, String> {
    load_checkpoint()
}

#[tauri::command]
fn clear_saved_checkpoint() -> Result<(), String> {
    clear_checkpoint()
}

#[tauri::command]
fn get_saved_download_checkpoint() -> Result<Option<DownloadCheckpoint>, String> {
    load_download_checkpoint()
}

#[tauri::command]
fn clear_saved_download_checkpoint() -> Result<(), String> {
    clear_download_checkpoint()
}

#[tauri::command]
fn list_saved_profiles() -> Result<Vec<ConnectionProfile>, String> {
    list_profiles()
}

#[tauri::command]
fn save_connection_profile(request: SaveProfileRequest) -> Result<ConnectionProfile, String> {
    save_profile(request)
}

#[tauri::command]
fn delete_connection_profile(id: String) -> Result<(), String> {
    delete_profile(id)
}

#[tauri::command]
fn list_upload_history() -> Result<Vec<history::UploadHistoryEntry>, String> {
    list_history()
}

#[tauri::command]
fn probe_remote_upload(request: ProbeUploadRequest) -> Result<RemoteUploadProbe, String> {
    if request.host.trim().is_empty() {
        return Err("请填写主机".into());
    }
    if request.remote_path.trim().is_empty() {
        return Err("请填写远端路径".into());
    }

    let local_path = PathBuf::from(&request.local_path);
    if !local_path.is_file() {
        return Err("本地文件不存在".into());
    }

    let local_size = std::fs::metadata(&local_path)
        .map_err(|e| e.to_string())?
        .len();
    let resolved_remote_path =
        resolve_remote_path(&request.remote_path, &local_path).map_err(|e| e.to_string())?;

    let conn = ConnectionRequest {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth_type: request.auth_type.clone(),
        password: request.password.clone(),
        key_path: request.key_path.clone(),
        key_passphrase: request.key_passphrase.clone(),
    };
    let config = build_connection_config(&conn)?;
    let (session, _) = connect_session(&config).map_err(|e| e.to_string())?;
    let sftp = session.sftp().map_err(|e| e.to_string())?;

    let actual_remote_path =
        resolve_upload_target(&sftp, &resolved_remote_path, &local_path).map_err(|e| e.to_string())?;

    let parent_hint = remote_parent_status(&sftp, &actual_remote_path);

    let remote_size = match sftp.stat(std::path::Path::new(&actual_remote_path)) {
        Ok(stat) => stat.size.unwrap_or(0),
        Err(_) => 0,
    };
    let remote_exists = remote_size > 0;

    let checkpoint = load_checkpoint().ok().flatten();
    let verified_bytes = checkpoint
        .as_ref()
        .filter(|cp| {
            cp.local_path == request.local_path
                && remote_paths_compatible(&cp.remote_path, &actual_remote_path, &request.remote_path)
                && cp.host == request.host
                && cp.port == request.port
                && cp.username == request.username
        })
        .map(|cp| {
            if cp.verified_bytes > 0 {
                cp.verified_bytes
            } else {
                cp.uploaded_bytes
            }
        })
        .unwrap_or(0);

    let (action, message) = if checkpoint.as_ref().is_some_and(|cp| {
        cp.failure_reason.as_deref() == Some("verify_read_failed")
            && cp.uploaded_bytes >= cp.file_size
    }) {
        (
            "verify_retry",
            "传输已完成，仅需重试最终校验，无需从头重传".into(),
        )
    } else if checkpoint.as_ref().is_some_and(|cp| {
        cp.failure_reason.as_deref() == Some("hash_mismatch")
            || (cp.status == CheckpointStatus::Failed
                && cp.uploaded_bytes >= cp.file_size
                && cp.failure_reason.as_deref() != Some("verify_read_failed"))
    }) {
        (
            "full_reupload",
            "上次校验未通过，将删除远端文件并从头重传".into(),
        )
    } else if verified_bytes > 0 && verified_bytes < local_size {
        (
            "resume",
            format!(
                "将从已验证的 {} 字节处续传（约 {:.1} MB）",
                verified_bytes,
                verified_bytes as f64 / 1024.0 / 1024.0
            ),
        )
    } else if remote_exists {
        (
            "overwrite",
            format!(
                "远端已有文件（{} 字节），将覆盖重传",
                remote_size
            ),
        )
    } else {
        ("new", "远端无此文件，将新建上传".into())
    };

    let message = if let Some(parent_msg) = parent_hint {
        format!("{message}。{parent_msg}")
    } else {
        message
    };

    Ok(RemoteUploadProbe {
        resolved_remote_path: actual_remote_path,
        remote_exists,
        remote_size,
        local_size,
        verified_bytes,
        action: action.into(),
        message,
    })
}

#[tauri::command]
fn probe_remote_download(request: ProbeDownloadRequest) -> Result<RemoteDownloadProbe, String> {
    if request.host.trim().is_empty() {
        return Err("请填写主机".into());
    }
    if request.remote_path.trim().is_empty() {
        return Err("请填写远端文件路径".into());
    }
    if request.local_path.trim().is_empty() {
        return Err("请填写本地保存路径".into());
    }

    let conn = ConnectionRequest {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth_type: request.auth_type.clone(),
        password: request.password.clone(),
        key_path: request.key_path.clone(),
        key_passphrase: request.key_passphrase.clone(),
    };
    let config = build_connection_config(&conn)?;
    let (session, _) = connect_session(&config).map_err(|e| e.to_string())?;
    let sftp = session.sftp().map_err(|e| e.to_string())?;

    let remote_path = request.remote_path.trim();
    let remote_stat = sftp
        .stat(std::path::Path::new(remote_path))
        .map_err(|e| format!("远端路径错误：{remote_path} — 无法访问：{e}"))?;
    if !remote_stat.is_file() {
        return Err(format!(
            "远端路径错误：{remote_path} — 请填写服务器上的文件路径，不能是目录"
        ));
    }
    let remote_size = remote_stat.size.unwrap_or(0);
    if remote_size == 0 {
        return Err("远端文件为空".into());
    }

    let file_name = remote_file_name(remote_path).map_err(|e| e.to_string())?;
    let resolved_local_path =
        resolve_local_path(&request.local_path, &file_name).map_err(|e| e.to_string())?;
    let local_size = std::fs::metadata(&resolved_local_path)
        .map(|m| m.len())
        .unwrap_or(0);

    let checkpoint = load_download_checkpoint().ok().flatten();
    let verified_bytes = checkpoint
        .as_ref()
        .filter(|cp| {
            local_paths_compatible(&cp.local_path, &resolved_local_path, &request.local_path)
                && remote_paths_compatible(&cp.remote_path, remote_path, remote_path)
                && cp.host == request.host
                && cp.port == request.port
                && cp.username == request.username
        })
        .map(|cp| {
            if cp.verified_bytes > 0 {
                cp.verified_bytes
            } else {
                cp.downloaded_bytes
            }
        })
        .unwrap_or(0);

    let local_exists = local_size > 0;
    let (action, message) = if checkpoint.as_ref().is_some_and(|cp| {
        cp.failure_reason.as_deref() == Some("verify_read_failed")
            && cp.downloaded_bytes >= cp.file_size
    }) {
        (
            "verify_retry",
            "传输已完成，仅需重试最终校验，无需从头重下".into(),
        )
    } else if checkpoint.as_ref().is_some_and(|cp| {
        cp.failure_reason.as_deref() == Some("hash_mismatch")
            || (cp.status == CheckpointStatus::Failed
                && cp.downloaded_bytes >= cp.file_size
                && cp.failure_reason.as_deref() != Some("verify_read_failed"))
    }) {
        (
            "full_redownload",
            "上次校验未通过，将删除本地文件并从头重下".into(),
        )
    } else if verified_bytes > 0 && verified_bytes < remote_size {
        (
            "resume",
            format!(
                "将从已验证的 {} 字节处续传（约 {:.1} MB）",
                verified_bytes,
                verified_bytes as f64 / 1024.0 / 1024.0
            ),
        )
    } else if local_exists {
        (
            "overwrite",
            format!(
                "本地已有文件（{} 字节），将覆盖重下",
                local_size
            ),
        )
    } else {
        ("new", "本地无此文件，将新建下载".into())
    };

    Ok(RemoteDownloadProbe {
        resolved_local_path,
        resolved_remote_path: remote_path.to_string(),
        remote_exists: true,
        remote_size,
        local_size,
        verified_bytes,
        action: action.into(),
        message,
    })
}

#[tauri::command]
fn test_connection(request: ConnectionRequest) -> Result<String, String> {
    if request.host.trim().is_empty() {
        return Err("请填写主机".into());
    }
    if request.username.trim().is_empty() {
        return Err("请填写用户名".into());
    }

    let config = build_connection_config(&request)?;
    test_sftp_connection(&config).map_err(|e| e.to_string())
}

#[tauri::command]
fn cancel_upload(
    app: AppHandle,
    runtime: State<'_, Mutex<UploadRuntime>>,
) -> Result<(), String> {
    let state = runtime.lock().map_err(|e| e.to_string())?;
    if !state.is_running {
        return Err("当前没有进行中的上传".into());
    }

    if let Some(flag) = &state.cancel_flag {
        flag.store(true, Ordering::Relaxed);
    }

    if let Some(holder) = &state.abort_socket {
        if let Ok(guard) = holder.lock() {
            if let Some(socket) = guard.as_ref() {
                abort_socket(socket);
            }
        }
    }

    let _ = app.emit(
        "upload-progress",
        UploadProgressEvent {
            uploaded_bytes: 0,
            total_bytes: 0,
            status: "cancelling".into(),
            message: "正在取消，正在中断连接...".into(),
            retry_count: 0,
            verify_summary: None,
        },
    );

    Ok(())
}

#[tauri::command]
fn reset_upload_state(runtime: State<'_, Mutex<UploadRuntime>>) -> Result<(), String> {
    // 置取消位 + 中断在途 IO，取出 worker 句柄后释放锁再 join：
    // worker 结束时会回锁 runtime 设 is_running=false，若持锁 join 会死锁。
    let worker = {
        let mut state = runtime.lock().map_err(|e| e.to_string())?;
        if let Some(flag) = &state.cancel_flag {
            flag.store(true, Ordering::Relaxed);
        }
        // 主动关闭 socket 让 worker 尽快跳出阻塞的网络 syscall，避免 join 长时间等待。
        if let Some(holder) = &state.abort_socket {
            if let Ok(guard) = holder.lock() {
                if let Some(socket) = guard.as_ref() {
                    abort_socket(socket);
                }
            }
        }
        state.worker.take()
    };
    // 等旧 worker 真正退出，确保它不再写断点文件，再放行新传输。
    if let Some(handle) = worker {
        let _ = handle.join();
    }
    let mut state = runtime.lock().map_err(|e| e.to_string())?;
    state.is_running = false;
    state.cancel_flag = None;
    state.abort_socket = None;
    state.worker = None;
    Ok(())
}

#[tauri::command]
fn start_upload(
    app: AppHandle,
    runtime: State<'_, Mutex<UploadRuntime>>,
    request: StartUploadRequest,
) -> Result<(), String> {
    let mut state = runtime.lock().map_err(|e| e.to_string())?;
    if state.is_running {
        return Err("已有上传任务在进行中".into());
    }

    let local_path = PathBuf::from(&request.local_path);
    if !local_path.is_file() {
        return Err("本地文件不存在".into());
    }

    if request.remote_path.trim().is_empty() {
        return Err("请填写远端路径".into());
    }

    let resolved_remote_path = sftp::resolve_remote_path(&request.remote_path, &local_path)
        .map_err(|e| e.to_string())?;

    let saved_key_path = request.key_path.clone();
    let auth = build_auth(&ConnectionRequest {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth_type: request.auth_type.clone(),
        password: request.password.clone(),
        key_path: request.key_path.clone(),
        key_passphrase: request.key_passphrase.clone(),
    })?;

    let auth_kind = match &auth {
        AuthMethod::Password(_) => AuthKind::Password,
        AuthMethod::PrivateKey { .. } => AuthKind::PrivateKey,
    };

    let (file_size, local_mtime) = file_fingerprint(&local_path)?;

    let existing_checkpoint = load_checkpoint().ok().flatten();
    let user_force_overwrite = request.force_overwrite.unwrap_or(false);
    let mut should_resume = existing_checkpoint.as_ref().is_some_and(|cp| {
        let trusted = if cp.verified_bytes > 0 {
            cp.verified_bytes
        } else {
            cp.uploaded_bytes
        };
        remote_paths_compatible(&cp.remote_path, &resolved_remote_path, &request.remote_path)
            && cp.local_path == request.local_path
            && cp.host == request.host
            && cp.port == request.port
            && cp.username == request.username
            && trusted > 0
            && trusted < cp.file_size
            && matches!(cp.status, CheckpointStatus::InProgress | CheckpointStatus::Failed)
    });

    if user_force_overwrite {
        should_resume = false;
        let _ = clear_checkpoint();
    } else if existing_checkpoint.as_ref().is_some_and(|cp| {
        let trusted = if cp.verified_bytes > 0 {
            cp.verified_bytes
        } else {
            cp.uploaded_bytes
        };
        cp.status == CheckpointStatus::Failed && trusted == 0
    }) {
        let _ = clear_checkpoint();
    }

    if should_resume {
        let cp = existing_checkpoint.as_ref().unwrap();
        if cp.file_size != file_size {
            return Err(format!(
                "本地文件大小已变化（断点记录 {} 字节，当前 {} 字节），请清除断点后重新上传",
                cp.file_size, file_size
            ));
        }
        if let Some(saved_mtime) = cp.local_mtime {
            if saved_mtime != local_mtime {
                return Err(
                    "本地文件已修改（修改时间变化），请清除断点后重新上传".into(),
                );
            }
        }
    }

    let (initial_uploaded_bytes, initial_verified_bytes, initial_retry_count) = if should_resume {
        let cp = existing_checkpoint.as_ref().unwrap();
        let trusted = if cp.verified_bytes > 0 {
            cp.verified_bytes
        } else {
            cp.uploaded_bytes
        };
        (trusted, cp.verified_bytes, cp.retry_count)
    } else {
        (0, 0, 0)
    };

    let checkpoint = Arc::new(Mutex::new(UploadCheckpoint {
        local_path: request.local_path.clone(),
        remote_path: existing_checkpoint
            .as_ref()
            .filter(|_| should_resume)
            .map(|cp| cp.remote_path.clone())
            .unwrap_or_else(|| resolved_remote_path.clone()),
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth_kind,
        key_path: saved_key_path,
        file_size,
        local_mtime: Some(local_mtime),
        uploaded_bytes: initial_uploaded_bytes,
        verified_bytes: initial_verified_bytes,
        retry_count: initial_retry_count,
        failure_reason: None,
        status: CheckpointStatus::InProgress,
    }));

    save_checkpoint(&*checkpoint.lock().map_err(|e| e.to_string())?)?;

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let force_overwrite = Arc::new(AtomicBool::new(!should_resume));
    let strict_chunk_verify = Arc::new(AtomicBool::new(
        request.strict_chunk_verify.unwrap_or(true),
    ));
    let abort_socket_holder = Arc::new(Mutex::new(None));
    state.cancel_flag = Some(cancel_flag.clone());
    state.abort_socket = Some(abort_socket_holder.clone());
    state.is_running = true;

    let config = ConnectionConfig {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth,
    };

    let remote_path = resolved_remote_path;
    let local_path_string = request.local_path.clone();
    let checkpoint_for_thread = checkpoint.clone();
    let host_for_history = request.host.clone();
    let username_for_history = request.username.clone();
    let port_for_history = request.port;

    let worker = thread::spawn(move || {
        let heartbeat = Arc::new(ProgressHeartbeat::new(initial_uploaded_bytes));
        let stall_timeout = Duration::from_secs(UPLOAD_STALL_TIMEOUT_SECS);
        let stall_secs = UPLOAD_STALL_TIMEOUT_SECS;

        let watchdog = spawn_stall_watchdog(
            heartbeat.clone(),
            cancel_flag.clone(),
            abort_socket_holder.clone(),
            stall_timeout,
            {
                let app = app.clone();
                let checkpoint = checkpoint_for_thread.clone();
                move |stalled_bytes| {
                    let (total, retry_count) = checkpoint
                        .lock()
                        .map(|cp| (cp.file_size, cp.retry_count))
                        .unwrap_or((0, 0));
                    let _ = app.emit(
                        "upload-progress",
                        UploadProgressEvent {
                            uploaded_bytes: stalled_bytes,
                            total_bytes: total,
                            status: "stalled".into(),
                            message: format!(
                                "传输超过 {stall_secs} 秒无响应，正在中断并重连..."
                            ),
                            retry_count,
                            verify_summary: None,
                        },
                    );
                }
            },
        );

        let strict_chunk_verify_summary = strict_chunk_verify.clone();
        let result = upload_with_resume(
            config,
            PathBuf::from(&local_path_string).as_path(),
            &remote_path,
            cancel_flag,
            force_overwrite,
            strict_chunk_verify,
            abort_socket_holder,
            checkpoint_for_thread.clone(),
            UploadCallbacks {
                on_progress: Box::new({
                    let app = app.clone();
                    let heartbeat = heartbeat.clone();
                    move |uploaded, total, status, retry_count| {
                        sync_transfer_heartbeat(&heartbeat, uploaded, status);
                        let retry_hint = if retry_count > 0 {
                            format!(" · 重试 {retry_count} 次")
                        } else {
                            String::new()
                        };
                        let _ = app.emit(
                            "upload-progress",
                            UploadProgressEvent {
                                uploaded_bytes: uploaded,
                                total_bytes: total,
                                status: status.to_string(),
                                message: format!("已上传 {uploaded} / {total} 字节{retry_hint}"),
                                retry_count,
                                verify_summary: None,
                            },
                        );
                    }
                }),
                on_retry: Box::new({
                    let app = app.clone();
                    let checkpoint = checkpoint_for_thread.clone();
                    let heartbeat = heartbeat.clone();
                    move |retry_count, delay_ms, message| {
                        let (uploaded, total) = checkpoint
                            .lock()
                            .map(|cp| (cp.uploaded_bytes, cp.file_size))
                            .unwrap_or((0, 0));
                        sync_transfer_heartbeat(&heartbeat, uploaded, "retrying");
                        if let Ok(cp) = checkpoint.lock() {
                            let _ = save_checkpoint(&cp);
                        }
                        let retry_message = format!("{message}（第 {retry_count} 次重试）");
                        let _ = app.emit(
                            "upload-retry",
                            UploadRetryEvent {
                                delay_ms,
                                message: retry_message.clone(),
                                retry_count,
                            },
                        );
                        let _ = app.emit(
                            "upload-progress",
                            UploadProgressEvent {
                                uploaded_bytes: uploaded,
                                total_bytes: total,
                                status: "retrying".into(),
                                message: retry_message,
                                retry_count,
                                verify_summary: None,
                            },
                        );
                    }
                }),
                on_activity: Box::new({
                    let heartbeat = heartbeat.clone();
                    move || {
                        heartbeat.defer_next_check();
                    }
                }),
            },
        );

        watchdog.stop();
        heartbeat.set_active(false);

        match result {
            Ok(()) => {
                if let Ok(cp) = checkpoint_for_thread.lock() {
                    let _ = save_checkpoint(&cp);
                    let _ = clear_checkpoint();
                    let retry_hint = if cp.retry_count > 0 {
                        format!("（共重试 {} 次）", cp.retry_count)
                    } else {
                        String::new()
                    };
                    let message = format!("上传完成{retry_hint}");
                    record_upload_result(
                        &cp.local_path,
                        &cp.remote_path,
                        &host_for_history,
                        port_for_history,
                        &username_for_history,
                        cp.file_size,
                        "completed",
                        cp.retry_count,
                        Some(message.clone()),
                    );
                    send_desktop_notification(&app, "ResiliSSH", &message);
                    let _ = app.emit(
                        "upload-progress",
                        UploadProgressEvent {
                            uploaded_bytes: cp.file_size,
                            total_bytes: cp.file_size,
                            status: "completed".into(),
                            message,
                            retry_count: cp.retry_count,
                            verify_summary: Some(completed_verify_summary(
                                strict_chunk_verify_summary.load(Ordering::Relaxed),
                            )),
                        },
                    );
                }
            }
            Err(UploadError::Cancelled) => {
                if let Ok(cp) = checkpoint_for_thread.lock() {
                    let _ = save_checkpoint(&cp);
                    let message = format!(
                        "上传已取消，可稍后继续（已重试 {} 次）",
                        cp.retry_count
                    );
                    record_upload_result(
                        &cp.local_path,
                        &cp.remote_path,
                        &host_for_history,
                        port_for_history,
                        &username_for_history,
                        cp.file_size,
                        "cancelled",
                        cp.retry_count,
                        Some(message.clone()),
                    );
                    let _ = app.emit(
                        "upload-progress",
                        UploadProgressEvent {
                            uploaded_bytes: cp.uploaded_bytes,
                            total_bytes: cp.file_size,
                            status: "cancelled".into(),
                            message,
                            retry_count: cp.retry_count,
                            verify_summary: None,
                        },
                    );
                }
            }
            Err(err) => {
                if let Ok(mut cp) = checkpoint_for_thread.lock() {
                    cp.status = CheckpointStatus::Failed;
                    let _ = save_checkpoint(&cp);
                    let message = if cp.failure_reason.as_deref() == Some("verify_read_failed") {
                        format!(
                            "传完后校验时网络中断，各数据块均已校验通过，请直接点「开始上传」重试（已重试 {} 次）",
                            cp.retry_count
                        )
                    } else {
                        format_upload_failure(&err, &cp.remote_path, cp.retry_count)
                    };
                    record_upload_result(
                        &cp.local_path,
                        &cp.remote_path,
                        &host_for_history,
                        port_for_history,
                        &username_for_history,
                        cp.file_size,
                        "failed",
                        cp.retry_count,
                        Some(message.clone()),
                    );
                    send_desktop_notification(&app, "ResiliSSH 上传失败", &message);
                    let _ = app.emit(
                        "upload-progress",
                        UploadProgressEvent {
                            uploaded_bytes: cp.uploaded_bytes,
                            total_bytes: cp.file_size,
                            status: "failed".into(),
                            message,
                            retry_count: cp.retry_count,
                            verify_summary: None,
                        },
                    );
                }
            }
        }

        if let Some(runtime) = app.try_state::<Mutex<UploadRuntime>>() {
            if let Ok(mut state) = runtime.lock() {
                state.is_running = false;
                state.cancel_flag = None;
                state.abort_socket = None;
            }
        }
    });

    // 记录 worker 句柄，供 reset_upload_state join，避免与新传输并发写断点。
    state.worker = Some(worker);

    Ok(())
}

#[tauri::command]
fn cancel_download(
    app: AppHandle,
    runtime: State<'_, Mutex<DownloadRuntime>>,
) -> Result<(), String> {
    let state = runtime.lock().map_err(|e| e.to_string())?;
    if !state.is_running {
        return Err("当前没有进行中的下载".into());
    }

    if let Some(flag) = &state.cancel_flag {
        flag.store(true, Ordering::Relaxed);
    }

    if let Some(holder) = &state.abort_socket {
        if let Ok(guard) = holder.lock() {
            if let Some(socket) = guard.as_ref() {
                abort_socket(socket);
            }
        }
    }

    let _ = app.emit(
        "download-progress",
        DownloadProgressEvent {
            downloaded_bytes: 0,
            total_bytes: 0,
            status: "cancelling".into(),
            message: "正在取消，正在中断连接...".into(),
            retry_count: 0,
            verify_summary: None,
        },
    );

    Ok(())
}

#[tauri::command]
fn reset_download_state(runtime: State<'_, Mutex<DownloadRuntime>>) -> Result<(), String> {
    // 见 reset_upload_state：取出 worker 句柄后释放锁再 join，避免死锁并杜绝并发写断点。
    let worker = {
        let mut state = runtime.lock().map_err(|e| e.to_string())?;
        if let Some(flag) = &state.cancel_flag {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(holder) = &state.abort_socket {
            if let Ok(guard) = holder.lock() {
                if let Some(socket) = guard.as_ref() {
                    abort_socket(socket);
                }
            }
        }
        state.worker.take()
    };
    if let Some(handle) = worker {
        let _ = handle.join();
    }
    let mut state = runtime.lock().map_err(|e| e.to_string())?;
    state.is_running = false;
    state.cancel_flag = None;
    state.abort_socket = None;
    state.worker = None;
    Ok(())
}

#[tauri::command]
fn start_download(
    app: AppHandle,
    runtime: State<'_, Mutex<DownloadRuntime>>,
    request: StartDownloadRequest,
) -> Result<(), String> {
    let mut state = runtime.lock().map_err(|e| e.to_string())?;
    if state.is_running {
        return Err("已有下载任务在进行中".into());
    }

    if request.remote_path.trim().is_empty() {
        return Err("请填写远端文件路径".into());
    }
    if request.local_path.trim().is_empty() {
        return Err("请填写本地保存路径".into());
    }

    let remote_path = request.remote_path.trim().to_string();
    let file_name = remote_file_name(&remote_path).map_err(|e| e.to_string())?;
    let resolved_local_path =
        resolve_local_path(&request.local_path, &file_name).map_err(|e| e.to_string())?;

    let saved_key_path = request.key_path.clone();
    let auth = build_auth(&ConnectionRequest {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth_type: request.auth_type.clone(),
        password: request.password.clone(),
        key_path: request.key_path.clone(),
        key_passphrase: request.key_passphrase.clone(),
    })?;

    let auth_kind = match &auth {
        AuthMethod::Password(_) => AuthKind::Password,
        AuthMethod::PrivateKey { .. } => AuthKind::PrivateKey,
    };

    let existing_checkpoint = load_download_checkpoint().ok().flatten();
    let user_force_overwrite = request.force_overwrite.unwrap_or(false);
    let mut should_resume = existing_checkpoint.as_ref().is_some_and(|cp| {
        let trusted = if cp.verified_bytes > 0 {
            cp.verified_bytes
        } else {
            cp.downloaded_bytes
        };
        local_paths_compatible(&cp.local_path, &resolved_local_path, &request.local_path)
            && remote_paths_compatible(&cp.remote_path, &remote_path, &remote_path)
            && cp.host == request.host
            && cp.port == request.port
            && cp.username == request.username
            && trusted > 0
            && trusted < cp.file_size
            && matches!(cp.status, CheckpointStatus::InProgress | CheckpointStatus::Failed)
    });

    if user_force_overwrite {
        should_resume = false;
        let _ = clear_download_checkpoint();
    } else if existing_checkpoint.as_ref().is_some_and(|cp| {
        let trusted = if cp.verified_bytes > 0 {
            cp.verified_bytes
        } else {
            cp.downloaded_bytes
        };
        cp.status == CheckpointStatus::Failed && trusted == 0
    }) {
        let _ = clear_download_checkpoint();
    }

    if should_resume {
        let cp = existing_checkpoint.as_ref().unwrap();
        let config = build_connection_config(&ConnectionRequest {
            host: request.host.clone(),
            port: request.port,
            username: request.username.clone(),
            auth_type: request.auth_type.clone(),
            password: request.password.clone(),
            key_path: request.key_path.clone(),
            key_passphrase: request.key_passphrase.clone(),
        })?;
        let (session, _) = connect_session(&config).map_err(|e| e.to_string())?;
        let sftp = session.sftp().map_err(|e| e.to_string())?;
        let stat = sftp
            .stat(std::path::Path::new(&cp.remote_path))
            .map_err(|e| format!("无法读取远端文件: {e}"))?;
        let remote_size = stat.size.unwrap_or(0);
        if cp.file_size != remote_size {
            return Err(format!(
                "远端文件大小已变化（断点记录 {} 字节，当前 {} 字节），请清除断点后重新下载",
                cp.file_size, remote_size
            ));
        }
        if let Some(saved_mtime) = cp.remote_mtime {
            if stat.mtime != Some(saved_mtime) {
                return Err("远端文件已修改（修改时间变化），请清除断点后重新下载".into());
            }
        }
    }

    let (initial_downloaded_bytes, initial_verified_bytes, initial_retry_count) = if should_resume {
        let cp = existing_checkpoint.as_ref().unwrap();
        let trusted = if cp.verified_bytes > 0 {
            cp.verified_bytes
        } else {
            cp.downloaded_bytes
        };
        (trusted, cp.verified_bytes, cp.retry_count)
    } else {
        (0, 0, 0)
    };

    let checkpoint = Arc::new(Mutex::new(DownloadCheckpoint {
        local_path: existing_checkpoint
            .as_ref()
            .filter(|_| should_resume)
            .map(|cp| cp.local_path.clone())
            .unwrap_or_else(|| resolved_local_path.clone()),
        remote_path: existing_checkpoint
            .as_ref()
            .filter(|_| should_resume)
            .map(|cp| cp.remote_path.clone())
            .unwrap_or_else(|| remote_path.clone()),
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth_kind,
        key_path: saved_key_path,
        file_size: existing_checkpoint
            .as_ref()
            .filter(|_| should_resume)
            .map(|cp| cp.file_size)
            .unwrap_or(0),
        remote_mtime: existing_checkpoint
            .as_ref()
            .filter(|_| should_resume)
            .and_then(|cp| cp.remote_mtime),
        downloaded_bytes: initial_downloaded_bytes,
        verified_bytes: initial_verified_bytes,
        retry_count: initial_retry_count,
        failure_reason: None,
        status: CheckpointStatus::InProgress,
    }));

    save_download_checkpoint(&*checkpoint.lock().map_err(|e| e.to_string())?)?;

    let cancel_flag = Arc::new(AtomicBool::new(false));
    let force_overwrite = Arc::new(AtomicBool::new(!should_resume));
    let strict_chunk_verify = Arc::new(AtomicBool::new(
        request.strict_chunk_verify.unwrap_or(true),
    ));
    let abort_socket_holder = Arc::new(Mutex::new(None));
    state.cancel_flag = Some(cancel_flag.clone());
    state.abort_socket = Some(abort_socket_holder.clone());
    state.is_running = true;

    let config = ConnectionConfig {
        host: request.host.clone(),
        port: request.port,
        username: request.username.clone(),
        auth,
    };

    let local_path_string = request.local_path.clone();
    let checkpoint_for_thread = checkpoint.clone();
    let host_for_history = request.host.clone();
    let username_for_history = request.username.clone();
    let port_for_history = request.port;

    let worker = thread::spawn(move || {
        let heartbeat = Arc::new(ProgressHeartbeat::new(initial_downloaded_bytes));
        let stall_timeout = Duration::from_secs(DOWNLOAD_STALL_TIMEOUT_SECS);
        let stall_secs = DOWNLOAD_STALL_TIMEOUT_SECS;

        let watchdog = spawn_stall_watchdog(
            heartbeat.clone(),
            cancel_flag.clone(),
            abort_socket_holder.clone(),
            stall_timeout,
            {
                let app = app.clone();
                let checkpoint = checkpoint_for_thread.clone();
                move |stalled_bytes| {
                    let (total, retry_count) = checkpoint
                        .lock()
                        .map(|cp| (cp.file_size, cp.retry_count))
                        .unwrap_or((0, 0));
                    let _ = app.emit(
                        "download-progress",
                        DownloadProgressEvent {
                            downloaded_bytes: stalled_bytes,
                            total_bytes: total,
                            status: "stalled".into(),
                            message: format!(
                                "传输超过 {stall_secs} 秒无响应，正在中断并重连..."
                            ),
                            retry_count,
                            verify_summary: None,
                        },
                    );
                }
            },
        );

        let strict_chunk_verify_summary = strict_chunk_verify.clone();
        let result = download_with_resume(
            config,
            &remote_path,
            &local_path_string,
            cancel_flag,
            force_overwrite,
            strict_chunk_verify,
            abort_socket_holder,
            checkpoint_for_thread.clone(),
            DownloadCallbacks {
                on_progress: Box::new({
                    let app = app.clone();
                    let heartbeat = heartbeat.clone();
                    move |downloaded, total, status, retry_count| {
                        sync_transfer_heartbeat(&heartbeat, downloaded, status);
                        let retry_hint = if retry_count > 0 {
                            format!(" · 重试 {retry_count} 次")
                        } else {
                            String::new()
                        };
                        let _ = app.emit(
                            "download-progress",
                            DownloadProgressEvent {
                                downloaded_bytes: downloaded,
                                total_bytes: total,
                                status: status.to_string(),
                                message: format!("已下载 {downloaded} / {total} 字节{retry_hint}"),
                                retry_count,
                                verify_summary: None,
                            },
                        );
                    }
                }),
                on_retry: Box::new({
                    let app = app.clone();
                    let checkpoint = checkpoint_for_thread.clone();
                    let heartbeat = heartbeat.clone();
                    move |retry_count, delay_ms, message| {
                        let (downloaded, total) = checkpoint
                            .lock()
                            .map(|cp| (cp.downloaded_bytes, cp.file_size))
                            .unwrap_or((0, 0));
                        sync_transfer_heartbeat(&heartbeat, downloaded, "retrying");
                        if let Ok(cp) = checkpoint.lock() {
                            let _ = save_download_checkpoint(&cp);
                        }
                        let retry_message = format!("{message}（第 {retry_count} 次重试）");
                        let _ = app.emit(
                            "download-retry",
                            DownloadRetryEvent {
                                delay_ms,
                                message: retry_message.clone(),
                                retry_count,
                            },
                        );
                        let _ = app.emit(
                            "download-progress",
                            DownloadProgressEvent {
                                downloaded_bytes: downloaded,
                                total_bytes: total,
                                status: "retrying".into(),
                                message: retry_message,
                                retry_count,
                                verify_summary: None,
                            },
                        );
                    }
                }),
                on_activity: Box::new({
                    let heartbeat = heartbeat.clone();
                    move || {
                        heartbeat.defer_next_check();
                    }
                }),
            },
        );

        watchdog.stop();
        heartbeat.set_active(false);

        match result {
            Ok(()) => {
                if let Ok(cp) = checkpoint_for_thread.lock() {
                    let _ = save_download_checkpoint(&cp);
                    let _ = clear_download_checkpoint();
                    let retry_hint = if cp.retry_count > 0 {
                        format!("（共重试 {} 次）", cp.retry_count)
                    } else {
                        String::new()
                    };
                    let message = format!("下载完成{retry_hint}");
                    record_download_result(
                        &cp.local_path,
                        &cp.remote_path,
                        &host_for_history,
                        port_for_history,
                        &username_for_history,
                        cp.file_size,
                        "completed",
                        cp.retry_count,
                        Some(message.clone()),
                    );
                    send_desktop_notification(&app, "ResiliSSH", &message);
                    let _ = app.emit(
                        "download-progress",
                        DownloadProgressEvent {
                            downloaded_bytes: cp.file_size,
                            total_bytes: cp.file_size,
                            status: "completed".into(),
                            message,
                            retry_count: cp.retry_count,
                            verify_summary: Some(completed_verify_summary(
                                strict_chunk_verify_summary.load(Ordering::Relaxed),
                            )),
                        },
                    );
                }
            }
            Err(DownloadError::Cancelled) => {
                if let Ok(cp) = checkpoint_for_thread.lock() {
                    let _ = save_download_checkpoint(&cp);
                    let message = format!(
                        "下载已取消，可稍后继续（已重试 {} 次）",
                        cp.retry_count
                    );
                    record_download_result(
                        &cp.local_path,
                        &cp.remote_path,
                        &host_for_history,
                        port_for_history,
                        &username_for_history,
                        cp.file_size,
                        "cancelled",
                        cp.retry_count,
                        Some(message.clone()),
                    );
                    let _ = app.emit(
                        "download-progress",
                        DownloadProgressEvent {
                            downloaded_bytes: cp.downloaded_bytes,
                            total_bytes: cp.file_size,
                            status: "cancelled".into(),
                            message,
                            retry_count: cp.retry_count,
                            verify_summary: None,
                        },
                    );
                }
            }
            Err(err) => {
                if let Ok(mut cp) = checkpoint_for_thread.lock() {
                    cp.status = CheckpointStatus::Failed;
                    let _ = save_download_checkpoint(&cp);
                    let message = if cp.failure_reason.as_deref() == Some("verify_read_failed") {
                        format!(
                            "传完后校验时网络中断，各数据块均已校验通过，请直接点「开始下载」重试（已重试 {} 次）",
                            cp.retry_count
                        )
                    } else {
                        format_download_failure(&err, &cp.remote_path, cp.retry_count)
                    };
                    record_download_result(
                        &cp.local_path,
                        &cp.remote_path,
                        &host_for_history,
                        port_for_history,
                        &username_for_history,
                        cp.file_size,
                        "failed",
                        cp.retry_count,
                        Some(message.clone()),
                    );
                    send_desktop_notification(&app, "ResiliSSH 下载失败", &message);
                    let _ = app.emit(
                        "download-progress",
                        DownloadProgressEvent {
                            downloaded_bytes: cp.downloaded_bytes,
                            total_bytes: cp.file_size,
                            status: "failed".into(),
                            message,
                            retry_count: cp.retry_count,
                            verify_summary: None,
                        },
                    );
                }
            }
        }

        if let Some(runtime) = app.try_state::<Mutex<DownloadRuntime>>() {
            if let Ok(mut state) = runtime.lock() {
                state.is_running = false;
                state.cancel_flag = None;
                state.abort_socket = None;
            }
        }
    });

    // 记录 worker 句柄，供 reset_download_state join，避免与新传输并发写断点。
    state.worker = Some(worker);

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Mutex::new(UploadRuntime::default()))
        .manage(Mutex::new(DownloadRuntime::default()))
        .manage(CloseGate::default())
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let gate = window.state::<CloseGate>();
                let upload = window.state::<Mutex<UploadRuntime>>();
                let download = window.state::<Mutex<DownloadRuntime>>();
                if should_block_app_close(&gate, &upload, &download) {
                    api.prevent_close();
                    let _ = window.emit("transfer-close-requested", ());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_hosts,
            get_host,
            get_saved_checkpoint,
            clear_saved_checkpoint,
            get_saved_download_checkpoint,
            clear_saved_download_checkpoint,
            list_saved_profiles,
            save_connection_profile,
            delete_connection_profile,
            list_upload_history,
            probe_remote_upload,
            probe_remote_download,
            list_remote_dir,
            create_remote_dir,
            test_connection,
            start_upload,
            start_download,
            reset_upload_state,
            reset_download_state,
            cancel_upload,
            cancel_download,
            get_transfer_running,
            prepare_app_close,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
