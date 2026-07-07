use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const CHECKPOINT_FILE: &str = "upload-checkpoint.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    Password,
    PrivateKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadCheckpoint {
    pub local_path: String,
    pub remote_path: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_kind: AuthKind,
    pub key_path: Option<String>,
    pub file_size: u64,
    #[serde(default)]
    pub local_mtime: Option<u64>,
    pub uploaded_bytes: u64,
    /// 已通过逐块 SHA-256 校验的安全续传位置（仅从此处之后续传）。
    #[serde(default)]
    pub verified_bytes: u64,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub failure_reason: Option<String>,
    pub status: CheckpointStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    InProgress,
    Failed,
    Completed,
}

impl Default for UploadCheckpoint {
    fn default() -> Self {
        Self {
            local_path: String::new(),
            remote_path: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            auth_kind: AuthKind::Password,
            key_path: None,
            file_size: 0,
            local_mtime: None,
            uploaded_bytes: 0,
            verified_bytes: 0,
            retry_count: 0,
            failure_reason: None,
            status: CheckpointStatus::InProgress,
        }
    }
}

pub fn checkpoint_path() -> PathBuf {
    crate::storage::app_data_dir().join(CHECKPOINT_FILE)
}

pub fn file_fingerprint(path: &Path) -> Result<(u64, u64), String> {
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok((meta.len(), mtime))
}

/// 断点文件让应用重启后仍可续传，避免大文件因中断从头再来。
pub fn save_checkpoint(checkpoint: &UploadCheckpoint) -> Result<(), String> {
    let path = checkpoint_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(checkpoint).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

pub fn load_checkpoint() -> Result<Option<UploadCheckpoint>, String> {
    let path = checkpoint_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let checkpoint: UploadCheckpoint = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    Ok(Some(checkpoint))
}

pub fn clear_checkpoint() -> Result<(), String> {
    let path = checkpoint_path();
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}
