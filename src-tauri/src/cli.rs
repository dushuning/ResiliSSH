//! 命令行入口：`resiliss put / get`，与图形界面共用同一套传输引擎。
//!
//! 图形界面适合手动传文件；脚本、CI、远程登录的机器上需要的是一条能写进脚本的命令，
//! 并且要有同样的保证——断线自动重连、从断点接着传、传完校验、校验通过才替换目标。
//! 这里只做参数解析、凭据来源、断点文件的安置和终端输出，传输本身完全交给 `sftp` 模块。

use crate::checkpoint::{
    clear_checkpoint, file_fingerprint, load_checkpoint, save_checkpoint, AuthKind, CheckpointStatus,
    UploadCheckpoint,
};
use crate::download_checkpoint::{
    clear_download_checkpoint, load_download_checkpoint, save_download_checkpoint, DownloadCheckpoint,
};
use crate::profiles::{list_profiles, profile_secrets};
use crate::sftp::{
    self, AbortHandles, AuthMethod, ConnectError, ConnectionConfig, DownloadCallbacks, UploadCallbacks,
    DEFAULT_CONNECTIONS,
};
use crate::{activity, storage, tr};
use sha2::{Digest, Sha256};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EXIT_OK: i32 = 0;
const EXIT_FAILED: i32 = 1;
const EXIT_USAGE: i32 = 2;
const DEFAULT_SSH_PORT: u16 = 22;
/// 密码与私钥口令只从环境变量读取：写在命令行参数里会留在 shell 历史和进程列表中。
const PASSWORD_ENV: &str = "RESILISS_PASSWORD";
const PASSPHRASE_ENV: &str = "RESILISS_KEY_PASSPHRASE";
/// 没有指定私钥时依次尝试的默认位置（与 OpenSSH 的习惯一致）。
const DEFAULT_KEYS: [&str; 3] = ["~/.ssh/id_ed25519", "~/.ssh/id_ecdsa", "~/.ssh/id_rsa"];
/// 进度行的最短刷新间隔：再快终端也看不过来，还会拖慢传输线程。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// 断点目录名取摘要的前几个字节，足以区分同一台机器上的不同传输任务。
const TASK_ID_BYTES: usize = 8;
/// 存放命令行各任务断点的子目录，与图形界面的断点分开，两边同时使用互不覆盖。
const CLI_CHECKPOINT_DIR: &str = "cli";

fn usage() -> String {
    tr!(
        "用法:
  resiliss put <本地文件>... <目标>     上传（多个文件时目标必须是目录，以 / 结尾）
  resiliss get <目标> [本地路径]        下载（本地路径默认为当前目录）
  resiliss servers                      列出图形界面里保存的服务器

目标的写法:
  [用户@]主机[:端口]:/远端/路径         直接指定服务器
  @服务器名:/远端/路径                  使用图形界面里保存的服务器（密码从钥匙串读取）

选项:
  -i, --identity <私钥>      私钥文件（默认依次尝试 ~/.ssh/id_ed25519、id_ecdsa、id_rsa，并先试 ssh-agent）
  -J, --jump <[用户@]主机[:端口]>  经跳板机连接（与目标使用同一种认证方式）
  -c, --connections <N>      并行连接数，1-8（默认 4；小文件始终只用 1 条）
      --fast                 快速模式：不逐块校验，只在最后做整文件 SHA-256
      --force                忽略已有断点，从头传
      --accept-new           首次连接的服务器自动信任其指纹（不询问）
  -v, --verbose              输出连接、重连等详细过程（-vv 连同逐块的调试信息）
  -h, --help                 显示本说明

环境变量:
  RESILISS_PASSWORD          密码登录时的密码
  RESILISS_KEY_PASSPHRASE    私钥口令

中断（Ctrl+C、断电、网络断开）后重新执行同一条命令即可从断点继续。",
        "Usage:
  resiliss put <local-file>... <target>   upload (with several files the target must be a directory ending in /)
  resiliss get <target> [local-path]      download (local path defaults to the current directory)
  resiliss servers                        list the servers saved in the desktop app

Target:
  [user@]host[:port]:/remote/path         a server given directly
  @server-name:/remote/path               a server saved in the desktop app (password read from the Keychain)

Options:
  -i, --identity <key>       private key file (default: ~/.ssh/id_ed25519, id_ecdsa, id_rsa in turn; ssh-agent is tried first)
  -J, --jump <[user@]host[:port]>  connect through a jump host (same authentication as the target)
  -c, --connections <N>      parallel connections, 1-8 (default 4; small files always use 1)
      --fast                 fast mode: no per-block verification, only a whole-file SHA-256 at the end
      --force                ignore an existing checkpoint and start over
      --accept-new           trust the host key of a server seen for the first time without asking
  -v, --verbose              print connection and reconnect details (-vv adds per-block debug output)
  -h, --help                 show this help

Environment:
  RESILISS_PASSWORD          password for password login
  RESILISS_KEY_PASSPHRASE    private key passphrase

After an interruption (Ctrl+C, power loss, network drop) run the same command again to resume."
    )
    .to_string()
}

