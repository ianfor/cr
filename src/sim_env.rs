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
/// 观测向量长度 = 全局 42 维 + 单位 20 × 8 维
/// 全局：圣水(存量×2/回复进度/倍率) 4 + 手牌 4 张卡种 one-hot 16 + 下一张 one-hot 4
///       + 阶段 one-hot 3 + 计时 1 + 塔血 6 + 塔交战 6 + 双方单位计数 2
/// 单位：阵营 1 + 卡种 one-hot 4 + 坐标 2 + 血量 1
pub const OBS_SIZE: usize = 42 + MAX_OBS_UNITS * 8;

// ===== 动作空间映射（训练与游戏内机器人共用） =====
/// 部署网格列数/行数：覆盖自己半场
pub const N_COLS: usize = 8;
pub const N_ROWS: usize = 14;
pub const N_CELLS: usize = N_COLS * N_ROWS;
/// 4 卡槽 × 112 部署格 + 1 不出牌
pub const N_ACTIONS: usize = 4 * N_CELLS + 1;
/// "不出牌"动作索引
pub const NOOP_ACTION: usize = N_ACTIONS - 1;

// ===== 脚本对手用的卡 id（与 constants.rs 的 CARDS 对应） =====
pub const KNIGHT_CARD: u8 = 0;
pub const MUSKETEER_CARD: u8 = 2;
pub const GIANT_CARD: u8 = 3;

fn faction_sign(faction: Faction) -> f32 {
    match faction {
        Faction::Player => -1.0,
        Faction::Enemy => 1.0,
    }
}

/// 格子 → 世界坐标（x 列均分 [-7,7]，z 行覆盖 [sign*2, sign*14]）
pub fn cell_to_pos(faction: Faction, cell: usize) -> (f32, f32) {
    let col = (cell % N_COLS) as f32;
    let row = (cell / N_COLS) as f32;
    let x = -7.0 + col * (14.0 / (N_COLS - 1) as f32);
    let z = faction_sign(faction) * (2.0 + row * (12.0 / (N_ROWS - 1) as f32));
    (x, z)
}

/// 动作索引 → EnvAction（超出出牌区 = None 不出牌）
pub fn idx_to_action(faction: Faction, idx: usize) -> Option<EnvAction> {
    if idx >= 4 * N_CELLS {
        return None;
    }
    let (x, z) = cell_to_pos(faction, idx % N_CELLS);
    Some(EnvAction {
        slot: idx / N_CELLS,
        x,
        z,
    })
}

// ===== 直接操作 World 的自由函数（SimWorld 与游戏内机器人共用） =====

/// 某方手牌槽位对应的卡 id
pub fn hand_card(world: &mut World, faction: Faction, slot: usize) -> Option<u8> {
    let decks = world.resource::<Decks>();
    decks.queue(faction).get(slot).copied()
}

/// 某方当前圣水
pub fn elixir_of(world: &mut World, faction: Faction) -> f32 {
    let e = world.resource::<Elixir>();
    match faction {
        Faction::Player => e.player,
        Faction::Enemy => e.enemy,
    }
}

/// 塔快照 (faction, is_king, pos)
pub fn tower_snaps(world: &mut World) -> Vec<(Faction, bool, Vec3)> {
    let mut q = world.query::<(&Tower, &Transform, Option<&KingTower>)>();
    q.iter(world)
        .map(|(t, tr, k)| (t.faction, k.is_some(), tr.translation))
        .collect()
}

/// 动作合法性掩码：卡槽需圣水足够；格子需部署规则允许
pub fn action_mask(world: &mut World, faction: Faction) -> Vec<bool> {
    let elixir = elixir_of(world, faction);
    let towers = tower_snaps(world);
    let mut mask = vec![false; N_ACTIONS];
    for slot in 0..4 {
        let Some(card_id) = hand_card(world, faction, slot) else {
            continue;
        };
        let cost = CARDS[card_id as usize].cost;
        if elixir < cost {
            continue;
        }
        for cell in 0..N_CELLS {
            let (x, z) = cell_to_pos(faction, cell);
            if cards::deploy_allowed(faction, Vec3::new(x, 0.0, z), &towers) {
                mask[slot * N_CELLS + cell] = true;
            }
        }
    }
    mask[NOOP_ACTION] = true; // 不出牌永远合法
    mask
}

