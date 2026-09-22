//! 均匀网格空间索引（AOI）：索敌最近邻 / 推挤邻域 / 溅射圆域共用。
//!
//! 为什么是均匀网格而不是四叉树/kd 树/sweep-prune：
//! 场地固定有界、查询半径同量级（推挤 ~2 / aggro ~7），
//! 每帧 O(n) 重建 + O(邻域密度) 查询，数组连续缓存友好。
//!
//! 帧同步确定性铁律：
//! - **禁止 HashMap 迭代**（RandomState 每进程随机种子，两端迭代序不同 → 失同步）。
//!   本结构是固定维度桶数组：行主序遍历、桶内按插入序（= 快照序）——全部固定
//! - 每帧 clear + 重插（Vec 复用容量不重分配）；只索引怪物，
//!   塔/建筑卡 ≤ 12 个且不动，最近邻仍走线性扫描
//! - 坐标越界钳到边缘格（投影到有界凸集不放大距离，环数上限仍覆盖一切
//!   能过精确过滤的候选）
//!
//! 语义注意（SIM_VERSION 变更原因）：最近邻的等距平局裁决顺序
//! 环形扫描序 ≠ 旧线性全扫序；推挤力的浮点累加顺序同理由升序对变为环序。

use bevy::prelude::*;

use crate::constants::*;

use super::UnitSnap;

/// 列数 / 行数（由常量算出，编译期固定）
const COLS: usize = ((GRID_MAX_X - GRID_MIN_X) / GRID_CELL) as usize;
const ROWS: usize = ((GRID_MAX_Z - GRID_MIN_Z) / GRID_CELL) as usize;

/// 空间网格：buckets 按 z * COLS + x 行主序排列
#[derive(Default)]
pub struct SpatialGrid {
    buckets: Vec<Vec<u32>>,
}

impl SpatialGrid {
    /// 坐标 → 格子（越界钳到边缘格）。前提：x ≥ GRID_MIN_X（截断即取整）
    fn cell_coord(&self, pos: Vec3) -> (usize, usize) {
        let x = (((pos.x - GRID_MIN_X) / GRID_CELL) as isize).clamp(0, COLS as isize - 1) as usize;
        let z = (((pos.z - GRID_MIN_Z) / GRID_CELL) as isize).clamp(0, ROWS as isize - 1) as usize;
        (x, z)
    }

    /// 每帧重建的第一步：清空全部桶（首次调用分配布局，之后复用容量）
    pub(crate) fn clear(&mut self) {
        if self.buckets.is_empty() {
            self.buckets = vec![Vec::new(); COLS * ROWS];
        } else {
            for b in &mut self.buckets {
                b.clear();
            }
        }
    }

    /// 插入一个快照下标（必须在 clear 之后调用）
    pub(crate) fn insert(&mut self, pos: Vec3, idx: u32) {
        let (x, z) = self.cell_coord(pos);
        self.buckets[z * COLS + x].push(idx);
    }

