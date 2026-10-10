//! 上传前的预检：远端剩余空间、远端是否已有完全相同的文件。
//!
//! 两项检查都是「尽力而为」：服务器不支持（如不提供 statvfs 扩展、只允许 SFTP 不允许执行命令）
//! 时返回 None，调用方按原有流程继续，绝不能因为预检本身失败而阻止传输。

use sha2::{Digest, Sha256};
use ssh2::{Session, Sftp};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const HASH_BUFFER_BYTES: usize = 1024 * 1024;
const SHA256_HEX_LEN: usize = 64;

/// 远端目录所在文件系统的可用字节数（普通用户可用的部分，即 f_bavail）。
pub fn remote_free_bytes(sftp: &Sftp, dir: &str) -> Option<u64> {
    let mut handle = sftp.opendir(Path::new(dir)).ok()?;
    let vfs = handle.statvfs().ok()?;
    // f_frsize 是 f_bavail 的计量单位；个别实现填 0，此时退回 f_bsize。
    let unit = if vfs.f_frsize > 0 { vfs.f_frsize } else { vfs.f_bsize };
    if unit == 0 {
        return None;
    }
    Some(vfs.f_bavail.saturating_mul(unit))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// 在服务器上直接算 SHA-256：只回传 64 个字符，不必把整个文件读回本地，弱网下也很快。
/// Linux 一般有 sha256sum，macOS/BSD 是 shasum -a 256；都没有或无执行权限时返回 None。
pub fn remote_sha256(session: &Session, remote_path: &str) -> Option<String> {
    let quoted = shell_quote(remote_path);
    let command = format!(
        "sha256sum -- {quoted} 2>/dev/null || shasum -a 256 -- {quoted} 2>/dev/null"
    );
    let mut channel = session.channel_session().ok()?;
    channel.exec(&command).ok()?;
    let mut output = String::new();
    channel.read_to_string(&mut output).ok()?;
    let _ = channel.wait_close();
    parse_sha256_output(&output)
}

fn parse_sha256_output(output: &str) -> Option<String> {
    let first = output.split_whitespace().next()?;
    (first.len() == SHA256_HEX_LEN && first.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| first.to_ascii_lowercase())
}

/// 本地哈希缓存键：路径 + 大小 + 修改时间，任一变化即视为不同文件。
type HashKey = (String, u64, Option<SystemTime>);

fn local_hash_cache() -> &'static Mutex<HashMap<HashKey, String>> {
    static CACHE: OnceLock<Mutex<HashMap<HashKey, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 本地文件的 SHA-256（小写十六进制）。预检会在路径输入框失焦时反复触发，
/// 大文件每次重算要好几秒，所以按「路径+大小+修改时间」缓存。
pub fn local_sha256(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    let key = (path.to_string_lossy().to_string(), meta.len(), meta.modified().ok());
    if let Some(hit) = local_hash_cache().lock().ok()?.get(&key).cloned() {
        return Some(hit);
    }
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_BYTES];
    loop {
        let n = file.read(&mut buffer).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if let Ok(mut cache) = local_hash_cache().lock() {
        cache.insert(key, hex.clone());
    }
    Some(hex)
}

/// 还需要多少远端空间：已在远端的字节会被续传或原地覆盖，不额外占用。
pub fn bytes_needed(local_size: u64, remote_existing: u64) -> u64 {
    local_size.saturating_sub(remote_existing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sha256sum_and_shasum_output() {
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(parse_sha256_output(&format!("{hash}  /tmp/a.jar\n")).as_deref(), Some(hash));
        assert_eq!(
            parse_sha256_output(&format!("{}  x\n", hash.to_uppercase())).as_deref(),
            Some(hash)
        );
    }

    #[test]
    fn rejects_unexpected_output() {
        assert_eq!(parse_sha256_output(""), None);
        assert_eq!(parse_sha256_output("sh: sha256sum: command not found"), None);
        assert_eq!(parse_sha256_output("abc123  file"), None);
    }

    #[test]
    fn existing_remote_bytes_reduce_space_needed() {
        assert_eq!(bytes_needed(100, 0), 100);
        assert_eq!(bytes_needed(100, 40), 60);
        assert_eq!(bytes_needed(100, 150), 0);
    }

    #[test]
    fn local_hash_matches_known_vector_and_is_cached() {
        let dir = std::env::temp_dir().join("resiliss-preflight-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.txt");
        std::fs::write(&path, b"abc").unwrap();
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(local_sha256(&path).as_deref(), Some(expected));
        assert_eq!(local_sha256(&path).as_deref(), Some(expected));
    }
}
