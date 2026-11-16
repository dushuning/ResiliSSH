/// 传输重试退避：弱网场景下需要快速重连，不宜等太久。
pub const INITIAL_BACKOFF_MS: u64 = 1_000;
/// 上限 15s（原 60s 在多次失败后让用户空等过久）。
pub const MAX_BACKOFF_MS: u64 = 15_000;

/// 指数退避下一档等待时间。
pub fn next_backoff_ms(current_ms: u64) -> u64 {
    current_ms.saturating_mul(2).min(MAX_BACKOFF_MS)
}

/// 一次传输的退避状态。只有「比上次失败时多传出了数据」才回到初始等待：
/// 以连接成功为准重置的话，能连上却传不动的服务器（如磁盘满，SFTP 只报笼统的 Failure，
/// 按设计无限重试）会被每秒完整握手、认证一次。
pub struct Backoff {
    current_ms: u64,
    bytes_at_last_failure: Option<u64>,
}

impl Backoff {
    pub fn new() -> Self {
        Self { current_ms: INITIAL_BACKOFF_MS, bytes_at_last_failure: None }
    }

    /// 记录一次失败并返回这次该等多久。`transferred_bytes` 是失败时断点里的已传字节数。
    pub fn next_delay_ms(&mut self, transferred_bytes: u64) -> u64 {
        if self.bytes_at_last_failure.is_some_and(|previous| transferred_bytes > previous) {
            self.current_ms = INITIAL_BACKOFF_MS;
        }
        self.bytes_at_last_failure = Some(transferred_bytes);
        let delay = self.current_ms;
        self.current_ms = next_backoff_ms(self.current_ms);
        delay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_without_progress_and_resets_after_progress() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next_delay_ms(0), 1_000);
        assert_eq!(backoff.next_delay_ms(0), 2_000);
        assert_eq!(backoff.next_delay_ms(0), 4_000);
        // 连上了但一个字节都没多传：继续增长，不因为「连接成功」而重置。
        assert_eq!(backoff.next_delay_ms(0), 8_000);
        assert_eq!(backoff.next_delay_ms(0), MAX_BACKOFF_MS);
        // 有新进度后回到初始值。
        assert_eq!(backoff.next_delay_ms(1), INITIAL_BACKOFF_MS);
        // 续传回退到更早的块边界不算进度。
        assert_eq!(backoff.next_delay_ms(0), 2_000);
    }
}
