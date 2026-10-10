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
    AuthMethod, ConnectionConfig, DownloadCallbacks, DownloadError, UploadCallbacks, UploadError,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

const MB: u64 = 1024 * 1024;
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
        Arc::new(Mutex::new(None)),
        checkpoint.clone(),
        callbacks,
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
        Arc::new(Mutex::new(None)),
        cp,
        callbacks,
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
        Arc::new(Mutex::new(None)),
        checkpoint.clone(),
        callbacks,
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
