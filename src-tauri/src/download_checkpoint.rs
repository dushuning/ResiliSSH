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
    /// 是否先写同目录的临时文件、校验通过后再换成正式文件。旧版本留下的断点没有这个字段（false），
    /// 它们的半截数据就在正式文件里，必须按原方式续传完。
    #[serde(default)]
    pub staged: bool,
}

pub fn checkpoint_path() -> PathBuf {
    crate::storage::app_data_dir().join(CHECKPOINT_FILE)
}

pub fn save_download_checkpoint(checkpoint: &DownloadCheckpoint) -> Result<(), String> {
    let path = checkpoint_path();
    let json = serde_json::to_string_pretty(checkpoint).map_err(|e| e.to_string())?;
    // 原子写：断点每块都要落盘，若写到一半崩溃会丢失全部续传进度。
    crate::storage::atomic_write(&path, json.as_bytes())
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