#[derive(Debug, Default, PartialEq)]
struct Options {
    identity: Option<String>,
    jump: Option<String>,
    connections: Option<usize>,
    fast: bool,
    force: bool,
    accept_new: bool,
    /// 0 = 安静；1（-v）= 连接、重连等过程；2 及以上（-vv）= 连同逐块的调试信息。
    verbose: u8,
}

#[derive(Debug, PartialEq)]
enum Command {
    Put { locals: Vec<String>, target: String },
    Get { target: String, local: String },
    Servers,
    Help,
}

/// 解析命令行。选项可以出现在任何位置，其余的按顺序作为位置参数。
fn parse_args(args: &[String]) -> Result<(Command, Options), String> {
    let mut options = Options::default();
    let mut positional: Vec<String> = Vec::new();
    let mut iter = args.iter();
    let value_of = |name: &str, iter: &mut std::slice::Iter<'_, String>| {
        iter.next()
            .cloned()
            .ok_or_else(|| tr!(format!("选项 {name} 需要一个值"), format!("Option {name} needs a value")))
    };
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok((Command::Help, options)),
            "-i" | "--identity" => options.identity = Some(value_of(arg, &mut iter)?),
            "-J" | "--jump" => options.jump = Some(value_of(arg, &mut iter)?),
            "-c" | "--connections" => {
                let value = value_of(arg, &mut iter)?;
                options.connections = Some(value.parse().map_err(|_| {
                    tr!(format!("并行连接数必须是数字: {value}"), format!("Connections must be a number: {value}"))
                })?);
            }
            "--fast" => options.fast = true,
            "--force" => options.force = true,
            "--accept-new" => options.accept_new = true,
            "-v" | "--verbose" => options.verbose += 1,
            "-vv" => options.verbose += 2,
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(tr!(format!("未知选项: {other}"), format!("Unknown option: {other}")));
            }
            _ => positional.push(arg.clone()),
        }
    }

    let mut positional = positional.into_iter();
    let command = match positional.next().as_deref() {
        None | Some("help") => Command::Help,
        Some("servers") => Command::Servers,
        Some("put") => {
            let mut paths: Vec<String> = positional.collect();
            let target = paths.pop().filter(|_| !paths.is_empty()).ok_or_else(|| {
                tr!("put 需要至少一个本地文件和一个目标", "put needs at least one local file and a target").to_string()
            })?;
            Command::Put { locals: paths, target }
        }
        Some("get") => {
            let target = positional.next().ok_or_else(|| {
                tr!("get 需要一个目标", "get needs a target").to_string()
            })?;
            Command::Get { target, local: positional.next().unwrap_or_else(|| "./".to_string()) }
        }
        Some(other) => return Err(tr!(format!("未知命令: {other}"), format!("Unknown command: {other}"))),
    };
    Ok((command, options))
}

/// 目标字符串拆出的三部分：服务器（直接给出的端点，或保存的服务器名）与远端路径。
#[derive(Debug, PartialEq)]
enum Server {
    Direct { user: Option<String>, host: String, port: Option<u16> },
    Saved(String),
}

