//! 上传与下载共用的「是否值得重试」判定。两边曾各维护一份关键字表并已出现漂移
//! （只有下载认识「远端不是文件」），统一到这里保证同一类错误在两个方向上行为一致。

/// `format_remote_sftp_error` 外壳与「远端路径错误」前缀的中英文写法。
/// 文案在生成时按界面语言二选一，而判定可能发生在用户切换语言之后，所以两种语言都必须识别。
const WRAPPER_PREFIX_ZH: &str = "远端路径「";
const WRAPPER_PATH_END_ZH: char = '」';
const WRAPPER_SUFFIX_ZH: &str = "操作失败：";
const WRAPPER_PREFIX_EN: &str = "Remote path \"";
const WRAPPER_PATH_END_EN: &str = "\" failed: ";
const REMOTE_PATH_ERROR_ZH: &str = "远端路径错误";
const REMOTE_PATH_ERROR_EN: &str = "Remote path error";

/// 去掉 `format_remote_sftp_error` 加的「远端路径「…」操作失败：」外壳，只对原始错误做匹配，
/// 避免路径本身恰好含有关键字（例如目录名带 auth）时被误判。
fn strip_sftp_wrapper(message: &str) -> &str {
    if message.starts_with(WRAPPER_PREFIX_ZH) {
        if let Some(pos) = message.find(WRAPPER_PATH_END_ZH) {
            let rest = message[pos + WRAPPER_PATH_END_ZH.len_utf8()..].trim_start();
            return rest.strip_prefix(WRAPPER_SUFFIX_ZH).unwrap_or(rest);
        }
    }
    if let Some(after_prefix) = message.strip_prefix(WRAPPER_PREFIX_EN) {
        if let Some(pos) = after_prefix.find(WRAPPER_PATH_END_EN) {
            return after_prefix[pos + WRAPPER_PATH_END_EN.len()..].trim_start();
        }
    }
    message
}

/// 文案是否已经带上了远端路径说明（任一语言）：是则原样展示，避免 `format_*_failure` 再套一层路径外壳。
pub fn is_remote_path_message(message: &str) -> bool {
    message.contains(REMOTE_PATH_ERROR_ZH)
        || message.contains(REMOTE_PATH_ERROR_EN)
        || message.starts_with(WRAPPER_PREFIX_ZH)
        || message.starts_with(WRAPPER_PREFIX_EN)
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
    "resume boundary verification failed",
    "sha-256",
    "远端路径错误",
    "remote path error",
    "远端不是文件",
    "remote path is not a file",
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

    #[test]
    fn keyword_inside_english_wrapped_path_is_ignored() {
        let wrapped = "Remote path \"/data/auth-backup/a.jar\" failed: Timed out waiting on socket";
        assert_eq!(strip_sftp_wrapper(wrapped), "Timed out waiting on socket");
        assert!(!is_permanent_sftp_error(wrapped));
        let denied = "Remote path \"/data/a.jar\" failed: Permission denied";
        assert!(is_permanent_sftp_error(denied));
    }

    #[test]
    fn english_messages_classify_like_chinese() {
        let pairs = [
            ("远端不是文件: /tmp", "Remote path is not a file: /tmp"),
            (
                "续传边界校验失败（offset=1048576），请清除断点后重新上传",
                "Resume boundary verification failed (offset=1048576); clear the checkpoint and upload again",
            ),
            ("远端路径错误：/a/b\n写入失败：Failure", "Remote path error: /a/b\nWrite failed: Failure"),
            ("块校验失败（offset=0，len=1），将重传该块", "Chunk verification failed (offset=0, len=1); resending the chunk"),
        ];
        for (zh, en) in pairs {
            assert_eq!(is_permanent_sftp_error(zh), is_permanent_sftp_error(en), "{zh} vs {en}");
        }
    }

    #[test]
    fn remote_path_messages_recognized_in_both_languages() {
        assert!(is_remote_path_message("远端路径错误：/a — x"));
        assert!(is_remote_path_message("远端路径「/a」操作失败：x"));
        assert!(is_remote_path_message("Remote path error: /a — x"));
        assert!(is_remote_path_message("Remote path \"/a\" failed: x"));
        assert!(!is_remote_path_message("Timed out waiting on socket"));
    }
}
