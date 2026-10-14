// SPDX-License-Identifier: GPL-3.0-or-later
//! 界面语言。给用户看的文字都同时写着中文和英文，按这里的开关取其中一种。
//!
//! 开关跟着线程走：界面只在一个线程上产生文字，切换后新显示的文字就是新语言；
//! 测试各在自己的线程里跑，改了开关也不会影响别的测试。

use std::cell::Cell;

thread_local! {
    static ENGLISH: Cell<bool> = const { Cell::new(false) };
}

pub fn set_english(on: bool) {
    ENGLISH.set(on);
}

pub fn english() -> bool {
    ENGLISH.get()
}

/// 按当前语言取一段固定的文字。
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    if english() {
        en
    } else {
        zh
    }
}

/// 按当前语言格式化：`trf!("第 {n} 个图层", "layer {n}")`。两种写法用到的变量要一样。
#[macro_export]
macro_rules! trf {
    ($zh:literal, $en:literal $(, $arg:expr)* $(,)?) => {
        if $crate::lang::english() { format!($en $(, $arg)*) } else { format!($zh $(, $arg)*) }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_current_language() {
        let n = 3;
        assert_eq!((tr("图层", "Layer"), trf!("第 {n} 层", "layer {n}")), ("图层", "第 3 层".to_string()));
        set_english(true);
        assert_eq!((tr("图层", "Layer"), trf!("第 {n} 层", "layer {n}")), ("Layer", "layer 3".to_string()));
        set_english(false);
    }
}
