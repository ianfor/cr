//! ECS 组件与资源定义

use std::collections::HashMap;

use bevy::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Faction {
    Player,
    Enemy,
}

impl Faction {
    /// 中继服务器分配的玩家序号 → 阵营（0 = 蓝方，1 = 红方）
    pub fn from_index(index: u8) -> Option<Faction> {
        match index {
            0 => Some(Faction::Player),
            1 => Some(Faction::Enemy),
            _ => None,
        }
    }

    /// 阵营序号，用于指令排序（保证双方执行顺序一致）
    pub fn index(self) -> u8 {
        match self {
            Faction::Player => 0,
            Faction::Enemy => 1,
        }
    }
}

pub fn faction_color(faction: Faction) -> Color {
    match faction {
        Faction::Player => Color::srgb(0.3, 0.5, 0.9),
        Faction::Enemy => Color::srgb(0.9, 0.3, 0.3),
    }
}

/// 单位类别。rank() 是快照构建的确定性类间排序键（怪→塔→建筑，
/// 对齐旧三 query 拼接序，等距平局 tie-break 依赖它）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnitKind {
    /// 部队（可移动、参与推挤）
    Troop,
    /// 塔（公主塔/国王塔，KingTower marker 另挂）
    Tower,
    /// 建筑卡（加农炮/墓碑，原地守卫、有寿命）
    Building,
}

impl UnitKind {
    /// 类间稳定排序键（快照确定性契约：怪→塔→建筑）
    pub fn rank(self) -> u8 {
        match self {
            UnitKind::Troop => 0,
            UnitKind::Tower => 1,
            UnitKind::Building => 2,
        }
    }
}

/// 场上单位（怪/塔/建筑卡的统一组件）：类别 + 阵营 + 卡种 + 物理尺寸。
/// 战斗机制拆在能力组件上（Attacker/Targeting/Mover/...），
/// 挂什么组件就有什么能力——加新机制 = 加新组件，不改现有类型
#[derive(Component, Clone, Copy)]
pub struct Unit {
    pub kind: UnitKind,
    pub faction: Faction,
    /// 卡牌 id（CARDS 中的索引）：观测用单位类型标识，不参与模拟逻辑。
    /// 塔 = None（Option 而非哨兵：sim_env 有 (card as usize).min(len-1) clamp，
    /// 哨兵会被静默钳成合法通道污染观测）
    pub card: Option<u8>,
    /// 碰撞半径（索敌边缘距离/推挤/静态阻挡共用）
    pub radius: f32,
    /// 质量：推挤时按质量分配力，大质量推开小质量（塔/建筑恒 0）
    pub mass: f32,
}

impl Unit {
    pub fn troop(faction: Faction, card: u8, radius: f32, mass: f32) -> Self {
        Self { kind: UnitKind::Troop, faction, card: Some(card), radius, mass }
    }
    pub fn tower(faction: Faction, radius: f32) -> Self {
        Self { kind: UnitKind::Tower, faction, card: None, radius, mass: 0.0 }
    }
    pub fn building(faction: Faction, card: u8, radius: f32) -> Self {
        Self { kind: UnitKind::Building, faction, card: Some(card), radius, mass: 0.0 }
    }
    /// 是否建筑类目标（塔或建筑卡）
    pub fn is_building_kind(&self) -> bool {
        matches!(self.kind, UnitKind::Tower | UnitKind::Building)
    }
}

// ===== 战斗能力组件（怪/塔/建筑按需挂载） =====

/// 攻击能力：伤害/射程/攻速/溅射/对空 + 运行时目标与冷却。
/// 塔（arena）、建筑卡（加农炮）、怪物（CardSpec）共用同一套开火逻辑
#[derive(Component)]
pub struct Attacker {
    pub damage: f32,
    /// 攻击范围（边缘距离）
    pub attack_range: f32,
    /// 攻击间隔（秒）
    pub interval: f32,
    /// 攻击冷却（秒，倒计数；仅在目标进入射程后流逝）
    pub cooldown: f32,
    /// 溅射半径（0 = 单体）
    pub splash_radius: f32,
    /// 能否攻击空中单位
    pub hits_air: bool,
    /// 远程（发射追踪子弹）还是近战（直接扣血）
    pub ranged: bool,
    /// 锁定的攻击目标
    pub target: Option<Entity>,
    /// 已进入过攻击范围（交战）：此后被挤出范围 = 打断解锁；
    /// 交战中锁定不换目标（防距离抖动 flip-flop，对齐 CR）
    pub engaged: bool,
}