/// 定长观测向量：flip=false 蓝方视角，flip=true 红方镜像视角
///
/// 布局（索引）：
/// 0-3   圣水：己方存量、对方存量、己方距下一点进度、回复倍率(/3)
/// 4-23  手牌 4 张 + 下一张，各占卡种 one-hot(CARDS.len())=4 维
/// 24-27 对局阶段 one-hot(3) + 剩余时间
/// 28-33 塔血：己方 王/左/右，对方 王/左/右（flip 时全场 180° 旋转，左右互换）
/// 34-39 塔交战状态：是否锁定目标（有目标=1）
/// 40-41 双方场上单位数(/MAX_OBS_UNITS)
/// 42+   单位 ×20：阵营、卡种 one-hot(4)、x(/9)、z(/15)、血量
pub fn compute_obs(world: &mut World, flip: bool) -> Vec<f32> {
    let mut v = vec![0.0f32; OBS_SIZE];

    let elixir = world.resource::<Elixir>();
    let (own_e, opp_e) = if flip {
        (elixir.enemy, elixir.player)
    } else {
        (elixir.player, elixir.enemy)
    };
    v[0] = own_e / ELIXIR_MAX;
    v[1] = opp_e / ELIXIR_MAX;
    v[2] = own_e.fract();

    let timer = world.resource::<MatchTimer>();
    v[3] = match_flow::elixir_multiplier(&timer) / 3.0;

    let own_faction = if flip {
        Faction::Enemy
    } else {
        Faction::Player
    };
    let decks = world.resource::<Decks>();
    let queue = decks.queue(own_faction);
    // 手牌 4 张 + 下一张（各 4 维 one-hot）
    for (i, c) in queue.iter().take(HAND_SIZE + 1).enumerate() {
        let base = 4 + i * CARDS.len();
        v[base + (*c as usize).min(CARDS.len() - 1)] = 1.0;
    }

    v[24] = (timer.phase == MatchPhase::Regular) as u8 as f32;
    v[25] = (timer.phase == MatchPhase::Overtime) as u8 as f32;
    v[26] = (timer.phase == MatchPhase::Drain) as u8 as f32;
    v[27] = timer.ticks_left as f32 / REGULAR_TICKS as f32;

    // 塔血 + 交战状态：己方 王/左/右，对方 王/左/右（flip 时全场 180° 旋转，左右互换）
    {
        let mut q = world.query::<(&Tower, &Health, Option<&KingTower>, &Transform)>();
        for (t, h, k, tr) in q.iter(world) {
            let same_side = t.faction == own_faction;
            let mut x = tr.translation.x;
            if flip {
                x = -x;
            }
            let side = match (same_side, k.is_some()) {
                (true, true) => 0,
                (true, false) => 1 + (x > 0.0) as usize,
                (false, true) => 3,
                (false, false) => 4 + (x > 0.0) as usize,
            };
            v[28 + side] = (h.current / h.max).clamp(0.0, 1.0);
            v[34 + side] = t.target.is_some() as u8 as f32;
        }
    }

    // 单位：flip 时阵营标签互换、坐标 180° 旋转
    {
        let mut counts = [0usize; 2];
        let mut q = world.query::<(&Monster, &Health, &Transform)>();
        for (i, (m, h, tr)) in q.iter(world).take(MAX_OBS_UNITS).enumerate() {
            let base = 42 + i * 8;
            let own = m.faction == own_faction;
            let (mut x, mut z) = (tr.translation.x, tr.translation.z);
            if flip {
                x = -x;
                z = -z;
            }
            v[base] = if own { 0.0 } else { 1.0 };
            v[base + 1 + (m.card as usize).min(CARDS.len() - 1)] = 1.0;
            v[base + 5] = x / 9.0;
            v[base + 6] = z / 15.0;
            v[base + 7] = (h.current / h.max).clamp(0.0, 1.0);
            counts[own as usize] += 1;
        }
        v[40] = counts[0] as f32 / MAX_OBS_UNITS as f32;
        v[41] = counts[1] as f32 / MAX_OBS_UNITS as f32;
    }
    v
}

