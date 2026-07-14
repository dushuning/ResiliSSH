use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 原子写文件：先写同目录临时文件并 fsync，再 rename 覆盖目标。
/// 断点等关键状态用它持久化——若直接 fs::write 原地截断写，进程在写到一半时
/// 崩溃/掉电会留下损坏的 JSON，加载端解析失败后会静默丢弃全部续传进度。
/// rename 在同一文件系统内是原子操作，保证目标要么是旧内容要么是完整新内容。
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // 临时文件与目标同目录，确保 rename 不跨文件系统（跨文件系统 rename 非原子）。
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "invalid checkpoint path".to_string())?;
    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(contents).map_err(|e| e.to_string())?;
        // rename 前先 fsync，确保新内容已落盘，避免 rename 后仍丢数据。
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub fn app_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sshutil")
}

pub fn ensure_app_data_dir() -> Result<PathBuf, String> {
    let dir = app_data_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

pub fn read_json_file<T: serde::de::DeserializeOwned>(filename: &str) -> Result<T, String> {
    let path = app_data_dir().join(filename);
    if !path.exists() {
        return Err("file not found".into());
    }
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

pub fn write_json_file<T: serde::Serialize>(filename: &str, value: &T) -> Result<(), String> {
    let dir = ensure_app_data_dir()?;
    let path = dir.join(filename);
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    fs::write(path, json).map_err(|e| e.to_string())
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{millis}")
}
