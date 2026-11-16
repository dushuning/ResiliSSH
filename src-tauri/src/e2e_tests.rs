//! 传输核心的端到端测试：对着一个真实的 sshd 上传、下载、中断后续传，并逐字节比对结果。
//!
//! 默认跳过。需要先启动一个测试用 sshd（见 scripts/e2e-sshd.sh），再运行：
//!   RESILISS_E2E_PORT=2299 RESILISS_E2E_KEY=/tmp/resiliss-sshd/client_ed25519 \
//!   RESILISS_E2E_HOSTKEY=/tmp/resiliss-sshd/host_ed25519.pub cargo test e2e -- --test-threads=1
//! 测试服务器就在本机，所以「远端文件」可以直接用 std::fs 读取来比对。
//! CI 中设置 RESILISS_E2E_REQUIRED=1：环境不全时直接失败，而不是静默跳过后显示「通过」。

use crate::checkpoint::{AuthKind, CheckpointStatus, UploadCheckpoint};
use crate::download_checkpoint::DownloadCheckpoint;
use crate::sftp::{
    download_with_resume, local_sha256, remote_free_bytes, remote_sha256, upload_with_resume,
    AbortHandles, AuthMethod, ConnectionConfig, DownloadCallbacks, DownloadError, UploadCallbacks, UploadError,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const MB: u64 = 1024 * 1024;
/// 多数用例验证的是与连接数无关的行为，用单连接保持时序简单；并行另有专门的用例。
const SINGLE_CONNECTION: usize = 1;
/// 故意不是 1 MB 的整数倍，覆盖最后一个不满块。
const FILE_SIZE: usize = (5 * MB + 300 * 1024 + 17) as usize;
/// 中断续传测试在传到这里时取消。
const CANCEL_AT: u64 = 3 * MB;

struct Env {
    config: ConnectionConfig,
    work: PathBuf,
}

/// 断点文件、数据目录环境变量都是全局的，测试必须串行。
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

/// 为真时缺少测试环境视为失败（CI 用），否则跳过（本地开发默认没有测试 sshd）。
const E2E_REQUIRED_ENV: &str = "RESILISS_E2E_REQUIRED";

/// 读取测试环境；未配置时返回 None，测试直接跳过。
fn env(name: &str) -> Option<Env> {
    let env = try_env(name);
    if env.is_none() && std::env::var_os(E2E_REQUIRED_ENV).is_some() {
        panic!("{E2E_REQUIRED_ENV} is set but the e2e environment is incomplete (RESILISS_E2E_PORT/KEY/HOSTKEY)");
    }
    env
}

fn try_env(name: &str) -> Option<Env> {
    let port: u16 = std::env::var("RESILISS_E2E_PORT").ok()?.parse().ok()?;
    let key = std::env::var("RESILISS_E2E_KEY").ok()?;
    let host_pub = std::fs::read_to_string(std::env::var("RESILISS_E2E_HOSTKEY").ok()?).ok()?;

    // 上一个测试可能把会话留在了连接池里；每个测试都从「没有热连接」开始，互不影响。
    crate::sftp::clear_session_pool();
    let work = std::env::temp_dir().join(format!("resiliss-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let data = work.join("appdata");
    std::fs::create_dir_all(work.join("remote")).ok()?;
    std::fs::create_dir_all(work.join("local")).ok()?;
    std::fs::create_dir_all(&data).ok()?;
    // 预先信任测试服务器的主机密钥，格式与 trust_host_key 写入的一致。
    let mut parts = host_pub.split_whitespace();
    let (kind, blob) = (parts.next()?, parts.next()?);
    std::fs::write(data.join("known_hosts"), format!("[127.0.0.1]:{port} {kind} {blob}\n")).ok()?;
    std::env::set_var(crate::storage::DATA_DIR_ENV, &data);
    // 避免 ssh-agent 里用户自己的密钥参与认证，只用测试密钥。
    std::env::remove_var("SSH_AUTH_SOCK");

    Some(Env {
        config: ConnectionConfig {
            host: "127.0.0.1".into(),
            port,
            username: std::env::var("USER").ok()?,
            auth: AuthMethod::PrivateKey { key_path: key, passphrase: None },
            jump: None,
        },
        work,
    })
}

/// 确定性的伪随机内容：块与块各不相同，错位写入一定会被比对发现。
fn sample_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn upload_checkpoint(local: &Path, remote: &str, env: &Env, size: u64) -> UploadCheckpoint {
    UploadCheckpoint {
        local_path: local.to_string_lossy().into(),
        remote_path: remote.into(),
        host: env.config.host.clone(),
        port: env.config.port,
        username: env.config.username.clone(),
        auth_kind: AuthKind::PrivateKey,
        file_size: size,
        status: CheckpointStatus::InProgress,
        staged: true,
        ..Default::default()
    }
}

struct Run {
    first_offset: Arc<AtomicU64>,
}

fn upload_callbacks(cancel_at: Option<(u64, Arc<AtomicBool>)>) -> (UploadCallbacks, Run) {
    let first_offset = Arc::new(AtomicU64::new(u64::MAX));
    let first = first_offset.clone();
    let callbacks = UploadCallbacks {
        on_progress: Box::new(move |done, _total, _status, _retries| {
            let _ = first.compare_exchange(u64::MAX, done, Ordering::Relaxed, Ordering::Relaxed);
            if let Some((at, flag)) = &cancel_at {
                if done >= *at {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        }),
        on_retry: Box::new(|_, _, _| {}),
        on_activity: Box::new(|| {}),
    };
    (callbacks, Run { first_offset })
}

fn upload(
    env: &Env,
    local: &Path,
    remote: &str,
    strict: bool,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
    cancel_at: Option<u64>,
) -> (Result<(), UploadError>, u64) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (callbacks, run) = upload_callbacks(cancel_at.map(|at| (at, cancel.clone())));
    let result = upload_with_resume(
        env.config.clone(),
        local,
        remote,
        cancel,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(strict)),
        Arc::new(AbortHandles::default()),
        checkpoint.clone(),
        callbacks,
        SINGLE_CONNECTION,
    );
    (result, run.first_offset.load(Ordering::Relaxed))
}

fn assert_same_file(a: &Path, b: &Path) {
    let (x, y) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
    assert_eq!(x.len(), y.len(), "size differs: {} vs {}", a.display(), b.display());
    assert!(x == y, "content differs: {} vs {}", a.display(), b.display());
}

fn upload_roundtrip(strict: bool, name: &str) {
    let _guard = serial();
    let Some(env) = env(name) else { return };
    let local = env.work.join("local/data.bin");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 7)).unwrap();
    let remote = env.work.join("remote/data.bin");
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));
    let (result, _) = upload(&env, &local, &remote_str, strict, &cp, None);
    result.expect("upload should succeed");
    assert_same_file(&local, &remote);
}

