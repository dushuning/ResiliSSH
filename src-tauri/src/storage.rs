use std::fs;
use std::path::PathBuf;

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
