use super::client::{is_sftp_not_found, ConnectionConfig};
use super::pool;
use super::transfer_common::is_staging_file;
use crate::local_files::{is_ignored_name, ExpandedPaths, LocalEntry, MAX_EXPANDED_FILES};
use serde::Serialize;
use ssh2::Sftp;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct RemoteDirEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RemoteDirListing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<RemoteDirEntry>,
}

/// 新建目录的权限（实际结果还会被服务器的 umask 收紧）。
const NEW_DIRECTORY_MODE: i32 = 0o755;

/// 列出远端目录；path 为空时从用户 SFTP 主目录开始。
pub fn list_remote_directory(
    config: &ConnectionConfig,
    path: Option<&str>,
) -> Result<RemoteDirListing, String> {
    // 浏览时每点一层目录都会走到这里：复用上一次的会话，不必每次重新握手。
    pool::with_session(config, |session| {
        let sftp = session.sftp().map_err(|e| e.to_string())?;
        let listing_path = resolve_listing_path(&sftp, path)?;
        read_directory_listing(&sftp, &listing_path)
    })
}

/// 在 parent_path 下新建目录并返回刷新后的列表。
pub fn create_remote_directory(
    config: &ConnectionConfig,
    parent_path: &str,
    dir_name: &str,
) -> Result<RemoteDirListing, String> {
    let name = validate_dir_name(dir_name)?;
    pool::with_session(config, |session| {
        let sftp = session.sftp().map_err(|e| e.to_string())?;
        let parent = resolve_listing_path(&sftp, Some(parent_path))?;
        let new_path = join_remote_path(&parent, name);

        if sftp.stat(Path::new(&new_path)).is_ok() {
            return Err(crate::tr!(format!("「{name}」已存在"), format!("\"{name}\" already exists")));
        }

        sftp.mkdir(Path::new(&new_path), NEW_DIRECTORY_MODE)
            .map_err(|e| crate::tr!(format!("创建目录失败: {e}"), format!("Failed to create directory: {e}")))?;

        read_directory_listing(&sftp, &parent)
    })
}

/// 把下载时选中的远端路径展开成文件列表：文件原样保留，文件夹递归展开，并记录每个文件
/// 相对所选文件夹的目录，用于在本地还原同样的结构。规则与本地展开（上传）一致。
/// 返回的 `path` 是远端路径；复用 LocalEntry 只是为了让前端两个方向用同一种数据结构。
pub fn expand_remote_paths(config: &ConnectionConfig, paths: &[String]) -> Result<ExpandedPaths, String> {
    pool::with_session(config, |session| {
        let sftp = session.sftp().map_err(|e| e.to_string())?;
        Ok(expand_paths(&sftp, paths))
    })
}

fn expand_paths(sftp: &Sftp, paths: &[String]) -> ExpandedPaths {
    let mut out = ExpandedPaths { entries: Vec::new(), truncated: false, unreadable_dirs: Vec::new() };
    for raw in paths {
        // 在处理下一个之前判断：恰好装满、后面也没有更多内容时不算被截断。
        if out.entries.len() >= MAX_EXPANDED_FILES {
            out.truncated = true;
            break;
        }
        let path = trim_trailing_slash(raw.trim());
        // 用户亲手选中的条目跟随符号链接（stat）：选中一个指向文件或目录的链接，意图就是下载它指向的内容，
        // 不跟随的话它既不是文件也不是目录，会被一声不响地丢掉。递归进入目录后则不再跟随（见 walk_remote）。
        let stat = match sftp.stat(Path::new(path)) {
            Ok(stat) => stat,
            Err(e) => {
                log::warn!("cannot stat remote path {path}: {e}");
                out.unreadable_dirs.push(path.to_string());
                continue;
            }
        };
        if stat.is_file() {
            push_remote_file(&mut out, path, String::new(), stat.size.unwrap_or(0));
        } else if stat.is_dir() {
            walk_remote(sftp, path, &entry_name(Path::new(path)), &mut out);
        }
    }
    out.entries.truncate(MAX_EXPANDED_FILES);
    out
}

/// 批量传输开始前统计哪些目标文件已存在，用于一次性确认。按目录列举而不是逐个 stat：
/// 弱网下每次 stat 都是一个往返，几百个文件要等几十秒；同一目录的文件只需一次列举。
/// 目录不存在时视为其中的文件都不存在（之后传输时会按需创建目录）；其他错误（超时、断线、无权限）
/// 必须报出来：当成「都不存在」的话，覆盖确认会被悄悄跳过。
pub fn remote_paths_exist(config: &ConnectionConfig, paths: &[String]) -> Result<Vec<bool>, String> {
    pool::with_session(config, |session| {
        let sftp = session.sftp().map_err(|e| e.to_string())?;
        let mut listings: HashMap<String, HashSet<String>> = HashMap::new();
        let mut exists = Vec::with_capacity(paths.len());
        for path in paths {
            let (dir, name) = path.rsplit_once('/').unwrap_or((".", path.as_str()));
            let dir = if dir.is_empty() { "/" } else { dir };
            if !listings.contains_key(dir) {
                let names = match sftp.readdir(Path::new(dir)) {
                    Ok(entries) => entries.iter().map(|(p, _)| entry_name(p)).collect(),
                    Err(e) if is_sftp_not_found(&e) => HashSet::new(),
                    Err(e) => {
                        log::warn!("cannot list remote dir {dir} for overwrite check: {e}");
                        return Err(crate::tr!(
                            format!("无法列出目录「{dir}」: {e}"),
                            format!("Cannot list directory \"{dir}\": {e}")
                        ));
                    }
                };
                listings.insert(dir.to_string(), names);
            }
            exists.push(listings[dir].contains(name));
        }
        Ok(exists)
    })
}