    /// 最近邻：从自身格向外扩环扫描，找到即早退。
    /// - 比较用 3D 距离平方（对齐旧线性全扫比较器，y 参与远近）
    /// - 剪枝用 xz：第 r+1 环候选的 xz 距离下界 = r × GRID_CELL，
    ///   已找到的 3D 最优 ≤ 该下界平方时可停（3D ≥ xz，不可能更近）
    /// - `filter` 做全部精确判定（阵营/对空/edge_dist ≤ range），
    ///   `max_center` 必须覆盖一切能过 filter 的候选中心距上限：
    ///   调用方传 range + self_radius + MONSTER_RADIUS_MAX
    pub(crate) fn query_nearest(
        &self,
        snaps: &[UnitSnap],
        pos: Vec3,
        max_center: f32,
        filter: &dyn Fn(&UnitSnap) -> bool,
    ) -> Option<(f32, u32)> {
        if self.buckets.is_empty() {
            return None;
        }
        let (cx, cz) = self.cell_coord(pos);
        let rings = (max_center / GRID_CELL).ceil() as usize;
        let mut best: Option<(f32, u32)> = None;
        for r in 0..=rings {
            if r == 0 {
                self.scan_cell(snaps, pos, filter, cx, cz, &mut best);
            } else {
                // 第 r 环周界（行序：上→下；边行整行扫，中间行只扫左右列，
                // 被 0/ROWS-1 钳掉的边行同样要扫左右列——否则贴边格漏查）
                let x_lo = cx.saturating_sub(r);
                let x_hi = (cx + r).min(COLS - 1);
                let z_from = cz.saturating_sub(r);
                let z_to = (cz + r).min(ROWS - 1);
                for z in z_from..=z_to {
                    if z + r == cz || z == cz + r {
                        // 顶/底边行（|z − cz| == r）：整行
                        for x in x_lo..=x_hi {
                            self.scan_cell(snaps, pos, filter, x, z, &mut best);
                        }
                    } else {
                        // 中间行（含被边界钳位替代的边行）：左右列
                        if cx >= r {
                            self.scan_cell(snaps, pos, filter, x_lo, z, &mut best);
                        }
                        if cx + r < COLS {
                            self.scan_cell(snaps, pos, filter, x_hi, z, &mut best);
                        }
                    }
                }
            }
            // 早退：下一环不可能严格更近
            if let Some((b, _)) = best {
                let bound = r as f32 * GRID_CELL;
                if b <= bound * bound {
                    break;
                }
            }
        }
        best
    }

    /// 扫单个格子：filter 精确判定 + 3D 距离平方比较（严格小于，先见者优先）
    fn scan_cell(
        &self,
        snaps: &[UnitSnap],
        pos: Vec3,
        filter: &dyn Fn(&UnitSnap) -> bool,
        x: usize,
        z: usize,
        best: &mut Option<(f32, u32)>,
    ) {
        for &i in &self.buckets[z * COLS + x] {
            let s = &snaps[i as usize];
            if !filter(s) {
                continue;
            }
            let d2 = pos.distance_squared(s.pos);
            if best.map_or(true, |(b, _)| d2 < b) {
                *best = Some((d2, i));
            }
        }
    }

