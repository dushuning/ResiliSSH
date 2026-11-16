use crate::sftp::AbortHandles;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const WATCHDOG_POLL_MS: u64 = 2_000;

pub struct ProgressHeartbeat {
    inner: Mutex<HeartbeatState>,
}

struct HeartbeatState {
    uploaded_bytes: u64,
    last_change: Instant,
    active: bool,
}

impl ProgressHeartbeat {
    pub fn new(initial_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(HeartbeatState {
                uploaded_bytes: initial_bytes,
                last_change: Instant::now(),
                active: true,
            }),
        }
    }

    pub fn set_active(&self, active: bool) {
        if let Ok(mut state) = self.inner.lock() {
            state.active = active;
            if active {
                state.last_change = Instant::now();
            }
        }
    }

    pub fn touch(&self, uploaded_bytes: u64) {
        if let Ok(mut state) = self.inner.lock() {
            if uploaded_bytes != state.uploaded_bytes {
                state.uploaded_bytes = uploaded_bytes;
            }
            state.last_change = Instant::now();
        }
    }

    pub fn defer_next_check(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.last_change = Instant::now();
        }
    }

    pub fn stalled_for(&self, threshold: Duration) -> Option<u64> {
        let state = self.inner.lock().ok()?;
        if !state.active {
            return None;
        }
        if state.last_change.elapsed() >= threshold {
            Some(state.uploaded_bytes)
        } else {
            None
        }
    }
}

pub struct StallWatchdog {
    stop: Arc<AtomicBool>,
}

impl StallWatchdog {
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn spawn_stall_watchdog(
    heartbeat: Arc<ProgressHeartbeat>,
    cancel_flag: Arc<AtomicBool>,
    aborts: Arc<AbortHandles>,
    stall_timeout: Duration,
    on_stall: impl Fn(u64) + Send + 'static,
) -> StallWatchdog {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    thread::spawn(move || {
        while !stop_clone.load(Ordering::Relaxed) && !cancel_flag.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(WATCHDOG_POLL_MS));

            if cancel_flag.load(Ordering::Relaxed) || stop_clone.load(Ordering::Relaxed) {
                break;
            }

            // 连接所用的本机地址没了（换网络、VPN 重连）：这些连接不可能再通，
            // 不必等到无响应阈值，立刻打断让重试循环在新网络上重连。
            let network_changed = crate::netwatch::local_addresses()
                .is_some_and(|present| crate::netwatch::any_address_gone(&aborts.local_addrs(), &present));
            if network_changed {
                log::warn!("local address of an active connection disappeared, reconnecting now");
                crate::activity::warn(
                    crate::activity::Category::Network,
                    crate::tr!("检测到网络切换，立即重新连接", "Network change detected; reconnecting now"),
                );
                aborts.abort_all();
                heartbeat.defer_next_check();
                continue;
            }

            let Some(stalled_bytes) = heartbeat.stalled_for(stall_timeout) else {
                continue;
            };

            log::warn!(
                "transfer stall detected at {stalled_bytes} bytes after {:?}, aborting socket",
                stall_timeout
            );

            // 并行传输时每条连接都要打断：只断一条的话，其余连接仍卡在各自的阻塞 IO 里。
            aborts.abort_all();

            heartbeat.defer_next_check();
            on_stall(stalled_bytes);
        }
    });

    StallWatchdog { stop }
}
