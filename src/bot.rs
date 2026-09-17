//! PvE 机器人：加载导出的策略权重（JSON），定时推理并下指令
//! 机器人走与人类点击完全相同的指令流（PendingClicks → 帧号 → 执行），
//! 因此录像/回放天然兼容

use bevy::prelude::*;

use crate::components::*;
use crate::net::SimState;
use crate::sim_env::{self, NOOP_ACTION};

/// PvE 模式标记资源：存在时玩家锁蓝方、机器人执红
#[derive(Resource)]
pub struct BotMode;

/// 导出的 MLP 策略权重（SB3 MlpPolicy：OBS_SIZE → 128 → 128 → 449）
#[derive(Resource)]
pub struct BotPolicy {
    w0: Vec<Vec<f32>>,
    b0: Vec<f32>,
    w1: Vec<Vec<f32>>,
    b1: Vec<f32>,
    w2: Vec<Vec<f32>>,
    b2: Vec<f32>,
}

/// 机器人决策状态（每个决策点只推一次）
#[derive(Resource, Default)]
pub struct BotState {
    last_tick: u32,
}

fn json_matrix(v: &serde_json::Value, key: &str) -> Option<Vec<Vec<f32>>> {
    Some(
        v.get(key)?
            .as_array()?
            .iter()
            .map(|row| {
                row.as_array()
                    .unwrap()
                    .iter()
                    .map(|x| x.as_f64().unwrap() as f32)
                    .collect()
            })
            .collect(),
    )
}

fn json_vector(v: &serde_json::Value, key: &str) -> Option<Vec<f32>> {
    Some(
        v.get(key)?
            .as_array()?
            .iter()
            .map(|x| x.as_f64().unwrap() as f32)
            .collect(),
    )
}

impl BotPolicy {
    pub fn load(path: &str) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        let w0 = json_matrix(&v, "w0")?;
        // 维度守卫：权重必须与当前观测布局匹配（旧 117 维权重对新观测会静默乱推）
        if w0.first().map_or(true, |row| row.len() != sim_env::OBS_SIZE) {
            error!(
                "bot 权重输入维 {:?} != 当前观测维 {}（模型与代码版本不匹配）：{}",
                w0.first().map(Vec::len),
                sim_env::OBS_SIZE,
                path
            );
            return None;
        }
        Some(Self {
            w0,
            b0: json_vector(&v, "b0")?,
            w1: json_matrix(&v, "w1")?,
            b1: json_vector(&v, "b1")?,
            w2: json_matrix(&v, "w2")?,
            b2: json_vector(&v, "b2")?,
        })
    }

    /// 前向推理：tanh MLP → 掩码 argmax
    fn forward(&self, obs: &[f32], mask: &[bool]) -> usize {
        let h = linear(&self.w0, &self.b0, obs, true);
        let h = linear(&self.w1, &self.b1, &h, true);
        let logits = linear(&self.w2, &self.b2, &h, false);
        let mut best = NOOP_ACTION;
        let mut best_v = f32::NEG_INFINITY;
        for (i, &l) in logits.iter().enumerate() {
            if mask.get(i).copied().unwrap_or(false) && l > best_v {
                best_v = l;
                best = i;
            }
        }
        best
    }
}

/// y = W x + b（torch Linear 权重为 [out, in] 行主序）
fn linear(w: &[Vec<f32>], b: &[f32], x: &[f32], tanh_act: bool) -> Vec<f32> {
    w.iter()
        .zip(b)
        .map(|(row, bias)| {
            let mut s = *bias;
            for (wi, xi) in row.iter().zip(x) {
                s += wi * xi;
            }
            if tanh_act {
                s.tanh()
            } else {
                s
            }
        })
        .collect()
}

/// 机器人决策（Update，exclusive）：每个决策点（15 tick）推理一次，
/// 以 GameCommand 进入 PendingClicks，与人类点击同一条路径
pub fn bot_think(world: &mut World) {
    if world.get_resource::<BotMode>().is_none() {
        return;
    }
    if !matches!(
        world.resource::<SimState>(),
        SimState::Solo | SimState::Playing
    ) {
        return;
    }
    let tick = world.resource::<Tick>().0;
    if tick % sim_env::STEP_TICKS != 0 {
        return;
    }
    {
        let mut state = world.resource_mut::<BotState>();
        if state.last_tick == tick {
            return;
        }
        state.last_tick = tick;
    }

    let obs = sim_env::compute_obs(world, true);
    let mask = sim_env::action_mask(world, Faction::Enemy);
    let action = world.resource::<BotPolicy>().forward(&obs, &mask);
    if action == NOOP_ACTION {
        return;
    }
    let Some(env_action) = sim_env::idx_to_action(Faction::Enemy, action) else {
        return;
    };
    let Some(card) = sim_env::hand_card(world, Faction::Enemy, env_action.slot) else {
        return;
    };
    world
        .resource_mut::<PendingClicks>()
        .0
        .push(GameCommand::Deploy {
            faction: Faction::Enemy,
            card,
            x: env_action.x,
            z: env_action.z,
        });
    info!(
        "bot 出牌: 卡{} @ ({:.1}, {:.1})",
        card, env_action.x, env_action.z
    );
}
