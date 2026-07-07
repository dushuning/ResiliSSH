/// 传输重试退避：弱网场景下需要快速重连，不宜等太久。
pub const INITIAL_BACKOFF_MS: u64 = 1_000;
/// 上限 15s（原 60s 在多次失败后让用户空等过久）。
pub const MAX_BACKOFF_MS: u64 = 15_000;

/// 指数退避下一档等待时间；成功连上或传完一块后会重置为 INITIAL。
pub fn next_backoff_ms(current_ms: u64) -> u64 {
    current_ms.saturating_mul(2).min(MAX_BACKOFF_MS)
}
