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
    /// 批量传输整体记一条：逐个文件记录时，一次几百个文件的上传会把历史（只保留最近 30 条）全部挤掉。
    #[serde(default)]
    pub batch: Option<BatchSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchSummary {
    /// 展示用名称，如「dist/」或「12 个文件」。
    pub label: String,
    pub file_count: u32,
    pub done_count: u32,
    pub failed_count: u32,
    /// 当初选择的路径（拖入的文件夹、勾选的文件）。复用时重新展开，而不是保存展开后的完整清单：
    /// 后者可能上万条，会让历史文件膨胀；重新展开也能带上文件夹里新增的文件。
    pub roots: Vec<String>,
}

/// 选择的路径太多（例如逐个勾选了上千个文件）时不保存，只是不能一键复用，避免历史文件膨胀。
const MAX_BATCH_ROOTS: usize = 500;

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
        batch: None,
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
        batch: None,
    };

    if let Err(err) = append_history(entry) {
        log::warn!("failed to save download history: {err}");
    }
}

/// 记录一次批量传输的整体结果（由前端在批量结束时调用）。
pub fn record_batch(mut entry: UploadHistoryEntry) -> Result<(), String> {
    if let Some(batch) = entry.batch.as_mut() {
        if batch.roots.len() > MAX_BATCH_ROOTS {
            log::info!("batch history: {} roots exceed the limit, reuse disabled", batch.roots.len());
            batch.roots.clear();
        }
    }
    entry.id = new_id();
    entry.finished_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    append_history(entry)
}
