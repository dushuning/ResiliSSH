//! 后端面向用户文案的中英切换。
//!
//! 错误信息、活动日志等由后端生成后直接展示给用户，必须跟随界面语言。
//! 语言由前端在启动和切换时通过 `set_language` 告知；用全局开关而不是层层传参，
//! 原因同 activity：生成文案的代码散落在调用链深处。

use std::sync::atomic::{AtomicBool, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);

pub fn set_english(english: bool) {
    ENGLISH.store(english, Ordering::Relaxed);
}

pub fn is_english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// 按当前界面语言二选一：`tr!(中文, English)`。只求值被选中的那个分支，
/// 所以两边都可以直接写 `format!(...)` 而不会多做一次格式化。
#[macro_export]
macro_rules! tr {
    ($zh:expr, $en:expr $(,)?) => {
        if $crate::i18n::is_english() {
            $en
        } else {
            $zh
        }
    };
}
