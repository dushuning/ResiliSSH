use super::host_key::{prefer_known_host_key_types, verify_host_key};
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
    /// 认证过程本身没走完（超时、断线等），重连后可能成功。
    Auth(String),
    /// 服务器明确拒绝了凭据，或私钥文件本身无法使用：重连多少次结果都一样。
    AuthRejected(String),
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
            // 两种认证错误对用户是同一句话，区别只在是否值得重试（见 is_permanent）。
            ConnectError::Auth(e) | ConnectError::AuthRejected(e) => {
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
    /// 否则弱网重试循环会对着可疑服务器无限握手。凭据被明确拒绝同理：每十几秒拿同一个
    /// 错误密码再试一次，除了触发服务器的 fail2ban / 账户锁定之外没有任何作用。
    pub fn is_permanent(&self) -> bool {
        match self {
            ConnectError::AuthRejected(_)
            | ConnectError::HostKeyUnknown(_)
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

/// 握手 + 认证整体的上限（见 new_session）。比单次操作超时宽：极差的链路上密钥交换本身就要好几个往返。
const SSH_HANDSHAKE_TIMEOUT_SECS: u64 = 90;
/// 单次 TCP 建连上限。系统默认可能等 75s 以上，期间无法取消；
/// 超时后交给外层重试循环，所以这里不会让弱网任务失败，只是让每次尝试可控、可取消。
const TCP_CONNECT_TIMEOUT_SECS: u64 = 20;
const SSH_OPERATION_TIMEOUT_MS: u64 = 30_000;
/// 需要 keepalive 的场合（见 enable_keepalive）使用的间隔。
const SSH_KEEPALIVE_INTERVAL_SECS: u32 = 30;
/// libssh2 的认证类错误码（libssh2.h）：私钥文件读不了或口令不对 / 服务器明确拒绝了凭据。
/// 不包括 -19（PUBLICKEY_UNVERIFIED）：libssh2 在「等认证回复时连接断了」也报这个码
/// （"Waiting for publickey USERAUTH response"），把它当成被拒绝会让一次断线变成永久失败。
const LIBSSH2_ERROR_FILE: i32 = -16;
const LIBSSH2_ERROR_AUTHENTICATION_FAILED: i32 = -18;

/// 把认证阶段的 libssh2 错误分成「被拒绝」和「没走完」：前者重试无益，后者（超时、断线）必须继续重试。
fn auth_error(err: ssh2::Error) -> ConnectError {
    let rejected = matches!(
        err.code(),
        ssh2::ErrorCode::Session(LIBSSH2_ERROR_FILE | LIBSSH2_ERROR_AUTHENTICATION_FAILED)
    );
    if rejected {
        ConnectError::AuthRejected(err.to_string())
    } else {
        ConnectError::Auth(err.to_string())
    }
}

/// libssh2 的自动 keepalive 默认关闭，只在连接上长时间没有自己流量的场合打开
/// （本地算哈希、等远端命令输出、跳板机转发线程）。原因：开着它时，libssh2 每次阻塞等待
/// 最多只等到「下一次该发 keepalive 的时刻」就按超时返回（_libssh2_wait_socket），
/// 传输中任何一次跨过这个时刻的停顿都会被当成超时、触发整条连接重连——弱网下正是最常见的情况。
pub(super) fn enable_keepalive(session: &Session) {
    session.set_keepalive(true, SSH_KEEPALIVE_INTERVAL_SECS);
}

pub(super) fn disable_keepalive(session: &Session) {
    session.set_keepalive(false, 0);
}

/// 会话暂时没活干、但稍后还要用时（并行传输中主连接等其他连接传完）定期调用：按间隔发一个 keepalive，
/// 免得 NAT / 防火墙 / 跳板机转发线程把这条安静的连接当成死连接掐掉。调用期间不在 libssh2 的
/// 阻塞等待里，所以不受「keepalive 让等待提前超时」的影响；发完立刻恢复关闭状态。
pub(super) fn keepalive_while_idle(session: &Session) {
    enable_keepalive(session);
    if let Err(e) = session.keepalive_send() {
        log::debug!("keepalive on idle session failed: {e}");
    }
    disable_keepalive(session);
}

/// 把会话的单次操作超时恢复为默认值（探活等场合会临时改短）。
pub(super) fn set_operation_timeout(session: &Session, timeout_ms: u64) {
    session.set_timeout(timeout_ms as u32);
}

pub(super) const DEFAULT_OPERATION_TIMEOUT_MS: u64 = SSH_OPERATION_TIMEOUT_MS;

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
    ConnectError::Tcp(crate::tr!(format!("连接初始化失败: {e}"), format!("Socket setup failed: {e}")))
}

/// libssh2 握手时会把连接切成非阻塞，socket 自身的读写超时对它不起作用；不设会话超时的话，
/// TCP 连上后对端一直不回应（单向黑洞、sshd 卡死）时握手和认证会无限期阻塞，而此时取消和
/// 无响应看门狗都还拿不到连接、打不断它。认证通过后再换成更短的单次操作超时。
/// 握手前还要按已记录的主机密钥类型调整协商顺序（见 prefer_known_host_key_types）。
fn new_session(host: &str, port: u16) -> Result<Session, ConnectError> {
    let session = Session::new().map_err(|e| ConnectError::Handshake(e.to_string()))?;
    session.set_timeout((SSH_HANDSHAKE_TIMEOUT_SECS * 1000) as u32);
    prefer_known_host_key_types(&session, host, port);
    Ok(session)
}

fn handshake_direct(host: &str, port: u16) -> Result<Transport, ConnectError> {
    let tcp = connect_with_timeout(host, port)?;
    let abort = tcp.try_clone().map_err(socket_error)?;
    let poll_fd = OwnedFd::from(tcp.try_clone().map_err(socket_error)?);

    let mut session = new_session(host, port)?;
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
    let poll_fd = OwnedFd::from(stream.try_clone().map_err(socket_error)?);

    let mut session = new_session(host, port)?;
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
        ConnectError::Auth(e) | ConnectError::AuthRejected(e) => activity::error(Category::Auth, crate::tr!(format!("认证失败 {target}：{e}"), format!("Authentication failed for {target}: {e}"))),
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
                .map_err(auth_error)?;
            crate::tr!("密码认证", "password auth")
        }
        AuthMethod::PrivateKey { key_path, passphrase } => {
            authenticate_with_private_key(session, &config.username, key_path, passphrase.as_deref())?
        }
    };

    if !session.authenticated() {
        return Err(ConnectError::AuthRejected(
            crate::tr!("服务器拒绝了提供的凭据", "The server rejected the credentials").into(),
        ));
    }

    session.set_timeout(SSH_OPERATION_TIMEOUT_MS as u32);
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
        .map_err(auth_error)?;
    Ok(())
}

