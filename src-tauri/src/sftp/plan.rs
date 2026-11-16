//! 多连接并行传输的调度：哪些字节已经完成、剩下的怎么分给各个连接。
//!
//! 丢包链路上单条 TCP 的吞吐会被拥塞控制压得很低，多开几条连接各传文件的不同部分，是弱网下
//! 提速最直接的办法。为此进度不能再用「已传到第几字节」这一个数表示，而要记录「哪些区间已完成」；
//! 没传的部分切成小段放进队列，每个连接领一段、传完再领，谁快谁多传，某个连接断了它手里的
//! 那段退回队列由别人接手。只开一个连接时，这套逻辑退化成原来的从头到尾顺序传。

use super::transfer_common::CHUNK_SIZE;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 每次领取的分段大小。太大则快慢连接之间分配不均、断线时退回的也多；太小则频繁加锁领取。
pub const PIECE_BYTES: u64 = 8 * CHUNK_SIZE;
/// 小于这个大小的文件只用一个连接：多连一次的握手时间比并行省下的还多。
pub const PARALLEL_MIN_BYTES: u64 = 32 * CHUNK_SIZE;
/// 并行连接数上限：再多收益递减，还容易撞上服务器对单个来源的连接数限制。
pub const MAX_CONNECTIONS: usize = 8;
/// 默认并行连接数。
pub const DEFAULT_CONNECTIONS: usize = 4;

/// 小于这个大小的文件不做「与目标端旧版本逐块比对」：比对本身的开销已经接近直接传一遍。
pub const DELTA_MIN_BYTES: u64 = 8 * CHUNK_SIZE;

/// 实际使用的连接数：小文件不并行，请求值限制在合理范围内。
pub fn effective_connections(requested: usize, file_size: u64) -> usize {
    if file_size < PARALLEL_MIN_BYTES {
        1
    } else {
        requested.clamp(1, MAX_CONNECTIONS)
    }
}

/// 已完成的字节区间：按起点排序、互不重叠、相邻的已合并。边界都落在块边界上（文件末尾除外）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RangeSet(Vec<(u64, u64)>);

impl RangeSet {
    /// 文件开头连续的 `len` 字节已完成——旧版断点只记录了这一个数。
    pub fn prefix(len: u64) -> Self {
        if len == 0 { Self::default() } else { Self(vec![(0, len)]) }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn ranges(&self) -> &[(u64, u64)] {
        &self.0
    }

    pub fn total(&self) -> u64 {
        self.0.iter().map(|(start, end)| end - start).sum()
    }

    pub fn insert(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        self.0.push((start, end));
        self.0.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.0.len());
        for &(start, end) in &self.0 {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        self.0 = merged;
    }

    /// 丢掉 `limit` 之后的部分：目标端实际只有这么多数据时，断点声称的更靠后的进度不可信。
    /// 截断点向下取整到块边界，半块数据一律重传。
    pub fn truncate(&mut self, limit: u64) {
        // 只对「被截到」的区间取整：完整保留下来的区间可以以文件末尾那个不满一块的尾巴结束。
        let cut = limit - limit % CHUNK_SIZE;
        self.0.retain_mut(|(start, end)| {
            if *end > limit {
                *end = cut;
            }
            *start < *end
        });
    }

    /// `[0, size)` 中尚未完成的区间。
    pub fn gaps(&self, size: u64) -> Vec<(u64, u64)> {
        let mut gaps = Vec::new();
        let mut cursor = 0;
        for &(start, end) in &self.0 {
            if start > cursor {
                gaps.push((cursor, start.min(size)));
            }
            cursor = cursor.max(end);
        }
        if cursor < size {
            gaps.push((cursor, size));
        }
        gaps.retain(|(start, end)| start < end);
        gaps
    }
}

/// `TransferPlan::status` 的结果，含义与 `Claim` 的三种情况对应。
#[derive(Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Work,
    Wait,
    Done,
}

