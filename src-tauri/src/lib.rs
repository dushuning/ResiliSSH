mod activity;
mod checkpoint;
mod download_checkpoint;
mod history;
mod i18n;
mod profiles;
mod retry;
mod secrets;
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
use profiles::{
    delete_profile, list_profiles, profile_secrets, save_profile, update_profile_extras,
    ConnectionProfile, ProfileSecrets, SaveProfileRequest, UpdateProfileExtrasRequest,
};
use sftp::{
    abort_socket, connect_session, create_remote_directory, download_with_resume,
    format_remote_sftp_error, inspect_host_key, list_remote_directory, open_handshake, remote_file_name, remote_parent_status,
    resolve_local_path, resolve_remote_path, resolve_upload_target,
    test_connection as test_sftp_connection, upload_with_resume, AuthMethod, ConnectionConfig,
    run_remote_command as run_sftp_remote_command, CommandContext, CommandOutcome, DownloadCallbacks,
    DownloadError, HostKeyInfo, RemoteDirListing, UploadCallbacks, UploadError,
};

use serde::{Deserialize, Serialize};
use ssh_config::{list_ssh_hosts, resolve_host, SshHostEntry};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
struct RunCommandRequest {
    connection: ConnectionRequest,
    template: String,
    context: CommandContext,
}

/// 传完后命令的运行状态：与上传/下载分开管理，命令执行期间可单独取消，
/// 关窗时也要能打断（远端进程不受影响，但本地不该卡着连接）。
#[derive(Default)]
struct CommandRuntime {
    cancel_flag: Option<Arc<AtomicBool>>,
    abort_socket: Option<Arc<Mutex<Option<TcpStream>>>>,
}

#[derive(Clone, Serialize)]
struct CommandOutputEvent {
    line: String,
}

#[derive(Debug, Deserialize)]
struct HostKeyRequest {
    host: String,
    port: u16,
}

#[derive(Debug, Deserialize)]
struct TrustHostKeyRequest {
    host: String,
    port: u16,
    fingerprint: String,
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
                .ok_or_else(|| crate::tr!("请填写密码", "Please enter a password").to_string())?;
            Ok(AuthMethod::Password(password))
        }
        "key" => {
            let key_path = request
                .key_path
                .clone()
                .filter(|p| !p.is_empty())
                .ok_or_else(|| crate::tr!("请选择私钥文件", "Please choose a private key file").to_string())?;
            Ok(AuthMethod::PrivateKey {
                key_path,
                passphrase: request.key_passphrase.clone().filter(|p| !p.is_empty()),
            })
        }
        _ => Err(crate::tr!("未知认证方式", "Unknown authentication method").into()),
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

/// 传输函数在最终校验读远端失败时写下的 failure_reason（见 sftp::upload/download）。
const FAILURE_VERIFY_READ: &str = "verify_read_failed";

/// 数据全部传完、只是最终校验时读远端断了（弱网常见）：此时 trusted == file_size，
/// 也必须走续传，传输循环会直接跳到校验阶段。若当成「无可续传进度」处理，
/// 预检提示「仅需重试校验」，实际却会把整个大文件从头重传。
fn can_resume_from(trusted: u64, file_size: u64, failure_reason: Option<&str>) -> bool {
    trusted > 0
        && (trusted < file_size
            || (trusted == file_size && failure_reason == Some(FAILURE_VERIFY_READ)))
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
            crate::tr!(
                "SHA-256 校验未通过，远端文件已删除，请重新上传",
                "SHA-256 verification failed; the remote file was deleted, please upload again"
            )
            .to_string()
        }
        UploadError::InvalidRemotePath(msg) => {
            crate::tr!(
                format!("远端路径错误：{remote_path} — {msg}"),
                format!("Remote path error: {remote_path} — {msg}")
            )
        }
        UploadError::RemoteLargerThanLocal { remote, local } => {
            crate::tr!(
                format!(
                    "远端路径错误：{remote_path} — 远端文件（{remote} 字节）大于本地文件（{local} 字节）"
                ),
                format!(
                    "Remote path error: {remote_path} — remote file ({remote} bytes) is larger than the local file ({local} bytes)"
                )
            )
        }
        UploadError::ResumeBoundaryMismatch { offset } => {
            crate::tr!(
                format!(
                    "远端路径错误：{remote_path} — 续传边界校验失败（offset={offset}），建议清除断点后重试"
                ),
                format!(
                    "Remote path error: {remote_path} — resume boundary verification failed (offset={offset}); clear the checkpoint and retry"
                )
            )
        }
        UploadError::Sftp(detail) => {
            if sftp::is_remote_path_message(detail) {
                detail.clone()
            } else {
                format_remote_sftp_error(detail, remote_path)
            }
        }
        _ => err.to_string(),
    };
    crate::tr!(
        format!("{body}（已累计自动重试 {retry_count} 次）"),
        format!("{body} (auto-retried {retry_count} times)")
    )
}

