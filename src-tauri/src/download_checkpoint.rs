use crate::checkpoint::{AuthKind, CheckpointStatus};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

const CHECKPOINT_FILE: &str = "download-checkpoint.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadCheckpoint {
    pub local_path: String,
    pub remote_path: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth_kind: AuthKind,
    pub key_path: Option<String>,
    pub file_size: u64,
    #[serde(default)]
    pub remote_mtime: Option<u64>,
    pub downloaded_bytes: u64,
    #[serde(default)]
    pub verified_bytes: u64,
    #[serde(default)]
    pub retry_count: u32,
    #[serde(default)]
    pub failure_reason: Option<String>,
    pub status: CheckpointStatus,
}

pub fn checkpoint_path() -> PathBuf {
    crate::storage::app_data_dir().join(CHECKPOINT_FILE)
}

pub fn save_download_checkpoint(checkpoint: &DownloadCheckpoint) -> Result<(), String> {
    let path = checkpoint_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(checkpoint).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

pub fn load_download_checkpoint() -> Result<Option<DownloadCheckpoint>, String> {
    let path = checkpoint_path();
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let checkpoint: DownloadCheckpoint = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    Ok(Some(checkpoint))
}

pub fn clear_download_checkpoint() -> Result<(), String> {
    let path = checkpoint_path();
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}
