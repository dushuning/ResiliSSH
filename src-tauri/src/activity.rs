//! 面向用户的活动日志：连接、认证、指纹、传输、校验各阶段发生了什么。
//!
//! 与 `log::` 诊断日志的区别：这里的每一条都是给人看的中文结论（「认证失败：密码错误」），
//! 实时推送到界面的「日志」页；同时也会写入诊断日志文件，便于事后排查。
//! 用全局 sink 而不是层层传 AppHandle：连接代码（sftp::client）位于调用链很深处，
//! 为了打日志去改十几个函数签名得不偿失。

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter};

/// 界面与内存中最多保留的条数：足够覆盖一次长时间弱网传输的重连过程，又不会无限增长。
const MAX_ENTRIES: usize = 500;
pub const ACTIVITY_EVENT: &str = "activity-log";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Success,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Connect,
    Auth,
    HostKey,
    Network,
    Permission,
    Transfer,
    Verify,
    Command,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEntry {
    pub id: u64,
    pub ts_ms: i64,
    pub level: Level,
    pub category: Category,
    pub message: String,
}

#[derive(Default)]
struct Sink {
    entries: VecDeque<ActivityEntry>,
    next_id: u64,
    app: Option<AppHandle>,
}

fn sink() -> &'static Mutex<Sink> {
    static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();
    SINK.get_or_init(|| Mutex::new(Sink::default()))
}

/// 应用启动时注册事件出口；注册前产生的记录只进内存，界面打开后通过 `entries` 补拉。
pub fn init(app: AppHandle) {
    if let Ok(mut s) = sink().lock() {
        s.app = Some(app);
    }
}

pub fn record(level: Level, category: Category, message: impl Into<String>) {
    let message = message.into();
    match level {
        Level::Error => log::error!("[activity:{category:?}] {message}"),
        Level::Warn => log::warn!("[activity:{category:?}] {message}"),
        Level::Info | Level::Success => log::info!("[activity:{category:?}] {message}"),
    }

    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let Ok(mut s) = sink().lock() else { return };
    s.next_id += 1;
    let entry = ActivityEntry {
        id: s.next_id,
        ts_ms,
        level,
        category,
        message,
    };
    if s.entries.len() >= MAX_ENTRIES {
        s.entries.pop_front();
    }
    s.entries.push_back(entry.clone());
    if let Some(app) = &s.app {
        let _ = app.emit(ACTIVITY_EVENT, entry);
    }
}

pub fn info(category: Category, message: impl Into<String>) {
    record(Level::Info, category, message);
}

pub fn success(category: Category, message: impl Into<String>) {
    record(Level::Success, category, message);
}

pub fn warn(category: Category, message: impl Into<String>) {
    record(Level::Warn, category, message);
}

pub fn error(category: Category, message: impl Into<String>) {
    record(Level::Error, category, message);
}

pub fn entries() -> Vec<ActivityEntry> {
    sink()
        .lock()
        .map(|s| s.entries.iter().cloned().collect())
        .unwrap_or_default()
}

pub fn clear() {
    if let Ok(mut s) = sink().lock() {
        s.entries.clear();
    }
}

/// 把底层错误文本归到用户能理解的类别：用户最关心的是「是网络问题、权限问题还是密码问题」。
pub fn classify_failure(message: &str) -> Category {
    let lower = message.to_ascii_lowercase();
    if ["permission denied", "access denied", "read-only file system", "权限"]
        .iter()
        .any(|k| lower.contains(k))
    {
        Category::Permission
    } else if ["auth", "认证", "凭据"].iter().any(|k| lower.contains(k)) {
        Category::Auth
    } else if ["指纹", "host key", "主机密钥"].iter().any(|k| lower.contains(k)) {
        Category::HostKey
    } else if [
        "timed out",
        "timeout",
        "reset",
        "broken pipe",
        "refused",
        "unreachable",
        "无响应",
        "连接失败",
        "tcp",
        "socket",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        Category::Network
    } else {
        Category::Transfer
    }
}

/// 日志里的字节数要让人一眼看懂，与前端 formatBytes 的口径保持一致（1024 进制）。
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    const STEP: f64 = 1024.0;
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= STEP && unit < UNITS.len() - 1 {
        value /= STEP;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_failures() {
        assert_eq!(classify_failure("SFTP 错误: Permission denied"), Category::Permission);
        assert_eq!(classify_failure("认证失败: [Session(-18)]"), Category::Auth);
        assert_eq!(classify_failure("Timed out waiting on socket"), Category::Network);
        assert_eq!(classify_failure("Connection reset by peer"), Category::Network);
        assert_eq!(classify_failure("SHA-256 校验未通过"), Category::Transfer);
    }

    #[test]
    fn formats_bytes_readably() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn ring_buffer_is_bounded() {
        for i in 0..(MAX_ENTRIES + 20) {
            info(Category::Transfer, format!("entry {i}"));
        }
        let all = entries();
        assert_eq!(all.len(), MAX_ENTRIES);
        assert_eq!(all.last().unwrap().message, format!("entry {}", MAX_ENTRIES + 19));
    }
}