/// 领取任务的结果。
pub enum Claim<'a, E> {
    /// 领到一段，传完（或中途放弃）时由 `Piece` 自己归还状态。
    Piece(Piece<'a, E>),
    /// 队列空了，但别的连接手里还有没传完的段——它们若断线，那些段会退回来，所以先等等。
    Wait,
    /// 全部完成，或传输已经结束（成功、失败、取消）。
    Done,
}

struct PlanState<E> {
    /// 首个连接完成准备工作（确定目标、核对已有进度）之后才有任务可领。
    ready: bool,
    pending: VecDeque<(u64, u64)>,
    in_flight: usize,
    finished: bool,
    /// 只由主连接完成（文件太小，不值得为它多连几次）。
    exclusive: bool,
    /// 辅助连接遇到的「重连也不会好」的错误，交给主连接上报。
    fatal: Option<E>,
}

/// 各连接共享的传输计划。
pub struct TransferPlan<E> {
    state: Mutex<PlanState<E>>,
    /// 各连接正在传、尚未完成的那一块里已经传了多少字节，只用于让进度显示更平滑。
    partial: AtomicU64,
}

impl<E> TransferPlan<E> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PlanState {
                ready: false,
                pending: VecDeque::new(),
                in_flight: 0,
                finished: false,
                exclusive: false,
                fatal: None,
            }),
            partial: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PlanState<E>> {
        // 持锁期间只做队列操作，不会 panic；锁中毒时里面的数据仍然一致。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 准备完成：把未完成的区间切成小段放进队列。
    pub fn start(&self, gaps: &[(u64, u64)]) {
        let mut state = self.lock();
        state.pending.clear();
        for &(start, end) in gaps {
            let mut cursor = start;
            while cursor < end {
                let piece_end = (cursor + PIECE_BYTES).min(end);
                state.pending.push_back((cursor, piece_end));
                cursor = piece_end;
            }
        }
        state.ready = true;
    }

    /// 与 `start` 相同，但任务只留给主连接：下载要连上之后才知道文件大小，
    /// 发现是小文件时用它让已经起好的辅助线程直接退出，不去建立连接。
    pub fn start_exclusive(&self, gaps: &[(u64, u64)]) {
        self.start(gaps);
        self.lock().exclusive = true;
    }

    pub fn claim(&self) -> Claim<'_, E> {
        let mut state = self.lock();
        if state.finished || state.fatal.is_some() {
            return Claim::Done;
        }
        match state.pending.pop_front() {
            Some((start, end)) => {
                state.in_flight += 1;
                Claim::Piece(Piece { plan: self, next: start, end, partial: 0 })
            }
            None if state.in_flight > 0 || !state.ready => Claim::Wait,
            None => Claim::Done,
        }
    }

    /// 不领取任务，只看现在有没有活可干。辅助连接据此决定要不要建立连接：
    /// 没有任务时白白握手一次，既浪费时间也占用服务器的连接名额。
    pub fn status(&self) -> PlanStatus {
        let state = self.lock();
        if state.finished || state.fatal.is_some() || state.exclusive {
            PlanStatus::Done
        } else if !state.pending.is_empty() {
            PlanStatus::Work
        } else if state.in_flight > 0 || !state.ready {
            PlanStatus::Wait
        } else {
            PlanStatus::Done
        }
    }

    /// 传输结束（无论结果）：让还在等任务的连接退出。
    pub fn finish(&self) {
        self.lock().finished = true;
    }

    /// 辅助连接上报不可恢复的错误；只保留第一个。
    pub fn fail(&self, error: E) {
        let mut state = self.lock();
        if state.fatal.is_none() {
            state.fatal = Some(error);
        }
    }

    pub fn take_fatal(&self) -> Option<E> {
        self.lock().fatal.take()
    }

    /// 正在传的块里已传的字节数之和（见 `partial` 字段）。
    pub fn partial_bytes(&self) -> u64 {
        self.partial.load(Ordering::Relaxed)
    }
}

/// 领到的一段任务。无论以何种方式结束（传完、出错、取消），析构时都会把没传完的部分退回队列，
/// 所以调用方不必在每个出错分支里手动归还。
pub struct Piece<'a, E> {
    plan: &'a TransferPlan<E>,
    /// 下一个要传的块的起点。
    pub next: u64,
    pub end: u64,
    partial: u64,
}

