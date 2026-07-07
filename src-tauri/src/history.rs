use crate::storage::{new_id, read_json_file, write_json_file};
use serde::{Deserialize, Serialize};

const HISTORY_FILE: &str = "upload-history.json";
const MAX_HISTORY_ENTRIES: usize = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadHistoryEntry {
    pub id: String,
    pub local_path: String,
    pub remote_path: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub file_size: u64,
    pub status: String,
    pub retry_count: u32,
    pub finished_at: i64,
    pub message: Option<String>,
    #[serde(default = "default_direction")]
    pub direction: String,
}

fn default_direction() -> String {
    "upload".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistoryStore {
    entries: Vec<UploadHistoryEntry>,
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

fn load_store() -> Result<HistoryStore, String> {
    match read_json_file(HISTORY_FILE) {
        Ok(store) => Ok(store),
        Err(err) if err.contains("not found") => Ok(HistoryStore::default()),
        Err(err) => Err(err),
    }
}

fn save_store(store: &HistoryStore) -> Result<(), String> {
    write_json_file(HISTORY_FILE, store)
}

pub fn list_history() -> Result<Vec<UploadHistoryEntry>, String> {
    let store = load_store()?;
    let mut entries = store.entries;
    entries.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
    Ok(entries)
}

pub fn append_history(entry: UploadHistoryEntry) -> Result<(), String> {
    let mut store = load_store()?;
    store.entries.insert(0, entry);
    store.entries.truncate(MAX_HISTORY_ENTRIES);
    save_store(&store)
}

pub fn record_upload_result(
    local_path: &str,
    remote_path: &str,
    host: &str,
    port: u16,
    username: &str,
    file_size: u64,
    status: &str,
    retry_count: u32,
    message: Option<String>,
) {
    let finished_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let entry = UploadHistoryEntry {
        id: new_id(),
        local_path: local_path.to_string(),
        remote_path: remote_path.to_string(),
        host: host.to_string(),
        port,
        username: username.to_string(),
        file_size,
        status: status.to_string(),
        retry_count,
        finished_at,
        message,
        direction: "upload".to_string(),
    };

    if let Err(err) = append_history(entry) {
        log::warn!("failed to save upload history: {err}");
    }
}

pub fn record_download_result(
    local_path: &str,
    remote_path: &str,
    host: &str,
    port: u16,
    username: &str,
    file_size: u64,
    status: &str,
    retry_count: u32,
    message: Option<String>,
) {
    let finished_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let entry = UploadHistoryEntry {
        id: new_id(),
        local_path: local_path.to_string(),
        remote_path: remote_path.to_string(),
        host: host.to_string(),
        port,
        username: username.to_string(),
        file_size,
        status: status.to_string(),
        retry_count,
        finished_at,
        message,
        direction: "download".to_string(),
    };

    if let Err(err) = append_history(entry) {
        log::warn!("failed to save download history: {err}");
    }
}
