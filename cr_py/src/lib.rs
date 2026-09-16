//! PyO3 封装：把 SimWorld 暴露给 Python 训练框架
//!
//! 接口设计：
//! - 动作是整数索引：4 卡槽 × 112 部署格 + 1 不出牌 = 449
//! - 部署格 ↔ 坐标的映射、动作合法性掩码都在 Rust 侧算好
//!   （Python 只跟索引打交道，不需要懂游戏规则）

use bevy_hello::cards::deploy_allowed;
use bevy_hello::components::Faction;
use bevy_hello::constants::CARDS;
use bevy_hello::sim_env::{EnvAction, SimWorld, OBS_SIZE};
use pyo3::prelude::*;

/// 部署网格：8 列 × 14 行，覆盖自己半场
pub const N_COLS: usize = 8;
pub const N_ROWS: usize = 14;
pub const N_CELLS: usize = N_COLS * N_ROWS;
pub const N_ACTIONS: usize = 4 * N_CELLS + 1;

fn faction_sign(faction: Faction) -> f32 {
    match faction {
        Faction::Player => -1.0,
        Faction::Enemy => 1.0,
    }
}

/// 格子 → 世界坐标（x 列均分 [-7,7]，z 行覆盖 [sign*2, sign*14]）
fn cell_to_pos(faction: Faction, cell: usize) -> (f32, f32) {
    let col = (cell % N_COLS) as f32;
    let row = (cell / N_COLS) as f32;
    let x = -7.0 + col * (14.0 / (N_COLS - 1) as f32);
    let z = faction_sign(faction) * (2.0 + row * (12.0 / (N_ROWS - 1) as f32));
    (x, z)
}

fn idx_to_action(faction: Faction, idx: usize) -> Option<EnvAction> {
    if idx >= 4 * N_CELLS {
        return None; // 不出牌
    }
    let (x, z) = cell_to_pos(faction, idx % N_CELLS);
    Some(EnvAction {
        slot: idx / N_CELLS,
        x,
        z,
    })
}

// unsendable：bevy App 内含非 Send 成员；GIL 保证单线程访问即可
#[pyclass(unsendable)]
struct CrEnv {
    world: SimWorld,
}

#[pymethods]
impl CrEnv {
    #[new]
    fn new() -> Self {
        Self {
            world: SimWorld::new(),
        }
    }

    /// 训练用短局版本（ticks 为常规时长，如 2700 = 90 秒）
    #[staticmethod]
    fn short(ticks: u32) -> Self {
        Self {
            world: SimWorld::new().with_regular_ticks(ticks),
        }
    }

    #[getter]
    fn obs_size(&self) -> usize {
        OBS_SIZE
    }

    #[getter]
    fn n_actions(&self) -> usize {
        N_ACTIONS
    }

    /// 开一局；seed 驱动洗牌
    fn reset(&mut self, seed: u32) -> Vec<f32> {
        self.world.reset(seed)
    }

    /// 双方动作索引 → (obs, reward, done, winner)
    /// winner: 1=蓝(Player) -1=红(Enemy) 0=平/未结束
    fn step(&mut self, blue_action: usize, red_action: usize) -> (Vec<f32>, f32, bool, i8) {
        let res = self.world.step(
            idx_to_action(Faction::Player, blue_action),
            idx_to_action(Faction::Enemy, red_action),
        );
        let winner = match res.winner {
            Some(Faction::Player) => 1,
            Some(Faction::Enemy) => -1,
            None => 0,
        };
        (res.obs, res.reward, res.done, winner)
    }

    /// 动作合法性掩码（faction: 0=蓝 1=红）：
    /// 卡槽需圣水足够；格子需部署规则允许（deploy_allowed 同一套规则）
    fn action_mask(&mut self, faction: u8) -> Vec<bool> {
        let faction = if faction == 0 {
            Faction::Player
        } else {
            Faction::Enemy
        };
        let elixir = self.world.elixir(faction);
        let towers = self.world.tower_snaps();
        let mut mask = vec![false; N_ACTIONS];
        for slot in 0..4 {
            let Some(card_id) = self.world.hand_card(faction, slot) else {
                continue;
            };
            let cost = CARDS[card_id as usize].cost;
            if elixir < cost {
                continue;
            }
            for cell in 0..N_CELLS {
                let (x, z) = cell_to_pos(faction, cell);
                if deploy_allowed(faction, bevy::math::Vec3::new(x, 0.0, z), &towers) {
                    mask[slot * N_CELLS + cell] = true;
                }
            }
        }
        mask[N_ACTIONS - 1] = true; // 不出牌永远合法
        mask
    }
}

#[pymodule]
fn cr_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CrEnv>()?;
    Ok(())
}