#[test]
fn e2e_upload_reliable_mode() {
    upload_roundtrip(true, "upload-reliable");
}

#[test]
fn e2e_upload_fast_mode() {
    upload_roundtrip(false, "upload-fast");
}

#[test]
fn e2e_upload_resumes_after_interruption() {
    let _guard = serial();
    let Some(env) = env("upload-resume") else { return };
    let local = env.work.join("local/big.bin");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 11)).unwrap();
    let remote = env.work.join("remote/big.bin");
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));

    let (first, _) = upload(&env, &local, &remote_str, true, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(UploadError::Cancelled)), "first run should be cancelled");
    let verified = cp.lock().unwrap().verified_bytes;
    assert!(verified >= MB, "checkpoint should keep verified progress, got {verified}");

    let (second, resumed_from) = upload(&env, &local, &remote_str, true, &cp, None);
    second.expect("resumed upload should succeed");
    assert!(resumed_from > 0, "second run must resume, not restart from 0");
    assert_eq!(resumed_from % MB, 0, "resume offset must be block aligned");
    assert_same_file(&local, &remote);
}

#[test]
fn e2e_upload_overwrites_different_remote_file() {
    let _guard = serial();
    let Some(env) = env("upload-overwrite") else { return };
    let local = env.work.join("local/app.jar");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 21)).unwrap();
    let remote = env.work.join("remote/app.jar");
    // 远端已有一个更大的旧文件：新文件写完后必须被截断到正确大小。
    std::fs::write(&remote, sample_bytes(FILE_SIZE + 3 * MB as usize, 99)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));
    let cancel = Arc::new(AtomicBool::new(false));
    let (callbacks, _) = upload_callbacks(None);
    let result = upload_with_resume(
        env.config.clone(),
        &local,
        &remote_str,
        cancel,
        Arc::new(AtomicBool::new(true)),
        Arc::new(AtomicBool::new(true)),
        Arc::new(AbortHandles::default()),
        cp,
        callbacks,
        SINGLE_CONNECTION,
    );
    result.expect("overwrite upload should succeed");
    assert_same_file(&local, &remote);
}

fn download_checkpoint(env: &Env, remote: &str, local: &str, size: u64) -> DownloadCheckpoint {
    DownloadCheckpoint {
        local_path: local.into(),
        remote_path: remote.into(),
        host: env.config.host.clone(),
        port: env.config.port,
        username: env.config.username.clone(),
        auth_kind: AuthKind::PrivateKey,
        key_path: None,
        file_size: size,
        remote_mtime: None,
        downloaded_bytes: 0,
        verified_bytes: 0,
        retry_count: 0,
        failure_reason: None,
        status: CheckpointStatus::InProgress,
        staged: true,
        all_chunks_verified: false,
        done_ranges: Default::default(),
    }
}

fn download(
    env: &Env,
    remote: &str,
    local: &str,
    checkpoint: &Arc<Mutex<DownloadCheckpoint>>,
    cancel_at: Option<u64>,
) -> (Result<(), DownloadError>, u64) {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let first_offset = Arc::new(AtomicU64::new(u64::MAX));
    let first = first_offset.clone();
    let callbacks = DownloadCallbacks {
        on_progress: Box::new(move |done, _total, _status, _retries| {
            let _ = first.compare_exchange(u64::MAX, done, Ordering::Relaxed, Ordering::Relaxed);
            if cancel_at.is_some_and(|at| done >= at) {
                flag.store(true, Ordering::Relaxed);
            }
        }),
        on_retry: Box::new(|_, _, _| {}),
        on_activity: Box::new(|| {}),
    };
    let result = download_with_resume(
        env.config.clone(),
        remote,
        local,
        cancel,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(true)),
        Arc::new(AbortHandles::default()),
        checkpoint.clone(),
        callbacks,
        SINGLE_CONNECTION,
    );
    (result, first_offset.load(Ordering::Relaxed))
}

#[test]
fn e2e_download_and_resume() {
    let _guard = serial();
    let Some(env) = env("download") else { return };
    let remote = env.work.join("remote/logs.tar");
    std::fs::write(&remote, sample_bytes(FILE_SIZE, 33)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let local = env.work.join("local/logs.tar");
    let local_str = local.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(download_checkpoint(&env, &remote_str, &local_str, FILE_SIZE as u64)));

    let (first, _) = download(&env, &remote_str, &local_str, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(DownloadError::Cancelled)), "first run should be cancelled");

    let (second, resumed_from) = download(&env, &remote_str, &local_str, &cp, None);
    second.expect("resumed download should succeed");
    assert!(resumed_from > 0, "second run must resume, not restart from 0");
    assert_same_file(&remote, &local);
}

#[test]
fn e2e_preflight_hash_and_free_space() {
    let _guard = serial();
    let Some(env) = env("preflight") else { return };
    let file = env.work.join("remote/same.bin");
    std::fs::write(&file, sample_bytes(2 * MB as usize, 5)).unwrap();
    let (session, _) = crate::sftp::connect_session(&env.config).expect("connect");
    let sftp = session.sftp().expect("sftp");
    let remote = remote_sha256(&session, &file.to_string_lossy()).expect("remote sha256");
    assert_eq!(Some(remote), local_sha256(&file));
    // 已存在的文件（走文件句柄）与尚未创建的子目录里的文件（走 df）都要能查到。
    let existing = remote_free_bytes(&session, &sftp, &file.to_string_lossy());
    assert!(existing.is_some_and(|f| f > 0), "free space via file handle");
    let pending = env.work.join("remote/not/yet/created/app.jar");
    let missing = remote_free_bytes(&session, &sftp, &pending.to_string_lossy());
    assert!(missing.is_some_and(|f| f > 0), "free space via df on nearest existing dir");
}

