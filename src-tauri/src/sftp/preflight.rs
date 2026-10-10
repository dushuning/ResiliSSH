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

/// f_frsize 是 f_bavail 的计量单位；个别实现填 0，此时退回 f_bsize。
/// （ssh2 未公开 statvfs 结构体的类型名，所以按字段传入。）
fn available_bytes(frsize: u64, bsize: u64, bavail: u64) -> Option<u64> {
    let unit = if frsize > 0 { frsize } else { bsize };
    (unit > 0).then(|| bavail.saturating_mul(unit))
}

/// 远端目标文件所在文件系统的可用字节数（普通用户可用的部分）。依次尝试：
/// 1. 目标文件已存在时，对文件句柄做 fstatvfs——OpenSSH 只支持对文件句柄查询，目录句柄会失败；
/// 2. 在服务器上执行 `df -Pk`，对象是最近一个已存在的上级目录（文件夹上传的子目录往往还没建）；
/// 3. 目录句柄 fstatvfs（部分非 OpenSSH 服务器支持）。
pub fn remote_free_bytes(session: &Session, sftp: &Sftp, remote_file: &str) -> Option<u64> {
    if let Ok(mut file) = sftp.open(Path::new(remote_file)) {
        if let Some(free) = file
            .statvfs()
            .ok()
            .and_then(|v| available_bytes(v.f_frsize, v.f_bsize, v.f_bavail))
        {
            return Some(free);
        }
    }
    let dir = nearest_existing_dir(sftp, remote_file)?;
    if let Some(free) = df_available(session, &dir) {
        return Some(free);
    }
    let mut handle = sftp.opendir(Path::new(&dir)).ok()?;
    let vfs = handle.statvfs().ok()?;
    available_bytes(vfs.f_frsize, vfs.f_bsize, vfs.f_bavail)
}

/// 最多向上找几级：再往上通常已经是挂载点之外，得到的空间信息没有参考意义。
const MAX_ANCESTOR_LOOKUP: usize = 8;

fn nearest_existing_dir(sftp: &Sftp, remote_file: &str) -> Option<String> {
    Path::new(remote_file)
        .ancestors()
        .skip(1)
        .take(MAX_ANCESTOR_LOOKUP)
        .map(|p| p.to_string_lossy().to_string())
        .map(|p| if p.is_empty() { ".".to_string() } else { p })
        .find(|p| sftp.stat(Path::new(p)).is_ok_and(|st| st.is_dir()))
}

fn df_available(session: &Session, dir: &str) -> Option<u64> {
    let mut channel = session.channel_session().ok()?;
    channel.exec(&format!("df -Pk -- {} 2>/dev/null", shell_quote(dir))).ok()?;
    let mut output = String::new();
    channel.read_to_string(&mut output).ok()?;
    let _ = channel.wait_close();
    parse_df_available_kb(&output).map(|kb| kb.saturating_mul(1024))
}

/// 解析 `df -Pk` 的「可用」列（单位 KB）。从使用率列（以 % 结尾）往前取一列，
/// 而不是按固定下标：文件系统名里可能有空格，固定下标会取错。
fn parse_df_available_kb(output: &str) -> Option<u64> {
    let line = output.lines().rev().find(|l| !l.trim().is_empty())?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    let pct = fields.iter().rposition(|f| f.ends_with('%'))?;
    fields.get(pct.checked_sub(1)?)?.parse().ok()
}

pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// 本地目标文件所在文件系统的可用字节数（普通用户可用的部分），用于下载前预检。
/// 目标及其上级目录可能还不存在（下载会自动创建），所以对最近一个已存在的上级目录查询。
pub fn local_free_bytes(local_file: &Path) -> Option<u64> {
    let dir = local_file
        .ancestors()
        .skip(1)
        .take(MAX_ANCESTOR_LOOKUP)
        .find(|p| p.is_dir())?;
    let c_path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).ok()?;
    let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: c_path 是以 NUL 结尾的有效路径，vfs 指向足够大的未初始化缓冲区；
    // 只有返回 0（成功）时才读取它，此时内核已完整填写该结构体。
    let vfs = unsafe {
        if libc::statvfs(c_path.as_ptr(), vfs.as_mut_ptr()) != 0 {
            return None;
        }
        vfs.assume_init()
    };
    #[allow(clippy::unnecessary_cast)] // 字段宽度因平台而异（macOS 上 f_bavail 是 u32）。
    available_bytes(vfs.f_frsize as u64, vfs.f_bsize as u64, vfs.f_bavail as u64)
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
    fn parses_df_available_column() {
        let linux = "Filesystem     1024-blocks    Used Available Capacity Mounted on\n/dev/sda1  41152812 9876543  29152345      26% /\n";
        assert_eq!(parse_df_available_kb(linux), Some(29_152_345));
        let spaced = "Filesystem 1024-blocks Used Available Capacity Mounted on\nmy disk 100 40 60 40% /Volumes/my disk\n";
        assert_eq!(parse_df_available_kb(spaced), Some(60));
        assert_eq!(parse_df_available_kb("df: /nope: No such file or directory"), None);
    }

    #[test]
    fn local_free_space_found_for_missing_subdirectory() {
        let missing = std::env::temp_dir().join("resiliss-no-such-dir/a/b/file.bin");
        assert!(local_free_bytes(&missing).is_some_and(|free| free > 0));
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
