//! 让服务器自己算 SHA-256，只把 64 个字符传回来。
//!
//! 校验原本靠「把写上去的数据再读回本地比对」：可靠模式下每块读回一次，快速模式结束时整个文件
//! 读回一次，等于流量翻倍——偏偏是在最缺带宽的弱网上。服务器允许执行命令时，哈希在服务器本地算，
//! 校验几乎不占带宽。服务器只开放 SFTP、或没有哈希工具时，这里的函数返回 None / Err，
//! 调用方退回读回比对，绝不能因为这条捷径走不通而让传输失败。

use super::client::{disable_keepalive, enable_keepalive};
use super::preflight::shell_quote;
use super::transfer_common::CHUNK_SIZE;
use ssh2::{Channel, Session};
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SHA256_HEX_LEN: usize = 64;
/// 估算整文件哈希最长要等多久用的保守速度：再慢的服务器磁盘也不至于低于这个值。
const SLOWEST_REMOTE_HASH_BYTES_PER_SEC: u64 = 5 * 1024 * 1024;
/// 在估算时间之外固定多等的余量（进程启动、磁盘缓存未命中等）。
const REMOTE_HASH_GRACE: Duration = Duration::from_secs(60);
/// 逐块哈希最多等多久。几 MB 的数据服务器瞬间就能算完，等不到回复只可能是连接出了问题；
/// 取得比会话的单次操作超时短，意思是「第一次读超时就放弃」，而不是像整文件哈希那样接着等。
const BLOCK_HASH_GRACE: Duration = Duration::from_secs(5);
/// 单行回复的长度上限：正常只有 64 个十六进制字符加文件名，超过说明对面输出的不是哈希。
const MAX_REPLY_BYTES: usize = 512;

/// 选出服务器上可用的哈希命令并存进 `$H`：Linux 一般有 sha256sum，macOS/BSD 是 shasum，
/// 精简系统可能只有 openssl。都没有时以非零状态退出，调用方据此退回读回比对。
const PICK_HASH_TOOL: &str = "if command -v sha256sum >/dev/null 2>&1; then H='sha256sum'; \
elif command -v shasum >/dev/null 2>&1; then H='shasum -a 256'; \
elif command -v openssl >/dev/null 2>&1; then H='openssl dgst -sha256 -r'; \
else exit 3; fi";

/// 用户的登录 shell 可能是 fish / csh，脚本语法不通用，统一交给 sh 执行。
fn sh(script: &str) -> String {
    format!("sh -c {}", shell_quote(script))
}

fn parse_hash(reply: &str) -> Option<[u8; 32]> {
    let hex = reply.split_whitespace().next()?;
    if hex.len() != SHA256_HEX_LEN {
        return None;
    }
    let mut out = [0_u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// 离开作用域时关掉 keepalive（原因见 client::enable_keepalive）。
struct KeepaliveScope<'a>(&'a Session);

impl<'a> KeepaliveScope<'a> {
    fn new(session: &'a Session) -> Self {
        enable_keepalive(session);
        Self(session)
    }
}

impl Drop for KeepaliveScope<'_> {
    fn drop(&mut self) {
        disable_keepalive(self.0);
    }
}

/// 读到一个换行为止。会话的单次读取超时（约 30 秒）到了不算失败——服务器可能正在读一个很大的文件，
/// 期间没有任何输出——只要没超过 `deadline` 就接着等；`cancel` 置位时立即放弃。
/// `on_wait` 在每次等待超时后调用，让调用方刷新「还活着」的心跳（否则会被无响应看门狗掐断）。
fn read_line(
    channel: &mut Channel,
    deadline: Instant,
    cancel: &AtomicBool,
    on_wait: &dyn Fn(),
) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        if cancel.load(Ordering::Relaxed) || line.len() > MAX_REPLY_BYTES {
            return None;
        }
        match channel.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) if byte[0] == b'\n' => return String::from_utf8(line).ok(),
            Ok(_) => line.push(byte[0]),
            Err(e) if e.kind() == ErrorKind::TimedOut && Instant::now() < deadline => on_wait(),
            Err(e) => {
                log::debug!("remote hash read failed: {e}");
                return None;
            }
        }
    }
}

/// 整个远端文件的 SHA-256。返回 None 表示这条路走不通（不能执行命令、没有工具、超时、被取消），
/// 调用方应改用 SFTP 读回计算。`size` 只用于估算最长等待时间。
pub fn remote_file_sha256(session: &Session, path: &str, size: u64, cancel: &AtomicBool) -> Option<[u8; 32]> {
    // 服务器读文件期间连接上没有任何流量，需要 keepalive 保住 NAT / 防火墙上的连接。
    let _keepalive = KeepaliveScope::new(session);
    let script = format!("{PICK_HASH_TOOL}; $H < {}", shell_quote(path));
    let mut channel = session.channel_session().ok()?;
    channel.exec(&sh(&script)).ok()?;
    let deadline = deadline_for(size);
    let hash = read_line(&mut channel, deadline, cancel, &|| {}).as_deref().and_then(parse_hash);
    if hash.is_none() {
        log::info!("remote sha256 unavailable for {path}, falling back to reading the file back");
    }
    hash
}

/// 服务器处理 `size` 字节的文件最长等多久：按保守速度估算，再加固定余量。
fn deadline_for(size: u64) -> Instant {
    Instant::now() + REMOTE_HASH_GRACE + Duration::from_secs(size / SLOWEST_REMOTE_HASH_BYTES_PER_SEC)
}