#[test]
fn e2e_expand_remote_folder_keeps_structure() {
    let _guard = serial();
    let Some(env) = env("expand") else { return };
    let dist = env.work.join("remote/dist");
    std::fs::create_dir_all(dist.join("js/vendor")).unwrap();
    std::fs::write(dist.join("index.html"), b"<html>").unwrap();
    std::fs::write(dist.join("js/app.js"), b"app").unwrap();
    std::fs::write(dist.join("js/vendor/lib.js"), b"lib").unwrap();
    std::fs::write(dist.join(".DS_Store"), b"junk").unwrap();
    // 指向目录树之外的符号链接不能被跟随，否则会把无关内容下载下来。
    std::os::unix::fs::symlink(env.work.join("local"), dist.join("outside")).unwrap();

    let expanded = crate::sftp::expand_remote_paths(&env.config, &[format!("{}/", dist.display())])
        .expect("expand");
    let got: Vec<(String, String)> = expanded
        .entries
        .iter()
        .map(|e| (e.rel_dir.clone(), e.path.rsplit('/').next().unwrap().to_string()))
        .collect();
    let want = [("dist", "index.html"), ("dist/js", "app.js"), ("dist/js/vendor", "lib.js")];
    assert_eq!(got, want.map(|(d, f)| (d.to_string(), f.to_string())));
    assert_eq!(expanded.entries[0].size, 6);
    assert!(!expanded.truncated);
    assert!(expanded.unreadable_dirs.is_empty());
}

/// 让测试 sshd 自己充当跳板机：先连上它，再经它的 direct-tcpip 转发连回它自己。
/// 链路与真实的「跳板机 → 内网服务器」完全相同，只是两跳恰好是同一台。
fn via_self_jump(mut env: Env) -> Env {
    let direct = env.config.clone();
    env.config.jump = Some(Box::new(direct));
    env
}

#[test]
fn e2e_upload_resumes_via_jump_host() {
    let _guard = serial();
    let Some(env) = env("jump-upload").map(via_self_jump) else { return };
    let local = env.work.join("local/app.jar");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 41)).unwrap();
    let remote = env.work.join("remote/app.jar");
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));

    let (first, _) = upload(&env, &local, &remote_str, true, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(UploadError::Cancelled)), "first run should be cancelled");
    let (second, resumed_from) = upload(&env, &local, &remote_str, true, &cp, None);
    second.expect("resumed upload through the jump host should succeed");
    assert!(resumed_from > 0, "second run must resume, not restart from 0");
    assert_same_file(&local, &remote);
}

#[test]
fn e2e_download_via_jump_host() {
    let _guard = serial();
    let Some(env) = env("jump-download").map(via_self_jump) else { return };
    let remote = env.work.join("remote/dump.sql");
    std::fs::write(&remote, sample_bytes(FILE_SIZE, 43)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let local = env.work.join("local/dump.sql");
    let local_str = local.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(download_checkpoint(&env, &remote_str, &local_str, FILE_SIZE as u64)));
    let (result, _) = download(&env, &remote_str, &local_str, &cp, None);
    result.expect("download through the jump host should succeed");
    assert_same_file(&remote, &local);
}

#[test]
fn e2e_jump_host_key_check_and_connection_test() {
    let _guard = serial();
    let Some(env) = env("jump-hostkey").map(via_self_jump) else { return };
    let jump = env.config.jump.as_deref();
    let (session, _) = crate::sftp::open_handshake(&env.config.host, env.config.port, jump).expect("handshake via jump");
    let info = crate::sftp::inspect_host_key(&session, &env.config.host, env.config.port).expect("inspect");
    assert_eq!(serde_json::to_value(&info).unwrap()["status"], "trusted");
    let message = crate::sftp::test_connection(&env.config).expect("connection test via jump");
    assert!(message.contains(&env.config.host), "{message}");
}

/// 取消和弱网重连都靠 shutdown 第一跳的 TCP 来打断阻塞 IO；经跳板机时目标会话跑在本地 socket 上，
/// 必须同样被立即打断，而不是等 90 秒的读超时。
#[test]
fn e2e_abort_breaks_jump_connection_quickly() {
    /// 远小于 socket 读超时（90 秒），足以区分「被打断」与「等到超时」。
    const ABORT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);
    let _guard = serial();
    let Some(env) = env("jump-abort").map(via_self_jump) else { return };
    let (session, abort) = crate::sftp::connect_session(&env.config).expect("connect via jump");
    let sftp = session.sftp().expect("sftp");
    crate::sftp::abort_socket(&abort);
    let started = std::time::Instant::now();
    let result = sftp.stat(&env.work);
    assert!(result.is_err(), "operation after abort must fail");
    assert!(started.elapsed() < ABORT_DEADLINE, "abort took {:?}", started.elapsed());
}

/// 弱网下会无限次重连；每次断开后转发线程必须退出并关闭它持有的 socket，否则长时间运行会耗尽文件描述符。
#[test]
fn e2e_jump_tunnel_releases_resources_after_disconnect() {
    const CYCLES: usize = 5;
    /// 转发线程发现本地端关闭后立即退出，留足余量避免偶发的调度延迟。
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);
    /// 测试框架或日志自身可能临时多开一两个描述符。
    const TOLERANCE: usize = 2;
    let open_fds = || std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(0);
    let _guard = serial();
    let Some(env) = env("jump-leak").map(via_self_jump) else { return };
    // 先连一次，让一次性初始化（日志、known_hosts 读取等）占用的描述符不计入。
    drop(crate::sftp::connect_session(&env.config).expect("warm-up"));
    std::thread::sleep(SETTLE);
    let before = open_fds();
    for _ in 0..CYCLES {
        let (session, _abort) = crate::sftp::connect_session(&env.config).expect("connect via jump");
        session.sftp().expect("sftp").stat(&env.work).expect("stat");
    }
    std::thread::sleep(SETTLE);
    let after = open_fds();
    assert!(after <= before + TOLERANCE, "fds grew from {before} to {after} after {CYCLES} jump connections");
}

/// 部署场景的关键保证：上传中途（含被取消、断线）目标路径上始终是完整的旧文件，
/// 只有校验通过后才一步换成新文件，并沿用旧文件的权限。
#[test]
fn e2e_staged_upload_never_exposes_partial_file() {
    use std::os::unix::fs::PermissionsExt;
    const EXECUTABLE: u32 = 0o755;
    let _guard = serial();
    let Some(env) = env("staged") else { return };
    let local = env.work.join("local/run.sh");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 51)).unwrap();
    let remote = env.work.join("remote/run.sh");
    let old = sample_bytes(2 * MB as usize, 52);
    std::fs::write(&remote, &old).unwrap();
    std::fs::set_permissions(&remote, std::fs::Permissions::from_mode(EXECUTABLE)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let part = PathBuf::from(crate::sftp::staging_path(&remote_str));
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));

    let (first, _) = upload(&env, &local, &remote_str, true, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(UploadError::Cancelled)), "first run should be cancelled");
    assert_eq!(std::fs::read(&remote).unwrap(), old, "target must still be the old file mid-upload");
    assert!(part.exists(), "progress must live in the staging file");

    let (second, resumed_from) = upload(&env, &local, &remote_str, true, &cp, None);
    second.expect("resumed staged upload should succeed");
    assert!(resumed_from > 0, "second run must resume from the staging file");
    assert_same_file(&local, &remote);
    assert!(!part.exists(), "staging file must be gone after the replace");
    let mode = std::fs::metadata(&remote).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, EXECUTABLE, "replaced file must keep the old file's permissions");
}