/// 索敌策略：怪物主动寻敌（含建筑兜底），塔/建筑原地守卫
#[derive(Component)]
pub struct Targeting(pub TargetPolicy);

pub enum TargetPolicy {
    /// 怪物：aggro 内最近目标（塔/怪/建筑一视同仁）；交战锁定；
    /// 未交战每帧重评；aggro 内无目标 → 全场最近敌方建筑为行军方向。
    /// building_only = 只攻建筑（巨人/野猪，索敌无视怪物）
    Seek {
        aggro_range: f32,
        building_only: bool,
    },
    /// 塔/建筑卡：射程内最近敌方怪物，目标出射程即丢锁（原地不动）
    Guard,
}

/// 移动能力（塔/建筑没有）：朝目标移动，过河走桥
#[derive(Component)]
pub struct Mover {
    pub speed: f32,
}

/// 冲锋（王子）：持续移动蓄力，蓄满移速×speed_mult、首击伤害×damage_mult；
/// 命中或被晕清零，受击不清零
#[derive(Component)]
pub struct Charge {
    /// 已持续移动的秒数
    pub progress: f32,
    /// 蓄力阈值（秒）
    pub windup: f32,
    pub speed_mult: f32,
    pub damage_mult: f32,
}

impl Charge {
    /// 是否已蓄满进入冲锋
    pub fn charged(&self) -> bool {
        self.progress >= self.windup
    }
}

/// 飞行单位：无视河道/地面推挤/静态阻挡，直线飞向目标；仅被 hits_air 攻击命中
#[derive(Component)]
pub struct Flying;

// ===== 控制标志位（打包进 buff 数据） =====
// 机制种类无限（眩晕/冰冻/缠绕/缴械/沉默/破被动/嘲讽/魔免...），
// 全部表达为 Buffs 里的标志位；消费方（行为系统/被动系统/伤害结算）
// 在用时实时折叠查询（channels()/has_cc()），无派生缓存、无同步。
//
// 加新机制三步曲：CCFlags 加位 → cc_channels 补映射（或消费方直接查位）
// → 施加方构造带位的 ActiveBuff。不为机制加任何组件。

/// 控制标志位
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct CCFlags(pub u8);

impl CCFlags {
    pub const NONE: CCFlags = CCFlags(0);
    /// 晕眩：禁移动+禁攻击+禁索敌，清冲锋
    pub const STUN: CCFlags = CCFlags(1);
    /// 缠绕/定身：禁移动（可攻击）
    pub const ROOT: CCFlags = CCFlags(1 << 1);
    /// 缴械：禁攻击（可移动）
    pub const DISARM: CCFlags = CCFlags(1 << 2);
    /// 致盲：禁索敌
    pub const BLIND: CCFlags = CCFlags(1 << 3);
    /// 沉默：禁施法（技能系统启用后生效）
    pub const SILENCE: CCFlags = CCFlags(1 << 4);
    /// 破被动：禁用被动（冲锋蓄力/将来的吸血/闪避等，各被动系统自查此位）
    pub const BREAK: CCFlags = CCFlags(1 << 5);
    // 物理免疫/魔法免疫/嘲讽/恐惧：伤害结算过滤与目标改写类，到时加位

    pub fn contains(self, other: CCFlags) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn with(self, other: CCFlags) -> CCFlags {
        CCFlags(self.0 | other.0)
    }
    /// 并集（多个 buff 的标志合成）
    pub fn union(flags: impl Iterator<Item = CCFlags>) -> CCFlags {
        flags.fold(CCFlags::NONE, |acc, f| acc.with(f))
    }
}

