//! 上传与下载共用的传输辅助：块对齐、退避等待、取消检查、重试计数、整文件哈希。
//!
//! 这些函数在 upload.rs / download.rs 里原本各有一份几乎逐字相同的实现，改一边忘另一边的风险很高
//! （可重试错误的关键字表就曾因此漂移）。两个方向的差异只在断点类型和错误类型上，
//! 用 `TransferCheckpoint` / `TransferError` 两个小 trait 抽象掉；真正因方向而不同的逻辑
//! （如续传偏移要同时看本地和远端）仍留在各自文件里。

use sha2::{Digest, Sha256};
use ssh2::{OpenFlags, OpenType, Session, Sftp};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

/// 固定块大小：续传时回退到块边界，避免“半块”脏数据。断点文件按此粒度记录，改动会让旧断点失效。
pub const CHUNK_SIZE: u64 = 1024 * 1024;
pub const HASH_READ_BUFFER_SIZE: usize = 256 * 1024;
/// 退避等待期间检查取消标志的间隔：足够短，用户点取消后不会明显迟滞。
const BACKOFF_POLL_MS: u64 = 200;

/// 方向相关的错误构造。各方向的错误枚举都有这三类变体，文案仍由各自的 Display 决定。
pub trait TransferError {
    fn cancelled() -> Self;
    fn sftp(detail: String) -> Self;
    fn local_io(detail: String) -> Self;
}

/// 两种断点的共同字段。关联错误类型让通用函数直接返回该方向的错误，调用处的 `?` 无需标注类型。
pub trait TransferCheckpoint {
    type Error: TransferError;
    /// 已写入目标端的字节数（上传为远端、下载为本地），未必都经过校验。
    fn transferred_bytes(&self) -> u64;
    fn verified_bytes(&self) -> u64;
    fn retry_count(&self) -> u32;
    fn set_retry_count(&mut self, count: u32);
    fn mark_failed(&mut self);
}

fn lock_error<E: TransferError>(err: impl std::fmt::Display) -> E {
    E::sftp(err.to_string())
}

/// 临时文件后缀。放在目标同目录（rename 在同一文件系统内才是原子的），并以点开头，
/// 避免出现在常规的目录列表里被误当成正式文件。
const STAGING_SUFFIX: &str = ".resiliss-part";

/// 目标文件对应的临时文件路径：`/opt/app/app.jar` → `/opt/app/.app.jar.resiliss-part`。
/// 上传（远端路径）与下载（本地路径）共用同一命名，用户在两端看到的临时文件一致。
pub fn staging_path(final_path: &str) -> String {
    match final_path.rsplit_once('/') {
        Some((dir, name)) => format!("{dir}/.{name}{STAGING_SUFFIX}"),
        None => format!(".{final_path}{STAGING_SUFFIX}"),
    }
}

/// 是否是本程序的临时文件（只看文件名）。展开文件夹、列远端目录时都要排除：
/// 它们是未完成传输的半截数据，被当成普通文件上传或下载只会制造垃圾。
pub fn is_staging_file(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(STAGING_SUFFIX)
}

pub fn align_down(offset: u64) -> u64 {
    offset - (offset % CHUNK_SIZE)
}

/// 直接对内存字节求 SHA-256：刚传输的块内容已在缓冲区里，与另一端读回结果比对时省去一次读取。
pub fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 分段睡眠而不是一次睡满：退避最长 15 秒，期间用户点取消要能及时生效。
pub fn sleep_backoff<E: TransferError>(cancel_flag: &AtomicBool, backoff_ms: u64) -> Result<(), E> {
    let mut waited = 0_u64;
    while waited < backoff_ms {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(E::cancelled());
        }
        thread::sleep(Duration::from_millis(BACKOFF_POLL_MS));
        waited += BACKOFF_POLL_MS;
    }
    Ok(())
}

/// 取消时同时把断点标记为失败：断点仍保留进度，下次可继续，但不能被当成「仍在进行」。
pub fn ensure_not_cancelled<C: TransferCheckpoint>(
    cancel_flag: &AtomicBool,
    checkpoint: &Mutex<C>,
) -> Result<(), C::Error> {
    if cancel_flag.load(Ordering::Relaxed) {
        checkpoint.lock().map_err(lock_error)?.mark_failed();
        return Err(C::Error::cancelled());
    }
    Ok(())
}

pub fn increment_retry_count<C: TransferCheckpoint>(checkpoint: &Mutex<C>) -> Result<u32, C::Error> {
    let mut cp = checkpoint.lock().map_err(lock_error)?;
    let next = cp.retry_count().saturating_add(1);
    cp.set_retry_count(next);
    Ok(next)
}

pub fn current_retry_count<C: TransferCheckpoint>(checkpoint: &Mutex<C>) -> u32 {
    checkpoint.lock().map(|cp| cp.retry_count()).unwrap_or(0)
}