fn parse_target(spec: &str) -> Result<(Server, String), String> {
    let bad = || {
        tr!(
            format!("无法识别的目标「{spec}」，应为 [用户@]主机[:端口]:/路径 或 @服务器名:/路径"),
            format!("Cannot parse target \"{spec}\"; expected [user@]host[:port]:/path or @server-name:/path")
        )
    };
    let (endpoint, rest) = spec.split_once(':').ok_or_else(bad)?;
    if endpoint.is_empty() || rest.is_empty() {
        return Err(bad());
    }
    if let Some(name) = endpoint.strip_prefix('@') {
        return Ok((Server::Saved(name.to_string()), rest.to_string()));
    }
    // 「主机:端口:路径」：冒号后先是一段纯数字才当作端口，否则整段都是路径。
    let (port, path) = match rest.split_once(':') {
        Some((port, path)) if !path.is_empty() => match port.parse::<u16>() {
            Ok(port) => (Some(port), path),
            Err(_) => (None, rest),
        },
        _ => (None, rest),
    };
    let (user, host) = match endpoint.split_once('@') {
        Some((user, host)) => (Some(user.to_string()), host.to_string()),
        None => (None, endpoint.to_string()),
    };
    if host.is_empty() {
        return Err(bad());
    }
    Ok((Server::Direct { user, host, port }, path.to_string()))
}

fn current_user() -> Result<String, String> {
    std::env::var("USER")
        .ok()
        .filter(|user| !user.is_empty())
        .ok_or_else(|| tr!("无法确定用户名，请写成 用户@主机", "Cannot determine the username; write user@host").to_string())
}

/// 直接指定服务器时的认证方式：给了私钥就用私钥；否则有密码环境变量用密码；再否则找默认私钥
/// （引擎会先试 ssh-agent）。
fn direct_auth(options: &Options) -> Result<AuthMethod, String> {
    let passphrase = std::env::var(PASSPHRASE_ENV).ok().filter(|p| !p.is_empty());
    if let Some(identity) = &options.identity {
        let key_path = crate::ssh_config::expand_home(identity);
        if !Path::new(&key_path).exists() {
            return Err(tr!(format!("私钥文件不存在: {identity}"), format!("Private key file does not exist: {identity}")));
        }
        return Ok(AuthMethod::PrivateKey { key_path, passphrase });
    }
    if let Some(password) = std::env::var(PASSWORD_ENV).ok().filter(|p| !p.is_empty()) {
        return Ok(AuthMethod::Password(password));
    }
    DEFAULT_KEYS
        .iter()
        .map(|key| crate::ssh_config::expand_home(key))
        .find(|key| Path::new(key).exists())
        .map(|key_path| AuthMethod::PrivateKey { key_path, passphrase })
        .ok_or_else(|| {
            tr!(
                format!("没有可用的凭据：请用 -i 指定私钥，或设置环境变量 {PASSWORD_ENV}"),
                format!("No credentials available: pass a private key with -i or set {PASSWORD_ENV}")
            )
        })
}

fn endpoint_config(spec: &str, auth: AuthMethod, jump: Option<Box<ConnectionConfig>>) -> Result<ConnectionConfig, String> {
    // 复用目标的解析规则：补一个占位路径，只取其中的服务器部分。
    let (server, _) = parse_target(&format!("{spec}:/"))?;
    let Server::Direct { user, host, port } = server else {
        return Err(tr!(
            format!("跳板机请写成 [用户@]主机[:端口]: {spec}"),
            format!("Write the jump host as [user@]host[:port]: {spec}")
        ));
    };
    Ok(ConnectionConfig {
        host,
        port: port.unwrap_or(DEFAULT_SSH_PORT),
        username: user.map_or_else(current_user, Ok)?,
        auth,
        jump,
    })
}

