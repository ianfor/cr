//! ECS 组件与资源定义

use std::collections::HashMap;

use bevy::prelude::*;

use crate::constants::TICK_DT;

/// 秒 → tick 数（round 量化，≥1；帧同步两端同算逐比特一致）
pub fn ticks_per_secs(secs: f32) -> u32 {
    ((secs / TICK_DT).round() as u32).max(1)
}

/// 出生首冷却（tick）= 周期 − 前摇：首击时序与旧冷却模型对齐
/// （旧 spawn cooldown=interval，进射程倒数到 0 立即命中；
/// 新模型倒数 cycle−W 后再摇 W 前摇，总量一致，量化差 ±1 tick）
pub fn initial_cooldown_ticks(interval_secs: f32, windup_secs: f32) -> u32 {
    ticks_per_secs(interval_secs).saturating_sub(ticks_per_secs(windup_secs))
}

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
    /// 公主塔
    Tower,
    /// 国王塔（被摧毁即输掉对局）
    KingTower,
    /// 建筑卡（加农炮/墓碑，原地守卫、有寿命）
    Building,
}

impl UnitKind {
    /// 类间稳定排序键（快照确定性契约：怪→塔→建筑）。
    /// 王塔与公主塔同为 1：保旧"塔 query"拼接序与 send_hash 哈希中立
    pub fn rank(self) -> u8 {
        match self {
            UnitKind::Troop => 0,
            UnitKind::Tower | UnitKind::KingTower => 1,
            UnitKind::Building => 2,
        }
    }

    /// 是否塔（公主塔或国王塔）——旧 `kind == Tower` 过滤点大多指此语义
    pub fn is_tower(self) -> bool {
        matches!(self, UnitKind::Tower | UnitKind::KingTower)
    }

    /// 是否建筑类目标（塔/王塔/建筑卡）。王塔必须算：巨人/野猪要打王塔
    pub fn is_building_kind(self) -> bool {
        matches!(self, UnitKind::Tower | UnitKind::KingTower | UnitKind::Building)
    }
}

/// 场上单位（怪/塔/建筑卡的统一组件）：类别 + 阵营 + 卡种 + 物理尺寸。
/// 战斗机制拆在能力组件上（Skill/Targeting/Mover/...），
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
    pub fn king(faction: Faction, radius: f32) -> Self {
        Self { kind: UnitKind::KingTower, faction, card: None, radius, mass: 0.0 }
    }
    pub fn building(faction: Faction, card: u8, radius: f32) -> Self {
        Self { kind: UnitKind::Building, faction, card: Some(card), radius, mass: 0.0 }
    }
}

// ===== 战斗能力组件（怪/塔/建筑按需挂载） =====

// ===== 攻击与打击 =====

/// 结算负载：命中时发生什么（普攻直击/近战溅射/法术/弹着点/波共用）。
/// 攻击方（Skill）与在途打击（Strike）各持一份，直击与 AOE 共用同一份
#[derive(Clone)]
pub struct Payload {
    pub damage: f32,
    /// 溅射/作用半径（0 = 单体直击）
    pub splash_radius: f32,
    /// 能否波及空中单位（对空攻击/法术恒 true）
    pub hits_air: bool,
    /// AOE 是否波及塔：近战溅射 true（瓦基丽溅塔）、法术/弹溅 false
    /// （显式化旧规则：弹溅与法术原本就跳塔，近战溅射原本就含塔）
    pub hits_towers: bool,
    /// 命中敌方时施加的 buff（仅部队；晕眩等）
    pub enemy_buffs: Vec<ActiveBuff>,
    /// 命中己方时施加的 buff（仅部队；狂暴）
    pub ally_buffs: Vec<ActiveBuff>,
}

impl Payload {
    /// 纯伤害负载（无 buff）
    pub fn damage_only(damage: f32, splash_radius: f32, hits_air: bool, hits_towers: bool) -> Self {
        Payload {
            damage,
            splash_radius,
            hits_air,
            hits_towers,
            enemy_buffs: vec![],
            ally_buffs: vec![],
        }
    }

    /// 是否有附带 buff（AOE 早退守卫用：纯伤害负载不进遍历）
    pub fn has_buffs(&self) -> bool {
        !self.enemy_buffs.is_empty() || !self.ally_buffs.is_empty()
    }
}

/// 投放方式：瞬发直击（近战）还是发射在途 Strike（远程追踪弹）
#[derive(Clone, Copy)]
pub enum Delivery {
    /// 近战：当场以自身位置为中心结算
    Melee,
    /// 远程：发射追踪弹（Strike + Flight::Homing）
    Homing,
}

/// 攻击过程状态机：普攻是一个动作过程（前摇→出手帧→后摇），
/// 不再是"冷却转完同 tick 立即结算"。
/// 三态循环（连续站桩输出）：Release → Recover(R) → Idle(cycle−W−R)
/// → Windup(W) → Release，release→release = cycle = interval/攻速（DPS 守恒）。
/// 全部时长用 u32 tick 倒数（进入 Windup 时一次性量化冻结，无 f32 累加残差）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SkillState {
    /// 待机：剩余冷却 tick。目标在射程内才流逝（行军途中不回复）；
    /// 归零且有有效目标 → 进 Windup
    Idle { left: u32 },
    /// 前摇：锁移动 + 锁目标（已 commit 的出手不被推挤打断）。
    /// 被晕/缴械 → 取消回 Idle{0}（白摇）；归零 → Release 结算 → Recover
    Windup { left: u32 },
    /// 后摇：可移动（走A），不可出手、索敌冻结；
    /// 归零 → Idle{剩余冷却}
    Recover { left: u32 },
}