/// 弱网下「改名成功但回复丢失」后会重试替换：第二次必须识别为已完成，而不是报错或重传；
/// 临时文件被外部删掉、目标又不完整时，必须作为永久错误停止，而不是无限重试。
#[test]
fn e2e_promote_is_idempotent() {
    let _guard = serial();
    let Some(env) = env("promote") else { return };
    let remote = env.work.join("remote/app.tar");
    let remote_str = remote.to_string_lossy().to_string();
    let part = crate::sftp::staging_path(&remote_str);
    let data = sample_bytes(MB as usize, 61);
    std::fs::write(&part, &data).unwrap();
    let (session, _) = crate::sftp::connect_session(&env.config).expect("connect");
    let size = data.len() as u64;
    crate::sftp::promote_staged_file(&session, &part, &remote_str, size).expect("first promote");
    crate::sftp::promote_staged_file(&session, &part, &remote_str, size).expect("repeat promote is a no-op");
    assert_eq!(std::fs::read(&remote).unwrap(), data);
    let missing = crate::sftp::promote_staged_file(&session, &part, &remote_str, size + 1);
    assert!(matches!(missing, Err(UploadError::InvalidRemotePath(_))), "{missing:?}");
}

/// 升级前留下的断点（没有 staged 标记）数据就在目标文件里，必须按原方式续传完。
#[test]
fn e2e_legacy_checkpoint_resumes_in_place() {
    let _guard = serial();
    let Some(env) = env("legacy") else { return };
    let local = env.work.join("local/old.bin");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 71)).unwrap();
    let remote = env.work.join("remote/old.bin");
    let remote_str = remote.to_string_lossy().to_string();
    let mut legacy = upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64);
    legacy.staged = false;
    let cp = Arc::new(Mutex::new(legacy));

    let (first, _) = upload(&env, &local, &remote_str, true, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(UploadError::Cancelled)));
    assert!(remote.exists(), "legacy uploads write the target directly");
    assert!(!PathBuf::from(crate::sftp::staging_path(&remote_str)).exists());
    let (second, resumed_from) = upload(&env, &local, &remote_str, true, &cp, None);
    second.expect("legacy resume should succeed");
    assert!(resumed_from > 0);
    assert_same_file(&local, &remote);
}

#[test]
fn e2e_remote_paths_exist_lists_each_directory_once() {
    let _guard = serial();
    let Some(env) = env("exists") else { return };
    let dir = env.work.join("remote/site");
    std::fs::create_dir_all(dir.join("js")).unwrap();
    std::fs::write(dir.join("index.html"), b"x").unwrap();
    std::fs::write(dir.join("js/app.js"), b"y").unwrap();
    let path = |p: &str| dir.join(p).to_string_lossy().to_string();
    let paths = [path("index.html"), path("about.html"), path("js/app.js"), path("css/site.css")];
    let exists = crate::sftp::remote_paths_exist(&env.config, &paths).expect("check");
    assert_eq!(exists, vec![true, false, true, false]);
}

/// 与上传对称：下载中途本地的同名旧文件保持完整，校验通过后才被替换。
#[test]
fn e2e_staged_download_never_exposes_partial_file() {
    let _guard = serial();
    let Some(env) = env("staged-download") else { return };
    let remote = env.work.join("remote/report.pdf");
    std::fs::write(&remote, sample_bytes(FILE_SIZE, 81)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let local = env.work.join("local/report.pdf");
    let old = sample_bytes(MB as usize, 82);
    std::fs::write(&local, &old).unwrap();
    let local_str = local.to_string_lossy().to_string();
    let part = PathBuf::from(crate::sftp::staging_path(&local_str));
    let cp = Arc::new(Mutex::new(download_checkpoint(&env, &remote_str, &local_str, FILE_SIZE as u64)));

    let (first, _) = download(&env, &remote_str, &local_str, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(DownloadError::Cancelled)), "first run should be cancelled");
    assert_eq!(std::fs::read(&local).unwrap(), old, "local file must still be the old one mid-download");
    assert!(part.exists(), "progress must live in the staging file");

    let (second, resumed_from) = download(&env, &remote_str, &local_str, &cp, None);
    second.expect("resumed staged download should succeed");
    assert!(resumed_from > 0, "second run must resume from the staging file");
    assert_same_file(&remote, &local);
    assert!(!part.exists(), "staging file must be gone after the replace");
}

#[test]
fn e2e_legacy_download_checkpoint_resumes_in_place() {
    let _guard = serial();
    let Some(env) = env("legacy-download") else { return };
    let remote = env.work.join("remote/old.iso");
    std::fs::write(&remote, sample_bytes(FILE_SIZE, 91)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let local = env.work.join("local/old.iso");
    let local_str = local.to_string_lossy().to_string();
    let mut legacy = download_checkpoint(&env, &remote_str, &local_str, FILE_SIZE as u64);
    legacy.staged = false;
    let cp = Arc::new(Mutex::new(legacy));

    let (first, _) = download(&env, &remote_str, &local_str, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(DownloadError::Cancelled)));
    assert!(local.exists(), "legacy downloads write the target directly");
    let (second, resumed_from) = download(&env, &remote_str, &local_str, &cp, None);
    second.expect("legacy resume should succeed");
    assert!(resumed_from > 0);
    assert_same_file(&remote, &local);
}

fn connection_request(env: &Env) -> crate::ConnectionRequest {
    crate::ConnectionRequest {
        host: env.config.host.clone(),
        port: env.config.port,
        username: env.config.username.clone(),
        auth_type: "key".into(),
        password: None,
        key_path: std::env::var("RESILISS_E2E_KEY").ok(),
        key_passphrase: None,
        jump: None,
    }
}

