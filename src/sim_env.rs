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
// ===== 观测网格（v4：21 卡种全量 one-hot 网格化观测） =====
/// 网格列数（与部署格一致）
pub const GRID_COLS: usize = N_COLS;
/// 网格行数：己方半场 14 行 + 敌方半场 14 行
pub const GRID_ROWS: usize = 2 * N_ROWS;
/// 每格通道数：己方 21 卡种 + 敌方 21 卡种 + 塔 4 通道
pub const GRID_CHANNELS: usize = 2 * CARDS.len() + 4;
/// 全局段维度：圣水 4 + 手牌 5 张×21 one-hot 105 + 阶段 3 + 计时 1 + 单位数 2
pub const GLOBAL_SIZE: usize = 4 + (HAND_SIZE + 1) * CARDS.len() + 3 + 1 + 2;
/// 观测向量长度 = 网格 8×28×46 + 全局 115 = 10419
///
/// 网格（行动视角，flip 时全场 180° 旋转）：行 0 贴河（己方），行 13 己方底线，
/// 行 14 敌方贴河，行 27 敌方底线；己方 14 行与部署动作格一一对应
/// （动作 = 卡槽 × 观测网格己方半场的格子）。
/// 通道：[己方卡种 0..21 计数, 敌方卡种 0..21 计数, 己方塔血, 敌方塔血,
///        己方王塔, 敌方王塔]——建筑卡按其卡种通道计数（可被"看见"）
///
/// 全局：圣水(存量×2/回复进度/倍率) 4 + 手牌 4 张+下一张卡种 one-hot 21
///       + 阶段 one-hot 3 + 计时 1 + 双方单位总数 2
///
/// 设计动机：v2 的 20 个单位槽位按 ECS 遍历序排列——同一局面输入不同，
/// MLP 学不稳；网格按位置数数，天然置换不变且空间局部。
/// v3 → v4：4 卡扩到 21 卡，卡种通道与 one-hot 相应扩宽（旧模型全部作废，
/// 维度守卫会拒载旧权重）。
pub const OBS_SIZE: usize = GRID_COLS * GRID_ROWS * GRID_CHANNELS + GLOBAL_SIZE;
/// 单位总数归一化上限
const UNIT_COUNT_NORM: usize = 30;
/// 每格每卡种计数归一化上限（野蛮人一张 4 只）
const CELL_COUNT_NORM: f32 = 4.0;

/// 通道索引
const CH_OWN_CARD: usize = 0;
const CH_ENEMY_CARD: usize = CARDS.len();
const CH_OWN_TOWER: usize = 2 * CARDS.len();
const CH_ENEMY_TOWER: usize = 2 * CARDS.len() + 1;
const CH_OWN_KING: usize = 2 * CARDS.len() + 2;
const CH_ENEMY_KING: usize = 2 * CARDS.len() + 3;

/// 世界坐标 → 观测网格行列。先按蓝方视角定桶，flip=true 时在索引层做
/// 镜像（河面反射 + 列反转）。必须索引层镜像而不能"视角坐标取负再定桶"：
/// x=0（王塔）/x=±2 等恰在两列格心之间的 tie，取负后 tie-break 两次落同列，
/// 会破坏红蓝观测的严格镜像对称
fn grid_cell(x: f32, z: f32, flip: bool) -> (usize, usize) {
    let col = ((x + 7.0) / 2.0)
        .round()
        .clamp(0.0, (GRID_COLS - 1) as f32) as usize;
    let row = if z < 0.0 {
        ((-z - 2.0) * (13.0 / 12.0))
            .round()
            .clamp(0.0, (N_ROWS - 1) as f32) as usize
    } else {
        (N_ROWS as f32 + (z - 2.0) * (13.0 / 12.0))
            .round()
            .clamp(N_ROWS as f32, (GRID_ROWS - 1) as f32) as usize
    };
    if !flip {
        (row, col)
    } else {
        let row_m = if row < N_ROWS {
            N_ROWS + row
        } else {
            row - N_ROWS
        };
        (row_m, GRID_COLS - 1 - col)
    }
}

fn grid_idx(row: usize, col: usize, ch: usize) -> usize {
    GLOBAL_SIZE + (row * GRID_COLS + col) * GRID_CHANNELS + ch
}

