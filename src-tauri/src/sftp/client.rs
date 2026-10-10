use super::host_key::verify_host_key;
use super::tunnel;
use crate::activity::{self, Category};
use ssh2::Session;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::Duration;

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
    /// 经由的跳板机（等同 ssh -J）。跳板机自身也可以再经由跳板机，形成多级链路。
    pub jump: Option<Box<ConnectionConfig>>,
}

/// 跳板机链最多几级：实际环境很少超过两级；限制层数也防止前端传来异常的超长链。
pub const MAX_JUMP_HOPS: usize = 3;

impl ConnectionConfig {
    pub fn endpoint(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    /// 经过的跳板机数量。
    pub fn jump_hops(&self) -> usize {
        self.jump.as_ref().map_or(0, |j| 1 + j.jump_hops())
    }
}

/// 不用 thiserror 的固定 `#[error]` 文案：错误会原样展示给用户，必须在展示时按界面语言选择。
#[derive(Debug)]
pub enum ConnectError {
    Tcp(String),
    Handshake(String),
    Auth(String),
    HostKeyUnknown(String),
    HostKeyChanged(String),
    HostKeyCheck(String),
    /// 跳板机禁止端口转发（如 sshd 配置了 AllowTcpForwarding no）。
    JumpForwardDenied(String),
    /// 连接跳板机这一跳失败；保留原始错误，界面上要能看出是跳板机而不是目标服务器的问题。
    Jump { hop: String, source: Box<ConnectError> },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            ConnectError::Tcp(e) => crate::tr!(format!("TCP 连接失败: {e}"), format!("TCP connection failed: {e}")),
            ConnectError::Handshake(e) => {
                crate::tr!(format!("SSH 握手失败: {e}"), format!("SSH handshake failed: {e}"))
            }
            ConnectError::Auth(e) => {
                crate::tr!(format!("认证失败: {e}"), format!("Authentication failed: {e}"))
            }
            ConnectError::HostKeyUnknown(fp) => crate::tr!(
                format!("未信任的服务器（指纹 {fp}），请确认后再连接"),
                format!("Untrusted server (host key fingerprint {fp}), please confirm before connecting")
            ),
            ConnectError::HostKeyChanged(fp) => crate::tr!(
                format!("服务器指纹已变化（{fp}），可能遭遇中间人攻击，已拒绝连接"),
                format!("Server host key fingerprint changed ({fp}), possible man-in-the-middle attack; connection refused")
            ),
            ConnectError::HostKeyCheck(e) => crate::tr!(
                format!("主机密钥校验失败: {e}"),
                format!("Host key verification failed: {e}")
            ),
            ConnectError::JumpForwardDenied(e) => crate::tr!(
                format!("跳板机不允许端口转发（需要 sshd 开启 AllowTcpForwarding）: {e}"),
                format!("The jump host does not allow port forwarding (sshd needs AllowTcpForwarding): {e}")
            ),
            ConnectError::Jump { hop, source } => {
                crate::tr!(format!("跳板机 {hop}：{source}"), format!("Jump host {hop}: {source}"))
            }
        };
        f.write_str(&text)
    }
}

impl std::error::Error for ConnectError {}

impl ConnectError {
    /// 主机密钥问题不会因重连自愈：未知主机需要用户确认，指纹变化更不能自动重试，
    /// 否则弱网重试循环会对着可疑服务器无限握手。
    pub fn is_permanent(&self) -> bool {
        match self {
            ConnectError::HostKeyUnknown(_)
            | ConnectError::HostKeyChanged(_)
            | ConnectError::HostKeyCheck(_)
            | ConnectError::JumpForwardDenied(_) => true,
            ConnectError::Jump { source, .. } => source.is_permanent(),
            _ => false,
        }
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
        .map_err(|e| ConnectError::Tcp(crate::tr!(format!("无法解析主机 {host}: {e}"), format!("Cannot resolve host {host}: {e}"))))?;
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
        last_err.map_or_else(|| crate::tr!(format!("主机 {host} 没有可用地址"), format!("Host {host} has no usable address")), |e| e.to_string()),
    ))
}

/// 已完成 SSH 握手的会话及其底层连接。
struct Transport {
    session: Session,
    /// 第一跳的 TCP 连接副本：shutdown 它即可打断整条链路上的阻塞 IO（取消、弱网重连都靠它）。
    abort: TcpStream,
    /// 本会话自身底层连接的副本，作为下一跳的跳板机时供转发线程 poll。
    poll_fd: OwnedFd,
}