/// 清除断点时，临时文件已无法续传，应一并删除；但绝不能碰目标文件，
/// 也不能在当前连接是另一台服务器时乱删（只报告残留位置）。
#[test]
fn e2e_clearing_checkpoints_removes_staging_files() {
    let _guard = serial();
    let Some(env) = env("cleanup") else { return };
    let remote = env.work.join("remote/app.jar");
    let remote_str = remote.to_string_lossy().to_string();
    let remote_part = PathBuf::from(crate::sftp::staging_path(&remote_str));
    std::fs::write(&remote, b"current release").unwrap();
    let local = env.work.join("local/app.jar");

    // 当前连接是另一台服务器（用户名不同）：只报告，不删除。
    std::fs::write(&remote_part, b"partial").unwrap();
    crate::checkpoint::save_checkpoint(&upload_checkpoint(&local, &remote_str, &env, 100)).unwrap();
    let mut other = connection_request(&env);
    other.username = "someone-else".into();
    let outcome = crate::discard_upload_checkpoint(Some(other)).expect("clear");
    assert_eq!(outcome.leftover.as_deref(), Some(remote_part.to_string_lossy().as_ref()));
    assert!(remote_part.exists(), "must not delete when connected to a different server");

    // 同一台服务器：删除临时文件，目标文件不动。
    crate::checkpoint::save_checkpoint(&upload_checkpoint(&local, &remote_str, &env, 100)).unwrap();
    let outcome = crate::discard_upload_checkpoint(Some(connection_request(&env))).expect("clear");
    assert!(outcome.leftover.is_none());
    assert!(!remote_part.exists(), "staging file should be removed");
    assert_eq!(std::fs::read(&remote).unwrap(), b"current release");
    assert!(crate::checkpoint::load_checkpoint().unwrap().is_none());

    // 旧版断点：半截数据就在目标文件里，绝不能删。
    let mut legacy = upload_checkpoint(&local, &remote_str, &env, 100);
    legacy.staged = false;
    crate::checkpoint::save_checkpoint(&legacy).unwrap();
    crate::discard_upload_checkpoint(Some(connection_request(&env))).expect("clear");
    assert!(remote.exists(), "legacy checkpoints must never delete the target file");

    // 下载：删除本地临时文件，本地已有的同名文件不动。
    let local_str = local.to_string_lossy().to_string();
    let local_part = PathBuf::from(crate::sftp::staging_path(&local_str));
    std::fs::write(&local, b"old copy").unwrap();
    std::fs::write(&local_part, b"partial").unwrap();
    crate::download_checkpoint::save_download_checkpoint(&download_checkpoint(&env, &remote_str, &local_str, 100)).unwrap();
    crate::discard_download_checkpoint().expect("clear download");
    assert!(!local_part.exists(), "local staging file should be removed");
    assert_eq!(std::fs::read(&local).unwrap(), b"old copy");
}

/// 未完成传输留下的临时文件不应出现在远端目录列表里，以免被误选下载；普通隐藏文件照常返回，由界面决定是否显示。
#[test]
fn e2e_remote_listing_hides_staging_files() {
    let _guard = serial();
    let Some(env) = env("listing") else { return };
    let dir = env.work.join("remote/app");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("app.jar"), b"x").unwrap();
    std::fs::write(dir.join(".env"), b"y").unwrap();
    std::fs::write(dir.join(".app.jar.resiliss-part"), b"half").unwrap();
    let listing = crate::sftp::list_remote_directory(&env.config, Some(&dir.to_string_lossy())).expect("list");
    let mut names: Vec<String> = listing.entries.iter().map(|e| e.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec![".env".to_string(), "app.jar".to_string()]);
}

/// 远端路径填的是已存在的目录、又没带结尾的 `/`：文件必须落在「目录/本地文件名」，
/// 临时文件也必须建在该目录里。曾经临时文件按用户填的路径算、建在上一级，
/// 替换时被 `mv` 挪进目录、留成一个隐藏文件，上传却报告成功。
#[test]
fn e2e_upload_into_existing_directory_without_trailing_slash() {
    let _guard = serial();
    let Some(env) = env("upload-into-dir") else { return };
    let local = env.work.join("local/data.bin");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 91)).unwrap();
    let dir = env.work.join("remote/site");
    std::fs::create_dir_all(&dir).unwrap();
    let dir_str = dir.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &dir_str, &env, FILE_SIZE as u64)));

    let (result, _) = upload(&env, &local, &dir_str, true, &cp, None);
    result.expect("upload into a directory should succeed");

    let target = dir.join("data.bin");
    assert_same_file(&local, &target);
    assert_eq!(cp.lock().unwrap().remote_path, target.to_string_lossy(), "checkpoint must record the real target");
    let leftovers: Vec<_> = [env.work.join("remote"), dir.clone()]
        .iter()
        .flat_map(|d| std::fs::read_dir(d).unwrap())
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| crate::sftp::is_staging_file(name))
        .collect();
    assert!(leftovers.is_empty(), "no staging file may be left behind: {leftovers:?}");
}

/// 命令安静的时间超过会话的单次读取超时 / keepalive 间隔（30 秒）时不能被当成超时：
/// 重启类脚本常常先 sleep 再输出。允许的静默时长由 remote_command 自己计时。
#[test]
fn e2e_remote_command_survives_a_quiet_period() {
    /// 略长于会话超时与 keepalive 间隔。
    const QUIET_SECS: u32 = 35;
    let _guard = serial();
    let Some(env) = env("command-quiet") else { return };
    let outcome = crate::sftp::run_remote_command(
        &env.config,
        &format!("sleep {QUIET_SECS}; echo 完成 {{file_name}}"),
        &crate::sftp::CommandContext { file_name: "a b.jar".into(), ..Default::default() },
        &AtomicBool::new(false),
        &Arc::new(Mutex::new(None)),
        |_| {},
    )
    .expect("command should run");
    assert!(!outcome.timed_out, "a quiet command must not be reported as timed out");
    assert_eq!(outcome.exit_code, Some(0));
    assert_eq!(outcome.output_tail, vec!["完成 a b.jar".to_string()]);
}