/// 旧版断点没有 verified 字段，只能信任已传字节中块对齐的部分（最后半块可能是脏数据）。
pub fn effective_verified_bytes<C: TransferCheckpoint>(cp: &C) -> u64 {
    if cp.verified_bytes() > 0 {
        return cp.verified_bytes();
    }
    align_down(cp.transferred_bytes())
}

/// 弱网可靠模式下每块都已读回比对过，全部块校验通过时整文件哈希只是重复劳动，可以跳过。
pub fn should_skip_full_file_hash<C: TransferCheckpoint>(
    strict_chunk_verify: &AtomicBool,
    checkpoint: &Mutex<C>,
    file_size: u64,
) -> Result<bool, C::Error> {
    if !strict_chunk_verify.load(Ordering::Relaxed) {
        return Ok(false);
    }
    let cp = checkpoint.lock().map_err(lock_error)?;
    Ok(cp.verified_bytes() == file_size && cp.transferred_bytes() >= file_size)
}

/// 哈希大文件可能要几十秒，期间 SSH 连接上没有任何流量；顺带发 keepalive，
/// 防止 NAT/防火墙或服务器 ClientAlive 把空闲连接掐掉，导致随后读远端时又得重连。
pub fn sha256_local_file<C: TransferCheckpoint>(
    session: &Session,
    path: &Path,
    cancel_flag: &AtomicBool,
    checkpoint: &Mutex<C>,
) -> Result<[u8; 32], C::Error> {
    let mut file = File::open(path).map_err(|e| C::Error::local_io(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    loop {
        ensure_not_cancelled(cancel_flag, checkpoint)?;
        // keepalive_send 内部按 set_keepalive 的间隔节流，每轮调用并不会真的每次发包。
        if let Err(e) = session.keepalive_send() {
            log::debug!("keepalive during local hash failed: {e}");
        }
        let read_bytes = file
            .read(&mut buffer)
            .map_err(|e| C::Error::local_io(e.to_string()))?;
        if read_bytes == 0 {
            break;
        }
        hasher.update(&buffer[..read_bytes]);
    }
    Ok(hasher.finalize().into())
}

/// 只哈希前 `size` 字节：以本地/断点记录的大小为准，远端若多出尾部数据会在大小校验处暴露。
pub fn sha256_remote_file<C: TransferCheckpoint>(
    sftp: &Sftp,
    remote_path: &str,
    size: u64,
    cancel_flag: &AtomicBool,
    checkpoint: &Mutex<C>,
) -> Result<[u8; 32], C::Error> {
    let mut remote_file = sftp
        .open_mode(Path::new(remote_path), OpenFlags::READ, 0, OpenType::File)
        .map_err(|e| C::Error::sftp(e.to_string()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_READ_BUFFER_SIZE];
    let mut remaining = size;
    while remaining > 0 {
        ensure_not_cancelled(cancel_flag, checkpoint)?;
        let to_read = (buffer.len() as u64).min(remaining) as usize;
        let read_bytes = remote_file
            .read(&mut buffer[..to_read])
            .map_err(|e| C::Error::sftp(e.to_string()))?;
        if read_bytes == 0 {
            return Err(C::Error::sftp(
                crate::tr!(
                    "读取远端文件用于校验时意外结束",
                    "Remote file ended unexpectedly while reading for verification"
                )
                .into(),
            ));
        }
        hasher.update(&buffer[..read_bytes]);
        remaining -= read_bytes as u64;
    }
    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_down_snaps_to_chunk_start() {
        assert_eq!(align_down(0), 0);
        assert_eq!(align_down(CHUNK_SIZE - 1), 0);
        assert_eq!(align_down(3 * CHUNK_SIZE + 7), 3 * CHUNK_SIZE);
    }

    #[test]
    fn staging_path_is_hidden_sibling() {
        assert_eq!(staging_path("/opt/app/app.jar"), "/opt/app/.app.jar.resiliss-part");
        assert_eq!(staging_path("app.jar"), ".app.jar.resiliss-part");
    }

    #[test]
    fn staging_files_are_recognized_by_name() {
        assert!(is_staging_file(".app.jar.resiliss-part"));
        assert!(!is_staging_file("app.jar"));
        assert!(!is_staging_file("app.jar.resiliss-part"));
    }

    #[test]
    fn hex_encode_is_lowercase() {
        assert_eq!(hex_encode(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn sleep_backoff_returns_immediately_when_cancelled() {
        #[derive(Debug, PartialEq)]
        struct E(&'static str);
        impl TransferError for E {
            fn cancelled() -> Self { E("cancelled") }
            fn sftp(_: String) -> Self { E("sftp") }
            fn local_io(_: String) -> Self { E("io") }
        }
        let flag = AtomicBool::new(true);
        assert_eq!(sleep_backoff::<E>(&flag, 60_000), Err(E("cancelled")));
    }
}