fn format_download_failure(err: &DownloadError, remote_path: &str, retry_count: u32) -> String {
    let body = match err {
        DownloadError::HashMismatch => {
            crate::tr!(
                "SHA-256 校验未通过，本地文件已删除，请重新下载",
                "SHA-256 verification failed; the local file was deleted, please download again"
            )
            .to_string()
        }
        DownloadError::InvalidRemotePath(msg) | DownloadError::RemoteNotAFile(msg) => {
            crate::tr!(
                format!("远端路径错误：{remote_path} — {msg}"),
                format!("Remote path error: {remote_path} — {msg}")
            )
        }
        DownloadError::Sftp(detail) => {
            if sftp::is_remote_path_message(detail) {
                detail.clone()
            } else {
                format_remote_sftp_error(detail, remote_path)
            }
        }
        _ => err.to_string(),
    };
    crate::tr!(
        format!("{body}（已累计自动重试 {retry_count} 次）"),
        format!("{body} (auto-retried {retry_count} times)")
    )
}

fn completed_verify_summary(strict_chunk_verify: bool) -> String {
    if strict_chunk_verify {
        crate::tr!(
            "校验通过 · 逐块校验 + SHA-256 一致",
            "Verified · per-chunk check + SHA-256 match"
        )
        .to_string()
    } else {
        crate::tr!("校验通过 · SHA-256 一致", "Verified · SHA-256 match").to_string()
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

// 约定：凡是会联网、等待其他线程或可能触发钥匙串授权弹窗的命令都标 async。
// Tauri 2 中非 async 命令跑在主线程，弱网下一次握手就能让整个窗口卡死几十秒。
#[tauri::command(async)]
fn create_remote_dir(request: CreateRemoteDirRequest) -> Result<RemoteDirListing, String> {
    if request.host.trim().is_empty() {
        return Err(crate::tr!("请填写主机", "Please enter a host").into());
    }
    if request.parent_path.trim().is_empty() {
        return Err(crate::tr!(
            "请先进入要创建目录的父路径",
            "Open the parent directory first before creating a folder"
        )
        .into());
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

#[tauri::command(async)]
fn list_remote_dir(request: ListRemoteDirRequest) -> Result<RemoteDirListing, String> {
    if request.host.trim().is_empty() {
        return Err(crate::tr!("请填写主机", "Please enter a host").into());
    }
    if request.username.trim().is_empty() {
        return Err(crate::tr!("请填写用户名", "Please enter a username").into());
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
    command_runtime: State<'_, Mutex<CommandRuntime>>,
) -> Result<(), String> {
    gate.0.store(true, Ordering::Relaxed);
    let _ = cancel_remote_command(command_runtime);

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

#[tauri::command(async)]
fn list_saved_profiles() -> Result<Vec<ConnectionProfile>, String> {
    list_profiles()
}

/// 读钥匙串可能等待用户在系统授权框里操作，必须 async，不能卡住 UI 线程。
#[tauri::command(async)]
fn update_connection_profile_extras(
    request: UpdateProfileExtrasRequest,
) -> Result<ConnectionProfile, String> {
    update_profile_extras(request)
}

#[tauri::command(async)]
fn run_remote_command(
    app: AppHandle,
    runtime: State<'_, Mutex<CommandRuntime>>,
    request: RunCommandRequest,
) -> Result<CommandOutcome, String> {
    if request.template.trim().is_empty() {
        return Err(crate::tr!("命令为空", "Command is empty").into());
    }
    let config = build_connection_config(&request.connection)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let abort_socket = Arc::new(Mutex::new(None));
    {
        let mut state = runtime.lock().map_err(|e| e.to_string())?;
        if state.cancel_flag.is_some() {
            return Err(crate::tr!("已有命令在执行", "A command is already running").into());
        }
        state.cancel_flag = Some(cancel.clone());
        state.abort_socket = Some(abort_socket.clone());
    }
    let result = run_sftp_remote_command(
        &config,
        &request.template,
        &request.context,
        &cancel,
        &abort_socket,
        |line| {
            let _ = app.emit("command-output", CommandOutputEvent { line: line.to_string() });
        },
    );
    if let Err(err) = &result {
        activity::error(activity::Category::Command, crate::tr!(format!("命令未能执行：{err}"), format!("Command could not run: {err}")));
    }
    if let Ok(mut state) = runtime.lock() {
        *state = CommandRuntime::default();
    }
    result
}

#[tauri::command]
fn cancel_remote_command(runtime: State<'_, Mutex<CommandRuntime>>) -> Result<(), String> {
    let state = runtime.lock().map_err(|e| e.to_string())?;
    if let Some(flag) = &state.cancel_flag {
        flag.store(true, Ordering::Relaxed);
    }
    // 关掉 socket 才能让阻塞中的 read 立刻返回，否则要等到下一段输出或超时。
    if let Some(holder) = &state.abort_socket {
        if let Ok(guard) = holder.lock() {
            if let Some(socket) = guard.as_ref() {
                abort_socket(socket);
            }
        }
    }
    Ok(())
}

/// 前端在启动和用户切换语言时调用，后续生成的错误信息与日志随之切换。
#[tauri::command]
fn set_language(language: String) {
    i18n::set_english(language == "en");
}

#[tauri::command]
fn list_activity_log() -> Vec<activity::ActivityEntry> {
    activity::entries()
}

#[tauri::command]
fn clear_activity_log() {
    activity::clear();
}

#[tauri::command(async)]
fn get_profile_secrets(id: String) -> ProfileSecrets {
    profile_secrets(&id)
}

#[tauri::command(async)]
fn save_connection_profile(request: SaveProfileRequest) -> Result<ConnectionProfile, String> {
    save_profile(request)
}

#[tauri::command(async)]
fn delete_connection_profile(id: String) -> Result<(), String> {
    delete_profile(id)
}

#[tauri::command]
fn list_upload_history() -> Result<Vec<history::UploadHistoryEntry>, String> {
    list_history()
}

#[tauri::command(async)]
fn probe_remote_upload(request: ProbeUploadRequest) -> Result<RemoteUploadProbe, String> {
    if request.host.trim().is_empty() {
        return Err(crate::tr!("请填写主机", "Please enter a host").into());
    }
    if request.remote_path.trim().is_empty() {
        return Err(crate::tr!("请填写远端路径", "Please enter a remote path").into());
    }

    let local_path = PathBuf::from(&request.local_path);
    if !local_path.is_file() {
        return Err(crate::tr!("本地文件不存在", "Local file does not exist").into());
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
            crate::tr!(
                "传输已完成，仅需重试最终校验，无需从头重传",
                "Transfer already complete; only the final verification will be retried, no full re-upload"
            )
            .into(),
        )
    } else if checkpoint.as_ref().is_some_and(|cp| {
        cp.failure_reason.as_deref() == Some("hash_mismatch")
            || (cp.status == CheckpointStatus::Failed
                && cp.uploaded_bytes >= cp.file_size
                && cp.failure_reason.as_deref() != Some("verify_read_failed"))
    }) {
        (
            "full_reupload",
            crate::tr!(
                "上次校验未通过，将删除远端文件并从头重传",
                "Last verification failed; the remote file will be deleted and uploaded again from scratch"
            )
            .into(),
        )
    } else if verified_bytes > 0 && verified_bytes < local_size {
        (
            "resume",
            crate::tr!(
                format!(
                    "将从已验证的 {} 字节处续传（约 {:.1} MB）",
                    verified_bytes,
                    verified_bytes as f64 / 1024.0 / 1024.0
                ),
                format!(
                    "Will resume from {} already verified",
                    activity::format_bytes(verified_bytes)
                )
            ),
        )
    } else if remote_exists {
        (
            "overwrite",
            crate::tr!(
                format!("远端已有文件（{} 字节），将覆盖重传", remote_size),
                format!(
                    "Remote file already exists ({}); it will be overwritten",
                    activity::format_bytes(remote_size)
                )
            ),
        )
    } else {
        (
            "new",
            crate::tr!(
                "远端无此文件，将新建上传",
                "Remote file does not exist; a new upload will be created"
            )
            .into(),
        )
    };

    let message = if let Some(parent_msg) = parent_hint {
        crate::tr!(format!("{message}。{parent_msg}"), format!("{message}. {parent_msg}"))
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

#[tauri::command(async)]
fn probe_remote_download(request: ProbeDownloadRequest) -> Result<RemoteDownloadProbe, String> {
    if request.host.trim().is_empty() {
        return Err(crate::tr!("请填写主机", "Please enter a host").into());
    }
    if request.remote_path.trim().is_empty() {
        return Err(crate::tr!("请填写远端文件路径", "Please enter a remote file path").into());
    }
    if request.local_path.trim().is_empty() {
        return Err(crate::tr!("请填写本地保存路径", "Please enter a local save path").into());
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
        .map_err(|e| {
            crate::tr!(
                format!("远端路径错误：{remote_path} — 无法访问：{e}"),
                format!("Remote path error: {remote_path} — cannot access: {e}")
            )
        })?;
    if !remote_stat.is_file() {
        return Err(crate::tr!(
            format!("远端路径错误：{remote_path} — 请填写服务器上的文件路径，不能是目录"),
            format!(
                "Remote path error: {remote_path} — enter a file path on the server, not a directory"
            )
        ));
    }
    let remote_size = remote_stat.size.unwrap_or(0);
    if remote_size == 0 {
        return Err(crate::tr!("远端文件为空", "Remote file is empty").into());
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
            crate::tr!(
                "传输已完成，仅需重试最终校验，无需从头重下",
                "Transfer already complete; only the final verification will be retried, no full re-download"
            )
            .into(),
        )
    } else if checkpoint.as_ref().is_some_and(|cp| {
        cp.failure_reason.as_deref() == Some("hash_mismatch")
            || (cp.status == CheckpointStatus::Failed
                && cp.downloaded_bytes >= cp.file_size
                && cp.failure_reason.as_deref() != Some("verify_read_failed"))
    }) {
        (
            "full_redownload",
            crate::tr!(
                "上次校验未通过，将删除本地文件并从头重下",
                "Last verification failed; the local file will be deleted and downloaded again from scratch"
            )
            .into(),
        )
    } else if verified_bytes > 0 && verified_bytes < remote_size {
        (
            "resume",
            crate::tr!(
                format!(
                    "将从已验证的 {} 字节处续传（约 {:.1} MB）",
                    verified_bytes,
                    verified_bytes as f64 / 1024.0 / 1024.0
                ),
                format!(
                    "Will resume from {} already verified",
                    activity::format_bytes(verified_bytes)
                )
            ),
        )
    } else if local_exists {
        (
            "overwrite",
            crate::tr!(
                format!("本地已有文件（{} 字节），将覆盖重下", local_size),
                format!(
                    "Local file already exists ({}); it will be overwritten",
                    activity::format_bytes(local_size)
                )
            ),
        )
    } else {
        (
            "new",
            crate::tr!(
                "本地无此文件，将新建下载",
                "Local file does not exist; a new download will be created"
            )
            .into(),
        )
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

#[tauri::command(async)]
fn test_connection(request: ConnectionRequest) -> Result<String, String> {
    if request.host.trim().is_empty() {
        return Err(crate::tr!("请填写主机", "Please enter a host").into());
    }
    if request.username.trim().is_empty() {
        return Err(crate::tr!("请填写用户名", "Please enter a username").into());
    }

    let config = build_connection_config(&request)?;
    test_sftp_connection(&config).map_err(|e| e.to_string())
}

/// 前端在任何需要认证的连接前先调这个：未知主机时拿到指纹弹窗让用户确认，
/// 而不是等连接失败后再从错误字符串里解析。async 是为了握手期间不阻塞 UI 线程。
#[tauri::command(async)]
fn check_host_key(request: HostKeyRequest) -> Result<HostKeyInfo, String> {
    let host = request.host.trim();
    if host.is_empty() {
        return Err(crate::tr!("请填写主机", "Please enter a host").into());
    }
    let (session, _) = open_handshake(host, request.port).map_err(|e| e.to_string())?;
    inspect_host_key(&session, host, request.port).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn trust_host_key(request: TrustHostKeyRequest) -> Result<(), String> {
    let host = request.host.trim();
    let (session, _) = open_handshake(host, request.port).map_err(|e| e.to_string())?;
    sftp::trust_host_key(&session, host, request.port, &request.fingerprint)
}

#[tauri::command]
fn cancel_upload(
    app: AppHandle,
    runtime: State<'_, Mutex<UploadRuntime>>,
) -> Result<(), String> {
    let state = runtime.lock().map_err(|e| e.to_string())?;
    if !state.is_running {
        return Err(crate::tr!("当前没有进行中的上传", "No upload is in progress").into());
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
            message: crate::tr!("正在取消，正在中断连接...", "Cancelling, closing the connection...").into(),
            retry_count: 0,
            verify_summary: None,
        },
    );

    Ok(())
}

#[tauri::command(async)]
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
        return Err(crate::tr!("已有上传任务在进行中", "An upload is already in progress").into());
    }

    let local_path = PathBuf::from(&request.local_path);
    if !local_path.is_file() {
        return Err(crate::tr!("本地文件不存在", "Local file does not exist").into());
    }

    if request.remote_path.trim().is_empty() {
        return Err(crate::tr!("请填写远端路径", "Please enter a remote path").into());
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
            && can_resume_from(trusted, cp.file_size, cp.failure_reason.as_deref())
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
            return Err(crate::tr!(
                format!(
                    "本地文件大小已变化（断点记录 {} 字节，当前 {} 字节），请清除断点后重新上传",
                    cp.file_size, file_size
                ),
                format!(
                    "Local file size changed (checkpoint {} bytes, now {} bytes); clear the checkpoint and upload again",
                    cp.file_size, file_size
                )
            ));
        }
        if let Some(saved_mtime) = cp.local_mtime {
            if saved_mtime != local_mtime {
                return Err(crate::tr!(
                    "本地文件已修改（修改时间变化），请清除断点后重新上传",
                    "Local file was modified (modification time changed); clear the checkpoint and upload again"
                )
                .into());
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
        activity::info(activity::Category::Transfer, crate::tr!(
                format!("开始上传 {} → {}:{}", local_path_string, host_for_history, remote_path),
                format!("Upload started {} → {}:{}", local_path_string, host_for_history, remote_path)
            ));
        let milestones = Arc::new(ProgressMilestones::new("上传", "Uploaded"));
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
                            message: {
                                activity::warn(
                                    activity::Category::Network,
                                    crate::tr!(
                                        format!("超过 {stall_secs} 秒没有任何进度，主动断开并重连"),
                                        format!("No progress for {stall_secs} seconds, disconnecting to reconnect")
                                    ),
                                );
                                crate::tr!(
                                    format!("传输超过 {stall_secs} 秒无响应，正在中断并重连..."),
                                    format!("Transfer had no response for {stall_secs} seconds, reconnecting...")
                                )
                            },
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
                    let milestones = milestones.clone();
                    move |uploaded, total, status, retry_count| {
                        sync_transfer_heartbeat(&heartbeat, uploaded, status);
                        milestones.observe(uploaded, total, status);
                        let retry_hint = if retry_count > 0 {
                            crate::tr!(
                                format!(" · 重试 {retry_count} 次"),
                                format!(" · {retry_count} retries")
                            )
                        } else {
                            String::new()
                        };
                        let _ = app.emit(
                            "upload-progress",
                            UploadProgressEvent {
                                uploaded_bytes: uploaded,
                                total_bytes: total,
                                status: status.to_string(),
                                message: crate::tr!(
                                    format!("已上传 {uploaded} / {total} 字节{retry_hint}"),
                                    format!("Uploaded {uploaded} / {total} bytes{retry_hint}")
                                ),
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
                        let retry_message = crate::tr!(
                            format!("{message}（第 {retry_count} 次重试）"),
                            format!("{message} (retry #{retry_count})")
                        );
                        activity::warn(activity::classify_failure(message), retry_message.clone());
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
                        crate::tr!(
                            format!("（共重试 {} 次）", cp.retry_count),
                            format!(" ({} retries in total)", cp.retry_count)
                        )
                    } else {
                        String::new()
                    };
                    let message = crate::tr!(
                        format!("上传完成{retry_hint}"),
                        format!("Upload complete{retry_hint}")
                    );
                    activity::success(
                        activity::Category::Verify,
                        completed_verify_summary(strict_chunk_verify_summary.load(Ordering::Relaxed)),
                    );
                    activity::success(
                        activity::Category::Transfer,
                        format!("{message} · {}", activity::format_bytes(cp.file_size)),
                    );
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
                    let message = crate::tr!(
                        format!("上传已取消，可稍后继续（已重试 {} 次）", cp.retry_count),
                        format!(
                            "Upload cancelled, you can continue later ({} retries so far)",
                            cp.retry_count
                        )
                    );
                    activity::info(
                        activity::Category::Transfer,
                        crate::tr!(
                            format!(
                                "上传已取消，断点保存在 {} / {}",
                                activity::format_bytes(cp.verified_bytes),
                                activity::format_bytes(cp.file_size)
                            ),
                            format!(
                                "Upload cancelled, checkpoint saved at {} / {}",
                                activity::format_bytes(cp.verified_bytes),
                                activity::format_bytes(cp.file_size)
                            )
                        ),
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
                        crate::tr!(
                            format!(
                                "传完后校验时网络中断，各数据块均已校验通过，请直接点「开始上传」重试（已重试 {} 次）",
                                cp.retry_count
                            ),
                            format!(
                                "Network dropped during final verification; all chunks already verified, click \"Start upload\" to retry ({} retries so far)",
                                cp.retry_count
                            )
                        )
                    } else {
                        format_upload_failure(&err, &cp.remote_path, cp.retry_count)
                    };
                    activity::error(activity::classify_failure(&message), message.clone());
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
                    send_desktop_notification(
                        &app,
                        crate::tr!("ResiliSSH 上传失败", "ResiliSSH Upload failed"),
                        &message,
                    );
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
        return Err(crate::tr!("当前没有进行中的下载", "No download is in progress").into());
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
            message: crate::tr!("正在取消，正在中断连接...", "Cancelling, closing the connection...").into(),
            retry_count: 0,
            verify_summary: None,
        },
    );

    Ok(())
}

#[tauri::command(async)]
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

#[tauri::command(async)]
fn start_download(
    app: AppHandle,
    runtime: State<'_, Mutex<DownloadRuntime>>,
    request: StartDownloadRequest,
) -> Result<(), String> {
    // 先快速检查、立即放锁：续传前要连服务器核对远端文件（弱网下可能几十秒），
    // 若全程持锁，关窗检查、取消等需要同一把锁的命令都会被卡住。
    if runtime.lock().map_err(|e| e.to_string())?.is_running {
        return Err(crate::tr!("已有下载任务在进行中", "A download is already in progress").into());
    }

    if request.remote_path.trim().is_empty() {
        return Err(crate::tr!("请填写远端文件路径", "Please enter a remote file path").into());
    }
    if request.local_path.trim().is_empty() {
        return Err(crate::tr!("请填写本地保存路径", "Please enter a local save path").into());
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
            && can_resume_from(trusted, cp.file_size, cp.failure_reason.as_deref())
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
            .map_err(|e| {
                crate::tr!(format!("无法读取远端文件: {e}"), format!("Cannot read remote file: {e}"))
            })?;
        let remote_size = stat.size.unwrap_or(0);
        if cp.file_size != remote_size {
            return Err(crate::tr!(
                format!(
                    "远端文件大小已变化（断点记录 {} 字节，当前 {} 字节），请清除断点后重新下载",
                    cp.file_size, remote_size
                ),
                format!(
                    "Remote file size changed (checkpoint {} bytes, now {} bytes); clear the checkpoint and download again",
                    cp.file_size, remote_size
                )
            ));
        }
        if let Some(saved_mtime) = cp.remote_mtime {
            if stat.mtime != Some(saved_mtime) {
                return Err(crate::tr!(
                    "远端文件已修改（修改时间变化），请清除断点后重新下载",
                    "Remote file was modified (modification time changed); clear the checkpoint and download again"
                )
                .into());
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

    // 联网核对期间锁是放开的，写断点、登记 worker 前必须重新持锁并复查，
    // 防止两次并发的开始请求覆盖彼此的断点文件。
    let mut state = runtime.lock().map_err(|e| e.to_string())?;
    if state.is_running {
        return Err(crate::tr!("已有下载任务在进行中", "A download is already in progress").into());
    }

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
        activity::info(activity::Category::Transfer, crate::tr!(
                format!("开始下载 {}:{} → {}", host_for_history, remote_path, local_path_string),
                format!("Download started {}:{} → {}", host_for_history, remote_path, local_path_string)
            ));
        let milestones = Arc::new(ProgressMilestones::new("下载", "Downloaded"));
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
                            message: {
                                activity::warn(
                                    activity::Category::Network,
                                    crate::tr!(
                                        format!("超过 {stall_secs} 秒没有任何进度，主动断开并重连"),
                                        format!("No progress for {stall_secs} seconds, disconnecting to reconnect")
                                    ),
                                );
                                crate::tr!(
                                    format!("传输超过 {stall_secs} 秒无响应，正在中断并重连..."),
                                    format!("Transfer had no response for {stall_secs} seconds, reconnecting...")
                                )
                            },
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
                    let milestones = milestones.clone();
                    move |downloaded, total, status, retry_count| {
                        sync_transfer_heartbeat(&heartbeat, downloaded, status);
                        milestones.observe(downloaded, total, status);
                        let retry_hint = if retry_count > 0 {
                            crate::tr!(
                                format!(" · 重试 {retry_count} 次"),
                                format!(" · {retry_count} retries")
                            )
                        } else {
                            String::new()
                        };
                        let _ = app.emit(
                            "download-progress",
                            DownloadProgressEvent {
                                downloaded_bytes: downloaded,
                                total_bytes: total,
                                status: status.to_string(),
                                message: crate::tr!(
                                    format!("已下载 {downloaded} / {total} 字节{retry_hint}"),
                                    format!("Downloaded {downloaded} / {total} bytes{retry_hint}")
                                ),
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
                        let retry_message = crate::tr!(
                            format!("{message}（第 {retry_count} 次重试）"),
                            format!("{message} (retry #{retry_count})")
                        );
                        activity::warn(activity::classify_failure(message), retry_message.clone());
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
                        crate::tr!(
                            format!("（共重试 {} 次）", cp.retry_count),
                            format!(" ({} retries in total)", cp.retry_count)
                        )
                    } else {
                        String::new()
                    };
                    let message = crate::tr!(
                        format!("下载完成{retry_hint}"),
                        format!("Download complete{retry_hint}")
                    );
                    activity::success(
                        activity::Category::Verify,
                        completed_verify_summary(strict_chunk_verify_summary.load(Ordering::Relaxed)),
                    );
                    activity::success(
                        activity::Category::Transfer,
                        format!("{message} · {}", activity::format_bytes(cp.file_size)),
                    );
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
                    let message = crate::tr!(
                        format!("下载已取消，可稍后继续（已重试 {} 次）", cp.retry_count),
                        format!(
                            "Download cancelled, you can continue later ({} retries so far)",
                            cp.retry_count
                        )
                    );
                    activity::info(
                        activity::Category::Transfer,
                        crate::tr!(
                            format!(
                                "下载已取消，断点保存在 {} / {}",
                                activity::format_bytes(cp.verified_bytes),
                                activity::format_bytes(cp.file_size)
                            ),
                            format!(
                                "Download cancelled, checkpoint saved at {} / {}",
                                activity::format_bytes(cp.verified_bytes),
                                activity::format_bytes(cp.file_size)
                            )
                        ),
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
                        crate::tr!(
                            format!(
                                "传完后校验时网络中断，各数据块均已校验通过，请直接点「开始下载」重试（已重试 {} 次）",
                                cp.retry_count
                            ),
                            format!(
                                "Network dropped during final verification; all chunks already verified, click \"Start download\" to retry ({} retries so far)",
                                cp.retry_count
                            )
                        )
                    } else {
                        format_download_failure(&err, &cp.remote_path, cp.retry_count)
                    };
                    activity::error(activity::classify_failure(&message), message.clone());
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
                    send_desktop_notification(
                        &app,
                        crate::tr!("ResiliSSH 下载失败", "ResiliSSH Download failed"),
                        &message,
                    );
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
/// 把进度折算成里程碑写进活动日志：每 10% 一条，不逐块刷屏；
/// 首个进度（续传起点）只作为基准，不会一口气补记之前已完成的里程碑。
struct ProgressMilestones {
    /// 中英文动词分开存：里程碑在传输过程中逐条生成，需按生成时刻的界面语言取用。
    verb_zh: &'static str,
    verb_en: &'static str,
    last_decile: AtomicU64,
    verifying_logged: AtomicBool,
}

/// 尚未收到首个进度时的哨兵值。
const MILESTONE_UNSET: u64 = u64::MAX;
const MILESTONE_STEPS: u64 = 10;

impl ProgressMilestones {
    fn new(verb_zh: &'static str, verb_en: &'static str) -> Self {
        Self {
            verb_zh,
            verb_en,
            last_decile: AtomicU64::new(MILESTONE_UNSET),
            verifying_logged: AtomicBool::new(false),
        }
    }

    fn observe(&self, transferred: u64, total: u64, status: &str) {
        if status == "verifying" {
            if !self.verifying_logged.swap(true, Ordering::Relaxed) {
                activity::info(
                    activity::Category::Verify,
                    crate::tr!(
                        "数据已全部传完，开始全文件 SHA-256 校验",
                        "All data transferred, starting full-file SHA-256 verification"
                    ),
                );
            }
            return;
        }
        if total == 0 {
            return;
        }
        let decile = transferred.saturating_mul(MILESTONE_STEPS) / total;
        let prev = self.last_decile.load(Ordering::Relaxed);
        if prev == MILESTONE_UNSET {
            self.last_decile.store(decile, Ordering::Relaxed);
            return;
        }
        if decile > prev && decile < MILESTONE_STEPS {
            self.last_decile.store(decile, Ordering::Relaxed);
            activity::info(
                activity::Category::Transfer,
                crate::tr!(
                    format!(
                        "已{} {}%（{} / {}）",
                        self.verb_zh,
                        decile * MILESTONE_STEPS,
                        activity::format_bytes(transferred),
                        activity::format_bytes(total)
                    ),
                    format!(
                        "{} {}% ({} / {})",
                        self.verb_en,
                        decile * MILESTONE_STEPS,
                        activity::format_bytes(transferred),
                        activity::format_bytes(total)
                    )
                ),
            );
        }
    }
}

/// 单个日志文件上限与保留份数：弱网长时间重试会持续写日志，必须有界，
/// 又要保留足够历史让用户事后能把一次失败的完整重试过程发出来排查。
const LOG_MAX_FILE_BYTES: u128 = 5 * 1024 * 1024;
const LOG_KEEP_FILES: usize = 3;

/// 不注册 logger 时 `log::` 宏全部是空操作——哈希不一致、卡住检测、重试原因都会丢失。
/// 日志写到 ~/Library/Logs/com.resiliss.desktop/，开发时同时输出到终端。
fn log_plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .max_file_size(LOG_MAX_FILE_BYTES)
        .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(LOG_KEEP_FILES))
        .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseLocal)
        .build()
}

pub fn run() {
    tauri::Builder::default()
        .plugin(log_plugin())
        .setup(|app| {
            activity::init(app.handle().clone());
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(Mutex::new(UploadRuntime::default()))
        .manage(Mutex::new(DownloadRuntime::default()))
        .manage(CloseGate::default())
        .manage(Mutex::new(CommandRuntime::default()))
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
            get_profile_secrets,
            set_language,
            list_activity_log,
            clear_activity_log,
            update_connection_profile_extras,
            run_remote_command,
            cancel_remote_command,
            save_connection_profile,
            delete_connection_profile,
            list_upload_history,
            probe_remote_upload,
            probe_remote_download,
            list_remote_dir,
            create_remote_dir,
            test_connection,
            check_host_key,
            trust_host_key,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_read_failure_at_full_size_resumes_instead_of_restarting() {
        const SIZE: u64 = 10 * 1024 * 1024;
        assert!(can_resume_from(SIZE, SIZE, Some(FAILURE_VERIFY_READ)));
        assert!(can_resume_from(SIZE / 2, SIZE, None));
        // 全部传完但失败原因不是校验读取失败（如哈希不一致）不能续传，应从头重传。
        assert!(!can_resume_from(SIZE, SIZE, Some("hash_mismatch")));
        assert!(!can_resume_from(SIZE, SIZE, None));
        assert!(!can_resume_from(0, SIZE, None));
    }
}