/// 图形界面里保存的服务器（含它的跳板机链），凭据从钥匙串读取。
fn saved_config(name: &str, depth: usize) -> Result<ConnectionConfig, String> {
    let profiles = list_profiles()?;
    let find = |key: &str| profiles.iter().find(|p| p.name == key || p.id == key);
    let profile = find(name).ok_or_else(|| {
        tr!(
            format!("没有名为「{name}」的已保存服务器（用 resiliss servers 查看）"),
            format!("No saved server named \"{name}\" (see: resiliss servers)")
        )
    })?;
    if depth > sftp::MAX_JUMP_HOPS {
        return Err(tr!("跳板机层数过多", "Too many jump hosts").into());
    }
    let secrets = profile_secrets(&profile.id);
    let auth = match profile.auth_kind {
        AuthKind::Password => AuthMethod::Password(
            secrets.password.or_else(|| std::env::var(PASSWORD_ENV).ok()).filter(|p| !p.is_empty()).ok_or_else(|| {
                tr!(
                    format!("服务器「{}」没有保存密码，请设置环境变量 {PASSWORD_ENV}", profile.name),
                    format!("Server \"{}\" has no saved password; set {PASSWORD_ENV}", profile.name)
                )
            })?,
        ),
        AuthKind::PrivateKey => AuthMethod::PrivateKey {
            key_path: crate::ssh_config::expand_home(profile.key_path.as_deref().unwrap_or_default()),
            passphrase: secrets.key_passphrase.or_else(|| std::env::var(PASSPHRASE_ENV).ok()).filter(|p| !p.is_empty()),
        },
    };
    let jump = match profile.jump_profile_id.as_deref().filter(|id| !id.is_empty()) {
        Some(id) => Some(Box::new(saved_config(id, depth + 1)?)),
        None => None,
    };
    Ok(ConnectionConfig {
        host: profile.host.clone(),
        port: profile.port,
        username: profile.username.clone(),
        auth,
        jump,
    })
}

fn resolve_target(spec: &str, options: &Options) -> Result<(ConnectionConfig, String), String> {
    let (server, path) = parse_target(spec)?;
    let config = match server {
        Server::Saved(name) => saved_config(&name, 0)?,
        Server::Direct { user, host, port } => {
            let auth = direct_auth(options)?;
            let jump = match &options.jump {
                Some(spec) => Some(Box::new(endpoint_config(spec, auth.clone(), None)?)),
                None => None,
            };
            ConnectionConfig {
                host,
                port: port.unwrap_or(DEFAULT_SSH_PORT),
                username: user.map_or_else(current_user, Ok)?,
                auth,
                jump,
            }
        }
    };
    Ok((config, path))
}

/// 连接一次以确认凭据可用，并处理首次连接的指纹确认。成功后这条连接留在连接池里，传输直接接着用。
fn connect_and_trust(config: &ConnectionConfig, options: &Options) -> Result<(), String> {
    let mut confirmations = 0;
    let mut backoff = crate::retry::Backoff::new();
    // 这一步连不上和传输中途断线是同一回事：网络问题一直重试，只有重连也不会好的错误才放弃。
    // 否则脚本里的一次 `resiliss put` 会因为握手时的一下抖动直接失败，传输引擎的无限重试根本没机会上场。
    let retry_later = |err: &ConnectError, backoff: &mut crate::retry::Backoff| {
        let delay_ms = backoff.next_delay_ms(0);
        announce_retry(&tr!(
            format!("连接失败，{delay_ms}ms 后重试: {err}"),
            format!("Connection failed, retrying in {delay_ms}ms: {err}")
        ));
        std::thread::sleep(Duration::from_millis(delay_ms));
    };
    loop {
        let (unknown, fingerprint) = match sftp::test_connection(config) {
            Ok(_) => return Ok(()),
            Err(ConnectError::HostKeyUnknown(fingerprint)) => (config, fingerprint),
            Err(ConnectError::Jump { source, .. }) => match (*source, config.jump.as_deref()) {
                (ConnectError::HostKeyUnknown(fingerprint), Some(jump)) => (jump, fingerprint),
                (other, _) if other.is_permanent() => return Err(other.to_string()),
                (other, _) => {
                    retry_later(&other, &mut backoff);
                    continue;
                }
            },
            Err(other) if other.is_permanent() => return Err(other.to_string()),
            Err(other) => {
                retry_later(&other, &mut backoff);
                continue;
            }
        };
        // 每一跳最多需要确认一次，链路再长也不会超过这个次数；超过说明确认没有生效，不能无限问下去。
        confirmations += 1;
        if confirmations > sftp::MAX_JUMP_HOPS + 1 {
            return Err(tr!("无法完成服务器指纹确认", "Could not complete host key confirmation").into());
        }
        let endpoint = format!("{}:{}", unknown.host, unknown.port);
        if !options.accept_new && !confirm_fingerprint(&endpoint, &fingerprint)? {
            return Err(tr!("未信任该服务器，已取消", "Server not trusted; cancelled").into());
        }
        match sftp::open_handshake(&unknown.host, unknown.port, unknown.jump.as_deref()) {
            Ok((session, _)) => sftp::trust_host_key(&session, &unknown.host, unknown.port, &fingerprint)?,
            // 写入信任前又断了：回到开头重连，下一轮会再走到这里（用户已确认过，--accept-new 或再答一次）。
            Err(err) => {
                confirmations -= 1;
                retry_later(&err, &mut backoff);
            }
        }
    }
}

