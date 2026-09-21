//! 所有数值调参集中在这里

/// 塔的规格（国王塔 / 公主塔共用一套生成代码）
pub struct TowerSpec {
    pub hp: f32,
    pub body_radius: f32,
    pub body_height: f32,
    pub roof_radius: f32,
    pub roof_height: f32,
    pub bar_width: f32,
    pub bar_y: f32,
    /// 索敌/攻击范围（按边缘距离算）
    pub attack_range: f32,
    /// 是否国王塔（被摧毁即输掉对局）
    pub is_king: bool,
}

// 帧同步常量
/// 模拟版本号：任何影响模拟结果的改动都必须 +1！
/// 包括：数值调整、AI/寻路逻辑、地图结构、牌库洗牌、帧率。
/// 录像回放只在本常量与录像文件中的版本一致时才保证结果正确。
/// v6：AOI 空间网格化（索敌/推挤/溅射邻域查询）——等距平局裁决顺序
/// 与浮点累加顺序变化，旧录像结果失真
pub const SIM_VERSION: u32 = 6;
/// 模拟帧率：所有客户端按同一固定步长推进
pub const TICKS_PER_SEC: f64 = 30.0;
/// 每帧固定步长（模拟中禁止用 delta_secs，必须用它）
pub const TICK_DT: f32 = 1.0 / TICKS_PER_SEC as f32;
/// 固定步长的 Duration（Timer 用，约 33.33ms）
pub const TICK_DURATION: std::time::Duration =
    std::time::Duration::from_nanos(1_000_000_000 / TICKS_PER_SEC as u64);
/// 输入延迟（帧数）：本地指令延迟 N 帧执行，用来掩盖网络延迟
pub const INPUT_DELAY: u32 = 4;

// 对局计时常量（帧同步：全部用帧数表示）
/// 常规时间：3 分钟
pub const REGULAR_TICKS: u32 = 3 * 60 * TICKS_PER_SEC as u32;
/// 常规时间最后 1 分钟双倍圣水（剩余帧数低于此值触发）
pub const DOUBLE_ELIXIR_TICKS: u32 = 60 * TICKS_PER_SEC as u32;
/// 加时赛：2 分钟
pub const OVERTIME_TICKS: u32 = 2 * 60 * TICKS_PER_SEC as u32;
/// 拼血阶段：所有塔每帧扣血（6000 血公主塔约 10 秒掉完）
pub const DRAIN_PER_TICK: f32 = 20.0;

// 地图常量
/// 两座桥的 x 坐标
pub const BRIDGES: [f32; 2] = [-4.5, 4.5];
/// 桥道半宽（桥宽 3）
pub const BRIDGE_HALF_WIDTH: f32 = 1.5;
/// 河道半宽（河宽 2.5）
pub const RIVER_HALF_WIDTH: f32 = 1.25;
/// 国王塔距场地中心的距离（靠近边界）
pub const TOWER_Z: f32 = 12.5;
/// 公主塔横向位置（靠边，两座公主塔拉开距离，射程互不覆盖）
pub const PRINCESS_X: f32 = 6.5;
/// 公主塔纵向位置（也是推塔后部署扩张区的纵深上限）
pub const PRINCESS_Z: f32 = 8.5;

// 战斗常量
pub const TOWER_HP: f32 = 10000.0;
/// 塔的攻击间隔（秒）；单位攻速由各卡 MonsterSpec.attack_interval 决定
pub const ATTACK_INTERVAL: f32 = 1.0;
pub const TOWER_ATTACK_DAMAGE: f32 = 200.0;
pub const PROJECTILE_SPEED: f32 = 14.0;
pub const PROJECTILE_RADIUS: f32 = 0.18;
/// 推挤转向力上限（单位/秒）：密集时也只以这个速度被推开，防止"挤得闪现"
pub const MAX_STEERING_FORCE: f32 = 4.0;
/// 空中单位离地高度（纯表现，模拟逻辑只用 xz 平面）
pub const FLY_HEIGHT: f32 = 1.6;

