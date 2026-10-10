//! 丢弃断点时清理它对应的临时文件（`.名称.resiliss-part`）。
//!
//! 断点被清除或被新的传输顶替后，临时文件已无法续传：留着只会占用空间，
//! 别人看到也不知道是什么。旧版本断点（staged=false）的半截数据就在目标文件里，绝不能删，
//! 所以只处理 staged 断点。清理都是尽力而为：失败只记日志并告知残留路径，不影响后续操作。

use crate::activity::{self, Category};
use crate::sftp::{connect_session, is_sftp_not_found, staging_path, ConnectionConfig};
use std::path::Path;

/// 删除下载的本地临时文件。
pub fn remove_local_staging(final_local: &str) {
    let part = staging_path(final_local);
    match std::fs::remove_file(&part) {
        Ok(()) => activity::info(
            Category::Transfer,
            crate::tr!(format!("已删除未完成下载的临时文件 {part}"), format!("Removed the temporary file of an unfinished download: {part}")),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => activity::warn(
            Category::Transfer,
            crate::tr!(
                format!("未能删除临时文件 {part}：{e}，可手动删除"),
                format!("Could not remove the temporary file {part}: {e}; you can delete it manually")
            ),
        ),
    }
}

/// 删除上传留在服务器上的临时文件。返回仍然残留的路径（删除失败时），供界面提示用户手动处理。
pub fn remove_remote_staging(config: &ConnectionConfig, final_remote: &str) -> Option<String> {
    let part = staging_path(final_remote);
    let result = connect_session(config)
        .map_err(|e| e.to_string())
        .and_then(|(session, _)| session.sftp().map_err(|e| e.to_string()))
        .and_then(|sftp| match sftp.unlink(Path::new(&part)) {
            Ok(()) => Ok(true),
            Err(e) if is_sftp_not_found(&e) => Ok(false),
            Err(e) => Err(e.to_string()),
        });
    match result {
        Ok(true) => {
            activity::info(
                Category::Transfer,
                crate::tr!(
                    format!("已删除服务器上未完成上传的临时文件 {part}"),
                    format!("Removed the temporary file of an unfinished upload on the server: {part}")
                ),
            );
            None
        }
        Ok(false) => None,
        Err(e) => {
            report_remote_leftover(&config.endpoint(), &part, &e);
            Some(part)
        }
    }
}

/// 无法删除（连不上、或断点属于另一台服务器）时，至少在日志里留下完整位置，方便用户手动清理。
pub fn report_remote_leftover(endpoint: &str, part: &str, reason: &str) {
    activity::warn(
        Category::Transfer,
        crate::tr!(
            format!("{endpoint} 上的临时文件 {part} 未能删除（{reason}），可手动删除"),
            format!("The temporary file {part} on {endpoint} could not be removed ({reason}); you can delete it manually")
        ),
    );
}
