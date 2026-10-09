//! 上传与下载共用的「是否值得重试」判定。两边曾各维护一份关键字表并已出现漂移
//! （只有下载认识「远端不是文件」），统一到这里保证同一类错误在两个方向上行为一致。

/// 去掉 `format_remote_sftp_error` 加的「远端路径「…」操作失败：」外壳，只对原始错误做匹配，
/// 避免路径本身恰好含有关键字（例如目录名带 auth）时被误判。
fn strip_sftp_wrapper(message: &str) -> &str {
    if let Some(pos) = message.find('」') {
        if message.starts_with("远端路径「") {
            let rest = message[pos + '」'.len_utf8()..].trim_start();
            return rest.strip_prefix("操作失败：").unwrap_or(rest);
        }
    }
    message
}

/// 只有「重连也不会好」的错误才停止重试；网络抖动、超时、断线一律无限重试——
/// 弱网下可能断上很久，设上限会让本可续传完成的任务提前失败。
/// 注意：OpenSSH 的 SFTP v3 会把磁盘满等错误笼统报成 "Failure"，无法与偶发失败区分，
/// 这类情况宁可继续重试（界面会持续显示重试原因，用户可自行取消）。
const PERMANENT_ERROR_KEYWORDS: &[&str] = &[
    "permission denied",
    "access denied",
    "no such file",
    "not a directory",
    "is a directory",
    "cannot create",
    "read-only file system",
    "no space left",
    "quota exceeded",
    "auth",
    "续传边界校验失败",
    "sha-256",
    "远端路径错误",
    "远端不是文件",
];

pub fn is_permanent_sftp_error(message: &str) -> bool {
    let lower = strip_sftp_wrapper(message).to_ascii_lowercase();
    PERMANENT_ERROR_KEYWORDS
        .iter()
        .any(|keyword| lower.contains(keyword))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_errors_stay_retryable() {
        assert!(!is_permanent_sftp_error("Timed out waiting on socket"));
        assert!(!is_permanent_sftp_error("[Session(-7)] Unable to send FXP_OPEN*"));
        assert!(!is_permanent_sftp_error("Failure"));
    }

    #[test]
    fn non_recoverable_errors_stop_retrying() {
        assert!(is_permanent_sftp_error("Permission denied"));
        assert!(is_permanent_sftp_error("No space left on device"));
        assert!(is_permanent_sftp_error("远端不是文件: /tmp"));
    }

    #[test]
    fn keyword_inside_wrapped_path_is_ignored() {
        let wrapped = "远端路径「/data/auth-backup/a.jar」操作失败：Timed out waiting on socket";
        assert!(!is_permanent_sftp_error(wrapped));
    }
}