fn trim_trailing_slash(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() { "/" } else { trimmed }
}

fn push_remote_file(out: &mut ExpandedPaths, path: &str, rel_dir: String, size: u64) {
    if !is_ignored_name(&entry_name(Path::new(path))) {
        out.entries.push(LocalEntry { path: path.to_string(), rel_dir, size });
    }
}

fn walk_remote(sftp: &Sftp, dir: &str, rel_dir: &str, out: &mut ExpandedPaths) {
    // SFTP 的 READDIR 返回的是 lstat 属性：符号链接既不是文件也不是目录，自然被跳过。
    let mut children = match sftp.readdir(Path::new(dir)) {
        Ok(children) => children,
        Err(e) => {
            log::warn!("cannot read remote dir {dir}: {e}");
            out.unreadable_dirs.push(dir.to_string());
            return;
        }
    };
    // 排序让下载顺序稳定、可预期。
    children.sort_by_key(|(path, _)| entry_name(path));
    for (child, stat) in children {
        if out.entries.len() >= MAX_EXPANDED_FILES {
            out.truncated = true;
            return;
        }
        let name = entry_name(&child);
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let child_path = join_remote_path(dir, &name);
        if stat.is_file() {
            push_remote_file(out, &child_path, rel_dir.to_string(), stat.size.unwrap_or(0));
        } else if stat.is_dir() {
            walk_remote(sftp, &child_path, &format!("{rel_dir}/{name}"), out);
        }
    }
}

fn validate_dir_name(name: &str) -> Result<&str, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(crate::tr!("目录名不能为空", "Directory name cannot be empty").into());
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(crate::tr!("目录名不能包含 / 或 \\", "Directory name cannot contain / or \\").into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err(crate::tr!("无效的目录名", "Invalid directory name").into());
    }
    Ok(trimmed)
}

fn resolve_listing_path(sftp: &Sftp, user_path: Option<&str>) -> Result<String, String> {
    let trimmed = user_path.unwrap_or("").trim();
    if trimmed.is_empty() {
        return canonicalize_remote_path(sftp, ".");
    }

    let path = Path::new(trimmed);
    if sftp.stat(path).map(|stat| stat.is_file()).unwrap_or(false) {
        if let Some(parent) = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_string_lossy().to_string())
        {
            return Ok(parent);
        }
        return canonicalize_remote_path(sftp, ".");
    }

    canonicalize_remote_path(sftp, trimmed)
}

fn canonicalize_remote_path(sftp: &Sftp, path: &str) -> Result<String, String> {
    let resolved = sftp
        .realpath(Path::new(path))
        .map_err(|e| {
            crate::tr!(
                format!("无法访问远端路径「{path}」: {e}"),
                format!("Cannot access remote path \"{path}\": {e}")
            )
        })?;
    Ok(resolved.to_string_lossy().to_string())
}

fn read_directory_listing(sftp: &Sftp, dir_path: &str) -> Result<RemoteDirListing, String> {
    let stat = sftp
        .stat(Path::new(dir_path))
        .map_err(|e| {
            crate::tr!(
                format!("无法读取远端目录「{dir_path}」: {e}"),
                format!("Cannot read remote directory \"{dir_path}\": {e}")
            )
        })?;
    if !stat.is_dir() {
        return Err(crate::tr!(format!("「{dir_path}」不是目录"), format!("\"{dir_path}\" is not a directory")));
    }

    let raw = sftp
        .readdir(Path::new(dir_path))
        .map_err(|e| {
            crate::tr!(
                format!("无法列出目录「{dir_path}」: {e}"),
                format!("Cannot list directory \"{dir_path}\": {e}")
            )
        })?;

    let mut entries = Vec::new();
    for (name_path, file_stat) in raw {
        let name = entry_name(&name_path);
        // 临时文件是未完成传输的半截数据，列出来只会让人困惑，甚至被误选下载。
        if name == "." || name == ".." || is_staging_file(&name) {
            continue;
        }
        let full_path = join_remote_path(dir_path, &name);
        // READDIR 给的是链接自身的属性：指向目录的符号链接要再 stat 一次才知道它是目录，
        // 否则会显示成文件、双击也进不去。链接已失效时保留原属性。
        let file_stat = if file_stat.file_type().is_symlink() {
            sftp.stat(Path::new(&full_path)).unwrap_or(file_stat)
        } else {
            file_stat
        };
        entries.push(RemoteDirEntry {
            name,
            path: full_path,
            is_dir: file_stat.is_dir(),
            size: file_stat.size.unwrap_or(0),
        });
    }

    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(RemoteDirListing {
        path: dir_path.to_string(),
        parent: parent_remote_path(dir_path),
        entries,
    })
}

fn entry_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

pub(super) fn join_remote_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

fn parent_remote_path(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_path_handles_trailing_slash() {
        assert_eq!(join_remote_path("/home/user", "dir"), "/home/user/dir");
        assert_eq!(join_remote_path("/home/user/", "dir"), "/home/user/dir");
    }

    #[test]
    fn parent_path_root_is_none() {
        assert_eq!(parent_remote_path("/"), None);
        assert_eq!(parent_remote_path("/home/user"), Some("/home".to_string()));
    }

    #[test]
    fn trailing_slash_trimmed_but_root_kept() {
        assert_eq!(trim_trailing_slash("/data/dist/"), "/data/dist");
        assert_eq!(trim_trailing_slash("/"), "/");
    }

    #[test]
    fn validate_dir_name_rejects_slashes() {
        assert!(validate_dir_name("a/b").is_err());
        assert!(validate_dir_name("ok").is_ok());
    }
}
