use crate::checkpoint::AuthKind;
use crate::secrets::{delete_secret, load_secret, store_secret, SecretKind};
use crate::storage::{new_id, read_json_file, write_json_file};
use serde::{Deserialize, Serialize};

const PROFILES_FILE: &str = "profiles.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_kind: AuthKind,
    pub key_path: Option<String>,
    /// 磁盘上恒为 None（真实值在钥匙串，按需通过 `profile_secrets` 读取）。
    /// 保留 `serde(default)` 以便读取旧版本写入的明文字段并迁移。
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub key_passphrase: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileStore {
    profiles: Vec<ConnectionProfile>,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
        }
    }
}

fn load_store() -> Result<ProfileStore, String> {
    let mut store: ProfileStore = match read_json_file(PROFILES_FILE) {
        Ok(store) => store,
        Err(err) if err.contains("not found") => return Ok(ProfileStore::default()),
        Err(err) => return Err(err),
    };
    if migrate_plaintext_secrets(&mut store) {
        save_store(&store)?;
    }
    Ok(store)
}

/// v1.0.0 把密码明文写在 profiles.json 里；加载时把残留明文搬进钥匙串并从文件抹掉。
/// 单条迁移失败时保留该明文，宁可暂不迁移也不能让用户丢失已保存的密码。
/// 返回是否有字段被迁移（需要回写文件）。
fn migrate_plaintext_secrets(store: &mut ProfileStore) -> bool {
    let mut migrated = false;
    for profile in &mut store.profiles {
        for (kind, slot) in [
            (SecretKind::Password, &mut profile.password),
            (SecretKind::KeyPassphrase, &mut profile.key_passphrase),
        ] {
            let Some(value) = slot.as_deref().filter(|v| !v.is_empty()) else {
                continue;
            };
            match store_secret(&profile.id, kind, value) {
                Ok(()) => {
                    log::info!("migrated plaintext {kind:?} to keychain: profile={}", profile.id);
                    *slot = None;
                    migrated = true;
                }
                Err(err) => log::warn!(
                    "keep plaintext {kind:?} (keychain migration failed): profile={} err={err}",
                    profile.id
                ),
            }
        }
    }
    migrated
}

/// 只在用户真正选中某个服务器时读取它的密钥，而不是列出列表时读全部：
/// 每读一个钥匙串条目 macOS 都可能弹一次授权框，批量读取会连续弹出一串。
#[derive(Debug, Clone, Serialize)]
pub struct ProfileSecrets {
    pub password: Option<String>,
    pub key_passphrase: Option<String>,
}

pub fn profile_secrets(id: &str) -> ProfileSecrets {
    ProfileSecrets {
        password: load_secret(id, SecretKind::Password),
        key_passphrase: load_secret(id, SecretKind::KeyPassphrase),
    }
}

/// 把密钥写进钥匙串；值为空则删除旧条目，避免改用私钥后旧密码仍残留在钥匙串里。
fn persist_secret(profile_id: &str, kind: SecretKind, value: Option<&str>) -> Result<(), String> {
    match value {
        Some(v) => store_secret(profile_id, kind, v),
        None => {
            delete_secret(profile_id, kind);
            Ok(())
        }
    }
}

fn save_store(store: &ProfileStore) -> Result<(), String> {
    write_json_file(PROFILES_FILE, store)
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

pub fn list_profiles() -> Result<Vec<ConnectionProfile>, String> {
    let store = load_store()?;
    let mut profiles = store.profiles;
    profiles.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(profiles)
}

#[derive(Debug, Deserialize)]
pub struct SaveProfileRequest {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_type: String,
    pub key_path: Option<String>,
    pub password: Option<String>,
    pub key_passphrase: Option<String>,
}

fn expand_key_path(path: &str) -> String {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped).to_string_lossy().to_string();
        }
    }
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().to_string();
        }
    }
    path.to_string()
}

pub fn save_profile(request: SaveProfileRequest) -> Result<ConnectionProfile, String> {
    let name = request.name.trim();
    let host = request.host.trim();
    let username = request.username.trim();

    if name.is_empty() {
        return Err("请填写连接名称".into());
    }
    if host.is_empty() {
        return Err("请填写主机".into());
    }
    if username.is_empty() {
        return Err("请填写用户名".into());
    }

    let auth_kind = match request.auth_type.as_str() {
        "password" => AuthKind::Password,
        "key" => AuthKind::PrivateKey,
        _ => return Err("未知认证方式".into()),
    };

    if matches!(auth_kind, AuthKind::PrivateKey) {
        let key_path = request
            .key_path
            .as_ref()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| "私钥认证需要填写私钥路径".to_string())?;
        if !std::path::Path::new(&expand_key_path(key_path)).exists() {
            return Err(format!("私钥文件不存在: {key_path}"));
        }
    }

    let mut store = load_store()?;
    let existing = store.profiles.iter().find(|p| p.name == name).cloned();
    let profile_id = existing
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_else(new_id);

    // 表单留空表示「沿用已保存的值」，所以回退到该连接现有的钥匙串条目。
    let password = non_empty(request.password).or_else(|| {
        existing
            .as_ref()
            .and_then(|p| load_secret(&p.id, SecretKind::Password))
            .filter(|p| !p.is_empty())
    });
    let key_passphrase = non_empty(request.key_passphrase).or_else(|| {
        existing
            .as_ref()
            .and_then(|p| load_secret(&p.id, SecretKind::KeyPassphrase))
            .filter(|p| !p.is_empty())
    });

    if matches!(auth_kind, AuthKind::Password) && password.is_none() {
        return Err("密码登录保存时请填写密码".into());
    }

    let created_at = existing.map(|p| p.created_at).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    });

    // 先写钥匙串再写 JSON：钥匙串失败时整个保存失败，绝不退回明文落盘。
    persist_secret(&profile_id, SecretKind::Password, password.as_deref())?;
    persist_secret(&profile_id, SecretKind::KeyPassphrase, key_passphrase.as_deref())?;

    let profile = ConnectionProfile {
        id: profile_id,
        name: name.to_string(),
        host: host.to_string(),
        port: request.port,
        username: username.to_string(),
        auth_kind,
        key_path: request.key_path.filter(|p| !p.trim().is_empty()),
        password: None,
        key_passphrase: None,
        created_at,
    };

    store.profiles.retain(|p| p.name != profile.name);
    store.profiles.push(profile.clone());
    save_store(&store)?;
    Ok(ConnectionProfile {
        password,
        key_passphrase,
        ..profile
    })
}

pub fn delete_profile(id: String) -> Result<(), String> {
    let mut store = load_store()?;
    let before = store.profiles.len();
    store.profiles.retain(|p| p.id != id);
    if store.profiles.len() == before {
        return Err("连接配置不存在".into());
    }
    save_store(&store)?;
    delete_secret(&id, SecretKind::Password);
    delete_secret(&id, SecretKind::KeyPassphrase);
    Ok(())
}