/// 记录一次上传过程中出现过的进度状态，并可在指定进度处执行一个动作（只执行一次）。
fn observing_upload(
    env: &Env,
    local: &Path,
    remote: &str,
    strict: bool,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
    at: Option<(u64, Box<dyn Fn() + Send + Sync>)>,
) -> (Result<(), UploadError>, Vec<String>) {
    let statuses = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = statuses.clone();
    let fired = AtomicBool::new(false);
    let callbacks = UploadCallbacks {
        on_progress: Box::new(move |done, _total, status, _retries| {
            let mut seen = seen.lock().unwrap();
            if seen.last().map(String::as_str) != Some(status) {
                seen.push(status.to_string());
            }
            if let Some((offset, action)) = &at {
                if done >= *offset && !fired.swap(true, Ordering::Relaxed) {
                    action();
                }
            }
        }),
        on_retry: Box::new(|_, _, _| {}),
        on_activity: Box::new(|| {}),
    };
    let result = upload_with_resume(
        env.config.clone(),
        local,
        remote,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(strict)),
        Arc::new(AbortHandles::default()),
        checkpoint.clone(),
        callbacks,
        SINGLE_CONNECTION,
    );
    let statuses = statuses.lock().unwrap().clone();
    (result, statuses)
}

/// 数据已全部传到临时文件、只差校验和替换时被打断（取消、退出、断网）：再次开始必须直接补完
/// 剩下的步骤，不能把完整的临时文件当成「无可续传进度」删掉重传。
#[test]
fn e2e_fully_staged_upload_only_needs_verify_and_replace() {
    let _guard = serial();
    let Some(env) = env("staged-complete") else { return };
    let local = env.work.join("local/app.jar");
    let data = sample_bytes(FILE_SIZE, 101);
    std::fs::write(&local, &data).unwrap();
    let remote = env.work.join("remote/app.jar");
    let remote_str = remote.to_string_lossy().to_string();
    let part = PathBuf::from(crate::sftp::staging_path(&remote_str));
    std::fs::write(&part, &data).unwrap();
    let mut checkpoint = upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64);
    checkpoint.uploaded_bytes = FILE_SIZE as u64;
    checkpoint.verified_bytes = FILE_SIZE as u64;
    let cp = Arc::new(Mutex::new(checkpoint));

    let (result, first_offset) = upload(&env, &local, &remote_str, false, &cp, None);
    result.expect("verify + replace should succeed");
    assert_eq!(first_offset, FILE_SIZE as u64, "no data may be sent again");
    assert_same_file(&local, &remote);
    assert!(!part.exists());
}

/// 「可靠模式跳过整文件哈希」只在每一块都读回过时成立：先用快速模式传一部分再切到可靠模式续传，
/// 前半段从未校验，结束时必须做整文件哈希；全程可靠模式则可以跳过。
#[test]
fn e2e_full_hash_is_skipped_only_when_every_chunk_was_read_back() {
    let _guard = serial();
    let Some(env) = env("mixed-modes") else { return };
    let local = env.work.join("local/mixed.bin");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 111)).unwrap();
    let remote = env.work.join("remote/mixed.bin");
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));

    let (first, _) = upload(&env, &local, &remote_str, false, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(UploadError::Cancelled)), "fast-mode run should be cancelled");
    let (second, statuses) = observing_upload(&env, &local, &remote_str, true, &cp, None);
    second.expect("resumed upload should succeed");
    assert!(statuses.iter().any(|s| s == "verifying"), "mixed-mode upload must hash the whole file: {statuses:?}");
    assert_same_file(&local, &remote);

    let remote = env.work.join("remote/strict.bin");
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));
    let (strict, statuses) = observing_upload(&env, &local, &remote_str, true, &cp, None);
    strict.expect("strict upload should succeed");
    assert!(!statuses.iter().any(|s| s == "verifying"), "all-strict upload can skip the full hash: {statuses:?}");
}

/// 上传途中本地文件被改写（如构建脚本重新生成了产物）：各块来自不同版本、逐块校验却都能通过，
/// 必须以明确的错误结束，目标路径上不能出现这个拼出来的文件。
#[test]
fn e2e_upload_stops_when_local_file_changes_midway() {
    let _guard = serial();
    let Some(env) = env("local-changed") else { return };
    let local = env.work.join("local/build.bin");
    std::fs::write(&local, sample_bytes(FILE_SIZE, 121)).unwrap();
    let remote = env.work.join("remote/build.bin");
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, FILE_SIZE as u64)));

    let rewritten = local.clone();
    let rewrite = Box::new(move || std::fs::write(&rewritten, sample_bytes(FILE_SIZE, 122)).unwrap());
    let (result, _) = observing_upload(&env, &local, &remote_str, true, &cp, Some((CANCEL_AT, rewrite)));
    assert!(matches!(result, Err(UploadError::LocalFileChanged)), "{result:?}");
    assert!(!remote.exists(), "a file stitched from two versions must never reach the target path");
}

/// 文件夹里常有空文件（`__init__.py`、`.gitkeep`）：下载必须和上传一样支持，而不是逐个报错。
#[test]
fn e2e_download_empty_file() {
    let _guard = serial();
    let Some(env) = env("download-empty") else { return };
    let remote = env.work.join("remote/__init__.py");
    std::fs::write(&remote, b"").unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let local = env.work.join("local/__init__.py");
    let local_str = local.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(download_checkpoint(&env, &remote_str, &local_str, 0)));

    let (result, _) = download(&env, &remote_str, &local_str, &cp, None);
    result.expect("empty file download should succeed");
    assert_eq!(std::fs::read(&local).unwrap(), b"");
    assert!(!PathBuf::from(crate::sftp::staging_path(&local_str)).exists());
}

/// 续传前远端文件已被换成另一个版本：已下载的部分不能再用，必须明确报错而不是接着拼。
#[test]
fn e2e_download_stops_when_remote_file_changed_since_checkpoint() {
    let _guard = serial();
    let Some(env) = env("remote-changed") else { return };
    let remote = env.work.join("remote/data.tar");
    std::fs::write(&remote, sample_bytes(FILE_SIZE, 131)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let local = env.work.join("local/data.tar");
    let local_str = local.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(download_checkpoint(&env, &remote_str, &local_str, FILE_SIZE as u64)));

    let (first, _) = download(&env, &remote_str, &local_str, &cp, Some(CANCEL_AT));
    assert!(matches!(first, Err(DownloadError::Cancelled)), "first run should be cancelled");
    // 同样大小、不同内容；把修改时间拨到明显不同，避免与第一次写入落在同一秒。
    std::fs::write(&remote, sample_bytes(FILE_SIZE, 132)).unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
    std::fs::File::options().write(true).open(&remote).unwrap().set_modified(later).unwrap();

    let (second, _) = download(&env, &remote_str, &local_str, &cp, None);
    assert!(matches!(second, Err(DownloadError::RemoteFileChanged)), "{second:?}");
    assert!(!local.exists(), "no file may appear at the destination");
}

