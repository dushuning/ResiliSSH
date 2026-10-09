use super::host_key::verify_host_key;
use crate::activity::{self, Category};
use ssh2::Session;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone)]
pub enum AuthMethod {
    Password(String),
    PrivateKey {
        key_path: String,
        passphrase: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
}

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("TCP 连接失败: {0}")]
    Tcp(String),
    #[error("SSH 握手失败: {0}")]
    Handshake(String),
    #[error("认证失败: {0}")]
    Auth(String),
    #[error("未信任的服务器（指纹 {0}），请确认后再连接")]
    HostKeyUnknown(String),
    #[error("服务器指纹已变化（{0}），可能遭遇中间人攻击，已拒绝连接")]
    HostKeyChanged(String),
    #[error("主机密钥校验失败: {0}")]
    HostKeyCheck(String),
}

impl ConnectError {
    /// 主机密钥问题不会因重连自愈：未知主机需要用户确认，指纹变化更不能自动重试，
    /// 否则弱网重试循环会对着可疑服务器无限握手。
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            ConnectError::HostKeyUnknown(_)
                | ConnectError::HostKeyChanged(_)
                | ConnectError::HostKeyCheck(_)
        )
    }
}

/// SFTP 协议状态码 SSH_FX_NO_SUCH_FILE（draft-ietf-secsh-filexfer §7）。
const SFTP_FX_NO_SUCH_FILE: i32 = 2;

/// 区分「远端文件确实不存在」与「stat 因超时/断线失败」：弱网下后者很常见，
/// 若一律当作不存在，会把已传进度当成 0 并截断重传，或把可重试的抖动判成永久失败。
pub fn is_sftp_not_found(err: &ssh2::Error) -> bool {
    matches!(err.code(), ssh2::ErrorCode::SFTP(SFTP_FX_NO_SUCH_FILE))
}

const SSH_SOCKET_TIMEOUT_SECS: u64 = 90;
/// 单次 TCP 建连上限。系统默认可能等 75s 以上，期间无法取消；
/// 超时后交给外层重试循环，所以这里不会让弱网任务失败，只是让每次尝试可控、可取消。
const TCP_CONNECT_TIMEOUT_SECS: u64 = 20;
const SSH_OPERATION_TIMEOUT_MS: u64 = 30_000;
const SSH_KEEPALIVE_INTERVAL_SECS: u32 = 30;

/// 域名可能解析出多个地址（IPv4/IPv6），逐个尝试，全部失败时报告最后一个错误。
fn connect_with_timeout(host: &str, port: u16) -> Result<TcpStream, ConnectError> {
    let timeout = Duration::from_secs(TCP_CONNECT_TIMEOUT_SECS);
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| ConnectError::Tcp(format!("无法解析主机 {host}: {e}")))?;
    let mut last_err = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                log::debug!("tcp connect {addr} failed: {e}");
                last_err = Some(e);
            }
        }
    }
    Err(ConnectError::Tcp(
        last_err.map_or_else(|| format!("主机 {host} 没有可用地址"), |e| e.to_string()),
    ))
}

/// 只完成 TCP + SSH 握手、不做认证：主机密钥检查/信任流程只需要服务器公钥，
/// 不应为此先把密码发出去。
pub fn open_handshake(host: &str, port: u16) -> Result<(Session, TcpStream), ConnectError> {
    let tcp = connect_with_timeout(host, port)?;
    tcp.set_read_timeout(Some(Duration::from_secs(SSH_SOCKET_TIMEOUT_SECS)))
        .map_err(|e| ConnectError::Tcp(e.to_string()))?;
    tcp.set_write_timeout(Some(Duration::from_secs(SSH_SOCKET_TIMEOUT_SECS)))
        .map_err(|e| ConnectError::Tcp(e.to_string()))?;

    let abort_socket = tcp
        .try_clone()
        .map_err(|e| ConnectError::Tcp(format!("clone socket failed: {e}")))?;

    let mut session = Session::new().map_err(|e| ConnectError::Handshake(e.to_string()))?;
    session.set_tcp_stream(tcp);
    session
        .handshake()
        .map_err(|e| ConnectError::Handshake(e.to_string()))?;
    Ok((session, abort_socket))
}

