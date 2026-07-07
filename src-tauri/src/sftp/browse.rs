use super::client::{connect_session, ConnectionConfig};
use serde::Serialize;
use ssh2::Sftp;
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

/// 列出远端目录；path 为空时从用户 SFTP 主目录开始。
pub fn list_remote_directory(
    config: &ConnectionConfig,
    path: Option<&str>,
) -> Result<RemoteDirListing, String> {
    let (session, _) = connect_session(config).map_err(|e| e.to_string())?;
    let sftp = session.sftp().map_err(|e| e.to_string())?;

    let listing_path = resolve_listing_path(&sftp, path)?;
    read_directory_listing(&sftp, &listing_path)
}

/// 在 parent_path 下新建目录并返回刷新后的列表。
pub fn create_remote_directory(
    config: &ConnectionConfig,
    parent_path: &str,
    dir_name: &str,
) -> Result<RemoteDirListing, String> {
    let name = validate_dir_name(dir_name)?;
    let (session, _) = connect_session(config).map_err(|e| e.to_string())?;
    let sftp = session.sftp().map_err(|e| e.to_string())?;

    let parent = resolve_listing_path(&sftp, Some(parent_path))?;
    let new_path = join_remote_path(&parent, name);

    if sftp.stat(Path::new(&new_path)).is_ok() {
        return Err(format!("「{name}」已存在"));
    }

    sftp.mkdir(Path::new(&new_path), 0o755)
        .map_err(|e| format!("创建目录失败: {e}"))?;

    read_directory_listing(&sftp, &parent)
}

fn validate_dir_name(name: &str) -> Result<&str, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("目录名不能为空".into());
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err("目录名不能包含 / 或 \\".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("无效的目录名".into());
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
        .map_err(|e| format!("无法访问远端路径「{path}」: {e}"))?;
    Ok(resolved.to_string_lossy().to_string())
}

fn read_directory_listing(sftp: &Sftp, dir_path: &str) -> Result<RemoteDirListing, String> {
    let stat = sftp
        .stat(Path::new(dir_path))
        .map_err(|e| format!("无法读取远端目录「{dir_path}」: {e}"))?;
    if !stat.is_dir() {
        return Err(format!("「{dir_path}」不是目录"));
    }

    let raw = sftp
        .readdir(Path::new(dir_path))
        .map_err(|e| format!("无法列出目录「{dir_path}」: {e}"))?;

    let mut entries = Vec::new();
    for (name_path, file_stat) in raw {
        let name = entry_name(&name_path);
        if name == "." || name == ".." {
            continue;
        }
        let full_path = join_remote_path(dir_path, &name);
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

fn join_remote_path(base: &str, name: &str) -> String {
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
    fn validate_dir_name_rejects_slashes() {
        assert!(validate_dir_name("a/b").is_err());
        assert!(validate_dir_name("ok").is_ok());
    }
}
