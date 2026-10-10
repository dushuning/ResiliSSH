//! 经跳板机连接（等同 `ssh -J`）：在已认证的跳板机会话上打开一条到目标 host:port 的
//! direct-tcpip 通道，再在这条通道上与目标服务器做 SSH 握手。
//!
//! libssh2 只能在一个真实的文件描述符上建立会话，不能直接把 SSH 通道当作底层连接，
//! 所以这里建一对本地 Unix socket：一端交给目标会话，另一端由后台线程与跳板机通道双向搬运数据。
//! 跳板机会话只在这个线程里使用（非阻塞 + poll），不会与其他线程争用 libssh2 的会话锁。

use super::client::ConnectError;
use ssh2::{BlockDirections, Channel, Session};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

/// 单次搬运的缓冲区大小：与 SFTP 的典型分片同量级，足以跑满窗口，又不至于占用过多内存。
const PUMP_BUFFER_BYTES: usize = 64 * 1024;
/// 无事件时 poll 的最长等待：兜底防止漏掉唤醒，同时让 keepalive 能按时发出。
const POLL_TIMEOUT_MS: i32 = 1_000;
/// 两个方向都没有任何数据流动的上限。目标会话每 30 秒会发 keepalive，正常连接不可能这么久没有流量；
/// 超过即认定链路已死，结束线程释放跳板机连接，避免弱网下线程和 socket 永久挂起。
const IDLE_LIMIT: Duration = Duration::from_secs(180);
/// libssh2 未给出下次 keepalive 时间时的重试间隔。
const KEEPALIVE_RETRY: Duration = Duration::from_secs(1);

/// libssh2 打开通道被拒时的原因文本（RFC 4254 §5.1 的 SSH_OPEN_ADMINISTRATIVELY_PROHIBITED）。
/// 这表示跳板机配置禁止转发（如 AllowTcpForwarding no），重连也不会好。
const FORWARD_PROHIBITED_MARKER: &str = "prohibited";

/// 在跳板机上打开到 host:port 的转发，返回交给目标会话使用的本地 socket。
/// `hop_fd` 是跳板机会话底层连接的一个副本，只用于 poll 等待可读/可写。
pub fn open(hop: Session, hop_fd: OwnedFd, host: &str, port: u16) -> Result<UnixStream, ConnectError> {
    let channel = hop.channel_direct_tcpip(host, port, None).map_err(|e| {
        let detail = e.to_string();
        if detail.to_ascii_lowercase().contains(FORWARD_PROHIBITED_MARKER) {
            ConnectError::JumpForwardDenied(detail)
        } else {
            ConnectError::Tcp(crate::tr!(
                format!("跳板机无法连到 {host}:{port}：{detail}"),
                format!("The jump host cannot reach {host}:{port}: {detail}")
            ))
        }
    })?;
    let (ours, theirs) = UnixStream::pair().map_err(|e| ConnectError::Tcp(e.to_string()))?;
    let target = format!("{host}:{port}");
    thread::Builder::new()
        .name("ssh-jump-tunnel".into())
        .spawn(move || pump(hop, channel, theirs, hop_fd, &target))
        .map_err(|e| ConnectError::Tcp(e.to_string()))?;
    Ok(ours)
}

