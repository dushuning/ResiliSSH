use crate::sftp::abort_socket;
use std::net::TcpStream;
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
    abort_socket_holder: Arc<Mutex<Option<TcpStream>>>,
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

            let Some(stalled_bytes) = heartbeat.stalled_for(stall_timeout) else {
                continue;
            };

            log::warn!(
                "transfer stall detected at {stalled_bytes} bytes after {:?}, aborting socket",
                stall_timeout
            );

            if let Ok(guard) = abort_socket_holder.lock() {
                if let Some(socket) = guard.as_ref() {
                    abort_socket(socket);
                }
            }

            heartbeat.defer_next_check();
            on_stall(stalled_bytes);
        }
    });

    StallWatchdog { stop }
}
