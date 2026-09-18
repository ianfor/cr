//! PyO3 封装：把 SimWorld 暴露给 Python 训练框架
//!
//! 接口设计：
//! - 动作是整数索引：4 卡槽 × 112 部署格 + 1 不出牌（sim_env::N_ACTIONS）
//! - 部署格 ↔ 坐标的映射、动作合法性掩码都在 Rust 侧算好
//!   （Python 只跟索引打交道，不需要懂游戏规则）

use bevy_hello::components::Faction;
use bevy_hello::sim_env::{idx_to_action, SimWorld, N_ACTIONS, OBS_SIZE};
use pyo3::prelude::*;

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

    /// 指定阵营视角的观测（自我对弈时红方对手用它，而不是蓝方视角）
    fn obs_for(&mut self, faction: u8) -> Vec<f32> {
        let faction = if faction == 0 {
            Faction::Player
        } else {
            Faction::Enemy
        };
        self.world.obs_for(faction)
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
        bevy_hello::sim_env::action_mask(self.world.world_mut(), faction)
    }

    /// 脚本化课程对手（faction: 0=蓝 1=红）：囤水到 8 打最贵牌压敌方弱侧桥头。
    /// 返回动作索引（含 NOOP）；训练时由 Python 侧决定何时调用
    fn scripted_action(&mut self, faction: u8) -> usize {
        let faction = if faction == 0 {
            Faction::Player
        } else {
            Faction::Enemy
        };
        bevy_hello::sim_env::scripted_action(self.world.world_mut(), faction)
    }

    /// 重放单机录像抽取 BC 决策点样本（player: 0=蓝 1=红，人类玩家阵营）。
    /// 返回 (样本列表[(obs, action)], 注入指令数, 实际执行数)；
    /// 注入 != 执行 = 牌库偏离（联网局无种子），该局应作废
    fn bc_replay(
        &mut self,
        path: &str,
        player: u8,
    ) -> Option<(Vec<(Vec<f32>, usize)>, usize, usize)> {
        let faction = if player == 0 {
            Faction::Player
        } else {
            Faction::Enemy
        };
        self.world.bc_replay(path, faction)
    }
}

#[pymodule]
fn cr_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CrEnv>()?;
    Ok(())
}