fn socket_error(e: std::io::Error) -> ConnectError {
    ConnectError::Tcp(format!("socket setup failed: {e}"))
}

fn handshake_direct(host: &str, port: u16) -> Result<Transport, ConnectError> {
    let tcp = connect_with_timeout(host, port)?;
    tcp.set_read_timeout(Some(Duration::from_secs(SSH_SOCKET_TIMEOUT_SECS))).map_err(socket_error)?;
    tcp.set_write_timeout(Some(Duration::from_secs(SSH_SOCKET_TIMEOUT_SECS))).map_err(socket_error)?;
    let abort = tcp.try_clone().map_err(socket_error)?;
    let poll_fd = OwnedFd::from(tcp.try_clone().map_err(socket_error)?);

    let mut session = Session::new().map_err(|e| ConnectError::Handshake(e.to_string()))?;
    session.set_tcp_stream(tcp);
    session.handshake().map_err(|e| ConnectError::Handshake(e.to_string()))?;
    Ok(Transport { session, abort, poll_fd })
}

/// 先完整连上（含认证）跳板机，再经它的转发通道与目标做握手。跳板机出错时包装成 Jump，
/// 让日志和界面指明是哪一跳的问题。
fn handshake_via(jump: &ConnectionConfig, host: &str, port: u16) -> Result<Transport, ConnectError> {
    let wrap = |source: ConnectError| ConnectError::Jump { hop: jump.endpoint(), source: Box::new(source) };
    let (hop, _) = establish(jump).map_err(wrap)?;
    let stream = tunnel::open(hop.session, hop.poll_fd, host, port).map_err(wrap)?;
    stream.set_read_timeout(Some(Duration::from_secs(SSH_SOCKET_TIMEOUT_SECS))).map_err(socket_error)?;
    stream.set_write_timeout(Some(Duration::from_secs(SSH_SOCKET_TIMEOUT_SECS))).map_err(socket_error)?;
    let poll_fd = OwnedFd::from(stream.try_clone().map_err(socket_error)?);

    let mut session = Session::new().map_err(|e| ConnectError::Handshake(e.to_string()))?;
    session.set_tcp_stream(stream);
    session.handshake().map_err(|e| ConnectError::Handshake(e.to_string()))?;
    Ok(Transport { session, abort: hop.abort, poll_fd })
}

fn handshake(host: &str, port: u16, jump: Option<&ConnectionConfig>) -> Result<Transport, ConnectError> {
    match jump {
        Some(jump) => handshake_via(jump, host, port),
        None => handshake_direct(host, port),
    }
}

/// 只完成 SSH 握手、不对目标做认证：主机密钥检查/信任流程只需要服务器公钥，
/// 不应为此先把密码发出去。经跳板机时，跳板机本身需要先认证才能转发。
pub fn open_handshake(
    host: &str,
    port: u16,
    jump: Option<&ConnectionConfig>,
) -> Result<(Session, TcpStream), ConnectError> {
    handshake(host, port, jump).map(|t| (t.session, t.abort))
}

/// 返回会话和可 shutdown 的 socket 副本，用于取消时立刻打断阻塞 IO。
/// 成功只记一行汇总（含耗时与认证方式），失败按卡住的环节分别说明，日志页才不会被刷屏。
pub fn connect_session(config: &ConnectionConfig) -> Result<(Session, TcpStream), ConnectError> {
    let started = std::time::Instant::now();
    let target = config.endpoint();
    // 日志里写明经由的跳板机：弱网排障时要知道慢/断发生在哪条链路上。
    let via = config.jump.as_ref().map(|j| j.endpoint());
    match establish(config) {
        Ok((transport, auth_label)) => {
            let ms = started.elapsed().as_millis();
            activity::success(
                Category::Connect,
                match &via {
                    Some(via) => crate::tr!(
                        format!("已经跳板机 {via} 连接 {target} · 用时 {ms} ms · {auth_label}"),
                        format!("Connected to {target} via jump host {via} · {ms} ms · {auth_label}")
                    ),
                    None => crate::tr!(
                        format!("已连接 {target} · 用时 {ms} ms · {auth_label}"),
                        format!("Connected to {target} · {ms} ms · {auth_label}")
                    ),
                },
            );
            Ok((transport.session, transport.abort))
        }
        Err(err) => {
            record_connect_failure(&target, &err);
            Err(err)
        }
    }
}

