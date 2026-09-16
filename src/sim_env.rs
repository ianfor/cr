//! 强化学习环境：把游戏模拟封装成 reset / step / obs / reward 的可编程接口
//!
//! 设计要点：
//! - 无头：MinimalPlugins + SimTick 链，不需要 GPU/窗口，全场模拟约 1~3 秒
//! - 决策点离散化：1 step = 15 tick（0.5 秒游戏时间），一局约 400 步
//! - 动作走与玩家点击完全相同的指令流（GameCommand::Deploy），
//!   费用由 play_card 权威校验，非法动作自然丢弃
//! - 后续 Python 对接（PyO3）只需薄包一层本模块

use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;
use crate::match_flow::{MatchPhase, MatchTimer};
use crate::net::SimState;
use crate::replay::SimTick;
use crate::{arena, cards, combat, elixir, match_flow, net};

/// 一次决策：出手牌 slot 到 (x, z)；None = 本步不出牌
#[derive(Clone, Copy, Debug)]
pub struct EnvAction {
    pub slot: usize,
    pub x: f32,
    pub z: f32,
}

/// 决策步长（tick）：0.5 秒游戏时间
pub const STEP_TICKS: u32 = 15;
/// 观测向量中记录的最多单位数
const MAX_OBS_UNITS: usize = 20;
/// 观测向量长度
pub const OBS_SIZE: usize = 17 + MAX_OBS_UNITS * 5;

pub struct StepResult {
    pub obs: Vec<f32>,
    /// 蓝方（Player）视角奖励：胜负 ±1 + 塔血差 shaping
    pub reward: f32,
    pub done: bool,
    pub winner: Option<Faction>,
}

/// 无头模拟世界：一个实例一局，reset 后可复用
pub struct SimWorld {
    app: App,
    /// 双方塔血总量快照（shaping 用）
    tower_hp: [f32; 2],
    /// 当局步数（防异常长局）
    steps: u32,
    /// 训练用短局时长（None = 正式 3 分钟）
    regular_ticks: Option<u32>,
}

impl Default for SimWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl SimWorld {
    pub fn new() -> Self {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(SimState::Solo)
            .insert_resource(Elixir {
                player: ELIXIR_START,
                enemy: ELIXIR_START,
            })
            .insert_resource(Decks::shuffled())
            .init_resource::<Tick>()
            .init_resource::<PendingClicks>()
            .init_resource::<CommandBuffer>()
            .init_resource::<CommandLog>()
            .init_resource::<net::OwnHashes>()
            .init_resource::<MatchTimer>()
            .init_resource::<combat::ProjectileAssets>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .add_systems(Startup, arena::setup)
            .add_systems(
                SimTick,
                (
                    combat::collect_inputs,
                    combat::apply_commands,
                    cards::process_deploying,
                    combat::monster_ai,
                    combat::tower_ai,
                    combat::move_projectiles,
                    combat::separate_monsters,
                    combat::separate_from_towers,
                    combat::keep_out_of_river,
                    combat::despawn_dead,
                    match_flow::tick_timer,
                    combat::check_game_over,
                    net::notify_game_over,
                    elixir::regen,
                    net::send_hash,
                    combat::advance_tick,
                )
                    .chain(),
            );
        let mut w = Self {
            app,
            tower_hp: [0.0; 2],
            steps: 0,
            regular_ticks: None,
        };
        w.reset(0);
        w
    }

    /// 训练用短局（如 90 秒）：单局更短，单位步数内样本更多、信用分配更容易
    pub fn with_regular_ticks(mut self, ticks: u32) -> Self {
        self.regular_ticks = Some(ticks);
        self
    }

    /// 重置对局：deck_seed 驱动洗牌（训练时每局变化，评估时固定）
    pub fn reset(&mut self, deck_seed: u32) -> Vec<f32> {
        crate::replay::reset_world(self.app.world_mut());
        *self.app.world_mut().resource_mut::<Decks>() = Decks::shuffled_with(deck_seed);
        // reset_world 不清对局状态，上一局的 GameOver 必须手动复位
        *self.app.world_mut().resource_mut::<SimState>() = SimState::Solo;
        if let Some(t) = self.regular_ticks {
            self.app.world_mut().resource_mut::<MatchTimer>().ticks_left = t;
        }
        self.tower_hp = self.tower_hp_sums();
        self.steps = 0;
        self.obs()
    }

