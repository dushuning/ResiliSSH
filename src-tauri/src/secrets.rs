use keyring::Entry;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 钥匙串服务名与 bundle identifier 保持一致，用户在「钥匙串访问」里能直接认出归属。
const KEYCHAIN_SERVICE: &str = "com.resiliss.desktop";

/// 连接配置里需要保密的字段种类；account 名按「profile id + 种类」区分，
/// 避免同一连接的密码与私钥口令互相覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    Password,
    KeyPassphrase,
}

impl SecretKind {
    fn account_suffix(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::KeyPassphrase => "key-passphrase",
        }
    }
}

/// 进程内缓存（含「条目不存在」）：macOS 对每个钥匙串条目的每次读取都要求授权，
/// 用户只点「允许」时下一次读取还会再弹窗。缓存后每个条目每次运行最多授权一次。
type SecretCache = Mutex<HashMap<(String, SecretKind), Option<String>>>;

fn cache() -> &'static SecretCache {
    static CACHE: OnceLock<SecretCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember(profile_id: &str, kind: SecretKind, value: Option<String>) {
    if let Ok(mut map) = cache().lock() {
        map.insert((profile_id.to_string(), kind), value);
    }
}

fn entry(profile_id: &str, kind: SecretKind) -> Result<Entry, String> {
    let account = format!("profile:{profile_id}:{}", kind.account_suffix());
    Entry::new(KEYCHAIN_SERVICE, &account).map_err(|e| format!("钥匙串不可用: {e}"))
}

/// 写入钥匙串而不是 profiles.json：JSON 文件可被同机其他进程/用户读取，
/// 而钥匙串条目受系统加密与访问控制保护。
pub fn store_secret(profile_id: &str, kind: SecretKind, value: &str) -> Result<(), String> {
    entry(profile_id, kind)?
        .set_password(value)
        .map_err(|e| {
            log::error!("keychain write failed: profile={profile_id} kind={kind:?} err={e}");
            format!("写入钥匙串失败: {e}")
        })?;
    remember(profile_id, kind, Some(value.to_string()));
    Ok(())
}

/// 读取失败（含条目不存在）一律返回 None：缺失密码只是让用户重新输入，
/// 不应阻断列出连接配置。
pub fn load_secret(profile_id: &str, kind: SecretKind) -> Option<String> {
    if let Some(hit) = cache()
        .lock()
        .ok()
        .and_then(|map| map.get(&(profile_id.to_string(), kind)).cloned())
    {
        return hit;
    }
    let entry = entry(profile_id, kind).ok()?;
    match entry.get_password() {
        Ok(value) => {
            remember(profile_id, kind, Some(value.clone()));
            Some(value)
        }
        Err(keyring::Error::NoEntry) => {
            remember(profile_id, kind, None);
            None
        }
        // 用户点了「拒绝」等错误不缓存：下次用户主动选择该服务器时允许再次请求授权。
        Err(e) => {
            log::warn!("keychain read failed: profile={profile_id} kind={kind:?} err={e}");
            None
        }
    }
}

/// 删除连接或切换为不需要该密钥的认证方式时调用；条目本就不存在视为成功。
pub fn delete_secret(profile_id: &str, kind: SecretKind) {
    let Ok(entry) = entry(profile_id, kind) else {
        return;
    };
    remember(profile_id, kind, None);
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(e) => log::warn!("keychain delete failed: profile={profile_id} kind={kind:?} err={e}"),
    }
}
