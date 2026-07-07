use ssh2::Session;
use std::net::{Shutdown, TcpStream};
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
}

const SSH_SOCKET_TIMEOUT_SECS: u64 = 90;
const SSH_OPERATION_TIMEOUT_MS: u64 = 30_000;
const SSH_KEEPALIVE_INTERVAL_SECS: u32 = 30;

/// 返回会话和可 shutdown 的 socket 副本，用于取消时立刻打断阻塞 IO。
pub fn connect_session(config: &ConnectionConfig) -> Result<(Session, TcpStream), ConnectError> {
    let address = format!("{}:{}", config.host, config.port);
    let tcp = TcpStream::connect(&address).map_err(|e| ConnectError::Tcp(e.to_string()))?;
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

    match &config.auth {
        AuthMethod::Password(password) => {
            session
                .userauth_password(&config.username, password)
                .map_err(|e| ConnectError::Auth(e.to_string()))?;
        }
        AuthMethod::PrivateKey { key_path, passphrase } => {
            authenticate_with_private_key(&session, &config.username, key_path, passphrase.as_deref())?;
        }
    }

    if !session.authenticated() {
        return Err(ConnectError::Auth("服务器拒绝了提供的凭据".into()));
    }

    session.set_timeout(SSH_OPERATION_TIMEOUT_MS as u32);
    session.set_keepalive(true, SSH_KEEPALIVE_INTERVAL_SECS);

    log::info!(
        "SSH connected: user={} host={}:{}",
        config.username,
        config.host,
        config.port
    );

    Ok((session, abort_socket))
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
) -> Result<(), ConnectError> {
    if has_passphrase(passphrase) {
        return auth_with_key_file(session, username, key_path, passphrase);
    }

    if std::env::var_os("SSH_AUTH_SOCK").is_some() {
        if session.userauth_agent(username).is_ok() && session.authenticated() {
            log::info!("authenticated via ssh-agent for {username}");
            return Ok(());
        }
    }

    auth_with_key_file(session, username, key_path, None)
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