    /// 圆域邻域遍历：覆盖圆的外接方格（行主序），精确距离由调用方判定。
    /// 推挤（j > i 去重每对一次）与溅射用
    pub(crate) fn for_each_in_circle(&self, pos: Vec3, radius: f32, f: &mut dyn FnMut(u32)) {
        if self.buckets.is_empty() {
            return;
        }
        let (cx, cz) = self.cell_coord(pos);
        let r = (radius / GRID_CELL).ceil() as usize;
        let x_lo = cx.saturating_sub(r).min(COLS - 1);
        let x_hi = (cx + r).min(COLS - 1);
        let z_lo = cz.saturating_sub(r).min(ROWS - 1);
        let z_hi = (cz + r).min(ROWS - 1);
        for z in z_lo..=z_hi {
            for x in x_lo..=x_hi {
                for &i in &self.buckets[z * COLS + x] {
                    f(i);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Faction, UnitKind};
    use crate::constants::{CardKind, CARDS};

    fn snap(i: usize, pos: Vec3, faction: Faction, flying: bool) -> UnitSnap {
        UnitSnap {
            entity: Entity::from_raw_u32(i as u32).unwrap(),
            kind: UnitKind::Troop,
            faction,
            pos,
            radius: 0.5,
            mass: 1.0,
            flying,
        }
    }

    fn dist_xz(a: Vec3, b: Vec3) -> f32 {
        let d = a - b;
        (d.x * d.x + d.z * d.z).sqrt()
    }

    /// 网格最近邻 == 暴力最近邻（距离值相等；等距平局允许下标不同）
    #[test]
    fn query_nearest_matches_brute_force() {
        let mut rng = 0x9E37_79B9u32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        for case in 0..40 {
            let mut grid = SpatialGrid::default();
            let mut snaps = Vec::new();
            for i in 0..50 {
                let x = (next() % 3200) as f32 / 100.0 - 16.0;
                let z = (next() % 4800) as f32 / 100.0 - 24.0;
                let flying = next() % 4 == 0;
                let y = if flying { 2.6 } else { 1.0 };
                let faction = if i % 2 == 0 {
                    Faction::Player
                } else {
                    Faction::Enemy
                };
                snaps.push(snap(i, Vec3::new(x, y, z), faction, flying));
            }
            grid.clear();
            for (i, s) in snaps.iter().enumerate() {
                grid.insert(s.pos, i as u32);
            }

            let pos = Vec3::new(
                (next() % 3200) as f32 / 100.0 - 16.0,
                1.0,
                (next() % 4800) as f32 / 100.0 - 24.0,
            );
            // 模拟真实用法：filter 内含 edge_dist ≤ range 的精确判定，
            // max_center = range + self_r + MONSTER_RADIUS_MAX 覆盖一切能过 filter 的候选
            let (self_r, range) = (0.5, 6.0);
            let filter = |s: &UnitSnap| {
                s.faction == Faction::Enemy && dist_xz(pos, s.pos) - self_r - s.radius <= range
            };
            let got = grid.query_nearest(&snaps, pos, range + self_r + MONSTER_RADIUS_MAX, &filter);
            let want = snaps
                .iter()
                .enumerate()
                .filter(|(_, s)| filter(s))
                .map(|(i, s)| (pos.distance_squared(s.pos), i as u32))
                .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            match (got, want) {
                (None, None) => {}
                (Some((dg, _)), Some((dw, _))) => {
                    assert!(
                        (dg - dw).abs() < 1e-9,
                        "case {case}: 最近距离不一致 {dg} vs {dw}"
                    );
                }
                (g, w) => panic!("case {case}: 结果缺失 got={g:?} want={w:?}"),
            }
        }
    }

    /// 圆域遍历 == 暴力圆域（下标集合相等）
    #[test]
    fn circle_query_matches_brute_force() {
        let mut rng = 0xDEAD_BEEFu32;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng
        };
        for case in 0..40 {
            let mut grid = SpatialGrid::default();
            let mut snaps = Vec::new();
            for i in 0..50 {
                let x = (next() % 3200) as f32 / 100.0 - 16.0;
                let z = (next() % 4800) as f32 / 100.0 - 24.0;
                snaps.push(snap(i, Vec3::new(x, 1.0, z), Faction::Player, false));
            }
            grid.clear();
            for (i, s) in snaps.iter().enumerate() {
                grid.insert(s.pos, i as u32);
            }
            let pos = Vec3::new(
                (next() % 3200) as f32 / 100.0 - 16.0,
                1.0,
                (next() % 4800) as f32 / 100.0 - 24.0,
            );
            let radius = (next() % 500) as f32 / 100.0 + 0.5;
            let mut got: Vec<u32> = Vec::new();
            grid.for_each_in_circle(pos, radius, &mut |i| {
                if dist_xz(pos, snaps[i as usize].pos) <= radius {
                    got.push(i);
                }
            });
            let want: Vec<u32> = snaps
                .iter()
                .enumerate()
                .filter(|(_, s)| dist_xz(pos, s.pos) <= radius)
                .map(|(i, _)| i as u32)
                .collect();
            let mut got_sorted = got.clone();
            got_sorted.sort_unstable();
            let mut want_sorted = want.clone();
            want_sorted.sort_unstable();
            assert_eq!(got_sorted, want_sorted, "case {case}: 圆域结果不一致");
        }
    }

    /// 卡牌半径上限守卫：网格查询半径补偿项必须覆盖所有卡牌的怪物半径
    #[test]
    fn monster_radius_bound_covers_all_cards() {
        for c in CARDS.iter() {
            if let CardKind::Troop(spec) = &c.kind {
                assert!(
                    spec.radius <= MONSTER_RADIUS_MAX,
                    "{} 半径 {} 超出 MONSTER_RADIUS_MAX {}",
                    c.name,
                    spec.radius,
                    MONSTER_RADIUS_MAX
                );
            }
        }
    }
}