impl<E> Piece<'_, E> {
    /// 当前块传到一半时更新平滑进度。
    pub fn set_partial(&mut self, bytes: u64) {
        if bytes >= self.partial {
            self.plan.partial.fetch_add(bytes - self.partial, Ordering::Relaxed);
        } else {
            self.plan.partial.fetch_sub(self.partial - bytes, Ordering::Relaxed);
        }
        self.partial = bytes;
    }

    /// 一块传完并已记入断点：前进到下一块。
    pub fn advance(&mut self, len: u64) {
        self.set_partial(0);
        self.next += len;
    }
}

impl<E> Drop for Piece<'_, E> {
    fn drop(&mut self) {
        self.set_partial(0);
        let mut state = self.plan.lock();
        state.in_flight -= 1;
        if self.next < self.end {
            // 放回队首：让文件尽量按顺序完成，已完成区间也更容易合并。
            state.pending.push_front((self.next, self.end));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = CHUNK_SIZE;

    #[test]
    fn ranges_merge_and_report_gaps() {
        let mut done = RangeSet::default();
        done.insert(2 * MB, 3 * MB);
        done.insert(0, MB);
        assert_eq!(done.ranges(), &[(0, MB), (2 * MB, 3 * MB)]);
        assert_eq!(done.gaps(5 * MB), vec![(MB, 2 * MB), (3 * MB, 5 * MB)]);
        done.insert(MB, 2 * MB);
        assert_eq!(done.ranges(), &[(0, 3 * MB)]);
        assert_eq!(done.total(), 3 * MB);
        assert!(RangeSet::prefix(5 * MB).gaps(5 * MB).is_empty());
    }

    #[test]
    fn truncate_drops_progress_beyond_what_exists() {
        let mut done = RangeSet::default();
        done.insert(0, 2 * MB);
        done.insert(4 * MB, 6 * MB);
        // 目标端只有 4.5 MB：第二段只剩半块，整段作废。
        done.truncate(4 * MB + MB / 2);
        assert_eq!(done.ranges(), &[(0, 2 * MB)]);
        done.truncate(MB + 1);
        assert_eq!(done.ranges(), &[(0, MB)]);
        // 文件末尾不满一块、而目标端正好有这么多：不算半块，原样保留。
        let mut whole = RangeSet::prefix(5 * MB + 17);
        whole.truncate(5 * MB + 17);
        assert_eq!(whole.ranges(), &[(0, 5 * MB + 17)]);
    }

    #[test]
    fn unfinished_part_of_a_piece_returns_to_the_queue() {
        let plan: TransferPlan<()> = TransferPlan::new();
        assert!(matches!(plan.claim(), Claim::Wait), "nothing to claim before the plan is ready");
        plan.start(&[(0, PIECE_BYTES + MB)]);
        let Claim::Piece(mut first) = plan.claim() else { panic!("expected a piece") };
        assert_eq!((first.next, first.end), (0, PIECE_BYTES));
        first.advance(MB);
        let Claim::Piece(second) = plan.claim() else { panic!("expected a piece") };
        assert!(matches!(plan.claim(), Claim::Wait), "others still hold pieces");
        drop(first);
        let Claim::Piece(retry) = plan.claim() else { panic!("the rest of the dropped piece must come back") };
        assert_eq!((retry.next, retry.end), (MB, PIECE_BYTES));
        let mut retry = retry;
        retry.next = retry.end;
        drop(retry);
        let mut second = second;
        second.next = second.end;
        drop(second);
        assert!(matches!(plan.claim(), Claim::Done));
    }

    #[test]
    fn small_files_use_a_single_connection() {
        assert_eq!(effective_connections(4, PARALLEL_MIN_BYTES - 1), 1);
        assert_eq!(effective_connections(4, PARALLEL_MIN_BYTES), 4);
        assert_eq!(effective_connections(64, PARALLEL_MIN_BYTES), MAX_CONNECTIONS);
        assert_eq!(effective_connections(0, PARALLEL_MIN_BYTES), 1);
    }
}