/// 机制 → 基础通道的映射（行为系统用；被动/伤害过滤类直接查位，不走这里）。
/// 加新机制 = 加标志位 + 这里补一行映射，消费方零改动
pub struct Channels {
    pub cannot_move: bool,
    pub cannot_attack: bool,
    pub cannot_seek: bool,
    pub cannot_cast: bool,
}

pub fn cc_channels(cc: CCFlags) -> Channels {
    let stunned = cc.contains(CCFlags::STUN);
    Channels {
        cannot_move: stunned || cc.contains(CCFlags::ROOT),
        cannot_attack: stunned || cc.contains(CCFlags::DISARM),
        cannot_seek: stunned || cc.contains(CCFlags::BLIND),
        cannot_cast: stunned || cc.contains(CCFlags::SILENCE),
    }
}

// ===== 属性修饰器管线 =====
// 数值类 buff 的统一表达：一个 buff = 属性修饰 + 控制标志位 + 持续时间 + 叠加策略。
// 消费方（attack/movement/...）不逐 buff 查询，而是
// buffs.stat(基础值, StatKind) 一次性合成最终值。
// 合成规则（Dota 式三槽）：final = (base + ΣAdd) × (1 + ΣPct) + ΣFlatAdd
// ——前置平顶吃百分比，百分比线性叠加（不爆炸），后置平顶不吃百分比

/// 受修饰的属性域（加新属性 = 加一个枚举值，缓存数组随 Max 自动扩容）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum StatKind {
    MoveSpeed,
    AttackSpeed,
    // 扩展位：Damage / DamageTaken / Armor / ...
    /// 属性域数量哨兵（必须保持最后）：数组长度用它定，禁止作为属性使用
    Max,
}

impl StatKind {
    fn index(self) -> usize {
        self as usize
    }
}

/// 每属性域的合成缓存三个槽位 [add, mul, flat]：
/// - add：前置平顶和（初始 0）
/// - mul：乘法槽（初始 1.0，Pct 累加百分比点，线性叠加）
/// - flat：后置平顶和（初始 0）
/// 合成公式：final = (base + add) × mul + flat（附加/过期时重算）
#[derive(Clone, Copy)]
struct StatFold([[f32; 3]; StatKind::Max as usize]);

impl Default for StatFold {
    fn default() -> Self {
        StatFold([[0.0, 1.0, 0.0]; StatKind::Max as usize])
    }
}

/// 修饰运算（三种语义槽，Dota 式合成公式）
#[derive(Clone, Copy)]
pub enum Op {
    /// 前置平顶：先加后乘（吃百分比），如 +2 移速
    Add,
    /// 百分比：乘法槽累加百分比点（线性叠加），+35% 记 0.35
    Pct,
    /// 后置平顶：先乘后加（不吃百分比），如固定附伤
    FlatAdd,
}

#[derive(Clone, Copy)]
pub struct StatMod {
    pub stat: StatKind,
    pub op: Op,
    pub value: f32,
}

/// 同名 buff 再次施加时的处理策略
#[derive(Clone, Copy)]
pub enum StackPolicy {
    /// 刷新持续时间，数值取新的（狂暴）
    Refresh,
    /// 取更长的剩余时间（晕眩：新的更久才算数）
    Longer,
    /// 独立共存：各倒计时各生效，数值叠乘（不同来源的减速）
    Independent,
    /// 最多叠 n 层，每层独立生效（叠层攻速）
    Stack(u8),
}

/// 一个活跃 buff 实例：属性修饰（effects）+ 控制标志（flags）+ 生命周期。
/// 如"狂暴"= 两条 StatMod；"晕眩"= 一个 STUN 标志位，无属性修饰
pub struct ActiveBuff {
    /// 同名 = 同种 buff（替换/叠层判定）
    pub name: &'static str,
    /// 剩余秒数
    pub secs: f32,
    /// 当前层数（仅 Stack 策略 > 1）
    pub stacks: u8,
    pub policy: StackPolicy,
    /// 控制标志位（晕眩/将来的定身/沉默）
    pub flags: CCFlags,
    pub effects: Vec<StatMod>,
}