/// 文件前 `blocks` 个整块各自的 SHA-256，一次命令全部算完（每块一行）。
/// 用来找出「目标端已有、且与源文件相同」的块，这些块不必再传。任何一步走不通都返回 None。
pub fn remote_block_hashes(
    session: &Session,
    path: &str,
    blocks: u64,
    cancel: &AtomicBool,
    on_wait: &dyn Fn(),
) -> Option<Vec<[u8; 32]>> {
    let _keepalive = KeepaliveScope::new(session);
    let script = format!(
        "{PICK_HASH_TOOL}; i=0; while [ \"$i\" -lt {blocks} ]; do \
         dd if={} bs={CHUNK_SIZE} skip=\"$i\" count=1 2>/dev/null | $H; i=$((i+1)); done",
        shell_quote(path)
    );
    let mut channel = session.channel_session().ok()?;
    channel.exec(&sh(&script)).ok()?;
    let deadline = deadline_for(blocks * CHUNK_SIZE);
    let mut hashes = Vec::with_capacity(blocks as usize);
    for _ in 0..blocks {
        on_wait();
        hashes.push(read_line(&mut channel, deadline, cancel, on_wait).as_deref().and_then(parse_hash)?);
    }
    Some(hashes)
}

/// 在服务器上把 `from` 复制为 `to`（不经过本机）。成功与否以退出码为准。
pub fn remote_copy(
    session: &Session,
    from: &str,
    to: &str,
    size: u64,
    cancel: &AtomicBool,
    on_wait: &dyn Fn(),
) -> bool {
    let _keepalive = KeepaliveScope::new(session);
    // 成功时输出一行标记：仅凭「通道关闭」分不清是复制完了还是连接断了。
    let script = format!("cp -- {} {} && echo {COPY_OK_MARK}", shell_quote(from), shell_quote(to));
    let Ok(mut channel) = session.channel_session() else { return false };
    if channel.exec(&sh(&script)).is_err() {
        return false;
    }
    read_line(&mut channel, deadline_for(size), cancel, on_wait).as_deref() == Some(COPY_OK_MARK)
}

/// `remote_copy` 成功时服务器回的那一行。
const COPY_OK_MARK: &str = "resiliss-copy-ok";

/// 逐块校验用的常驻哈希进程：在服务器上起一个读指令的小循环，每校验一块只需发一行、收一行，
/// 一个往返即可，不必为每一块都新开一个通道。
pub struct BlockHasher {
    channel: Channel,
}

impl BlockHasher {
    /// 启动常驻进程。服务器不允许执行命令时返回 None。没有哈希工具的情况要到第一次 `hash` 才会暴露
    /// （进程立即退出，读到 EOF），同样由调用方退回读回比对。
    pub fn start(session: &Session, path: &str) -> Option<Self> {
        // 每行指令是「块序号 字节数」：dd 取出那一块，head 截到实际长度（最后一块不满、
        // 或文件后面还有别的内容时都要截），再交给哈希工具。
        let script = format!(
            "{PICK_HASH_TOOL}; while read skip len; do dd if={} bs={CHUNK_SIZE} skip=\"$skip\" count=1 2>/dev/null \
             | head -c \"$len\" | $H; done",
            shell_quote(path)
        );
        let mut channel = session.channel_session().ok()?;
        channel.exec(&sh(&script)).ok()?;
        Some(Self { channel })
    }

    /// 一次核对多块：请求连着发出去、回复连着收回来，总共约一个往返，而不是每块一个。
    /// 每个 `(offset, len)` 的含义与 `hash` 相同。
    pub fn hash_many(&mut self, blocks: &[(u64, u64)], cancel: &AtomicBool) -> Result<Vec<[u8; 32]>, String> {
        let requests: String = blocks
            .iter()
            .map(|(offset, len)| {
                debug_assert_eq!(offset % CHUNK_SIZE, 0, "block hasher works on block-aligned offsets");
                format!("{} {len}\n", offset / CHUNK_SIZE)
            })
            .collect();
        self.channel.write_all(requests.as_bytes()).and_then(|()| self.channel.flush()).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + BLOCK_HASH_GRACE;
        blocks
            .iter()
            .map(|_| {
                read_line(&mut self.channel, deadline, cancel, &|| {})
                    .as_deref()
                    .and_then(parse_hash)
                    .ok_or_else(|| "remote block hasher gave no usable reply".to_string())
            })
            .collect()
    }

    /// 服务器上 `[offset, offset+len)` 这一块的 SHA-256；`offset` 必须是块大小的整数倍。
    pub fn hash(&mut self, offset: u64, len: u64, cancel: &AtomicBool) -> Result<[u8; 32], String> {
        debug_assert_eq!(offset % CHUNK_SIZE, 0, "block hasher works on block-aligned offsets");
        let request = format!("{} {len}\n", offset / CHUNK_SIZE);
        self.channel.write_all(request.as_bytes()).and_then(|()| self.channel.flush()).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + BLOCK_HASH_GRACE;
        read_line(&mut self.channel, deadline, cancel, &|| {})
            .as_deref()
            .and_then(parse_hash)
            .ok_or_else(|| "remote block hasher gave no usable reply".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_tool_outputs() {
        let hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        // sha256sum / shasum：「哈希  文件名」；openssl dgst -r：「哈希 *文件名」。
        for reply in [format!("{hex}  -"), format!("{hex} *stdin"), hex.to_string()] {
            let parsed = parse_hash(&reply).expect("hash should parse");
            assert_eq!(parsed[0], 0xe3);
            assert_eq!(parsed[31], 0x55);
        }
        assert!(parse_hash("sh: sha256sum: not found").is_none());
        assert!(parse_hash("").is_none());
    }
}