fn confirm_fingerprint(endpoint: &str, fingerprint: &str) -> Result<bool, String> {
    if !std::io::stdin().is_terminal() {
        // 没有人可以回答（脚本、CI）：宁可失败也不能替用户默认信任一台没见过的服务器。
        return Err(tr!(
            format!("首次连接 {endpoint}（指纹 {fingerprint}）。核对后加上 --accept-new 重试"),
            format!("First connection to {endpoint} (fingerprint {fingerprint}). Verify it, then retry with --accept-new")
        ));
    }
    eprint!(
        "{}",
        tr!(
            format!("首次连接 {endpoint}\n指纹: {fingerprint}\n请与服务器管理员核对。信任并继续？[y/N] "),
            format!("First connection to {endpoint}\nFingerprint: {fingerprint}\nVerify it with the server administrator. Trust and continue? [y/N] ")
        )
    );
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// 每个传输任务有自己的断点目录（由方向、服务器、两端路径共同决定）：
/// 同一条命令重新执行能找到上次的断点，不同任务之间、以及与图形界面之间互不覆盖。
fn use_task_checkpoint_dir(direction: &str, config: &ConnectionConfig, remote: &str, local: &str) -> PathBuf {
    let identity = format!("{direction}|{}|{remote}|{local}", config.endpoint());
    let digest = Sha256::digest(identity.as_bytes());
    let id: String = digest.iter().take(TASK_ID_BYTES).map(|byte| format!("{byte:02x}")).collect();
    let dir = storage::app_data_dir().join(CLI_CHECKPOINT_DIR).join(id);
    storage::set_checkpoint_dir(Some(dir.clone()));
    dir
}

fn auth_kind(config: &ConnectionConfig) -> (AuthKind, Option<String>) {
    match &config.auth {
        AuthMethod::Password(_) => (AuthKind::Password, None),
        AuthMethod::PrivateKey { key_path, .. } => (AuthKind::PrivateKey, Some(key_path.clone())),
    }
}

/// 终端上的单行进度。传输线程每传一小片就会回调一次，这里限速刷新。
struct ProgressLine {
    started: Instant,
    start_bytes: Mutex<Option<u64>>,
    last_draw: Mutex<Instant>,
    interactive: bool,
}

impl ProgressLine {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            start_bytes: Mutex::new(None),
            last_draw: Mutex::new(Instant::now() - PROGRESS_INTERVAL),
            interactive: std::io::stderr().is_terminal(),
        }
    }

    fn update(&self, done: u64, total: u64, status: &str) {
        // 输出被重定向到文件时不画进度行，否则日志里全是回车符。
        if !self.interactive {
            return;
        }
        let Ok(mut last) = self.last_draw.lock() else { return };
        if last.elapsed() < PROGRESS_INTERVAL && done < total {
            return;
        }
        *last = Instant::now();
        // 速度按本次运行实际传的量算：续传时开头那一大截不是这次传的。
        let base = match self.start_bytes.lock() {
            Ok(mut start) => *start.get_or_insert(done),
            Err(_) => done,
        };
        let elapsed = self.started.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 { (done.saturating_sub(base) as f64 / elapsed) as u64 } else { 0 };
        // 空文件没有「百分之几」可言，直接算作完成。
        let percent = (done * 100).checked_div(total).unwrap_or(100);
        let phase = match status {
            "verifying" => tr!("校验中", "verifying"),
            "chunk_verifying" => tr!("逐块校验", "checking block"),
            _ => "",
        };
        eprint!(
            "\r{percent:>3}%  {} / {}  {}/s  {phase}\x1b[K",
            activity::format_bytes(done),
            activity::format_bytes(total),
            activity::format_bytes(speed)
        );
        let _ = std::io::stderr().flush();
    }

    fn finish(&self) {
        if self.interactive {
            eprintln!();
        }
    }
}

