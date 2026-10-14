// SPDX-License-Identifier: GPL-3.0-or-later
//! 稀疏分块存储。没画过的地方没有分块，不占内存；分块写时复制，
//! 所以撤销栈只要留着旧分块的引用就行。

use std::collections::HashMap;
use std::sync::Arc;

use crate::Rect;

/// 分块边长（像素）。
pub const TILE: i32 = 64;
const N: usize = (TILE * TILE) as usize;

pub type TileCoord = (i32, i32);

/// 一个 64×64 的 RGBA 分块：预乘透明度，每通道 15 位定点。
#[derive(Clone)]
pub struct Tile(pub Vec<u16>);

impl Tile {
    pub fn new() -> Self {
        Tile(vec![0; N * 4])
    }
}

impl Default for Tile {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Default)]
pub struct TileMap {
    tiles: HashMap<TileCoord, Arc<Tile>>,
}

impl TileMap {
    pub fn get(&self, c: TileCoord) -> Option<&Arc<Tile>> {
        self.tiles.get(&c)
    }

    /// 取可写的分块；不存在就新建一个全透明的，被别处引用着就先复制。
    pub fn get_mut(&mut self, c: TileCoord) -> &mut Tile {
        Arc::make_mut(self.tiles.entry(c).or_insert_with(|| Arc::new(Tile::new())))
    }

    /// 把某个位置换成给定的分块（None 表示空），返回原来的。
    pub fn replace(&mut self, c: TileCoord, t: Option<Arc<Tile>>) -> Option<Arc<Tile>> {
        match t {
            Some(t) => self.tiles.insert(c, t),
            None => self.tiles.remove(&c),
        }
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    /// 所有存在的分块的坐标，顺序不定。
    pub fn coords(&self) -> impl Iterator<Item = TileCoord> + '_ {
        self.tiles.keys().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }
}

/// 单通道的稀疏分块：一笔之内的覆盖率（0..=ONE）。
#[derive(Default)]
pub struct MaskMap {
    tiles: HashMap<TileCoord, Vec<u16>>,
}

impl MaskMap {
    pub fn get(&self, c: TileCoord) -> Option<&[u16]> {
        self.tiles.get(&c).map(Vec::as_slice)
    }

    pub fn get_mut(&mut self, c: TileCoord) -> &mut [u16] {
        self.tiles.entry(c).or_insert_with(|| vec![0; N])
    }

    pub fn coords(&self) -> impl Iterator<Item = TileCoord> + '_ {
        self.tiles.keys().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    /// 一次拿到多个分块的可写引用（没有的先建成全零），可以分给几个线程各写各的。
    pub fn many_mut(&mut self, coords: &[TileCoord]) -> Vec<(TileCoord, &mut [u16])> {
        for &c in coords {
            self.tiles.entry(c).or_insert_with(|| vec![0; N]);
        }
        let wanted: std::collections::HashSet<TileCoord> = coords.iter().copied().collect();
        self.tiles.iter_mut().filter(|(c, _)| wanted.contains(*c)).map(|(c, t)| (*c, t.as_mut_slice())).collect()
    }
}

/// 矩形碰到的所有分块坐标。
pub fn tiles_in(r: Rect) -> impl Iterator<Item = TileCoord> {
    let (tx0, ty0) = (r.x0.div_euclid(TILE), r.y0.div_euclid(TILE));
    let (tx1, ty1) = ((r.x1 - 1).div_euclid(TILE), (r.y1 - 1).div_euclid(TILE));
    let empty = r.is_empty();
    (ty0..=ty1).flat_map(move |ty| (tx0..=tx1).map(move |tx| (tx, ty))).filter(move |_| !empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_on_write_keeps_the_old_tile() {
        let mut m = TileMap::default();
        m.get_mut((0, 0)).0[0] = 100;
        let snapshot = m.get((0, 0)).cloned().unwrap();
        m.get_mut((0, 0)).0[0] = 200;
        assert_eq!(snapshot.0[0], 100);
        assert_eq!(m.get((0, 0)).unwrap().0[0], 200);
    }

    #[test]
    fn tiles_in_handles_negative_coordinates() {
        let v: Vec<_> = tiles_in(Rect { x0: -1, y0: 0, x1: 65, y1: 1 }).collect();
        assert_eq!(v, vec![(-1, 0), (0, 0), (1, 0)]);
        assert_eq!(tiles_in(Rect::EMPTY).count(), 0);
    }
}
