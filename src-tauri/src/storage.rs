use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

/// 给每次原子写的临时文件一个独一无二的名字。
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 「读出 → 修改 → 写回」必须整体互斥：上传线程、下载线程和界面命令会同时追加历史或改服务器列表，
/// 各自读到旧内容再写回的话，后写的会把先写的那条更新悄悄盖掉。
static STORE_LOCK: Mutex<()> = Mutex::new(());

/// 修改 JSON 存储前持有；守卫存活期间其他线程的修改会排队等待。不可重入，只在最外层函数里取一次。
pub fn lock_store() -> MutexGuard<'static, ()> {
    // 被保护的是磁盘上的文件而不是内存数据，持锁线程 panic 不会留下不一致的状态。
    STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

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
    // 临时文件名带进程号和序号：两个线程同时写同一个目标时若共用一个临时文件，
    // 内容会交错，再被 rename 成正式文件就是一份损坏的 JSON。
    let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{file_name}.{}.{sequence}.tmp", std::process::id()));
    {
        let mut f = fs::File::create(&tmp).map_err(|e| e.to_string())?;
        f.write_all(contents).map_err(|e| e.to_string())?;
        // rename 前先 fsync，确保新内容已落盘，避免 rename 后仍丢数据。
        f.sync_all().map_err(|e| e.to_string())?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        // 改名失败时别把临时文件留在数据目录里越积越多。
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// 覆盖数据目录的环境变量：端到端测试用它把断点、已信任指纹等写到临时目录，
/// 不污染用户真实的应用数据；也可用于同时运行多个互不干扰的实例。
pub const DATA_DIR_ENV: &str = "RESILISS_DATA_DIR";

pub fn app_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("sshutil")
}

/// 断点文件所在目录的覆盖值，见 `set_checkpoint_dir`。
static CHECKPOINT_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 让之后的断点读写改用指定目录（None 恢复默认）。命令行版本给每个传输任务一个独立目录：
/// 图形界面每个方向只有一份断点，命令行若共用它，两边同时使用或连续传不同文件时会互相覆盖。
pub fn set_checkpoint_dir(dir: Option<PathBuf>) {
    *CHECKPOINT_DIR.lock().unwrap_or_else(|e| e.into_inner()) = dir;
}

/// 断点文件所在目录：默认就是应用数据目录。
pub fn checkpoint_dir() -> PathBuf {
    CHECKPOINT_DIR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(app_data_dir)
}

/// 读取一份 JSON 存储；文件不存在时返回默认值（首次运行）。
/// 文件存在却解析不了（被外部改坏、磁盘故障）时，把它改名留作备份再按空数据继续：
/// 否则这份存储会永远读不出来——历史再也记不上，服务器列表的所有操作都报错，用户只能自己去删文件。
pub fn read_json_store<T: serde::de::DeserializeOwned + Default>(filename: &str) -> Result<T, String> {
    let path = app_data_dir().join(filename);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(e) => return Err(e.to_string()),
    };
    match serde_json::from_str(&content) {
        Ok(value) => Ok(value),
        Err(e) => {
            let backup = path.with_file_name(format!("{filename}.corrupt-{}", new_id()));
            log::error!(
                "{} is not valid JSON ({e}); moving it to {} and starting empty",
                path.display(),
                backup.display()
            );
            fs::rename(&path, &backup).map_err(|e| e.to_string())?;
            Ok(T::default())
        }
    }
}

pub fn write_json_file<T: serde::Serialize>(filename: &str, value: &T) -> Result<(), String> {
    let path = app_data_dir().join(filename);
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    // 连接配置、历史同样是用户状态，走原子写避免半截 JSON 导致整份数据读不出来。
    atomic_write(&path, json.as_bytes())
}

pub fn new_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{millis}")
}