fn announce_retry(message: &str) {
    eprintln!("\n{message}");
}

fn put_file(config: &ConnectionConfig, local: &str, remote: &str, options: &Options) -> Result<String, String> {
    let local_path = std::fs::canonicalize(local)
        .map_err(|e| tr!(format!("无法读取本地文件 {local}: {e}"), format!("Cannot read local file {local}: {e}")))?;
    if !local_path.is_file() {
        return Err(tr!(format!("{local} 不是文件"), format!("{local} is not a file")));
    }
    let local_str = local_path.to_string_lossy().to_string();
    let resolved_remote = sftp::resolve_remote_path(remote, &local_path).map_err(|e| e.to_string())?;
    let checkpoint_dir = use_task_checkpoint_dir("put", config, remote, &local_str);
    let (file_size, mtime) = file_fingerprint(&local_path)?;

    // 上次的断点还对得上（同一个文件、没改过、进度可用）就接着传，否则从头开始。
    let resumable = load_checkpoint().ok().flatten().filter(|cp| {
        !options.force
            && cp.local_path == local_str
            && cp.file_size == file_size
            && cp.local_mtime == Some(mtime)
            && crate::can_resume_from(cp.verified_bytes, cp.file_size, cp.failure_reason.as_deref())
    });
    let resume = resumable.is_some();
    let (kind, key_path) = auth_kind(config);
    let checkpoint = match resumable {
        Some(cp) => UploadCheckpoint { status: CheckpointStatus::InProgress, failure_reason: None, ..cp },
        None => UploadCheckpoint {
            local_path: local_str.clone(),
            remote_path: resolved_remote,
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            auth_kind: kind,
            key_path,
            file_size,
            local_mtime: Some(mtime),
            // 新传输一律先写临时文件，校验通过后再替换目标。
            staged: true,
            ..Default::default()
        },
    };
    save_checkpoint(&checkpoint)?;
    let checkpoint = Arc::new(Mutex::new(checkpoint));

    let progress = Arc::new(ProgressLine::new());
    let line = progress.clone();
    let result = sftp::upload_with_resume(
        config.clone(),
        &local_path,
        remote,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(!resume)),
        Arc::new(AtomicBool::new(!options.fast)),
        Arc::new(AbortHandles::default()),
        checkpoint.clone(),
        UploadCallbacks {
            on_progress: Box::new(move |done, total, status, _retries| line.update(done, total, status)),
            on_retry: Box::new(|_count, _delay, message| announce_retry(message)),
            on_activity: Box::new(|| {}),
        },
        options.connections.unwrap_or(DEFAULT_CONNECTIONS),
    );
    progress.finish();

    let final_remote = checkpoint.lock().map(|cp| cp.remote_path.clone()).unwrap_or_default();
    match result {
        Ok(()) => {
            clear_checkpoint()?;
            let _ = std::fs::remove_dir(&checkpoint_dir);
            Ok(final_remote)
        }
        Err(err) => {
            // 留下断点：重新执行同一条命令时从这里继续。
            if let Ok(cp) = checkpoint.lock() {
                let _ = save_checkpoint(&cp);
            }
            Err(err.to_string())
        }
    }
}