/// 返回会话和可 shutdown 的 socket 副本，用于取消时立刻打断阻塞 IO。
/// 成功只记一行汇总（含耗时与认证方式），失败按卡住的环节分别说明，日志页才不会被刷屏。
pub fn connect_session(config: &ConnectionConfig) -> Result<(Session, TcpStream), ConnectError> {
    let started = std::time::Instant::now();
    let target = format!("{}@{}:{}", config.username, config.host, config.port);
    match establish(config) {
        Ok((session, socket, auth_label)) => {
            activity::success(
                Category::Connect,
                format!(
                    "已连接 {target} · 用时 {} ms · {auth_label}",
                    started.elapsed().as_millis()
                ),
            );
            Ok((session, socket))
        }
        Err(err) => {
            record_connect_failure(&target, &err);
            Err(err)
        }
    }
}

fn record_connect_failure(target: &str, err: &ConnectError) {
    match err {
        ConnectError::Tcp(e) => activity::warn(Category::Network, format!("无法连接 {target}：{e}")),
        ConnectError::Handshake(e) => {
            activity::warn(Category::Network, format!("SSH 握手失败 {target}：{e}"))
        }
        ConnectError::Auth(e) => activity::error(Category::Auth, format!("认证失败 {target}：{e}")),
        ConnectError::HostKeyUnknown(fp) => activity::warn(
            Category::HostKey,
            format!("{target} 是未信任的服务器（指纹 {fp}），需要确认后才能连接"),
        ),
        ConnectError::HostKeyChanged(fp) => activity::error(
            Category::HostKey,
            format!("{target} 的指纹与记录不一致（当前 {fp}），已拒绝连接"),
        ),
        ConnectError::HostKeyCheck(e) => {
            activity::error(Category::HostKey, format!("{target} 指纹校验出错：{e}"))
        }
    }
}

/// 返回值第三项是实际生效的认证方式描述，用于日志（私钥可能实际走了 ssh-agent）。
fn establish(config: &ConnectionConfig) -> Result<(Session, TcpStream, &'static str), ConnectError> {
    let (session, abort_socket) = open_handshake(&config.host, config.port)?;
    verify_host_key(&session, &config.host, config.port)?;

    let auth_label = match &config.auth {
        AuthMethod::Password(password) => {
            session
                .userauth_password(&config.username, password)
                .map_err(|e| ConnectError::Auth(e.to_string()))?;
            "密码认证"
        }
        AuthMethod::PrivateKey { key_path, passphrase } => {
            authenticate_with_private_key(&session, &config.username, key_path, passphrase.as_deref())?
        }
    };

    if !session.authenticated() {
        return Err(ConnectError::Auth("服务器拒绝了提供的凭据".into()));
    }

    session.set_timeout(SSH_OPERATION_TIMEOUT_MS as u32);
    session.set_keepalive(true, SSH_KEEPALIVE_INTERVAL_SECS);
    Ok((session, abort_socket, auth_label))
}

fn has_passphrase(passphrase: Option<&str>) -> bool {
    passphrase.map(|p| !p.is_empty()).unwrap_or(false)
}

/// 私钥认证：有口令时读文件；否则先走 ssh-agent（与终端 ssh 一致），再回退私钥文件。
fn authenticate_with_private_key(
    session: &Session,
    username: &str,
    key_path: &str,
    passphrase: Option<&str>,
) -> Result<&'static str, ConnectError> {
    if has_passphrase(passphrase) {
        auth_with_key_file(session, username, key_path, passphrase)?;
        return Ok("私钥认证");
    }

    if std::env::var_os("SSH_AUTH_SOCK").is_some()
        && session.userauth_agent(username).is_ok()
        && session.authenticated()
    {
        return Ok("ssh-agent 认证");
    }

    auth_with_key_file(session, username, key_path, None)?;
    Ok("私钥认证")
}

fn auth_with_key_file(
    session: &Session,
    username: &str,
    key_path: &str,
    passphrase: Option<&str>,
) -> Result<(), ConnectError> {
    session
        .userauth_pubkey_file(username, None, Path::new(key_path), passphrase)
        .map_err(|e| ConnectError::Auth(e.to_string()))?;
    Ok(())
}

pub fn abort_socket(socket: &TcpStream) {
    let _ = socket.shutdown(Shutdown::Both);
}

/// 验证 SSH 握手、认证与 SFTP 子系统可用。
pub fn test_connection(config: &ConnectionConfig) -> Result<String, ConnectError> {
    let (session, _) = connect_session(config)?;
    let sftp = session
        .sftp()
        .map_err(|e| ConnectError::Handshake(format!("SFTP 子系统不可用: {e}")))?;
    sftp.stat(Path::new("."))
        .map_err(|e| ConnectError::Handshake(format!("SFTP 访问失败: {e}")))?;
    Ok(format!(
        "连接成功：{}@{}:{}，SFTP 可用",
        config.username, config.host, config.port
    ))
}
