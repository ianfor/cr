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

#[derive(Component)]
pub struct Monster {
    pub faction: Faction,
    // 以下属性由卡牌规格决定
    pub damage: f32,
    /// 攻击范围（边缘距离）
    pub attack_range: f32,
    /// 索敌范围（边缘距离）
    pub aggro_range: f32,
    pub speed: f32,
    pub radius: f32,
    pub ranged: bool,
    /// 锁定的攻击目标：不切换，直到目标消失（死亡）才重新索敌
    pub target: Option<Entity>,
}

#[derive(Component)]
pub struct Tower {
    pub faction: Faction,
    pub radius: f32,
    /// 索敌/攻击范围（按边缘距离算）
    pub attack_range: f32,
    /// 锁定的攻击目标：不切换，除非目标死亡或跑出攻击范围
    pub target: Option<Entity>,
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

/// 攻击计时器（攻击间隔）
#[derive(Component)]
pub struct AttackTimer(pub Timer);

/// 国王塔发射的子弹（追踪目标的小球）
#[derive(Component)]
pub struct Projectile {
    pub target: Entity,
    pub damage: f32,
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