// ===== 攻击三段结构（组件合一） =====
// 普攻按关注点分三段——select（选谁）→ flow（何时打）→ effect（打到会怎样）。
// 三段恒同时出现（不存在"有选择器没流程"的单位），拆成独立组件只会
// 让 query 参数膨胀、构造点三份样板——所以组件上合一（单一 Skill），
// 结构上分段（嵌套子结构体，设计意图由类型承载）。
// 写者约定：select.target/engaged 由 targeting 系统独占写（attacking/moving 只读）；
// flow.state 由 attacking 系统独占推进。

/// 目标选择策略：怪物主动寻敌（含建筑兜底），塔/建筑原地守卫
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

/// 目标选择段：怎么选目标（锁定谁、何时保持/失效/重锁）
pub struct TargetSelector {
    pub policy: TargetPolicy,
    /// 攻击范围（边缘距离）：锁定保持判定 + 流程系统"在射程内"判定共用
    pub range: f32,
    /// 能否选中空中单位（选择侧过滤）。与 payload.hits_air 解耦——
    /// 当前各卡同值，将来"能选空军但溅射对地"类卡可独立表达
    pub hits_air: bool,
    /// 锁定的攻击目标（写者：targeting 系统）
    pub target: Option<Entity>,
    /// 已进入过攻击范围（交战）：此后被挤出范围 = 打断解锁；
    /// 交战中锁定不换目标（防距离抖动 flip-flop，对齐 CR）
    pub engaged: bool,
}

/// 执行流程段：什么时候打（纯计时，不知道效果是什么）。
/// 前摇/出手帧/后摇三态循环，冷却合并在 Idle.left
pub struct AttackFlow {
    /// 攻击间隔（秒）——两次命中的完整周期（含前摇后摇）
    pub interval: f32,
    /// 前摇时长（秒）：出手帧前的动作时间，攻速 buff 同步缩短；
    /// 进 Windup 时量化为 tick 冻结
    pub windup_secs: f32,
    pub state: SkillState,
}

/// 结算效果段：打到会怎样（纯规格，不知道何时打）。
/// 塔（arena）、建筑卡（加农炮）、怪物（CardSpec）共用；
/// 出手帧执行见 attack::resolve_release，命中结算是统一的
/// detonate（strike 模块）
pub struct SkillEffect {
    pub payload: Payload,
    pub delivery: Delivery,
}

/// 技能组件：一次普攻的完整定义与运行时状态（怪/塔/建筑卡共用）
#[derive(Component)]
pub struct Skill {
    /// 选谁：目标选择（锁定/保持/重锁规则 + 运行时 target/engaged）
    pub select: TargetSelector,
    /// 何时打：执行流程（前摇/出手帧/后摇三态）
    pub flow: AttackFlow,
    /// 打到会怎样：结算效果规格
    pub effect: SkillEffect,
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
/// 如"狂暴"= 两条 StatMod；"晕眩"= 一个 STUN 标志位，无属性修饰；
/// "建筑衰减"= hp_per_sec 持续扣血（负 = DoT 正 = HoT）
#[derive(Clone)]
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
    /// 每秒生命增减（负=持续伤害，正=持续回复）；
    /// 跨 buff 直接相加，不参与 StackPolicy 层叠
    pub hp_per_sec: f32,
}

impl Default for ActiveBuff {
    fn default() -> Self {
        ActiveBuff {
            name: "",
            secs: 0.0,
            stacks: 1,
            policy: StackPolicy::Refresh,
            flags: CCFlags::NONE,
            effects: vec![],
            hp_per_sec: 0.0,
        }
    }
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
    /// 每秒生命增减缓存（Σ hp_per_sec，跨 buff 相加、不参与层叠）
    drain: f32,
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
        self.drain = self.list.iter().map(|b| b.hp_per_sec).sum();
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

    /// 每秒生命增减（读缓存，不遍历）：负 = 持续掉血（建筑衰减/毒），
    /// 正 = 持续回复。status_effects 消费
    pub fn drain_per_sec(&self) -> f32 {
        self.drain
    }
}

/// 出兵建筑（墓碑）：每 interval 秒在自身位置出一只 card_id 对应的小兵
#[derive(Component)]
pub struct Spawner {
    pub interval: f32,
    pub card_id: u8,
    /// 出兵倒计时（秒）
    pub cooldown: f32,
}

/// 在途打击：已释放、未结算的技能实例（追踪弹 / 多段法术波）。
/// 挂 SimTick 链由 strike_tick 逐帧推进（确定性）。
/// 带 Transform 纯为让 reset_world 能把它当场景实体清掉
#[derive(Component)]
pub struct Strike {
    /// 施放方阵营（结算判定敌我）
    pub attacker: Faction,
    pub payload: Payload,
    pub flight: Flight,
}

/// 飞行方式：追踪目标直击（远程普攻弹）或原地分波结算（多段法术）
pub enum Flight {
    /// 追踪目标：贴身时以目标位置为中心结算（原 Projectile）
    Homing {
        target: Entity,
    },
    /// 原地多波：到点以 (x, 0, z) 为中心结算一波（原 SpellVolley）。
    /// 时间表（constants.rs 的 SPELL_WAVE_FIRST_TICKS / INTERVAL_TICKS）
    /// 是结算与特效的共同权威——箭矢飞行与光环扩散的落点时刻都从它反推
    Volley {
        x: f32,
        z: f32,
        /// 剩余波数
        waves_left: u32,
        /// 距下一波帧数（每帧 -1，到 0 结算一波后重置为 interval）
        next_in: u32,
        /// 波间隔（帧）
        interval: u32,
    },
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