// AOI 空间网格（combat/grid.rs）
/// 网格单元边长：≥ 最大接触对距离，常规单位推挤邻域 3×3 起步
pub const GRID_CELL: f32 = 2.0;
/// 网格边界：比部署区 [-8,8]×[-14,14] 外扩足够余量，越界位置钳到边缘格
pub const GRID_MIN_X: f32 = -16.0;
pub const GRID_MAX_X: f32 = 16.0;
pub const GRID_MIN_Z: f32 = -24.0;
pub const GRID_MAX_Z: f32 = 24.0;
/// 怪物半径上限：网格查询半径的补偿项（range + self_r + 本值定环数上限）。
/// 加新卡超出此半径会被 grid 模块的测试拦下
pub const MONSTER_RADIUS_MAX: f32 = 1.5;

/// 冲锋规格
#[derive(Clone, Copy)]
pub struct ChargeSpec {
    /// 蓄力时长（秒）：持续移动累积，满后进入冲锋
    pub windup_secs: f32,
    /// 冲锋状态下的移速倍率
    pub speed_mult: f32,
    /// 冲锋首击伤害倍率
    pub damage_mult: f32,
}

/// 怪物个体属性（由卡牌规格决定）
pub struct MonsterSpec {
    pub hp: f32,
    pub damage: f32,
    /// 攻击范围（边缘距离）
    pub attack_range: f32,
    /// 索敌范围（边缘距离）
    pub aggro_range: f32,
    pub speed: f32,
    pub radius: f32,
    /// 质量：推挤时按质量分配力，大质量推开小质量
    pub mass: f32,
    /// 是否远程（攻击时发射子弹而非直接扣血）
    pub ranged: bool,
    /// 攻击间隔（秒）——各卡独立，骑士 1.0s 为数值锚
    pub attack_interval: f32,
    /// 溅射半径（0 = 单体伤害）
    pub splash_radius: f32,
    /// 能否攻击空中单位
    pub hits_air: bool,
    /// 是否飞行单位：无视河道/地面单位推挤，仅被 hits_air 的攻击命中
    pub flying: bool,
    /// 只攻击建筑（塔/建筑卡），无视怪物（巨人/野猪）
    pub building_only: bool,
    /// 冲锋机制（王子）
    pub charge: Option<ChargeSpec>,
}

/// MonsterSpec 便捷构造（const 上下文用），未列字段取默认值
const fn m(
    hp: f32,
    damage: f32,
    attack_range: f32,
    aggro_range: f32,
    speed: f32,
    radius: f32,
    mass: f32,
    ranged: bool,
    attack_interval: f32,
) -> MonsterSpec {
    MonsterSpec {
        hp,
        damage,
        attack_range,
        aggro_range,
        speed,
        radius,
        mass,
        ranged,
        attack_interval,
        splash_radius: 0.0,
        hits_air: ranged,
        flying: false,
        building_only: false,
        charge: None,
    }
}

/// 狂暴 buff 规格
#[derive(Clone, Copy)]
pub struct RageSpec {
    /// 增速百分比点（0.35 = +35%，攻速/移速线性叠加）
    pub pct: f32,
    /// 持续秒数
    pub secs: f32,
}

/// 法术效果：瞬发，作用目标点 (x, z)
pub struct SpellSpec {
    /// 直接伤害（0 = 无伤害）
    pub damage: f32,
    /// 作用半径
    pub radius: f32,
    /// 晕眩秒数（打断冲锋与攻击；电击 0.5s）
    pub stun_secs: f32,
    /// 狂暴（对己方单位生效）
    pub rage: Option<RageSpec>,
}

/// 建筑卡的攻击属性（加农炮/特斯拉类）
#[derive(Clone, Copy)]
pub struct BuildingAttack {
    pub damage: f32,
    /// 攻击范围（边缘距离，从建筑半径外缘起算）
    pub range: f32,
    pub interval: f32,
    pub hits_air: bool,
}

/// 出兵建筑属性（墓碑类）
#[derive(Clone, Copy)]
pub struct BuildingSpawner {
    /// 出兵间隔（秒）
    pub interval_secs: f32,
    /// 出的兵对应的卡 id（骷髅 = 1）
    pub card_id: u8,
}

/// 建筑卡属性：部署于己方半场，有寿命，速度为 0
pub struct BuildingSpec {
    pub hp: f32,
    /// 寿命（秒）：到时自毁（不返还圣水）
    pub lifetime_secs: f32,
    pub attack: Option<BuildingAttack>,
    pub spawner: Option<BuildingSpawner>,
}