pub fn abort_socket(socket: &TcpStream) {
    let _ = socket.shutdown(Shutdown::Both);
}

/// 一次传输用到的所有连接的「可打断」副本。并行传输时不止一条连接：取消、无响应看门狗、
/// 网络切换都要能把它们一起打断，让各自阻塞中的网络 IO 立刻返回。
#[derive(Default)]
pub struct AbortHandles {
    sockets: std::sync::Mutex<Vec<(u64, TcpStream)>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl AbortHandles {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(u64, TcpStream)>> {
        // 持锁期间只增删列表项；锁中毒时列表仍然可用，而此时更需要能打断连接。
        self.sockets.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 登记一条连接，返回之后用来注销它的编号。
    pub fn register(&self, socket: TcpStream) -> u64 {
        let id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.lock().push((id, socket));
        id
    }

    /// 连接不再使用时注销，并交还它的 socket 副本（会话要放回连接池时需要）。
    pub fn remove(&self, id: u64) -> Option<TcpStream> {
        let mut sockets = self.lock();
        let index = sockets.iter().position(|(socket_id, _)| *socket_id == id)?;
        Some(sockets.swap_remove(index).1)
    }

    pub fn abort_all(&self) {
        for (_, socket) in self.lock().iter() {
            abort_socket(socket);
        }
    }

    /// 各连接本机一侧的地址，用来判断网络切换后它们是否已经失效。
    pub fn local_addrs(&self) -> Vec<std::net::IpAddr> {
        self.lock().iter().filter_map(|(_, socket)| socket.local_addr().ok()).map(|addr| addr.ip()).collect()
    }

    /// 当前登记的连接数（端到端测试用来确认并行确实发生）。
    #[cfg(test)]
    pub fn count(&self) -> usize {
        self.lock().len()
    }
}

/// 验证 SSH 握手、认证与 SFTP 子系统可用。
pub fn test_connection(config: &ConnectionConfig) -> Result<String, ConnectError> {
    // 测试连接总是新建：用户要验证的正是「现在能不能连上」，复用旧会话会掩盖问题。
    let (session, abort) = connect_session(config)?;
    let sftp = session
        .sftp()
        .map_err(|e| ConnectError::Handshake(crate::tr!(format!("SFTP 子系统不可用: {e}"), format!("SFTP subsystem unavailable: {e}"))))?;
    sftp.stat(Path::new("."))
        .map_err(|e| ConnectError::Handshake(crate::tr!(format!("SFTP 访问失败: {e}"), format!("SFTP access failed: {e}"))))?;
    // 测试通过后用户多半紧接着就开始传输，把这条已认证的连接留给下一步用。
    drop(sftp);
    super::pool::release(config, session, abort);
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