/// 双向搬运直到任一端关闭。两个方向各有一个待发缓冲：对端暂时写不进时先留着，
/// 写完之前不再读新数据，保证字节顺序且内存占用有上限。
fn pump(hop: Session, mut channel: Channel, mut local: UnixStream, hop_fd: OwnedFd, target: &str) {
    if let Err(e) = local.set_nonblocking(true) {
        log::warn!("jump tunnel to {target}: cannot set nonblocking: {e}");
        return;
    }
    hop.set_blocking(false);

    let mut buffer = vec![0_u8; PUMP_BUFFER_BYTES];
    let mut up: Vec<u8> = Vec::new(); // 目标会话 → 跳板机通道
    let mut down: Vec<u8> = Vec::new(); // 跳板机通道 → 目标会话
    let mut last_activity = Instant::now();
    let mut next_keepalive = Instant::now();

    let reason = loop {
        let mut progressed = false;

        if up.is_empty() {
            match local.read(&mut buffer) {
                Ok(0) => break "target session closed".to_string(),
                Ok(n) => {
                    up.extend_from_slice(&buffer[..n]);
                    progressed = true;
                }
                Err(e) if is_transient(&e) => {}
                Err(e) => break format!("local read: {e}"),
            }
        }
        if !up.is_empty() {
            match channel.write(&up) {
                Ok(n) => {
                    up.drain(..n);
                    progressed |= n > 0;
                }
                Err(e) if is_transient(&e) => {}
                Err(e) => break format!("channel write: {e}"),
            }
        }
        if down.is_empty() {
            match channel.read(&mut buffer) {
                Ok(0) if channel.eof() => break "jump host closed the channel".to_string(),
                Ok(0) => {}
                Ok(n) => {
                    down.extend_from_slice(&buffer[..n]);
                    progressed = true;
                }
                Err(e) if is_transient(&e) => {}
                Err(e) => break format!("channel read: {e}"),
            }
        }
        if !down.is_empty() {
            match local.write(&down) {
                Ok(n) => {
                    down.drain(..n);
                    progressed |= n > 0;
                }
                Err(e) if is_transient(&e) => {}
                Err(e) => break format!("local write: {e}"),
            }
        }

        let now = Instant::now();
        if now >= next_keepalive {
            next_keepalive = match hop.keepalive_send() {
                Ok(secs) if secs > 0 => now + Duration::from_secs(u64::from(secs)),
                _ => now + KEEPALIVE_RETRY,
            };
        }

        if progressed {
            last_activity = now;
            continue;
        }
        if now.duration_since(last_activity) > IDLE_LIMIT {
            break format!("no traffic for {}s", IDLE_LIMIT.as_secs());
        }
        // 目标会话已关闭、但待发缓冲还没写完（跳板机侧卡住）时，读端不会再被尝试，
        // 只能从 poll 的挂断标志得知，否则线程会一直等下去。
        if wait_for_events(&hop, &hop_fd, &local, up.is_empty(), !down.is_empty()) && !up.is_empty() {
            break "target session closed while data was pending".to_string();
        }
    };

    log::debug!("jump tunnel to {target} closed: {reason}");
    let _ = local.shutdown(std::net::Shutdown::Both);
    // channel 与 hop 在此处析构，关闭跳板机上的通道与连接。
}

fn is_transient(err: &std::io::Error) -> bool {
    matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
}

/// 等到任一端可能有进展。返回本地 socket 是否已挂断。
fn wait_for_events(
    hop: &Session,
    hop_fd: &OwnedFd,
    local: &UnixStream,
    want_local_read: bool,
    want_local_write: bool,
) -> bool {
    // libssh2 告诉我们它卡在哪个方向：发送窗口满时要等可写，否则等新数据（含窗口调整包）。
    let hop_events = match hop.block_directions() {
        BlockDirections::Outbound => libc::POLLOUT,
        BlockDirections::Both => libc::POLLIN | libc::POLLOUT,
        BlockDirections::Inbound | BlockDirections::None => libc::POLLIN,
    };
    let mut local_events = 0;
    if want_local_read {
        local_events |= libc::POLLIN;
    }
    if want_local_write {
        local_events |= libc::POLLOUT;
    }
    let mut fds = [
        libc::pollfd { fd: hop_fd.as_raw_fd(), events: hop_events, revents: 0 },
        libc::pollfd { fd: local.as_raw_fd(), events: local_events, revents: 0 },
    ];
    // SAFETY: fds 是长度为 2 的有效 pollfd 数组，两个描述符在调用期间都由本线程持有、保持打开。
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_TIMEOUT_MS) };
    ready > 0 && fds[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}