/// 卡牌类别
pub enum CardKind {
    Troop(MonsterSpec),
    Spell(SpellSpec),
    Building(BuildingSpec),
}

/// 卡牌定义
pub struct CardSpec {
    pub id: u8,
    pub name: &'static str,
    pub cost: f32,
    /// 一次出兵数量（仅 Troop）
    pub count: u32,
    /// 放置时间（帧数，30 = 1 秒）：下卡后先出虚影，倒计时结束才生效
    pub deploy_ticks: u32,
    pub kind: CardKind,
}

/// 卡牌目录（21 张，UI 无中文字形，名字用英文）
/// 数值锚：3 费骑士 = 2000HP + 100DPS 白板近战（每费 667HP / 33DPS）
pub const CARDS: [CardSpec; 21] = [
    // ===== 地面基础 =====
    // 骑士【锚】
    CardSpec { id: 0, name: "Knight", cost: 3.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(m(2000.0, 100.0, 0.75, 5.0, 1.5, 0.5, 1.0, false, 1.0)) },
    // 骷髅军团
    CardSpec { id: 1, name: "Skeletons", cost: 1.0, count: 3, deploy_ticks: 30, kind: CardKind::Troop(m(300.0, 50.0, 0.45, 2.0, 2.0, 0.3, 0.3, false, 1.0)) },
    // 火枪手（对空）
    CardSpec { id: 2, name: "Musketeer", cost: 4.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(m(1000.0, 120.0, 4.0, 5.0, 1.5, 0.5, 0.8, true, 1.0)) },
    // 巨人：只攻击建筑（对齐真 CR）
    CardSpec { id: 3, name: "Giant", cost: 5.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { building_only: true, ..m(5000.0, 150.0, 1.2, 5.0, 1.0, 0.8, 3.0, false, 1.5) }) },
    // 哥布林
    CardSpec { id: 4, name: "Goblins", cost: 2.0, count: 3, deploy_ticks: 30, kind: CardKind::Troop(m(360.0, 70.0, 0.45, 2.0, 2.5, 0.3, 0.4, false, 1.1)) },
    // 弓箭手（对空）
    CardSpec { id: 5, name: "Archers", cost: 3.0, count: 2, deploy_ticks: 30, kind: CardKind::Troop(m(450.0, 80.0, 4.0, 5.0, 1.5, 0.4, 0.6, true, 1.2)) },
    // 迷你皮卡：慢攻速重击
    CardSpec { id: 6, name: "MiniPEKKA", cost: 4.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(m(1600.0, 350.0, 0.75, 5.0, 2.0, 0.5, 1.2, false, 1.8)) },
    // 野蛮人
    CardSpec { id: 7, name: "Barbarians", cost: 5.0, count: 4, deploy_ticks: 30, kind: CardKind::Troop(m(900.0, 100.0, 0.7, 5.0, 1.5, 0.45, 1.0, false, 1.4)) },
    // ===== 只攻击建筑 =====
    // 野猪骑士：快攻
    CardSpec { id: 8, name: "HogRider", cost: 4.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { building_only: true, ..m(1600.0, 150.0, 0.9, 5.0, 2.5, 0.5, 1.2, false, 1.6) }) },
    // ===== 冲锋 =====
    // 王子：蓄力 2.5s → 移速×2、首击伤害×2（400）；受击不清零，攻击命中或被晕眩才清
    CardSpec { id: 9, name: "Prince", cost: 5.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { charge: Some(ChargeSpec { windup_secs: 2.5, speed_mult: 2.0, damage_mult: 2.0 }), ..m(1900.0, 200.0, 0.9, 5.0, 1.5, 0.55, 1.5, false, 1.4) }) },
    // ===== AOE =====
    // 炸弹人：溅射仅对地
    CardSpec { id: 10, name: "Bomber", cost: 3.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { splash_radius: 1.5, hits_air: false, ..m(400.0, 190.0, 3.5, 4.5, 1.5, 0.35, 0.5, true, 1.9) }) },
    // 瓦基丽：360° 近战溅射仅对地
    CardSpec { id: 11, name: "Valkyrie", cost: 4.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { splash_radius: 1.5, hits_air: false, ..m(1800.0, 210.0, 0.9, 5.0, 1.5, 0.55, 1.2, false, 1.5) }) },
    // 法师：远程溅射对空对地
    CardSpec { id: 12, name: "Wizard", cost: 5.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { splash_radius: 1.2, ..m(1100.0, 182.0, 4.0, 5.0, 1.5, 0.5, 0.8, true, 1.4) }) },
    // ===== 空军 =====
    // 亡灵：飞行近战，可对空
    CardSpec { id: 13, name: "Minions", cost: 3.0, count: 3, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { flying: true, hits_air: true, ..m(320.0, 80.0, 0.45, 3.0, 2.0, 0.3, 0.3, false, 1.0) }) },
    // 飞龙：飞行远程溅射，对空对地
    CardSpec { id: 14, name: "BabyDragon", cost: 4.0, count: 1, deploy_ticks: 30, kind: CardKind::Troop(MonsterSpec { splash_radius: 1.2, flying: true, ..m(1200.0, 160.0, 2.5, 4.0, 1.5, 0.6, 0.8, true, 1.6) }) },
    // ===== 法术（瞬发，全场任意格子）=====
    CardSpec { id: 15, name: "Zap", cost: 2.0, count: 0, deploy_ticks: 0, kind: CardKind::Spell(SpellSpec { damage: 160.0, radius: 1.2, stun_secs: 0.5, rage: None }) },
    CardSpec { id: 16, name: "Arrows", cost: 3.0, count: 0, deploy_ticks: 0, kind: CardKind::Spell(SpellSpec { damage: 300.0, radius: 2.0, stun_secs: 0.0, rage: None }) },
    CardSpec { id: 17, name: "Fireball", cost: 4.0, count: 0, deploy_ticks: 0, kind: CardKind::Spell(SpellSpec { damage: 550.0, radius: 1.5, stun_secs: 0.0, rage: None }) },
    // 狂暴：己方单位攻速/移速 +35%，持续 6s
    CardSpec { id: 18, name: "Rage", cost: 2.0, count: 0, deploy_ticks: 0, kind: CardKind::Spell(SpellSpec { damage: 0.0, radius: 3.0, stun_secs: 0.0, rage: Some(RageSpec { pct: 0.35, secs: 6.0 }) }) },
    // ===== 建筑（仅己方半场可部署，有寿命）=====
    // 加农炮：仅对地
    CardSpec { id: 19, name: "Cannon", cost: 3.0, count: 1, deploy_ticks: 30, kind: CardKind::Building(BuildingSpec { hp: 1400.0, lifetime_secs: 30.0, attack: Some(BuildingAttack { damage: 90.0, range: 5.0, interval: 0.9, hits_air: false }), spawner: None }) },
    // 墓碑：每 4s 出 1 骷髅
    CardSpec { id: 20, name: "Tombstone", cost: 3.0, count: 1, deploy_ticks: 30, kind: CardKind::Building(BuildingSpec { hp: 800.0, lifetime_secs: 30.0, attack: None, spawner: Some(BuildingSpawner { interval_secs: 4.0, card_id: 1 }) }) },
];