/// 连续的短操作复用同一条连接：测试连接之后的预检、浏览不应各自重新握手。
#[test]
fn e2e_short_operations_reuse_one_connection() {
    let _guard = serial();
    let Some(env) = env("pool") else { return };
    crate::sftp::test_connection(&env.config).expect("test connection");
    assert!(crate::sftp::session_pool_has_idle(), "a successful test must leave a warm session");
    let listing = crate::sftp::list_remote_directory(&env.config, Some(&env.work.to_string_lossy()))
        .expect("listing on the warm session");
    assert!(listing.entries.iter().any(|e| e.name == "remote"));
    assert!(crate::sftp::session_pool_has_idle(), "the session must be handed back after the operation");
    // 换了凭据（这里是另一个用户名）就不能复用别人的会话。
    let mut other = env.config.clone();
    other.username = format!("{}-other", other.username);
    assert!(crate::sftp::list_remote_directory(&other, None).is_err());
    assert!(crate::sftp::session_pool_has_idle(), "a mismatching request must not consume the warm session");
}

/// 用户报告的卡顿来源：测试连接、预检、传输各自握手一次。现在整条链路——以及批量里的下一个文件——
/// 都接着用同一条连接，活动日志里只应出现一次「已连接」。
#[test]
fn e2e_test_then_transfer_then_next_file_share_one_connection() {
    let _guard = serial();
    let Some(env) = env("pool-transfer") else { return };
    let connects = || {
        crate::activity::entries()
            .iter()
            .filter(|entry| matches!(entry.category, crate::activity::Category::Connect))
            .count()
    };
    crate::activity::clear();
    crate::sftp::test_connection(&env.config).expect("test connection");
    for name in ["a.bin", "b.bin"] {
        let local = env.work.join("local").join(name);
        std::fs::write(&local, sample_bytes(2 * MB as usize, 141)).unwrap();
        let remote = env.work.join("remote").join(name);
        let remote_str = remote.to_string_lossy().to_string();
        let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, 2 * MB)));
        let (result, _) = upload(&env, &local, &remote_str, true, &cp, None);
        result.expect("upload on the warm session");
        assert_same_file(&local, &remote);
    }
    assert_eq!(connects(), 1, "test + two uploads must run over a single SSH connection");
}

/// 大到足以启用并行的文件大小（不是块大小的整数倍，覆盖末尾不满一块的情况）。
const PARALLEL_FILE_SIZE: usize = (40 * MB + 123_457) as usize;
const PARALLEL_CONNECTIONS: usize = 4;

/// 并行上传：返回结果，以及传输过程中同时存在的连接数峰值。
fn parallel_upload(
    env: &Env,
    local: &Path,
    remote: &str,
    strict: bool,
    checkpoint: &Arc<Mutex<UploadCheckpoint>>,
    cancel_at: Option<u64>,
) -> (Result<(), UploadError>, usize) {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let aborts = Arc::new(AbortHandles::default());
    let watched = aborts.clone();
    let peak = Arc::new(AtomicU64::new(0));
    let peak_seen = peak.clone();
    let callbacks = UploadCallbacks {
        on_progress: Box::new(move |done, _total, _status, _retries| {
            peak_seen.fetch_max(watched.count() as u64, Ordering::Relaxed);
            if cancel_at.is_some_and(|at| done >= at) {
                flag.store(true, Ordering::Relaxed);
            }
        }),
        on_retry: Box::new(|_, _, _| {}),
        on_activity: Box::new(|| {}),
    };
    let result = upload_with_resume(
        env.config.clone(),
        local,
        remote,
        cancel,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(strict)),
        aborts,
        checkpoint.clone(),
        callbacks,
        PARALLEL_CONNECTIONS,
    );
    (result, peak.load(Ordering::Relaxed) as usize)
}

/// 多条连接同时传一个文件的不同部分：结果必须逐字节一致，中途取消后续传只补没传完的部分。
#[test]
fn e2e_parallel_upload_and_resume() {
    let _guard = serial();
    let Some(env) = env("parallel-upload") else { return };
    let local = env.work.join("local/big.iso");
    std::fs::write(&local, sample_bytes(PARALLEL_FILE_SIZE, 151)).unwrap();
    let remote = env.work.join("remote/big.iso");
    let remote_str = remote.to_string_lossy().to_string();
    let part = PathBuf::from(crate::sftp::staging_path(&remote_str));
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, PARALLEL_FILE_SIZE as u64)));

    let (first, peak) = parallel_upload(&env, &local, &remote_str, true, &cp, Some(20 * MB));
    assert!(matches!(first, Err(UploadError::Cancelled)), "first run should be cancelled: {first:?}");
    assert!(peak > 1, "a large file must use more than one connection (peak {peak})");
    let (done_before, ranges) = {
        let cp = cp.lock().unwrap();
        (cp.verified_bytes, cp.done_ranges.ranges().len())
    };
    // 取消点按「含各连接正在传的那一块」的进度触发，已完成并记入断点的会略少于它。
    assert!(done_before >= 10 * MB && done_before < PARALLEL_FILE_SIZE as u64, "partial progress: {done_before}");
    assert!(ranges > 1, "parallel progress is not one contiguous prefix");
    assert!(part.exists() && !remote.exists(), "data lives in the staging file until verified");

    let (second, _) = parallel_upload(&env, &local, &remote_str, true, &cp, None);
    second.expect("resumed parallel upload should succeed");
    assert_same_file(&local, &remote);
    assert!(!part.exists());
}

/// 快速模式的并行上传靠最后的整文件哈希把关（由服务器计算，不把文件读回来）。
#[test]
fn e2e_parallel_upload_fast_mode() {
    let _guard = serial();
    let Some(env) = env("parallel-fast") else { return };
    let local = env.work.join("local/fast.iso");
    std::fs::write(&local, sample_bytes(PARALLEL_FILE_SIZE, 161)).unwrap();
    let remote = env.work.join("remote/fast.iso");
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, PARALLEL_FILE_SIZE as u64)));
    let (result, _) = parallel_upload(&env, &local, &remote_str, false, &cp, None);
    result.expect("parallel fast upload should succeed");
    assert_same_file(&local, &remote);
}

