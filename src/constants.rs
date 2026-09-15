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
pub const ATTACK_INTERVAL: f32 = 1.0;
pub const TOWER_ATTACK_DAMAGE: f32 = 200.0;
pub const PROJECTILE_SPEED: f32 = 14.0;
pub const PROJECTILE_RADIUS: f32 = 0.18;

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
    /// 是否远程（攻击时发射子弹而非直接扣血）
    pub ranged: bool,
}

/// 卡牌定义
pub struct CardSpec {
    pub id: u8,
    pub name: &'static str,
    pub cost: f32,
    /// 一次出兵数量
    pub count: u32,
    pub monster: MonsterSpec,
}

/// 卡牌目录（UI 无中文字形，名字用英文）
pub const CARDS: [CardSpec; 4] = [
    // 骑士：均衡近战（原胶囊怪数值）
    CardSpec {
        id: 0,
        name: "Knight",
        cost: 3.0,
        count: 1,
        monster: MonsterSpec {
            hp: 2000.0,
            damage: 100.0,
            attack_range: 0.75,
            aggro_range: 5.0,
            speed: 1.5,
            radius: 0.5,
            ranged: false,
        },
    },
    // 骷髅军团：1 费 3 只小骷髅，炮灰
    CardSpec {
        id: 1,
        name: "Skeletons",
        cost: 1.0,
        count: 3,
        monster: MonsterSpec {
            hp: 300.0,
            damage: 50.0,
            attack_range: 0.45,
            aggro_range: 2.0,
            speed: 2.0,
            radius: 0.3,
            ranged: false,
        },
    },
    // 火枪手：远程单体
    CardSpec {
        id: 2,
        name: "Musketeer",
        cost: 4.0,
        count: 1,
        monster: MonsterSpec {
            hp: 1000.0,
            damage: 120.0,
            attack_range: 4.0,
            aggro_range: 5.0,
            speed: 1.5,
            radius: 0.5,
            ranged: true,
        },
    },
    // 巨人：高血低速坦克
    CardSpec {
        id: 3,
        name: "Giant",
        cost: 5.0,
        count: 1,
        monster: MonsterSpec {
            hp: 5000.0,
            damage: 150.0,
            attack_range: 1.2,
            aggro_range: 5.0,
            speed: 1.0,
            radius: 0.8,
            ranged: false,
        },
    },
];

/// 牌库大小（4 种卡各两张，CR 为 8 张）
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
