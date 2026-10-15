// SPDX-License-Identifier: GPL-3.0-or-later
//! 纸笔的核心：分块存储、笔画引擎、文档与撤销。不依赖任何平台接口。
//!
//! 设计依据是 `docs/spec/` 下的行为规格和其中列出的公开文献。

pub mod document;
pub mod fill;
pub mod filter;
pub mod format;
pub mod lang;
pub mod liquify;
pub mod ora;
pub mod psd;
pub mod selection;
pub mod stabilizer;
pub mod stroke;
pub mod tile;
pub mod transform;
pub mod view;
pub mod warp;

/// 15 位定点里的 1.0。通道值的范围是 0..=ONE。
pub const ONE: u32 = 1 << 15;

/// 画布坐标系里的轴对齐矩形，右、下边界不含。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub const EMPTY: Rect = Rect { x0: 0, y0: 0, x1: 0, y1: 0 };

    pub fn is_empty(&self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    pub fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        Rect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }

    pub fn intersect(self, o: Rect) -> Rect {
        let r = Rect { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) };
        if r.is_empty() {
            Rect::EMPTY
        } else {
            r
        }
    }
}
