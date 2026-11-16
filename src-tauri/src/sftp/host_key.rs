use super::client::ConnectError;
use crate::storage::app_data_dir;
use serde::Serialize;
use ssh2::{CheckResult, HashType, HostKeyType, KnownHostFileKind, KnownHosts, MethodType, Session};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// 应用自己确认过的主机写到独立文件，而不是改写 ~/.ssh/known_hosts：
/// libssh2 无法解析的行（证书、sk-* 密钥等）若经它回写会被丢掉，不能冒险破坏用户的 ssh 配置。
const APP_KNOWN_HOSTS_FILE: &str = "known_hosts";
/// known_hosts 里记录的是信任关系，泄露会暴露用户连过哪些服务器，按 ssh 惯例仅本人可读写。
const KNOWN_HOSTS_FILE_MODE: u32 = 0o600;
/// OpenSSH 对默认端口只写主机名，非默认端口写成 `[host]:port`。
const DEFAULT_SSH_PORT: u16 = 22;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyStatus {
    Trusted,
    Unknown,
    Changed,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostKeyInfo {
    pub status: HostKeyStatus,
    /// 与 `ssh-keygen -lf` 相同的 `SHA256:<base64>` 格式，方便用户与服务器管理员核对。
    pub fingerprint: String,
    pub key_type: String,
}

pub fn app_known_hosts_path() -> PathBuf {
    app_data_dir().join(APP_KNOWN_HOSTS_FILE)
}

fn user_known_hosts_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh").join("known_hosts"))
}