    /// 双方各给一个动作（None = 不出牌），推进 STEP_TICKS 个模拟帧
    pub fn step(&mut self, blue: Option<EnvAction>, red: Option<EnvAction>) -> StepResult {
        self.steps += 1;
        let tick = self.app.world().resource::<Tick>().0 + 1;
        let log_before = self.app.world().resource::<CommandLog>().0.len();
        for (faction, act) in [(Faction::Player, blue), (Faction::Enemy, red)] {
            if let Some(a) = act {
                if let Some(cmd) = self.action_to_command(faction, a) {
                    self.app
                        .world_mut()
                        .resource_mut::<CommandBuffer>()
                        .local
                        .entry(tick)
                        .or_default()
                        .push(cmd);
                }
            }
        }

        // 推进到下一决策点（或提前结束）
        let end_tick = tick + STEP_TICKS - 1;
        let monsters_before = self.count_monsters();
        loop {
            let state = *self.app.world().resource::<SimState>();
            if matches!(state, SimState::GameOver(_)) {
                break;
            }
            let _ = self.app.world_mut().try_run_schedule(SimTick);
            if self.app.world().resource::<Tick>().0 >= end_tick {
                break;
            }
        }

        // 奖励：塔血差 shaping + 终局胜负 + 圣水使用引导 + 击杀交换
        let now = self.tower_hp_sums();
        let mut reward = (self.tower_hp[1] - now[1] - (self.tower_hp[0] - now[0])) * 0.0002;
        self.tower_hp = now;

        // 出牌激励：本步内蓝方实际执行的指令数（被 play_card 接受的）
        let log = &self.app.world().resource::<CommandLog>().0;
        let deployed_blue = log[log_before..]
            .iter()
            .filter(|(_, c)| matches!(c, GameCommand::Deploy { faction: Faction::Player, .. }))
            .count();
        reward += deployed_blue as f32 * 0.02;
        // 囤水惩罚：圣水满着不用就是浪费
        if self.app.world().resource::<Elixir>().player >= ELIXIR_MAX - 1e-6 {
            reward -= 0.005;
        }
        // 击杀交换 shaping：本步双方阵亡数差（击杀密度是这场游戏最重要的局部信号）
        let after = self.count_monsters();
        let log = &self.app.world().resource::<CommandLog>().0;
        let spawned_red = log[log_before..]
            .iter()
            .filter(|(_, c)| matches!(c, GameCommand::Deploy { faction: Faction::Enemy, .. }))
            .count();
        let spawned_blue = log[log_before..]
            .iter()
            .filter(|(_, c)| matches!(c, GameCommand::Deploy { faction: Faction::Player, .. }))
            .count();
        let red_deaths =
            (monsters_before[1] + spawned_red).saturating_sub(after[1]) as f32;
        let blue_deaths =
            (monsters_before[0] + spawned_blue).saturating_sub(after[0]) as f32;
        reward += (red_deaths - blue_deaths) * 0.01;
        let (done, winner) = match *self.app.world().resource::<SimState>() {
            SimState::GameOver(w) => (true, w),
            // 兜底：异常长局强制结束（理论上计时系统会终结对局）
            _ if self.steps > 2000 => (true, None),
            _ => (false, None),
        };
        if done {
            reward += match winner {
                Some(Faction::Player) => 1.0,
                Some(Faction::Enemy) => -1.0,
                None => 0.0,
            };
        }
        StepResult {
            obs: self.obs(),
            reward,
            done,
            winner,
        }
    }

    fn action_to_command(&self, faction: Faction, a: EnvAction) -> Option<GameCommand> {
        let decks = self.app.world().resource::<Decks>();
        let card = *decks.queue(faction).get(a.slot)?;
        Some(GameCommand::Deploy {
            faction,
            card,
            x: a.x,
            z: a.z,
        })
    }

    /// 双方场上怪物数量 [blue, red]
    fn count_monsters(&mut self) -> [usize; 2] {
        let mut counts = [0usize; 2];
        let mut q = self.app.world_mut().query::<&Monster>();
        for m in q.iter(self.app.world()) {
            counts[m.faction.index() as usize] += 1;
        }
        counts
    }

    fn tower_hp_sums(&mut self) -> [f32; 2] {
        let mut sums = [0.0; 2];
        let mut q = self
            .app
            .world_mut()
            .query::<(&Tower, &Health)>();
        for (t, h) in q.iter(self.app.world()) {
            sums[t.faction.index() as usize] += h.current.max(0.0);
        }
        sums
    }