fn faction_sign(faction: Faction) -> f32 {
    match faction {
        Faction::Player => -1.0,
        Faction::Enemy => 1.0,
    }
}

/// 格子 → 世界坐标（x 列均分 [-7,7]，z 行覆盖 [sign*2, sign*14]）
///
/// 红方（Enemy）的动作列做 x 镜像：动作格列 c 落在世界 x=7-2c，与红方视角
/// 观测的列 c 完全对齐（观测是 180° 镜像的）。若不镜像，红方模型在视角里
/// 看到"右路的敌人"、出牌却落到左路——攻防全部左右错位（实测执红模型
/// 因此被同水平蓝方打 70%）。蓝方视角观测与动作同为恒等映射，天然对齐。
pub fn cell_to_pos(faction: Faction, cell: usize) -> (f32, f32) {
    let col = (cell % N_COLS) as f32;
    let row = (cell / N_COLS) as f32;
    let x = match faction {
        Faction::Player => -7.0 + col * (14.0 / (N_COLS - 1) as f32),
        Faction::Enemy => 7.0 - col * (14.0 / (N_COLS - 1) as f32),
    };
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
/// 布局：
/// [0, 115)   全局段
///   0-3     圣水：己方存量、对方存量、己方距下一点进度、回复倍率(/3)
///   4-108   手牌 4 张 + 下一张，各占卡种 one-hot(21)
///   109-111 对局阶段 one-hot(3)
///   112     剩余时间
///   113-114 双方场上单位总数(/30，含建筑卡)
/// [115, OBS_SIZE) 网格段：8 列 × 28 行 × 46 通道（行 0 贴河，己方 14 行
///   与部署动作格一一对应；flip 时全场 180° 旋转）
///   通道：己方卡种 0..21 计数、敌方卡种 0..21 计数、己方塔血、敌方塔血、
///         己方王塔标记、敌方王塔标记（计数归一化 /4；建筑卡按卡种入格）
pub fn compute_obs(world: &mut World, flip: bool) -> Vec<f32> {
    let mut v = vec![0.0f32; OBS_SIZE];

    let own_faction = if flip {
        Faction::Enemy
    } else {
        Faction::Player
    };

    // ===== 全局段 =====
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

    let decks = world.resource::<Decks>();
    let queue = decks.queue(own_faction);
    // 手牌 4 张 + 下一张（各 21 维 one-hot）
    for (i, c) in queue.iter().take(HAND_SIZE + 1).enumerate() {
        let base = 4 + i * CARDS.len();
        v[base + (*c as usize).min(CARDS.len() - 1)] = 1.0;
    }

    v[109] = (timer.phase == MatchPhase::Regular) as u8 as f32;
    v[110] = (timer.phase == MatchPhase::Overtime) as u8 as f32;
    v[111] = (timer.phase == MatchPhase::Drain) as u8 as f32;
    v[112] = timer.ticks_left as f32 / REGULAR_TICKS as f32;

    // ===== 网格段：塔 =====
    // 塔血按位置入格（王塔另有标记通道）；flip 时坐标取反
    {
        let mut q = world.query::<(&Tower, &Health, Option<&KingTower>, &Transform)>();
        for (t, h, k, tr) in q.iter(world) {
            let (row, col) = grid_cell(tr.translation.x, tr.translation.z, flip);
            let own = t.faction == own_faction;
            let hp_ch = if own { CH_OWN_TOWER } else { CH_ENEMY_TOWER };
            let king_ch = if own { CH_OWN_KING } else { CH_ENEMY_KING };
            v[grid_idx(row, col, hp_ch)] = (h.current / h.max).clamp(0.0, 1.0);
            if k.is_some() {
                v[grid_idx(row, col, king_ch)] = 1.0;
            }
        }
    }

    // ===== 网格段：单位（按格计数，置换不变；建筑卡按卡种入格） =====
    let mut total_counts = [0usize; 2];
    {
        let mut q = world.query::<(&Monster, &Transform)>();
        for (m, tr) in q.iter(world) {
            let (row, col) = grid_cell(tr.translation.x, tr.translation.z, flip);
            let own = m.faction == own_faction;
            let ch = if own { CH_OWN_CARD } else { CH_ENEMY_CARD }
                + (m.card as usize).min(CARDS.len() - 1);
            v[grid_idx(row, col, ch)] += 1.0;
            total_counts[own as usize] += 1;
        }
    }
    {
        let mut q = world.query::<(&BuildingCard, &Transform)>();
        for (b, tr) in q.iter(world) {
            let (row, col) = grid_cell(tr.translation.x, tr.translation.z, flip);
            let own = b.faction == own_faction;
            let ch = if own { CH_OWN_CARD } else { CH_ENEMY_CARD }
                + (b.card as usize).min(CARDS.len() - 1);
            v[grid_idx(row, col, ch)] += 1.0;
            total_counts[own as usize] += 1;
        }
    }
    // 计数归一化：每格每卡种最多计 4（野蛮人一张 4 只）
    let count_channels = 2 * CARDS.len();
    for cell in 0..GRID_COLS * GRID_ROWS {
        for ch in 0..count_channels {
            let i = GLOBAL_SIZE + cell * GRID_CHANNELS + ch;
            v[i] = (v[i] / CELL_COUNT_NORM).min(1.0);
        }
    }
    v[113] = (total_counts[0] as f32 / UNIT_COUNT_NORM as f32).min(1.0);
    v[114] = (total_counts[1] as f32 / UNIT_COUNT_NORM as f32).min(1.0);
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
    // 兜底：牌池 21 选 8 常抽不到三张优先卡 → 圣水 ≥9 时出手中任意部队顶前线
    // （防"没抽到组合拳就全程挂机白给"；纯法术手牌则继续囤水）
    if elixir >= 9.0 {
        let sign = faction_sign(faction);
        for slot in 0..HAND_SIZE {
            let Some(card) = hand_card(world, faction, slot) else {
                continue;
            };
            if !matches!(&CARDS[card as usize].kind, CardKind::Troop(_)) {
                continue;
            }
            let mut best_cell = None;
            let mut best_d = f32::MAX;
            for cell in 0..N_CELLS {
                let (x, cz) = cell_to_pos(faction, cell);
                if !cards::deploy_allowed(faction, Vec3::new(x, 0.0, cz), &towers) {
                    continue;
                }
                let d = (x - lane_x) * (x - lane_x) + (cz - sign * 2.0) * (cz - sign * 2.0);
                if d < best_d {
                    best_d = d;
                    best_cell = Some(cell);
                }
            }
            if let Some(cell) = best_cell {
                return slot * N_CELLS + cell;
            }
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
            .init_resource::<combat::WorldSnaps>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .add_systems(Startup, arena::setup)
            .add_systems(
                SimTick,
                (
                    combat::collect_inputs,
                    combat::apply_commands,
                    cards::process_deploying,
                    combat::status_effects,
                    combat::targeting,
                    combat::attacking,
                    combat::moving,
                    combat::building_lifetime,
                    combat::building_spawner,
                    combat::move_projectiles,
                    combat::separate_monsters,
                    combat::separate_from_statics,
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
        // 对称牌库：双方同一洗牌序（仅训练环境；正式对局仍各自洗牌）。
        // 双方摸牌运气差异是胜负 ±1 信号的主要噪声源——技能差被牌运稀释，
        // 实测所有策略（模型/脚本/随机）在非对称牌库下全部挤在 40~55%。
        {
            let same = self.app.world().resource::<Decks>().player.clone();
            self.app.world_mut().resource_mut::<Decks>().enemy = same;
        }
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

    /// 重放一局单机录像，抽取指定阵营（人类玩家）的 BC 决策点样本：
    /// 每 0.5 秒窗口一个 (视角观测, 玩家实际动作索引)。
    /// 窗口内无出牌 → NOOP 样本（教"什么时候不出"的节奏直觉）；
    /// 多条出牌取首条（手牌槽位按窗口起点状态查询，与 obs 一致）。
    ///
    /// 单机/BotMode 局牌库固定种子 42（replays 由 save_replay_on_game_over
    /// 自动落盘）；联网局种子未入录像无法重建——若注入指令数 ≠ 实际执行数
    /// （play_card 拒收 = 牌库偏离），调用方应丢弃该局。
    /// 返回 (样本, 注入指令数, 实际执行数)
    pub fn bc_replay(
        &mut self,
        path: &str,
        player: Faction,
    ) -> Option<(Vec<(Vec<f32>, usize)>, usize, usize)> {
        let log = crate::replay::load_replay_file(path)?;
        let end_tick = log.target.max(log.map.keys().copied().max().unwrap_or(0));
        let total: usize = log.map.values().map(|v| v.len()).sum();

        // 还原真实对局状态：固定种子 + 不对称牌库（训练用的对称牌库是
        // SimWorld::reset 的训练专用行为，真实单机局双方各自洗牌）
        self.reset(42);
        *self.app.world_mut().resource_mut::<Decks>() = Decks::shuffled_with(42);

        let mut samples: Vec<(Vec<f32>, usize)> = Vec::new();
        let flip = player == Faction::Enemy;
        let mut tick = 0u32;
        loop {
            if matches!(
                *self.app.world().resource::<SimState>(),
                SimState::GameOver(_)
            ) || tick > end_tick
            {
                break;
            }
            // 1) 玩家在本窗口的指令（取首条）→ 标签 (card, x, z)
            let mut label = None;
            for t in tick..tick + STEP_TICKS {
                if let Some(cmds) = log.map.get(&t) {
                    for c in cmds {
                        if let GameCommand::Deploy { faction, card, x, z } = *c {
                            if faction == player && label.is_none() {
                                label = Some((card, x, z));
                            }
                        }
                    }
                }
            }
            // 2) obs + 标签 → 动作索引样本
            //    手牌对不上 = 重建偏离 → 跳过本窗口
            let mut sample: Option<usize> = Some(NOOP_ACTION);
            if let Some((card, x, z)) = label {
                let slot = (0..HAND_SIZE)
                    .find(|&s| hand_card(self.app.world_mut(), player, s) == Some(card));
                if let Some(slot) = slot {
                    // 点击坐标 → 最近部署格
                    let mut best = (0usize, f32::MAX);
                    for cell in 0..N_CELLS {
                        let (cx, cz) = cell_to_pos(player, cell);
                        let d = (cx - x) * (cx - x) + (cz - z) * (cz - z);
                        if d < best.1 {
                            best = (cell, d);
                        }
                    }
                    sample = Some(slot * N_CELLS + best.0);
                } else {
                    sample = None;
                }
            }
            if let Some(action) = sample {
                let obs = compute_obs(self.app.world_mut(), flip);
                let mask = action_mask(self.app.world_mut(), player);
                if action == NOOP_ACTION || mask[action] {
                    samples.push((obs, action));
                }
            }

            // 3) 注入本窗口双方全部指令（按录像原始帧号）
            for t in tick..tick + STEP_TICKS {
                if let Some(cmds) = log.map.get(&t) {
                    for c in cmds {
                        self.app
                            .world_mut()
                            .resource_mut::<CommandBuffer>()
                            .local
                            .entry(t)
                            .or_default()
                            .push(*c);
                    }
                }
            }
            // 4) 推进一个决策窗口
            let end = tick + STEP_TICKS - 1;
            loop {
                if matches!(
                    *self.app.world().resource::<SimState>(),
                    SimState::GameOver(_)
                ) {
                    break;
                }
                let _ = self.app.world_mut().try_run_schedule(SimTick);
                if self.app.world().resource::<Tick>().0 >= end {
                    break;
                }
            }
            tick += STEP_TICKS;
        }
        let executed = self.app.world().resource::<CommandLog>().0.len();
        Some((samples, total, executed))
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

    /// 观测布局完整性：维度/one-hot/网格塔位/镜像 180° 对称
    #[test]
    fn obs_layout_is_sound() {
        let mut w = SimWorld::new();
        let obs = w.obs();
        assert_eq!(obs.len(), OBS_SIZE);
        // 手牌 4 张 + 下一张：5 组 one-hot，各恰好一个 1
        for i in 0..5 {
            let block = &obs[4 + i * CARDS.len()..4 + (i + 1) * CARDS.len()];
            assert_eq!(block.iter().sum::<f32>(), 1.0, "第 {} 组卡种应为 one-hot", i);
            assert!(block.iter().all(|&x| x == 0.0 || x == 1.0));
        }
        // 初始满血：己/敌方塔血通道各 3.0（3 座塔），王塔标记各 1.0
        let tower_sum = |v: &[f32], ch: usize| -> f32 {
            (0..GRID_COLS * GRID_ROWS)
                .map(|cell| v[GLOBAL_SIZE + cell * GRID_CHANNELS + ch])
                .sum()
        };
        assert_eq!(tower_sum(&obs, CH_OWN_TOWER), 3.0);
        assert_eq!(tower_sum(&obs, CH_ENEMY_TOWER), 3.0);
        assert_eq!(tower_sum(&obs, CH_OWN_KING), 1.0);
        assert_eq!(tower_sum(&obs, CH_ENEMY_KING), 1.0);
        // 场上无单位：全部卡种通道为 0
        for ch in 0..2 * CARDS.len() {
            assert_eq!(tower_sum(&obs, ch), 0.0);
        }
        assert_eq!(obs[113], 0.0);
        assert_eq!(obs[114], 0.0);
        // 圣水初值 5/10
        assert_eq!(obs[0], ELIXIR_START / ELIXIR_MAX);

        // 红方镜像视角：维度一致；红方视角的"己方塔"应与蓝方视角的"敌方塔"
        // 在 180° 旋转后的格子上一致（(row,col) -> (27-row 映射, 7-col)）
        let red = w.obs_for(Faction::Enemy);
        assert_eq!(red.len(), OBS_SIZE);
        let mirror = |row: usize| if row < N_ROWS { N_ROWS + row } else { row - N_ROWS };
        for row in 0..GRID_ROWS {
            for col in 0..GRID_COLS {
                let (r2, c2) = (mirror(row), GRID_COLS - 1 - col);
                for &(ch_own, ch_enemy) in
                    &[(CH_OWN_TOWER, CH_ENEMY_TOWER), (CH_OWN_KING, CH_ENEMY_KING)]
                {
                    assert_eq!(
                        obs[grid_idx(row, col, ch_own)],
                        red[grid_idx(r2, c2, ch_enemy)],
                        "镜像不对称: ({row},{col}) ch{ch_own}"
                    );
                }
            }
        }
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

    /// 对称牌库：训练环境 reset 后双方牌序完全一致（消除摸牌运气）
    #[test]
    fn reset_gives_symmetric_decks() {
        let mut w = SimWorld::new();
        w.reset(7);
        let decks = w.world().resource::<Decks>();
        assert_eq!(decks.player, decks.enemy, "训练环境双方应共享同一洗牌序");
        // 双方各自出同槽位的牌（各自半场），循环推进后双方队列仍逐位一致
        let blue = Some(EnvAction {
            slot: 0,
            x: 0.0,
            z: -5.0,
        });
        let red = Some(EnvAction {
            slot: 0,
            x: 0.0,
            z: 5.0,
        });
        w.step(blue, red);
        let decks = w.world().resource::<Decks>();
        assert_eq!(decks.player, decks.enemy);
    }

    /// 脚本对手：圣水低于最低出牌门槛（4）时挂机；囤到 9 必出合法组合动作
    #[test]
    fn scripted_opponent_deploys_when_rich() {        let mut w = SimWorld::new();
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

    /// 动作格与观测网格必须视角对齐：任何阵营的动作格 (row, col) 落到世界后，
    /// 经该阵营视角 grid_cell 桶化必须回到同一 (row, col)。红方曾因动作列
    /// 不镜像导致"看右路防左路"（执红模型被同水平蓝方打 70%）
    #[test]
    fn action_grid_aligns_with_view() {
        for &cell in &[0usize, 1, 5, 7, 8, 55, 76, 111] {
            let row = cell / N_COLS;
            let col = cell % N_COLS;
            let (bx, bz) = cell_to_pos(Faction::Player, cell);
            assert_eq!(grid_cell(bx, bz, false), (row, col), "蓝方格 {cell}");
            let (rx, rz) = cell_to_pos(Faction::Enemy, cell);
            assert_eq!(grid_cell(rx, rz, true), (row, col), "红方格 {cell}");
        }
    }

    /// 全链路冒烟：PendingClicks → collect_inputs → apply_commands →
    /// process_deploying → 实体落地（与真实游戏点击完全同路径）
    /// 种子 42 手牌 = 墓碑(建筑)/王子(部队)/箭雨(法术)/骷髅(3只部队)
    #[test]
    fn pending_clicks_full_chain_deploys() {
        let mut w = SimWorld::new();
        w.reset(42);
        for slot in 0..HAND_SIZE {
            let card = w.hand_card(Faction::Player, slot).unwrap();
            // 点前给满圣水：聚焦链路本身，不测圣水经济
            w.world_mut().resource_mut::<Elixir>().player = ELIXIR_MAX;
            w.world_mut()
                .resource_mut::<PendingClicks>()
                .0
                .push(GameCommand::Deploy {
                    faction: Faction::Player,
                    card,
                    x: 0.0,
                    z: -5.0 - slot as f32,
                });
            // 跑一帧让 collect_inputs 打帧号入缓冲
            let _ = w.world_mut().try_run_schedule(SimTick);
        }
        // 跑完 INPUT_DELAY + deploy_ticks，让全部虚影落地
        for _ in 0..50 {
            let _ = w.world_mut().try_run_schedule(SimTick);
        }
        let world = w.world_mut();
        let mut monsters = world.query::<&Monster>();
        let n_monsters = monsters.iter(world).count();
        let mut buildings = world.query::<&BuildingCard>();
        let n_buildings = buildings.iter(world).count();
        assert_eq!(n_buildings, 1, "墓碑应落地为建筑实体");
        assert_eq!(n_monsters, 4, "王子 1 + 骷髅 3（箭雨瞬发不出实体）");
    }

    /// AOI 压测（手动跑：cargo test --lib -- --ignored stress --nocapture）
    /// 300 只不死怪混战 + 推挤 + 攻击，全程满负载跑 600 tick，打印吞吐。
    /// 同一场景用于空间网格（AOI）改造的前后性能对比。
    #[test]
    #[ignore]
    fn stress_300_units_perf() {
        let mut w = SimWorld::new();
        w.reset(0);
        {
            let world = w.world_mut();
            // 塔血拉到不会死：保证全程不死局、单位数恒定（负载稳定可对比）
            let mut towers = world.query::<&mut Health>();
            for mut h in towers.iter_mut(world) {
                h.current = 1e9;
                h.max = 1e9;
            }
            // xorshift32 布点：双方交错混在全场，出生即交战
            let mut rng = 0x1234_5678u32;
            let mut next = || {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                rng
            };
            for i in 0..300 {
                let faction = if i % 2 == 0 {
                    Faction::Player
                } else {
                    Faction::Enemy
                };
                let x = (next() % 1400) as f32 / 100.0 - 7.0;
                let z = (next() % 2000) as f32 / 100.0 - 10.0;
                world.spawn((
                    Monster {
                        faction,
                        card: 0,
                        radius: 0.5,
                        mass: 1.0,
                    },
                    Attacker {
                        damage: 100.0,
                        attack_range: 0.75,
                        interval: 1.0,
                        cooldown: 1.0,
                        splash_radius: 0.0,
                        hits_air: false,
                        ranged: false,
                        target: None,
                        engaged: false,
                    },
                    Targeting(TargetPolicy::Seek {
                        aggro_range: 5.0,
                        building_only: false,
                    }),
                    Mover { speed: 2.0 },
                    Health {
                        current: 1e9,
                        max: 1e9,
                    },
                    Transform::from_xyz(x, 1.0, z),
                ));
            }
        }
        let t0 = std::time::Instant::now();
        for _ in 0..600 {
            let _ = w.world_mut().try_run_schedule(SimTick);
        }
        let secs = t0.elapsed().as_secs_f32();
        let alive = {
            let world = w.world_mut();
            let mut q = world.query::<&Monster>();
            q.iter(world).count()
        };
        println!(
            "stress: 600 ticks, {alive} units alive, {secs:.2}s, {:.0} ticks/s",
            600.0 / secs
        );
        assert_eq!(alive, 300, "不死局：单位必须全部存活");
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
