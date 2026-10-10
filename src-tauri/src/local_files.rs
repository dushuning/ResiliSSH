//! 把用户拖入/选择的路径展开成待上传的文件列表：文件原样保留，文件夹递归展开，
//! 并记录每个文件相对所选文件夹的目录，用于在远端还原同样的目录结构。

use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

/// 单次最多展开的文件数：防止误把整个家目录拖进来，生成一个几十万项的队列。
pub(crate) const MAX_EXPANDED_FILES: usize = 10_000;
/// 这些是系统或编辑器生成的杂项，上传到服务器没有意义。
const IGNORED_NAMES: &[&str] = &[".DS_Store", "Thumbs.db", ".localized"];

/// 远端文件夹展开（下载）沿用同一份忽略列表，两个方向对「杂项文件」的判断保持一致。
/// 本程序的临时文件（未完成传输的半截数据）同样排除，否则会被当成普通文件传出去。
pub(crate) fn is_ignored_name(name: &str) -> bool {
    IGNORED_NAMES.contains(&name) || crate::sftp::is_staging_file(name)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LocalEntry {
    pub path: String,
    /// 远端相对目录，不含首尾斜杠；直接选中的文件为空串。
    /// 例：拖入 `/a/dist`，其中的 `/a/dist/js/app.js` 得到 `dist/js`。
    pub rel_dir: String,
    pub size: u64,
}

#[derive(Debug, Serialize)]
pub struct ExpandedPaths {
    pub entries: Vec<LocalEntry>,
    /// 超过上限被截断时为 true，界面需要提醒用户。
    pub truncated: bool,
    /// 无权限等原因读不了的子目录。不能静默跳过：用户会以为整个文件夹都传完了。
    pub unreadable_dirs: Vec<String>,
}

pub fn expand_local_paths(paths: &[String]) -> ExpandedPaths {
    let mut entries = Vec::new();
    let mut truncated = false;
    let mut unreadable_dirs = Vec::new();
    for raw in paths {
        let path = PathBuf::from(raw);
        // symlink_metadata：不跟随符号链接，避免链接成环或把树外的内容带进来。
        let Ok(meta) = fs::symlink_metadata(&path) else { continue };
        if meta.is_file() {
            push_file(&mut entries, &path, String::new(), meta.len());
        } else if meta.is_dir() {
            let root_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            walk(&path, &root_name, &mut entries, &mut truncated, &mut unreadable_dirs);
        }
        if entries.len() >= MAX_EXPANDED_FILES {
            truncated = true;
            break;
        }
    }
    entries.truncate(MAX_EXPANDED_FILES);
    ExpandedPaths { entries, truncated, unreadable_dirs }
}

fn push_file(entries: &mut Vec<LocalEntry>, path: &Path, rel_dir: String, size: u64) {
    let ignored = path
        .file_name()
        .is_some_and(|n| is_ignored_name(&n.to_string_lossy()));
    if !ignored {
        entries.push(LocalEntry {
            path: path.to_string_lossy().to_string(),
            rel_dir,
            size,
        });
    }
}

fn walk(
    dir: &Path,
    rel_dir: &str,
    entries: &mut Vec<LocalEntry>,
    truncated: &mut bool,
    unreadable_dirs: &mut Vec<String>,
) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) => {
            log::warn!("cannot read local dir {}: {e}", dir.display());
            unreadable_dirs.push(dir.to_string_lossy().to_string());
            return;
        }
    };
    // 排序让上传顺序稳定、可预期（与 Finder 中看到的顺序一致）。
    let mut children: Vec<_> = read.flatten().collect();
    children.sort_by_key(|e| e.file_name());
    for child in children {
        if entries.len() >= MAX_EXPANDED_FILES {
            *truncated = true;
            return;
        }
        let Ok(meta) = child.path().symlink_metadata() else { continue };
        let name = child.file_name().to_string_lossy().to_string();
        if meta.is_file() {
            push_file(entries, &child.path(), rel_dir.to_string(), meta.len());
        } else if meta.is_dir() {
            walk(&child.path(), &format!("{rel_dir}/{name}"), entries, truncated, unreadable_dirs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_folder_keeping_structure_and_skips_junk() {
        let root = std::env::temp_dir().join(format!("resiliss-expand-{}", std::process::id()));
        let dist = root.join("dist");
        fs::create_dir_all(dist.join("js")).unwrap();
        fs::write(dist.join("index.html"), b"<html>").unwrap();
        fs::write(dist.join("js/app.js"), b"x").unwrap();
        fs::write(dist.join(".DS_Store"), b"junk").unwrap();
        fs::write(dist.join(".index.html.resiliss-part"), b"half").unwrap();
        let single = root.join("notes.txt");
        fs::write(&single, b"hi").unwrap();

        let result = expand_local_paths(&[
            dist.to_string_lossy().to_string(),
            single.to_string_lossy().to_string(),
        ]);
        let rel: Vec<(&str, String)> = result
            .entries
            .iter()
            .map(|e| (e.rel_dir.as_str(), Path::new(&e.path).file_name().unwrap().to_string_lossy().to_string()))
            .collect();
        assert_eq!(
            rel,
            vec![
                ("dist", "index.html".to_string()),
                ("dist/js", "app.js".to_string()),
                ("", "notes.txt".to_string()),
            ]
        );
        assert!(!result.truncated);
        assert!(result.unreadable_dirs.is_empty());
        fs::remove_dir_all(&root).ok();
    }
}