/// buff 容器：每单位一个（懒插入——没 buff 就没组件）。
/// 唯一数据源，两类派生值都在变更点（apply/tick/构造）重算一次：
/// - cc：控制标志位并集（channels()/has_cc() 读缓存）
/// - stats：每属性域的 (ΣAdd, ΠMul)（stat() 读缓存拼基础值）
/// 修改 list 必须走 apply()/tick()，否则缓存会过期
#[derive(Component, Default)]
pub struct Buffs {
    pub list: Vec<ActiveBuff>,
    /// 标志位并集缓存
    cc: CCFlags,
    /// 属性合成缓存
    stats: StatFold,
}

impl Buffs {
    /// 单条 buff 起手构造（play_card / 测试用；等价 apply 后的容器）
    pub fn new(buff: ActiveBuff) -> Self {
        let mut b = Buffs::default();
        b.list.push(buff);
        b.recompute();
        b
    }

    /// 重算全部派生缓存（仅变更点调用：apply / 过期 / 构造）
    fn recompute(&mut self) {
        self.cc = CCFlags::union(self.list.iter().map(|b| b.flags));
        let mut fold = StatFold::default();
        for b in &self.list {
            for e in &b.effects {
                // Stack 策略按层数放大：加法 ×n，乘法 value^n
                let n = match b.policy {
                    StackPolicy::Stack(_) => b.stacks,
                    _ => 1,
                };
                let slot = &mut fold.0[e.stat.index()];
                match e.op {
                    Op::Add => slot[0] += e.value * n as f32,
                    Op::Pct => {
                        // 百分比点线性叠加：mul += val × stack（初始 1.0）。
                        // 显式乘法不用 powi——libm 跨平台（Windows/Android）
                        // 可能在末位不一致，帧同步要求逐比特确定
                        slot[1] += e.value * n as f32;
                    }
                    Op::FlatAdd => slot[2] += e.value * n as f32,
                }
            }
        }
        self.stats = fold;
    }

    /// 施加 buff：按 name 与策略合并（刷新/取更久/叠层）或共存（独立）
    pub fn apply(&mut self, incoming: ActiveBuff) {
        if let Some(existing) = self.list.iter_mut().find(|b| b.name == incoming.name) {
            match incoming.policy {
                StackPolicy::Refresh => {
                    existing.secs = incoming.secs;
                    existing.effects = incoming.effects;
                }
                StackPolicy::Longer => {
                    if incoming.secs > existing.secs {
                        existing.secs = incoming.secs;
                    }
                }
                StackPolicy::Stack(n) => {
                    existing.stacks = (existing.stacks + 1).min(n);
                    existing.secs = incoming.secs;
                }
                // 同名独立共存（罕见，但保留语义完整性）
                StackPolicy::Independent => self.list.push(incoming),
            }
        } else {
            self.list.push(incoming);
        }
        self.recompute();
    }

    /// 推进一个模拟步：倒计时、过期、重算缓存。返回容器是否已清空
    pub fn tick(&mut self, dt: f32) -> bool {
        for b in self.list.iter_mut() {
            b.secs -= dt;
        }
        self.list.retain(|b| b.secs > 0.0);
        self.recompute();
        self.list.is_empty()
    }

    /// 是否带有某控制标志（读缓存，不遍历）
    pub fn has_cc(&self, flag: CCFlags) -> bool {
        self.cc.contains(flag)
    }

    /// 基础通道映射（读缓存映射，不遍历）
    pub fn channels(&self) -> Channels {
        cc_channels(self.cc)
    }

    /// 属性解析：读缓存拼基础值，final = (base + add) × mul + flat
    pub fn stat(&self, base: f32, kind: StatKind) -> f32 {
        let [add, mul, flat] = self.stats.0[kind.index()];
        (base + add) * mul + flat
    }
}

/// 建筑寿命：归零自毁（不返圣水）
#[derive(Component)]
pub struct Lifetime {
    pub secs: f32,
}

/// 出兵建筑（墓碑）：每 interval 秒在自身位置出一只 card_id 对应的小兵
#[derive(Component)]
pub struct Spawner {
    pub interval: f32,
    pub card_id: u8,
    /// 出兵倒计时（秒）
    pub cooldown: f32,
}

/// 国王塔标记（被摧毁即输掉对局）
#[derive(Component)]
pub struct KingTower;