/// 牌库大小（每局从 21 种卡随机抽 8 种，双方同池）
pub const DECK_SIZE: usize = 8;
/// 手牌数
pub const HAND_SIZE: usize = 4;

// 圣水常量
pub const ELIXIR_MAX: f32 = 10.0;
pub const ELIXIR_START: f32 = 5.0;
/// 回魔速度：约 2.8 秒一点（CR 常规时间速度）
pub const ELIXIR_PER_SEC: f32 = 1.0 / 2.8;

pub const KING_TOWER: TowerSpec = TowerSpec {
    hp: TOWER_HP,
    body_radius: 1.2,
    body_height: 2.5,
    roof_radius: 1.5,
    roof_height: 1.2,
    bar_width: 2.4,
    bar_y: 4.3,
    // 刚好覆盖"攻击公主塔的位置"（国王塔到公主塔 7.63，扣半径）
    attack_range: 6.0,
    is_king: true,
};

pub const PRINCESS_TOWER: TowerSpec = TowerSpec {
    hp: 6000.0,
    body_radius: 1.0,
    body_height: 2.2,
    roof_radius: 1.25,
    roof_height: 1.0,
    bar_width: 2.0,
    bar_y: 3.8,
    // 只守塔周边：桥上（边缘距 7.2）打不到；也打不到攻击对面公主塔的怪（9.25）
    attack_range: 6.0,
    is_king: false,
};
