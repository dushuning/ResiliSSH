//! 传输期间阻止 Mac 因空闲自动睡眠：弱网下一个大文件可能要传一两个小时，
//! 中途睡眠会让连接断开，回来才发现停在半路。
//!
//! 借助系统自带的 `caffeinate`，不链接 IOKit：
//! - `-i` 只阻止空闲睡眠，用户主动合盖或选择睡眠照常生效（尊重用户的明确意图）；
//! - `-w <pid>` 让它在本应用退出时自行结束，应用崩溃也不会留下一个一直阻止睡眠的进程。

use std::process::{Child, Command, Stdio};

const CAFFEINATE: &str = "/usr/bin/caffeinate";

/// 持有期间系统不会因空闲而睡眠；析构时恢复。
pub struct SleepGuard {
    child: Child,
}

impl SleepGuard {
    /// 拿不到时（如 caffeinate 不存在）返回 None：防睡眠只是锦上添花，不能因此影响传输。
    pub fn acquire(reason: &str) -> Option<SleepGuard> {
        let spawned = Command::new(CAFFEINATE)
            .args(["-i", "-w", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => {
                log::info!("preventing idle sleep during {reason} (caffeinate pid {})", child.id());
                Some(SleepGuard { child })
            }
            Err(e) => {
                log::warn!("cannot prevent idle sleep during {reason}: {e}");
                None
            }
        }
    }

    #[cfg(test)]
    fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // 回收子进程，避免留下僵尸进程。
        let _ = self.child.wait();
        log::info!("idle sleep allowed again");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_running(pid: u32) -> bool {
        // SAFETY: 信号 0 只检查进程是否存在，不会真正发送信号。
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }

    #[test]
    fn guard_holds_caffeinate_until_dropped() {
        let guard = SleepGuard::acquire("test").expect("caffeinate should be available on macOS");
        let pid = guard.pid();
        assert!(is_running(pid), "caffeinate should run while the guard is held");
        drop(guard);
        assert!(!is_running(pid), "caffeinate must be gone after the guard is dropped");
    }
}
