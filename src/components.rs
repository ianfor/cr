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

/// 场上单位（怪/塔/建筑卡的统一标记）：阵营 + 卡种 + 物理尺寸。
/// 战斗机制拆在能力组件上（Attacker/Targeting/Mover/...），
/// 挂什么组件就有什么能力——加新机制 = 加新组件，不改现有类型
#[derive(Component)]
pub struct Monster {
    pub faction: Faction,
    /// 卡牌 id（CARDS 中的索引）：观测用单位类型标识，不参与模拟逻辑
    pub card: u8,
    /// 碰撞半径（索敌边缘距离/推挤/静态阻挡共用）
    pub radius: f32,
    /// 质量：推挤时按质量分配力，大质量推开小质量
    pub mass: f32,
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

/// 晕眩（法术插入，status_effects 倒计时后移除）：
/// 无法索敌/攻击/移动，冲锋清零；目标锁定保留
#[derive(Component)]
pub struct Stun {
    pub secs: f32,
}

/// 狂暴（法术插入）：攻速/移速 ×mult，持续 secs
#[derive(Component)]
pub struct Rage {
    pub secs: f32,
    pub mult: f32,
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

/// 建筑卡实体（加农炮/墓碑）：部署于己方半场，速度为 0。
/// 可被怪物/只攻建筑单位当作目标（与塔同属"建筑"类）；
/// 攻击/出兵/寿命分别由 Attacker/Spawner/Lifetime 能力组件表达
#[derive(Component)]
pub struct BuildingCard {
    pub faction: Faction,
    /// 对应卡 id（观测网格/出兵用）
    pub card: u8,
    pub radius: f32,
}

#[derive(Component)]
pub struct Tower {
    pub faction: Faction,
    pub radius: f32,
}

/// 国王塔标记（被摧毁即输掉对局）
#[derive(Component)]
pub struct KingTower;

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