/// 脚本化课程对手（只供训练用，不进游戏/模拟链）：
/// CR 基本功——"坦克+远程"组合拳，实测 vs 随机 60%（孤身巨人只有 36%）：
/// 1. 巨人在手且圣水 ≥5 → 顶敌方弱侧桥头（z=2 吸塔伤）
/// 2. 火枪手在手且圣水 ≥4 → 跟场上巨人同路的后排（z=5，躲巨人后面输出）
/// 3. 骑士在手且圣水 ≥8 → 富余圣水补一波前排
/// 其余情况挂机囤水。路线：默认右路，敌方右塔被我方打伤后换攻左路（牵制空档）。
pub fn scripted_action(world: &mut World, faction: Faction) -> usize {
    let elixir = elixir_of(world, faction);

    // 进攻方向：默认主攻右路；当敌方右塔血量低于左塔（被我方打伤）时换攻左路。
    // 实测（200 局）这种"打伤一路就换路"≈53.5%，死磕单路/打弱侧≈38%——
    // 换路能牵制对手把防守资源堆到受伤一侧后的空档
    let enemy = if faction == Faction::Player {
        Faction::Enemy
    } else {
        Faction::Player
    };
    let mut enemy_left_hp = f32::INFINITY;
    let mut enemy_right_hp = f32::INFINITY;
    {
        let mut q = world.query::<(&Tower, &Health, Option<&KingTower>, &Transform)>();
        for (t, h, k, tr) in q.iter(world) {
            if t.faction == enemy && k.is_none() {
                if tr.translation.x < 0.0 {
                    enemy_left_hp = h.current;
                } else {
                    enemy_right_hp = h.current;
                }
            }
        }
    }
    let lane_x = if enemy_right_hp < enemy_left_hp {
        BRIDGES[0]
    } else {
        BRIDGES[1]
    };

    let towers = tower_snaps(world);
    // (卡 id, 圣水门槛, 目标 z) 的出牌优先级：巨人前排 → 火枪后排 → 骑士补刀
    let plays: [(u8, f32, f32); 3] = [
        (GIANT_CARD, 5.0, 2.0),
        (MUSKETEER_CARD, 4.0, 5.0),
        (KNIGHT_CARD, 8.0, 2.0),
    ];
    for &(card, min_elixir, z) in &plays {
        if elixir < min_elixir {
            continue;
        }
        let Some(slot) = (0..HAND_SIZE).find(|&s| hand_card(world, faction, s) == Some(card)) else {
            continue;
        };
        // 落点：目标点（桥头路线上）附近最近的合法格
        let sign = faction_sign(faction);
        let target_z = sign * z;
        let mut best_cell = None;
        let mut best_d = f32::MAX;
        for cell in 0..N_CELLS {
            let (x, cz) = cell_to_pos(faction, cell);
            if !cards::deploy_allowed(faction, Vec3::new(x, 0.0, cz), &towers) {
                continue;
            }
            let d = (x - lane_x) * (x - lane_x) + (cz - target_z) * (cz - target_z);
            if d < best_d {
                best_d = d;
                best_cell = Some(cell);
            }
        }
        if let Some(cell) = best_cell {
            return slot * N_CELLS + cell;
        }
    }
    NOOP_ACTION
}

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
        // 释放上一局累计的渲染资产：每次出牌/单位出生都会 meshes.add / materials.add
        // 新资产，而 Assets 存储只增不减（实体 despawn 不释放资产）——
        // 游戏内一局几百个无所谓，训练连跑几千局会无界增长直到 OOM。
        // 无头模拟不渲染，整体换新对模拟零影响（旧 handle 悬空无害）。
        *self.app.world_mut().resource_mut::<Assets<Mesh>>() = Assets::default();
        *self.app.world_mut().resource_mut::<Assets<StandardMaterial>>() = Assets::default();
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

        // 奖励（蓝方视角）：
        // - 塔血差 shaping ×0.0005：与"赢"最对齐的稠密信号（拆满一侧面 ≈ ±11，
        //   典型胜局净差 4000 HP ≈ +2.0，量级压过其他 shaping 但不超过胜负和太多）
        // - 出牌激励 ×0.004：只做"别囤死水"的引导（×57 次/局 ≈ +0.23，
        //   原 0.02 时 ≈ +1.14 与胜负 ±1 同量级，策略会被"刷出牌"绑架）
        // - 囤水罚/击杀交换：保持不变
        let now = self.tower_hp_sums();
        let mut reward = (self.tower_hp[1] - now[1] - (self.tower_hp[0] - now[0])) * 0.0005;
        self.tower_hp = now;

        // 出牌激励：本步内蓝方实际执行的指令数（被 play_card 接受的）
        let log = &self.app.world().resource::<CommandLog>().0;
        let deployed_blue = log[log_before..]
            .iter()
            .filter(|(_, c)| matches!(c, GameCommand::Deploy { faction: Faction::Player, .. }))
            .count();
        reward += deployed_blue as f32 * 0.004;
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

    /// 蓝方视角定长观测向量
    pub fn obs(&mut self) -> Vec<f32> {
        self.obs_impl(false)
    }

    /// 指定阵营视角的观测（自我对弈用）：
    /// 红方视角 = 圣水/手牌换成自己的、塔序己方在前、全场 180° 旋转
    pub fn obs_for(&mut self, faction: Faction) -> Vec<f32> {
        self.obs_impl(faction == Faction::Enemy)
    }

    fn obs_impl(&mut self, flip: bool) -> Vec<f32> {
        compute_obs(self.app.world_mut(), flip)
    }

    pub fn tick(&self) -> u32 {
        self.app.world().resource::<Tick>().0
    }

    /// 访问内部 ECS World（cr_py / 游戏内机器人用）
    pub fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }

    pub fn world(&self) -> &World {
        self.app.world()
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

    /// 观测布局完整性：维度正确、手牌/卡种 one-hot 结构、塔血/计数就位、镜像视角维度一致
    #[test]
    fn obs_layout_is_sound() {
        let mut w = SimWorld::new();
        let obs = w.obs();
        assert_eq!(obs.len(), OBS_SIZE);
        // 手牌 4 张 + 下一张：5 组 one-hot，各恰好一个 1
        for i in 0..5 {
            let block = &obs[4 + i * 4..4 + (i + 1) * 4];
            assert_eq!(block.iter().sum::<f32>(), 1.0, "第 {} 组卡种应为 one-hot", i);
            assert!(block.iter().all(|&x| x == 0.0 || x == 1.0));
        }
        // 初始塔满血、未交战
        assert!(obs[28..34].iter().all(|&x| x == 1.0));
        assert!(obs[34..40].iter().all(|&x| x == 0.0));
        // 场上无单位
        assert_eq!(obs[40], 0.0);
        assert_eq!(obs[41], 0.0);
        // 圣水初值 5/10
        assert_eq!(obs[0], ELIXIR_START / ELIXIR_MAX);
        // 红方镜像视角：维度一致，圣水字段取自红方
        let red = w.obs_for(Faction::Enemy);
        assert_eq!(red.len(), OBS_SIZE);
        assert_eq!(red[0], obs[1]);
        assert_eq!(red[1], obs[0]);
    }

    /// reset 必须释放上一局累计的渲染资产（否则训练长跑内存无界增长）
    #[test]
    fn reset_frees_visual_assets() {
        let mut w = SimWorld::new();
        // 出一张牌：Deploying 幽灵 + 落地单位都会 meshes.add / materials.add
        let act = Some(EnvAction {
            slot: 0,
            x: 0.0,
            z: -5.0,
        });
        for _ in 0..4 {
            w.step(act, None);
        }
        let meshes = w.world_mut().resource_mut::<Assets<Mesh>>().len();
        let mats = w
            .world_mut()
            .resource_mut::<Assets<StandardMaterial>>()
            .len();
        assert!(meshes > 0, "出牌后应存在渲染资产，实际 {}", meshes);
        assert!(mats > 0, "出牌后应存在材质，实际 {}", mats);
        w.reset(1);
        assert_eq!(w.world_mut().resource::<Assets<Mesh>>().len(), 0);
        assert_eq!(
            w.world_mut().resource::<Assets<StandardMaterial>>().len(),
            0
        );
        // 清空后能继续正常出牌（新资产照常创建）
        for _ in 0..4 {
            w.step(act, None);
        }
        assert!(w.world_mut().resource::<Assets<Mesh>>().len() > 0);
    }

    /// 脚本对手：圣水低于最低出牌门槛（4）时挂机；囤到 9 必出合法组合动作
    #[test]
    fn scripted_opponent_deploys_when_rich() {
        let mut w = SimWorld::new();
        // 圣水 3：低于所有出牌门槛（巨人5/火枪4/骑士8）→ 挂机
        w.world_mut().resource_mut::<Elixir>().enemy = 3.0;
        assert_eq!(scripted_action(w.world_mut(), Faction::Enemy), NOOP_ACTION);
        // 攒到 9：必有可出组合动作（手牌 4 张里必有非骷髅牌）
        w.world_mut().resource_mut::<Elixir>().enemy = 9.0;
        let a = scripted_action(w.world_mut(), Faction::Enemy);
        assert_ne!(a, NOOP_ACTION);
        let mask = action_mask(w.world_mut(), Faction::Enemy);
        assert!(mask[a], "脚本动作必须落在合法掩码内");
    }

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