/// 用户的 known_hosts 与应用自己的 known_hosts 中的有效行（去掉空行和注释）。
fn known_hosts_lines() -> Vec<String> {
    let sources = [user_known_hosts_path(), Some(app_known_hosts_path())];
    sources
        .into_iter()
        .flatten()
        .filter_map(|path| fs::read_to_string(path).ok())
        .flat_map(|content| {
            content
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// 逐行喂给 libssh2 而不用 read_file：read_file 遇到第一行无法解析的条目就整体中止，
/// 会导致后续合法条目全部丢失、已信任主机被误报为未知。
fn load_lines<'a>(
    session: &Session,
    lines: impl IntoIterator<Item = &'a str>,
) -> Result<KnownHosts, ConnectError> {
    let mut known_hosts = session
        .known_hosts()
        .map_err(|e| ConnectError::HostKeyCheck(e.to_string()))?;
    let mut skipped = 0_usize;
    for line in lines {
        if known_hosts.read_str(line, KnownHostFileKind::OpenSSH).is_err() {
            skipped += 1;
        }
    }
    if skipped > 0 {
        log::debug!("known_hosts: skipped {skipped} unsupported lines");
    }
    Ok(known_hosts)
}

/// 按 OpenSSH 的规则核对：先查这个 host:port 自己的记录；非默认端口没有记录时，
/// 再看不带端口的记录——但只接受「匹配」，不匹配不算数。
/// 不直接用 libssh2 的 check_port：它在 `[host]:port` 没有记录时会回落到裸主机名并把不一致
/// 报成「指纹已变化」，而同一主机名的另一个端口完全可能是另一台机器（端口转发、容器）。
fn check_known_host(known_hosts: &KnownHosts, host: &str, port: u16, key: &[u8]) -> CheckResult {
    match known_hosts.check(&known_hosts_host_field(host, port), key) {
        CheckResult::NotFound if port != DEFAULT_SSH_PORT => match known_hosts.check(host, key) {
            CheckResult::Match => CheckResult::Match,
            _ => CheckResult::NotFound,
        },
        result => result,
    }
}

/// 探测某类记录里是否有这台主机时用的占位密钥：只关心「有没有记录」，不关心是否匹配。
const PROBE_KEY: &[u8] = b"probe";
/// 协商主机密钥时的完整候选（libssh2 支持的算法，按其默认优先级）。已有记录的类型会被挪到最前。
const HOST_KEY_ALGORITHMS: &[(&str, &[&str])] = &[
    ("ssh-ed25519", &["ssh-ed25519"]),
    ("ecdsa-sha2-nistp256", &["ecdsa-sha2-nistp256"]),
    ("ecdsa-sha2-nistp384", &["ecdsa-sha2-nistp384"]),
    ("ecdsa-sha2-nistp521", &["ecdsa-sha2-nistp521"]),
    // 同一把 RSA 密钥对应三种签名算法，记录里的类型名都是 ssh-rsa。
    ("ssh-rsa", &["rsa-sha2-512", "rsa-sha2-256", "ssh-rsa"]),
];

/// 已有记录的密钥类型排在前面的协商顺序；这台主机没有任何记录时返回 None（保持 libssh2 默认）。
fn preferred_host_key_algorithms(session: &Session, lines: &[String], host: &str, port: u16) -> Option<String> {
    let known_types: Vec<&str> = HOST_KEY_ALGORITHMS
        .iter()
        .map(|(key_type, _)| *key_type)
        .filter(|key_type| {
            // known_hosts 行的第二个字段是密钥类型；只装入这一类的行，再问 libssh2 里面有没有这台主机。
            let same_type = lines
                .iter()
                .map(String::as_str)
                .filter(|line| line.split_whitespace().nth(1) == Some(*key_type));
            load_lines(session, same_type)
                .is_ok_and(|known| !matches!(check_known_host(&known, host, port, PROBE_KEY), CheckResult::NotFound))
        })
        .collect();
    if known_types.is_empty() {
        return None;
    }
    let (known, others): (Vec<_>, Vec<_>) =
        HOST_KEY_ALGORITHMS.iter().partition(|(key_type, _)| known_types.contains(key_type));
    let order: Vec<&str> = known.iter().chain(&others).flat_map(|(_, algorithms)| algorithms.iter().copied()).collect();
    Some(order.join(","))
}

/// 握手前调用：让服务器优先出示我们已有记录的那种主机密钥（与 OpenSSH 的做法一致）。
/// 否则 known_hosts 里只有 ecdsa / rsa 记录、而协商默认选了 ed25519 时，一台没变过的服务器
/// 会被报成「指纹已变化」，而且这种状态在应用内无法解除。
pub fn prefer_known_host_key_types(session: &Session, host: &str, port: u16) {
    let Some(order) = preferred_host_key_algorithms(session, &known_hosts_lines(), host, port) else {
        return;
    };
    if let Err(e) = session.method_pref(MethodType::HostKey, &order) {
        log::warn!("cannot set host key algorithm preference for {host}:{port} ({order}): {e}");
    }
}

fn key_type_name(key_type: HostKeyType) -> Option<&'static str> {
    match key_type {
        HostKeyType::Rsa => Some("ssh-rsa"),
        HostKeyType::Dss => Some("ssh-dss"),
        HostKeyType::Ecdsa256 => Some("ecdsa-sha2-nistp256"),
        HostKeyType::Ecdsa384 => Some("ecdsa-sha2-nistp384"),
        HostKeyType::Ecdsa521 => Some("ecdsa-sha2-nistp521"),
        HostKeyType::Ed25519 => Some("ssh-ed25519"),
        HostKeyType::Unknown => None,
    }
}

/// 只为生成指纹和 known_hosts 行引入一个 base64 依赖不划算，这里手写标准字母表编码。
fn base64_encode(bytes: &[u8], pad: bool) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const SEXTET_MASK: u32 = 0x3f;
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        // 3 字节输入产出 4 个字符；不足 3 字节时只有前 len+1 个字符有效。
        let emitted = chunk.len() + 1;
        for i in 0..4 {
            if i < emitted {
                let index = (n >> (18 - 6 * i)) & SEXTET_MASK;
                out.push(ALPHABET[index as usize] as char);
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

fn known_hosts_host_field(host: &str, port: u16) -> String {
    if port == DEFAULT_SSH_PORT {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// 与 OpenSSH 相同的 `host keytype base64(blob)` 行格式，这样用户需要时可以直接拷到 ~/.ssh/known_hosts。
fn known_hosts_line(host: &str, port: u16, type_name: &str, key: &[u8]) -> String {
    format!(
        "{} {type_name} {}\n",
        known_hosts_host_field(host, port),
        base64_encode(key, true)
    )
}

/// 握手后、发送任何凭据之前调用：在这一步拒绝冒充者，密码才不会被中间人拿到。
pub fn inspect_host_key(session: &Session, host: &str, port: u16) -> Result<HostKeyInfo, ConnectError> {
    let (key, key_type) = session
        .host_key()
        .ok_or_else(|| ConnectError::HostKeyCheck(crate::tr!("服务器未提供主机密钥", "The server did not provide a host key").into()))?;
    let digest = session
        .host_key_hash(HashType::Sha256)
        .ok_or_else(|| ConnectError::HostKeyCheck(
            crate::tr!("无法计算主机密钥指纹", "Cannot compute the host key fingerprint").into(),
        ))?;
    let fingerprint = format!("SHA256:{}", base64_encode(digest, false));
    let key_type = key_type_name(key_type).unwrap_or("unknown").to_string();

    let lines = known_hosts_lines();
    let known_hosts = load_lines(session, lines.iter().map(String::as_str))?;
    let status = match check_known_host(&known_hosts, host, port, key) {
        CheckResult::Match => HostKeyStatus::Trusted,
        CheckResult::NotFound => HostKeyStatus::Unknown,
        CheckResult::Mismatch => HostKeyStatus::Changed,
        CheckResult::Failure => {
            return Err(ConnectError::HostKeyCheck(
                crate::tr!("known_hosts 校验失败", "known_hosts verification failed").into(),
            ));
        }
    };
    Ok(HostKeyInfo {
        status,
        fingerprint,
        key_type,
    })
}

/// 连接流程中的强制校验：只有已信任的主机才放行，未知/变化都转为永久性错误交给前端处理。
pub fn verify_host_key(session: &Session, host: &str, port: u16) -> Result<(), ConnectError> {
    let info = inspect_host_key(session, host, port)?;
    match info.status {
        HostKeyStatus::Trusted => Ok(()),
        HostKeyStatus::Unknown => {
            log::warn!("unknown host key: host={host}:{port} fp={}", info.fingerprint);
            Err(ConnectError::HostKeyUnknown(info.fingerprint))
        }
        HostKeyStatus::Changed => {
            log::error!("HOST KEY CHANGED: host={host}:{port} fp={}", info.fingerprint);
            Err(ConnectError::HostKeyChanged(info.fingerprint))
        }
    }
}

/// 用户在界面上确认指纹后写入信任。要求传回用户看到的指纹并重新比对，
/// 防止「展示时是 A、写入时已被换成 B」的确认窗口期攻击。
pub fn trust_host_key(
    session: &Session,
    host: &str,
    port: u16,
    expected_fingerprint: &str,
) -> Result<(), String> {
    let info = inspect_host_key(session, host, port).map_err(|e| e.to_string())?;
    if info.fingerprint != expected_fingerprint {
        log::error!(
            "host key changed during confirmation: host={host}:{port} shown={expected_fingerprint} now={}",
            info.fingerprint
        );
        return Err(crate::tr!(
            "确认期间服务器指纹发生了变化，已拒绝信任，请重新连接核对",
            "The server host key fingerprint changed during confirmation; trust refused, please reconnect and check again"
        )
        .into());
    }
    match info.status {
        HostKeyStatus::Trusted => return Ok(()),
        // 指纹变化必须由用户手动清理旧记录，应用内一键覆盖等于替攻击者点了确认。
        HostKeyStatus::Changed => {
            return Err(crate::tr!(
                "该服务器已记录的指纹与当前不一致，请先核实并手动从 known_hosts 中移除旧记录",
                "The recorded host key fingerprint for this server does not match; verify it and remove the old entry from known_hosts manually"
            )
            .into())
        }
        HostKeyStatus::Unknown => {}
    }

    let (key, key_type) = session
        .host_key()
        .ok_or_else(|| crate::tr!("服务器未提供主机密钥", "The server did not provide a host key").to_string())?;
    let type_name = key_type_name(key_type).ok_or_else(|| crate::tr!("不支持的主机密钥类型", "Unsupported host key type").to_string())?;
    let line = known_hosts_line(host, port, type_name, key);

    let path = app_known_hosts_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(KNOWN_HOSTS_FILE_MODE)
        .open(&path)
        .map_err(|e| crate::tr!(format!("写入 known_hosts 失败: {e}"), format!("Failed to write known_hosts: {e}")))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| crate::tr!(format!("写入 known_hosts 失败: {e}"), format!("Failed to write known_hosts: {e}")))?;
    crate::activity::success(
        crate::activity::Category::HostKey,
        crate::tr!(
            format!("已信任 {host}:{port} 的指纹 {type_name} {}", info.fingerprint),
            format!("Trusted host key fingerprint for {host}:{port}: {type_name} {}", info.fingerprint)
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64_encode(b"", true), "");
        assert_eq!(base64_encode(b"f", true), "Zg==");
        assert_eq!(base64_encode(b"fo", true), "Zm8=");
        assert_eq!(base64_encode(b"foo", true), "Zm9v");
        assert_eq!(base64_encode(b"foob", true), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba", false), "Zm9vYmE");
        assert_eq!(base64_encode(b"foobar", false), "Zm9vYmFy");
    }

    /// 构造 OpenSSH 线格式的 ed25519 公钥 blob：string("ssh-ed25519") + string(32 字节公钥)。
    fn ed25519_blob(seed: u8) -> Vec<u8> {
        const ED25519_KEY_LEN: usize = 32;
        let name = b"ssh-ed25519";
        let mut blob = Vec::new();
        blob.extend_from_slice(&(name.len() as u32).to_be_bytes());
        blob.extend_from_slice(name);
        blob.extend_from_slice(&(ED25519_KEY_LEN as u32).to_be_bytes());
        blob.extend((0..ED25519_KEY_LEN as u8).map(|i| i.wrapping_add(seed)));
        blob
    }

    /// 用真实 libssh2 读回我们写出的行，确保 trust_host_key 写下的记录下次连接能被识别。
    #[test]
    fn written_line_round_trips_through_libssh2() {
        let session = Session::new().unwrap();
        let mut known_hosts = session.known_hosts().unwrap();
        let trusted = ed25519_blob(1);
        let other = ed25519_blob(2);
        for (host, port) in [("example.com", 22), ("10.0.0.1", 2222)] {
            let line = known_hosts_line(host, port, "ssh-ed25519", &trusted);
            known_hosts
                .read_str(line.trim_end(), KnownHostFileKind::OpenSSH)
                .unwrap();
        }

        let check = |host: &str, port: u16, key: &[u8]| check_known_host(&known_hosts, host, port, key);
        assert!(matches!(check("example.com", 22, &trusted), CheckResult::Match));
        assert!(matches!(check("example.com", 22, &other), CheckResult::Mismatch));
        assert!(matches!(check("unknown.example", 22, &trusted), CheckResult::NotFound));
        assert!(matches!(check("10.0.0.1", 2222, &trusted), CheckResult::Match));
    }

    /// 同一主机名的非默认端口没有记录时：裸主机名的记录匹配就信任，不匹配只算「未知」，
    /// 不能报成「已变化」——那可能根本是另一台机器。
    #[test]
    fn other_port_without_record_is_unknown_not_changed() {
        let session = Session::new().unwrap();
        let trusted = ed25519_blob(1);
        let other = ed25519_blob(2);
        let line = known_hosts_line("example.com", 22, "ssh-ed25519", &trusted);
        let known_hosts = load_lines(&session, [line.trim_end()]).unwrap();
        assert!(matches!(check_known_host(&known_hosts, "example.com", 2222, &other), CheckResult::NotFound));
        assert!(matches!(check_known_host(&known_hosts, "example.com", 2222, &trusted), CheckResult::Match));
        assert!(matches!(check_known_host(&known_hosts, "example.com", 22, &other), CheckResult::Mismatch));
    }

    #[test]
    fn known_key_type_is_negotiated_first() {
        let session = Session::new().unwrap();
        // 密钥内容在这里无关紧要，只看类型字段。
        let rsa_line = format!("example.com ssh-rsa {}", base64_encode(&ed25519_blob(3), true));
        let lines = vec![rsa_line];
        let order = preferred_host_key_algorithms(&session, &lines, "example.com", 22).unwrap();
        assert!(order.starts_with("rsa-sha2-512,rsa-sha2-256,ssh-rsa,ssh-ed25519"), "{order}");
        assert_eq!(preferred_host_key_algorithms(&session, &lines, "other.example", 22), None);
    }

    #[test]
    fn host_field_brackets_non_default_port() {
        assert_eq!(known_hosts_host_field("example.com", 22), "example.com");
        assert_eq!(known_hosts_host_field("10.0.0.1", 2222), "[10.0.0.1]:2222");
    }
}