#[test]
fn e2e_parallel_download_and_resume() {
    let _guard = serial();
    let Some(env) = env("parallel-download") else { return };
    let remote = env.work.join("remote/dump.sql");
    std::fs::write(&remote, sample_bytes(PARALLEL_FILE_SIZE, 171)).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let local = env.work.join("local/dump.sql");
    let local_str = local.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(download_checkpoint(&env, &remote_str, &local_str, PARALLEL_FILE_SIZE as u64)));

    let run = |cancel_at: Option<u64>| {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let callbacks = DownloadCallbacks {
            on_progress: Box::new(move |done, _total, _status, _retries| {
                if cancel_at.is_some_and(|at| done >= at) {
                    flag.store(true, Ordering::Relaxed);
                }
            }),
            on_retry: Box::new(|_, _, _| {}),
            on_activity: Box::new(|| {}),
        };
        download_with_resume(
            env.config.clone(),
            &remote_str,
            &local_str,
            cancel,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AbortHandles::default()),
            cp.clone(),
            callbacks,
            PARALLEL_CONNECTIONS,
        )
    };
    let first = run(Some(20 * MB));
    assert!(matches!(first, Err(DownloadError::Cancelled)), "first run should be cancelled: {first:?}");
    assert!(cp.lock().unwrap().done_ranges.ranges().len() > 1, "parallel progress is not one contiguous prefix");
    assert!(!local.exists(), "no half-downloaded file at the destination");
    run(None).expect("resumed parallel download should succeed");
    assert_same_file(&remote, &local);
}

/// 校验改由服务器自己算哈希后，这条路必须真的走得通——否则会一声不响地退回「读回来比对」，
/// 功能没坏但流量翻倍，没人会发现。
#[test]
fn e2e_server_side_hashing_matches_local() {
    use sha2::{Digest, Sha256};
    let _guard = serial();
    let Some(env) = env("remote-hash") else { return };
    let file = env.work.join("remote/hash me.bin");
    let data = sample_bytes(FILE_SIZE, 181);
    std::fs::write(&file, &data).unwrap();
    let path = file.to_string_lossy().to_string();
    let (session, _) = crate::sftp::connect_session(&env.config).expect("connect");
    let never = AtomicBool::new(false);

    let whole = crate::sftp::remote_file_sha256(&session, &path, FILE_SIZE as u64, &never).expect("whole-file hash");
    assert_eq!(whole.as_slice(), Sha256::digest(&data).as_slice());

    let mut hasher = crate::sftp::BlockHasher::start(&session, &path).expect("block hasher");
    // 中间的整块、以及末尾不满一块的那一段。
    let tail_start = 5 * MB as usize;
    for (offset, len) in [(2 * MB as usize, MB as usize), (tail_start, FILE_SIZE - tail_start)] {
        let hash = hasher.hash(offset as u64, len as u64, &never).expect("block hash");
        assert_eq!(hash.as_slice(), Sha256::digest(&data[offset..offset + len]).as_slice(), "block at {offset}");
    }
}

/// 把 `data` 中从 `at` 起的 `len` 字节改掉，得到「只改了一小部分的新版本」。
fn altered(data: &[u8], at: usize, len: usize) -> Vec<u8> {
    let mut changed = data.to_vec();
    for byte in &mut changed[at..at + len] {
        *byte = byte.wrapping_add(1);
    }
    changed
}

/// 本次传输实际发送/接收了多少字节（按首次与最后一次进度之差估算）。
fn transferred_bytes<F: FnOnce(Box<dyn Fn(u64, u64, &str, u32) + Send + Sync>)>(run: F) -> u64 {
    let first = Arc::new(AtomicU64::new(u64::MAX));
    let last = Arc::new(AtomicU64::new(0));
    let (first_seen, last_seen) = (first.clone(), last.clone());
    run(Box::new(move |done, _total, _status, _retries| {
        let _ = first_seen.compare_exchange(u64::MAX, done, Ordering::Relaxed, Ordering::Relaxed);
        last_seen.fetch_max(done, Ordering::Relaxed);
    }));
    last.load(Ordering::Relaxed).saturating_sub(first.load(Ordering::Relaxed))
}

/// 目标位置已有旧版本、新版本只改了其中一块：只应传变化的部分，结果仍须逐字节一致。
#[test]
fn e2e_upload_sends_only_changed_blocks() {
    const SIZE: usize = (12 * MB + 4321) as usize;
    let _guard = serial();
    let Some(env) = env("delta-upload") else { return };
    let old = sample_bytes(SIZE, 191);
    let new = altered(&old, 5 * MB as usize + 100, 1000);
    let remote = env.work.join("remote/image.raw");
    std::fs::write(&remote, &old).unwrap();
    let local = env.work.join("local/image.raw");
    std::fs::write(&local, &new).unwrap();
    let remote_str = remote.to_string_lossy().to_string();
    let cp = Arc::new(Mutex::new(upload_checkpoint(&local, &remote_str, &env, SIZE as u64)));

    let sent = transferred_bytes(|on_progress| {
        upload_with_resume(
            env.config.clone(),
            &local,
            &remote_str,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AbortHandles::default()),
            cp.clone(),
            UploadCallbacks { on_progress, on_retry: Box::new(|_, _, _| {}), on_activity: Box::new(|| {}) },
            SINGLE_CONNECTION,
        )
        .expect("delta upload should succeed");
    });
    assert_same_file(&local, &remote);
    // 变化的那一块，加上末尾不满一块的部分。
    assert!(sent <= 2 * MB, "only the changed block and the tail should be sent, got {sent} bytes");
}

#[test]
fn e2e_download_fetches_only_changed_blocks() {
    const SIZE: usize = (12 * MB + 4321) as usize;
    let _guard = serial();
    let Some(env) = env("delta-download") else { return };
    let old = sample_bytes(SIZE, 201);
    let new = altered(&old, 7 * MB as usize + 5, 2000);
    let remote = env.work.join("remote/db.dump");
    std::fs::write(&remote, &new).unwrap();
    let local = env.work.join("local/db.dump");
    std::fs::write(&local, &old).unwrap();
    let (remote_str, local_str) = (remote.to_string_lossy().to_string(), local.to_string_lossy().to_string());
    let cp = Arc::new(Mutex::new(download_checkpoint(&env, &remote_str, &local_str, SIZE as u64)));

    let received = transferred_bytes(|on_progress| {
        download_with_resume(
            env.config.clone(),
            &remote_str,
            &local_str,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AbortHandles::default()),
            cp.clone(),
            DownloadCallbacks { on_progress, on_retry: Box::new(|_, _, _| {}), on_activity: Box::new(|| {}) },
            SINGLE_CONNECTION,
        )
        .expect("delta download should succeed");
    });
    assert_same_file(&remote, &local);
    assert!(received <= 2 * MB, "only the changed block and the tail should be fetched, got {received} bytes");
}