    /// 蓝方视角定长观测向量（红方训练时用镜像坐标即可）
    pub fn obs(&mut self) -> Vec<f32> {
        let mut v = vec![0.0f32; OBS_SIZE];
        let world = self.app.world_mut();

        let elixir = world.resource::<Elixir>();
        v[0] = elixir.player / ELIXIR_MAX;
        v[1] = elixir.enemy / ELIXIR_MAX;

        let decks = world.resource::<Decks>();
        for (i, c) in decks.player.iter().take(HAND_SIZE).enumerate() {
            v[2 + i] = *c as f32 / (CARDS.len() - 1) as f32;
        }
        v[6] = decks.player[HAND_SIZE] as f32 / (CARDS.len() - 1) as f32;

        let timer = world.resource::<MatchTimer>();
        v[7] = (timer.phase == MatchPhase::Regular) as u8 as f32;
        v[8] = (timer.phase == MatchPhase::Overtime) as u8 as f32;
        v[9] = (timer.phase == MatchPhase::Drain) as u8 as f32;
        v[10] = timer.ticks_left as f32 / REGULAR_TICKS as f32;

        // 塔血：蓝王/蓝左/蓝右/红王/红左/红右（hp/max，死亡为 0）
        {
            let mut q = world.query::<(&Tower, &Health, Option<&KingTower>, &Transform)>();
            for (t, h, k, tr) in q.iter(world) {
                let side = match (t.faction, k.is_some()) {
                    (Faction::Player, true) => 0,
                    (Faction::Player, false) => 1 + (tr.translation.x > 0.0) as usize,
                    (Faction::Enemy, true) => 3,
                    (Faction::Enemy, false) => 4 + (tr.translation.x > 0.0) as usize,
                };
                v[11 + side] = (h.current / h.max).clamp(0.0, 1.0);
            }
        }

        // 单位：按生成顺序取前 MAX_OBS_UNITS 个
        {
            let mut q = world.query::<(&Monster, &Health, &Transform)>();
            for (i, (m, h, tr)) in q.iter(world).take(MAX_OBS_UNITS).enumerate() {
                let base = 17 + i * 5;
                v[base] = m.faction.index() as f32;
                v[base + 1] = m.damage / 200.0; // 用伤害近似卡种区分度
                v[base + 2] = tr.translation.x / 9.0;
                v[base + 3] = tr.translation.z / 15.0;
                v[base + 4] = (h.current / h.max).clamp(0.0, 1.0);
            }
        }
        v
    }

    pub fn tick(&self) -> u32 {
        self.app.world().resource::<Tick>().0
    }

    /// 某方手牌槽位对应的卡 id（动作掩码用）
    pub fn hand_card(&self, faction: Faction, slot: usize) -> Option<u8> {
        let decks = self.app.world().resource::<Decks>();
        decks.queue(faction).get(slot).copied()
    }

    /// 某方当前圣水（动作掩码用）
    pub fn elixir(&self, faction: Faction) -> f32 {
        let e = self.app.world().resource::<Elixir>();
        match faction {
            Faction::Player => e.player,
            Faction::Enemy => e.enemy,
        }
    }

    /// 塔快照 (faction, is_king, pos)（部署区域判定用）
    pub fn tower_snaps(&mut self) -> Vec<(Faction, bool, Vec3)> {
        let mut q = self
            .app
            .world_mut()
            .query::<(&Tower, &Transform, Option<&KingTower>)>();
        q.iter(self.app.world())
            .map(|(t, tr, k)| (t.faction, k.is_some(), tr.translation))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 随机策略自对弈：环境能跑完整局并给出胜负
    #[test]
    fn random_self_play_completes_episode() {
        let mut w = SimWorld::new();
        // 缩短常规时间让测试快一点
        w.app.world_mut().resource_mut::<MatchTimer>().ticks_left = 600;
        let t0 = std::time::Instant::now();
        let mut result = None;
        for i in 0..2000 {
            // 简单伪随机动作：每 4 步双方各出一次牌（位置固定几种）
            let act = |salt: u32, z: f32| {
                (i % 4 == 0).then(|| EnvAction {
                    slot: ((i + salt) % 4) as usize,
                    x: ((i * 7 + salt) % 13) as f32 - 6.0,
                    z,
                })
            };
            let r = w.step(act(1, -5.0), act(3, 5.0));
            if r.done {
                result = Some(r);
                break;
            }
        }
        let result = result.expect("对局应该结束");
        let secs = t0.elapsed().as_secs_f32();
        println!(
            "episode: {} ticks in {:.2}s ({:.0} ticks/s), winner: {:?}",
            w.tick(),
            secs,
            w.tick() as f32 / secs,
            result.winner
        );
        assert!(w.tick() > 0);
    }
}