fn get_file(config: &ConnectionConfig, remote: &str, local: &str, options: &Options) -> Result<String, String> {
    // 本地路径是目录（已存在，或以 / 结尾）时把文件放进去；统一成绝对路径，断点才能和下次执行对上。
    let local_base = std::path::absolute(local).map_err(|e| e.to_string())?;
    let as_dir = local.ends_with('/') || local_base.is_dir();
    let mut local_str = local_base.to_string_lossy().to_string();
    if as_dir && !local_str.ends_with('/') {
        local_str.push('/');
    }
    let checkpoint_dir = use_task_checkpoint_dir("get", config, remote, &local_str);

    // 远端文件是否还是断点记录的那一个，由引擎在连接后核对；这里只看进度本身是否可用。
    let resumable = load_download_checkpoint().ok().flatten().filter(|cp| {
        !options.force && crate::can_resume_from(cp.verified_bytes, cp.file_size, cp.failure_reason.as_deref())
    });
    let resume = resumable.is_some();
    let (kind, key_path) = auth_kind(config);
    let checkpoint = match resumable {
        Some(cp) => DownloadCheckpoint { status: CheckpointStatus::InProgress, failure_reason: None, ..cp },
        None => DownloadCheckpoint {
            local_path: local_str.clone(),
            remote_path: remote.to_string(),
            host: config.host.clone(),
            port: config.port,
            username: config.username.clone(),
            auth_kind: kind,
            key_path,
            file_size: 0,
            remote_mtime: None,
            downloaded_bytes: 0,
            verified_bytes: 0,
            retry_count: 0,
            failure_reason: None,
            status: CheckpointStatus::InProgress,
            staged: true,
            all_chunks_verified: false,
            done_ranges: Default::default(),
        },
    };
    save_download_checkpoint(&checkpoint)?;
    let checkpoint = Arc::new(Mutex::new(checkpoint));

    let progress = Arc::new(ProgressLine::new());
    let line = progress.clone();
    let result = sftp::download_with_resume(
        config.clone(),
        remote,
        &local_str,
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(!resume)),
        Arc::new(AtomicBool::new(!options.fast)),
        Arc::new(AbortHandles::default()),
        checkpoint.clone(),
        DownloadCallbacks {
            on_progress: Box::new(move |done, total, status, _retries| line.update(done, total, status)),
            on_retry: Box::new(|_count, _delay, message| announce_retry(message)),
            on_activity: Box::new(|| {}),
        },
        options.connections.unwrap_or(DEFAULT_CONNECTIONS),
    );
    progress.finish();

    let final_local = checkpoint.lock().map(|cp| cp.local_path.clone()).unwrap_or_default();
    match result {
        Ok(()) => {
            clear_download_checkpoint()?;
            let _ = std::fs::remove_dir(&checkpoint_dir);
            Ok(final_local)
        }
        Err(err) => {
            if let Ok(cp) = checkpoint.lock() {
                let _ = save_download_checkpoint(&cp);
            }
            Err(err.to_string())
        }
    }
}