fn record_connect_failure(target: &str, err: &ConnectError) {
    match err {
        ConnectError::Tcp(e) => activity::warn(Category::Network, crate::tr!(format!("无法连接 {target}：{e}"), format!("Cannot connect to {target}: {e}"))),
        ConnectError::Handshake(e) => {
            activity::warn(Category::Network, crate::tr!(format!("SSH 握手失败 {target}：{e}"), format!("SSH handshake failed for {target}: {e}")))
        }
        ConnectError::Auth(e) => activity::error(Category::Auth, crate::tr!(format!("认证失败 {target}：{e}"), format!("Authentication failed for {target}: {e}"))),
        ConnectError::HostKeyUnknown(fp) => activity::warn(
            Category::HostKey,
            crate::tr!(
                format!("{target} 是未信任的服务器（指纹 {fp}），需要确认后才能连接"),
                format!("{target} is an untrusted server (host key fingerprint {fp}); confirm it before connecting")
            ),
        ),
        ConnectError::HostKeyChanged(fp) => activity::error(
            Category::HostKey,
            crate::tr!(
                format!("{target} 的指纹与记录不一致（当前 {fp}），已拒绝连接"),
                format!("{target} host key fingerprint does not match the record (now {fp}); connection refused")
            ),
        ),
        ConnectError::HostKeyCheck(e) => {
            activity::error(Category::HostKey, crate::tr!(format!("{target} 指纹校验出错：{e}"), format!("Host key fingerprint check failed for {target}: {e}")))
        }
        ConnectError::JumpForwardDenied(_) => activity::error(Category::Network, format!("{target}: {err}")),
        // 跳板机那一跳的失败按它自己的类型记录，日志分类（网络/认证/指纹）才对得上。
        ConnectError::Jump { hop, source } => record_connect_failure(hop, source),
    }
}

/// 返回值第二项是实际生效的认证方式描述，用于日志（私钥可能实际走了 ssh-agent）。
fn establish(config: &ConnectionConfig) -> Result<(Transport, &'static str), ConnectError> {
    let transport = handshake(&config.host, config.port, config.jump.as_deref())?;
    let session = &transport.session;
    // 经跳板机时仍按目标自己的 host:port 校验指纹（与 OpenSSH 的 ProxyJump 行为一致）。
    verify_host_key(session, &config.host, config.port)?;

    let auth_label = match &config.auth {
        AuthMethod::Password(password) => {
            session
                .userauth_password(&config.username, password)
                .map_err(|e| ConnectError::Auth(e.to_string()))?;
            crate::tr!("密码认证", "password auth")
        }
        AuthMethod::PrivateKey { key_path, passphrase } => {
            authenticate_with_private_key(session, &config.username, key_path, passphrase.as_deref())?
        }
    };

    if !session.authenticated() {
        return Err(ConnectError::Auth(
            crate::tr!("服务器拒绝了提供的凭据", "The server rejected the credentials").into(),
        ));
    }

    session.set_timeout(SSH_OPERATION_TIMEOUT_MS as u32);
    session.set_keepalive(true, SSH_KEEPALIVE_INTERVAL_SECS);
    Ok((transport, auth_label))
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
        return Ok(crate::tr!("私钥认证", "private key auth"));
    }

    if std::env::var_os("SSH_AUTH_SOCK").is_some()
        && session.userauth_agent(username).is_ok()
        && session.authenticated()
    {
        return Ok(crate::tr!("ssh-agent 认证", "ssh-agent auth"));
    }

    auth_with_key_file(session, username, key_path, None)?;
    Ok(crate::tr!("私钥认证", "private key auth"))
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
        .map_err(|e| ConnectError::Handshake(crate::tr!(format!("SFTP 子系统不可用: {e}"), format!("SFTP subsystem unavailable: {e}"))))?;
    sftp.stat(Path::new("."))
        .map_err(|e| ConnectError::Handshake(crate::tr!(format!("SFTP 访问失败: {e}"), format!("SFTP access failed: {e}"))))?;
    let target = config.endpoint();
    Ok(match config.jump.as_ref().map(|j| j.endpoint()) {
        Some(via) => crate::tr!(
            format!("连接成功：{target}（经跳板机 {via}），SFTP 可用"),
            format!("Connected: {target} (via jump host {via}), SFTP available")
        ),
        None => crate::tr!(
            format!("连接成功：{target}，SFTP 可用"),
            format!("Connected: {target}, SFTP available")
        ),
    })
}
