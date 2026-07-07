use crate::checkpoint::AuthKind;
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
    match read_json_file(PROFILES_FILE) {
        Ok(store) => Ok(store),
        Err(err) if err.contains("not found") => Ok(ProfileStore::default()),
        Err(err) => Err(err),
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

    let password = non_empty(request.password).or_else(|| {
        existing
            .as_ref()
            .and_then(|p| p.password.clone())
            .filter(|p| !p.is_empty())
    });
    let key_passphrase = non_empty(request.key_passphrase).or_else(|| {
        existing
            .as_ref()
            .and_then(|p| p.key_passphrase.clone())
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

    let profile = ConnectionProfile {
        id: profile_id,
        name: name.to_string(),
        host: host.to_string(),
        port: request.port,
        username: username.to_string(),
        auth_kind,
        key_path: request.key_path.filter(|p| !p.trim().is_empty()),
        password,
        key_passphrase,
        created_at,
    };

    store.profiles.retain(|p| p.name != profile.name);
    store.profiles.push(profile.clone());
    save_store(&store)?;
    Ok(profile)
}

pub fn delete_profile(id: String) -> Result<(), String> {
    let mut store = load_store()?;
    let before = store.profiles.len();
    store.profiles.retain(|p| p.id != id);
    if store.profiles.len() == before {
        return Err("连接配置不存在".into());
    }
    save_store(&store)
}