/// `--verbose` 时把引擎的日志（连接、重连、续传位置等）打到终端。
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        // 只要本程序的日志；依赖库的内部日志对用户没有意义。
        metadata.target().starts_with(env!("CARGO_CRATE_NAME"))
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            // 带上从启动算起的秒数：排查「卡在哪一步、卡了多久」时，先后顺序和间隔比绝对时间有用。
            let elapsed = LOG_START.get_or_init(Instant::now).elapsed().as_secs_f64();
            eprintln!("[{elapsed:8.3}s {}] {}", record.level().as_str().to_ascii_lowercase(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;
static LOG_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn list_servers() -> Result<(), String> {
    let profiles = list_profiles()?;
    if profiles.is_empty() {
        println!("{}", tr!("还没有保存的服务器（在图形界面的「服务器」页添加）", "No saved servers yet (add one on the Servers page of the desktop app)"));
    }
    for profile in profiles {
        println!("@{}\t{}@{}:{}", profile.name, profile.username, profile.host, profile.port);
    }
    Ok(())
}

fn execute(command: Command, options: &Options) -> Result<(), String> {
    match command {
        Command::Help => {
            println!("{}", usage());
            Ok(())
        }
        Command::Servers => list_servers(),
        Command::Put { locals, target } => {
            let (config, remote) = resolve_target(&target, options)?;
            if locals.len() > 1 && !remote.ends_with('/') {
                return Err(tr!(
                    "上传多个文件时目标必须是目录（以 / 结尾），否则它们会互相覆盖",
                    "With several files the target must be a directory (ending in /), otherwise they would overwrite each other"
                )
                .into());
            }
            connect_and_trust(&config, options)?;
            // 长时间传输期间不让机器因空闲而睡眠。
            let _awake = crate::power::SleepGuard::acquire("cli upload");
            for local in &locals {
                let remote_path = put_file(&config, local, &remote, options)?;
                println!("{}", tr!(format!("已上传并校验: {local} → {remote_path}"), format!("Uploaded and verified: {local} → {remote_path}")));
            }
            Ok(())
        }
        Command::Get { target, local } => {
            let (config, remote) = resolve_target(&target, options)?;
            connect_and_trust(&config, options)?;
            let _awake = crate::power::SleepGuard::acquire("cli download");
            let local_path = get_file(&config, &remote, &local, options)?;
            println!("{}", tr!(format!("已下载并校验: {remote} → {local_path}"), format!("Downloaded and verified: {remote} → {local_path}")));
            Ok(())
        }
    }
}

/// 命令行主函数，返回进程退出码。
pub fn run(args: Vec<String>) -> i32 {
    // 终端的语言环境不是中文就用英文输出。
    let locale = std::env::var("LC_ALL").or_else(|_| std::env::var("LANG")).unwrap_or_default();
    crate::i18n::set_english(!locale.to_ascii_lowercase().starts_with("zh"));

    let (command, options) = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}\n\n{}", usage());
            return EXIT_USAGE;
        }
    };
    if options.verbose > 0 && log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(if options.verbose > 1 { log::LevelFilter::Debug } else { log::LevelFilter::Info });
    }
    match execute(command, &options) {
        Ok(()) => EXIT_OK,
        Err(message) => {
            eprintln!("{}", tr!(format!("失败: {message}"), format!("Failed: {message}")));
            EXIT_FAILED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn parses_put_with_options_anywhere() {
        let (command, options) = parse_args(&args("put --fast a.jar b.jar -c 2 deploy@host:/opt/app/")).unwrap();
        assert_eq!(
            command,
            Command::Put { locals: vec!["a.jar".into(), "b.jar".into()], target: "deploy@host:/opt/app/".into() }
        );
        assert!(options.fast);
        assert_eq!(options.connections, Some(2));
    }

    #[test]
    fn get_defaults_to_current_directory() {
        let (command, _) = parse_args(&args("get host:/var/log/app.log")).unwrap();
        assert_eq!(command, Command::Get { target: "host:/var/log/app.log".into(), local: "./".into() });
    }

    #[test]
    fn rejects_incomplete_or_unknown_input() {
        assert!(parse_args(&args("put only-one")).is_err());
        assert!(parse_args(&args("put a.jar host:/x --bogus")).is_err());
        assert!(parse_args(&args("put a.jar host:/x -i")).is_err());
        assert!(parse_args(&args("sync a b")).is_err());
        assert_eq!(parse_args(&[]).unwrap().0, Command::Help);
    }

    #[test]
    fn parses_target_forms() {
        let direct = |user: Option<&str>, host: &str, port: Option<u16>| Server::Direct {
            user: user.map(str::to_string),
            host: host.to_string(),
            port,
        };
        assert_eq!(parse_target("host:/data/a").unwrap(), (direct(None, "host", None), "/data/a".into()));
        assert_eq!(
            parse_target("deploy@10.0.0.5:2222:/opt/app/").unwrap(),
            (direct(Some("deploy"), "10.0.0.5", Some(2222)), "/opt/app/".into())
        );
        // 相对路径，以及路径里自带冒号的情况：冒号前不是纯数字就不是端口。
        assert_eq!(parse_target("host:uploads/a.jar").unwrap(), (direct(None, "host", None), "uploads/a.jar".into()));
        assert_eq!(parse_target("host:/logs/12:30.log").unwrap(), (direct(None, "host", None), "/logs/12:30.log".into()));
        assert_eq!(parse_target("@prod:/srv/").unwrap(), (Server::Saved("prod".into()), "/srv/".into()));
        assert!(parse_target("no-path").is_err());
        assert!(parse_target(":/x").is_err());
    }
}