/// 多段法术（waves > 1，万箭齐发）：分波延迟结算的范围伤害实体。
/// 时间表（constants.rs 的 SPELL_WAVE_FIRST_TICKS / INTERVAL_TICKS）
/// 是结算与特效的共同权威——箭矢飞行与光环扩散的落点时刻
/// 都从这张表反推。挂 SimTick 链逐帧推进（确定性）。
/// 带 Transform 纯为让 reset_world 能把它当场景实体清掉
#[derive(Component)]
pub struct SpellVolley {
    pub faction: Faction,
    /// 每波伤害（= 卡牌伤害 / 波数，总量守恒）
    pub damage: f32,
    /// 作用半径（按落波时刻的位置判定——期间可以走位躲）
    pub radius: f32,
    pub x: f32,
    pub z: f32,
    /// 剩余波数
    pub waves_left: u32,
    /// 距下一波帧数（每帧 -1，到 0 结算一波后重置为 interval）
    pub next_in: u32,
    /// 波间隔（帧）
    pub interval: u32,
}

#[derive(Component)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

impl Health {
    pub fn new(max: f32) -> Self {
        Self { current: max, max }
    }
}

/// 放置中的虚影：下卡后先显示一个半透明占位，倒计时结束变成真兵
#[derive(Component)]
pub struct Deploying {
    pub card: u8,
    pub faction: Faction,
    pub ticks_left: u32,
}

/// 国王塔/远程单位发射的子弹（追踪目标的小球）
#[derive(Component)]
pub struct Projectile {
    pub target: Entity,
    pub damage: f32,
    /// 溅射半径（0 = 单体）：命中时对攻击方阵营的敌人范围伤害
    pub splash_radius: f32,
    /// 攻击者能否对空（溅射是否波及空中单位）
    pub hits_air: bool,
    /// 攻击者阵营（溅射判定敌我）
    pub attacker: Faction,
}

/// 血条根节点
#[derive(Component)]
pub struct HealthBar;

/// 血条前景，按血量缩放
#[derive(Component)]
pub struct HealthBarFill {
    pub width: f32,
}

/// 已执行指令的完整记录（帧号, 指令）：apply_commands 追加，录像保存的数据源
#[derive(Resource, Default)]
pub struct CommandLog(pub Vec<(u32, GameCommand)>);

/// 圣水（双方各一池，点击哪边半场就扣哪边的）
#[derive(Resource)]
pub struct Elixir {
    pub player: f32,
    pub enemy: f32,
}

/// 操作指令——帧同步模型下，客户端之间唯一需要同步的数据
#[derive(Clone, Copy)]
pub enum GameCommand {
    /// 用手牌 card（CARDS 中的 id）在地面 (x, z) 部署
    Deploy {
        faction: Faction,
        card: u8,
        x: f32,
        z: f32,
    },
}

/// 当前模拟帧号（所有客户端保持一致）
#[derive(Resource, Default)]
pub struct Tick(pub u32);

/// 已采集、尚未打帧号发出的本地点击
#[derive(Resource, Default)]
pub struct PendingClicks(pub Vec<GameCommand>);

/// 指令缓冲：帧号 → 该帧要执行的指令
/// 本地指令（自己打的帧号）和远端指令（对手发来）分开存，屏障只等远端
#[derive(Resource, Default)]
pub struct CommandBuffer {
    pub local: HashMap<u32, Vec<GameCommand>>,
    pub remote: HashMap<u32, Vec<GameCommand>>,
}

/// 双方牌库：队列结构，前 HAND_SIZE 张为手牌，打出的牌排到队尾
/// 牌序是模拟状态（帧同步：由固定种子洗牌，两端一致）
#[derive(Resource)]
pub struct Decks {
    pub player: Vec<u8>,
    pub enemy: Vec<u8>,
}

/// 圣水条 UI 填充
#[derive(Component)]
pub struct ElixirFill;

/// 圣水数值文本
#[derive(Component)]
pub struct ElixirText;

/// 圣水倍数指示文本（x2 / x3）
#[derive(Component)]
pub struct ElixirMultiplier;
