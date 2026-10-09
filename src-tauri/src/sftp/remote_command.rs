//! 传完后在服务器上执行命令（如重启服务），把「传文件」延伸成「发布」。
//!
//! 命令模板里的变量由这里替换并做 shell 单引号转义：文件名可能含空格、引号或 `;`，
//! 直接拼接字符串轻则命令出错，重则执行了文件名里夹带的内容。

use super::client::{connect_session, ConnectionConfig};
use crate::activity::{self, Category};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Read;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 命令长时间没有任何输出才算超时（不是总时长）：重启类命令可能安静地跑几分钟。
const COMMAND_IDLE_TIMEOUT_MS: u32 = 10 * 60 * 1000;
/// 结果页只展示末尾若干行：出错信息几乎总在最后。
const OUTPUT_TAIL_LINES: usize = 40;
/// 写进活动日志的行数上限，防止 `tail -f` 之类的命令把日志刷爆。
const MAX_LOGGED_LINES: usize = 300;
const READ_BUFFER_BYTES: usize = 4096;

/// 变量的取值来自本次传输；多文件时 `file_name`/`remote_path` 指最后一个文件，`files` 为全部文件名。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CommandContext {
    pub remote_dir: String,
    pub file_name: String,
    pub remote_path: String,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandOutcome {
    /// 实际执行的命令（变量已替换），便于用户核对。
    pub command: String,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
    pub timed_out: bool,
    pub output_tail: Vec<String>,
}

impl CommandOutcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0) && !self.cancelled && !self.timed_out
    }
}

/// POSIX shell 单引号转义：单引号内一切按字面量处理，唯一要处理的是单引号本身。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// 只替换已知变量名，模板里其他花括号（如 `${HOME}`、awk 脚本）原样保留。
pub fn expand_template(template: &str, ctx: &CommandContext) -> String {
    let files = ctx
        .files
        .iter()
        .map(|f| shell_quote(f))
        .collect::<Vec<_>>()
        .join(" ");
    template
        .replace("{remote_dir}", &shell_quote(&ctx.remote_dir))
        .replace("{file_name}", &shell_quote(&ctx.file_name))
        .replace("{remote_path}", &shell_quote(&ctx.remote_path))
        .replace("{files}", &files)
}

/// 用 `sh -c` 包一层并合并 stderr：只读一个流，避免 stdout/stderr 分别阻塞造成死锁，
/// 也让错误输出和正常输出按真实顺序交错显示。
fn wrap_for_exec(command: &str) -> String {
    format!("sh -c {} 2>&1", shell_quote(command))
}

pub fn run_remote_command(
    config: &ConnectionConfig,
    template: &str,
    ctx: &CommandContext,
    cancel: &AtomicBool,
    abort_socket: &Arc<Mutex<Option<TcpStream>>>,
    mut on_line: impl FnMut(&str),
) -> Result<CommandOutcome, String> {
    let command = expand_template(template, ctx);
    activity::info(Category::Command, format!("开始执行：{command}"));

    let (session, socket) = connect_session(config).map_err(|e| e.to_string())?;
    if let Ok(mut holder) = abort_socket.lock() {
        *holder = Some(socket);
    }
    session.set_timeout(COMMAND_IDLE_TIMEOUT_MS);

    let mut channel = session
        .channel_session()
        .map_err(|e| format!("无法打开命令通道: {e}"))?;
    channel
        .exec(&wrap_for_exec(&command))
        .map_err(|e| format!("无法执行命令: {e}"))?;

    let mut tail: VecDeque<String> = VecDeque::with_capacity(OUTPUT_TAIL_LINES);
    let mut logged = 0_usize;
    let mut pending = String::new();
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    let mut timed_out = false;

    let mut emit = |line: &str, tail: &mut VecDeque<String>, logged: &mut usize| {
        let line = line.trim_end_matches('\r');
        if tail.len() == OUTPUT_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line.to_string());
        if *logged < MAX_LOGGED_LINES {
            activity::info(Category::Command, format!("│ {line}"));
        }
        *logged += 1;
        on_line(line);
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match channel.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                pending.push_str(&String::from_utf8_lossy(&buffer[..n]));
                while let Some(pos) = pending.find('\n') {
                    let line: String = pending.drain(..=pos).collect();
                    emit(line.trim_end_matches('\n'), &mut tail, &mut logged);
                }
            }
            Err(e) => {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                if e.kind() == std::io::ErrorKind::TimedOut {
                    timed_out = true;
                    break;
                }
                return Err(format!("读取命令输出失败: {e}"));
            }
        }
    }
    if !pending.is_empty() {
        let rest = std::mem::take(&mut pending);
        emit(&rest, &mut tail, &mut logged);
    }
    if logged > MAX_LOGGED_LINES {
        activity::info(
            Category::Command,
            format!("…另有 {} 行输出未写入日志", logged - MAX_LOGGED_LINES),
        );
    }

    let cancelled = cancel.load(Ordering::Relaxed);
    let exit_code = if cancelled || timed_out {
        None
    } else {
        let _ = channel.wait_close();
        channel.exit_status().ok()
    };

    let outcome = CommandOutcome {
        command,
        exit_code,
        cancelled,
        timed_out,
        output_tail: tail.into_iter().collect(),
    };
    record_outcome(&outcome);
    Ok(outcome)
}

fn record_outcome(outcome: &CommandOutcome) {
    if outcome.cancelled {
        activity::warn(Category::Command, "命令已取消（远端进程可能仍在运行）");
    } else if outcome.timed_out {
        activity::error(Category::Command, "命令超过 10 分钟没有任何输出，已停止等待");
    } else if outcome.succeeded() {
        activity::success(Category::Command, "命令执行成功（退出码 0）");
    } else {
        let code = outcome
            .exit_code
            .map_or_else(|| "未知".to_string(), |c| c.to_string());
        activity::error(Category::Command, format!("命令执行失败（退出码 {code}）"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> CommandContext {
        CommandContext {
            remote_dir: "/opt/my app/".into(),
            file_name: "it's.jar".into(),
            remote_path: "/opt/my app/it's.jar".into(),
            files: vec!["a.jar".into(), "b c.jar".into()],
        }
    }

    #[test]
    fn variables_are_shell_quoted() {
        let expanded = expand_template("cd {remote_dir} && java -jar {file_name}", &ctx());
        assert_eq!(expanded, r"cd '/opt/my app/' && java -jar 'it'\''s.jar'");
    }

    #[test]
    fn files_expand_to_separately_quoted_words() {
        assert_eq!(expand_template("ls {files}", &ctx()), "ls 'a.jar' 'b c.jar'");
    }

    #[test]
    fn unknown_braces_are_left_alone() {
        let template = "echo ${HOME} && awk '{print $1}'";
        assert_eq!(expand_template(template, &ctx()), template);
    }

    #[test]
    fn injection_in_file_name_stays_literal() {
        let mut c = ctx();
        c.file_name = "x; rm -rf ~".into();
        assert_eq!(expand_template("ls {file_name}", &c), "ls 'x; rm -rf ~'");
    }

    #[test]
    fn exec_wrapper_merges_stderr() {
        assert_eq!(wrap_for_exec("echo hi"), "sh -c 'echo hi' 2>&1");
    }
}
