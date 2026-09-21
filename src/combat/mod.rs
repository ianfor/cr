//! 战斗模块：机制拆分为能力组件 + 小系统（组合优于配置）
//!
//! - [`targeting`]：统一索敌（怪物 Seek / 塔与建筑 Guard），每帧构建全场快照
//! - [`attack`]（attacking）：统一开火（近战直伤/自中心溅射、远程子弹、冲锋首击）
//! - [`movement`]（moving）：移动（桥道转向/飞行直线/冲锋蓄力/狂暴加速）
//! - [`status`]（status_effects）：晕眩/狂暴计时（Stun/Rage 组件生命周期）
//! - [`physics`]：推挤/河道禁入/静态阻挡（飞行单位全部跳过）
//! - [`projectile`]（move_projectiles）：子弹追踪与命中点溅射
//! - [`buildings`]：建筑寿命自毁/墓碑出兵
//!
//! 帧同步确定性：全部系统挂 SimTick 链（lib.rs / sim_env.rs / replay.rs 三处），
//! 链内顺序显式固定。快照（[WorldSnaps]）由 targeting 每帧构建一次，
//! attacking/moving 复用——既避免 Transform 读写冲突，也保证三个系统
//! 看到的是同一份战场视图。

mod attack;
mod buildings;
mod movement;
mod physics;
mod projectile;
mod status;
mod targeting;

pub use attack::attacking;
pub use buildings::{building_lifetime, building_spawner};
pub use movement::moving;
pub use physics::{keep_out_of_river, separate_monsters, separate_from_statics};
pub use projectile::{move_projectiles, ProjectileAssets};
pub use status::status_effects;
pub use targeting::targeting;

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::bot::BotMode;
use crate::cards::{self, SelectedCard};
use crate::components::*;
use crate::constants::*;
use crate::net::{self, NetClient};

// ===== 共享快照 =====

/// 可被攻击单位的快照（索敌/攻击/移动共用），避免嵌套查询与读写冲突
pub(crate) struct UnitSnap {
    pub entity: Entity,
    pub faction: Faction,
    pub pos: Vec3,
    pub radius: f32,
    pub is_tower: bool,
    /// 建筑卡（与塔同属"建筑"类目标，只攻建筑单位的索敌目标）
    pub is_building: bool,
    pub flying: bool,
}

impl UnitSnap {
    /// 是否建筑类目标（塔或建筑卡）
    pub(crate) fn is_building_kind(&self) -> bool {
        self.is_tower || self.is_building
    }
}

/// 本帧单位快照：targeting 写入，attacking/moving 读取（帧同步链内顺序保证新鲜）
#[derive(Resource, Default)]
pub struct WorldSnaps(pub(crate) Vec<UnitSnap>);

/// 水平边缘距离（忽略 y，减去双方半径）
pub(crate) fn edge_dist(a_pos: Vec3, a_r: f32, b_pos: Vec3, b_r: f32) -> f32 {
    let mut d = a_pos - b_pos;
    d.y = 0.0;
    d.length() - a_r - b_r
}

/// 攻击者能否把该快照当作目标：
/// - 不能对空 → 打不了飞行单位
/// - 只攻建筑 → 只索塔/建筑卡，无视怪物（巨人/野猪）
pub(crate) fn can_target(attacker: &Attacker, building_only: bool, s: &UnitSnap) -> bool {
    if s.flying && !attacker.hits_air {
        return false;
    }
    if building_only {
        return s.is_building_kind();
    }
    true
}

// ===== 输入采集与指令执行 =====

/// 输入采集（Update，渲染帧率）：点击 → 射线求交 → 生成操作指令暂存
/// 注意：这里只表达"意图"，不碰任何模拟状态，保证帧同步确定性
pub fn gather_input(
    state: Res<net::SimState>,
    window: Single<&Window>,
    camera: Single<(&Camera, &GlobalTransform)>,
    mouse: Res<ButtonInput<MouseButton>>,
    touches: Res<Touches>,
    decks: Res<Decks>,
    selected: Res<SelectedCard>,
    buttons: Query<&Interaction, With<Button>>,
    towers: Query<(&Tower, &Transform, Option<&KingTower>)>,
    bot_mode: Option<Res<BotMode>>,
    mut pending: ResMut<PendingClicks>,
    net: Option<Res<NetClient>>,
) {
    // 等待/追帧/回放/对局结束期间不采集点击（防止恢复后指令倾泻）
    if !matches!(*state, net::SimState::Solo | net::SimState::Playing) {
        return;
    }
    // 点击源：鼠标按下（PC）或触摸按下（Android）——同一套处理
    let click = if mouse.just_pressed(MouseButton::Left) {
        window.cursor_position()
    } else {
        touches.iter_just_pressed().next().map(|t| t.position())
    };
    let Some(cursor) = click else {
        return;
    };
    // 点在 UI（卡槽按钮）上：那是选牌操作，不在场景放怪
    if buttons.iter().any(|i| *i != Interaction::None) {
        return;
    }
    let (camera, camera_transform) = *camera;
    let Ok(ray) = camera.viewport_to_world(camera_transform, cursor) else {
        return;
    };
    let Some(t) = ray.intersect_plane(Vec3::ZERO, InfinitePlane3d::new(Vec3::Y)) else {
        return;
    };
    let mut point = ray.get_point(t);
    // 限制在竞技场范围内
    point.x = point.x.clamp(-8.0, 8.0);
    point.z = point.z.clamp(-14.0, 14.0);

    let faction = match net.as_ref() {
        // 联网：阵营由服务器序号决定
        Some(n) => {
            let Some(f) = Faction::from_index(n.my_index) else {
                return; // 还没分配到序号（观战/等待中）
            };
            f
        }
        // PvE（有机器人）：玩家固定蓝方
        None if bot_mode.is_some() => Faction::Player,
        // 单机：点哪个半场就属于哪方
        None => {
            if point.z < 0.0 {
                Faction::Player
            } else {
                Faction::Enemy
            }
        }
    };
    // 当前手牌槽位 → 卡牌 id（牌序两端一致，本地读取即可）
    let card = decks.queue(faction)[selected.0.min(HAND_SIZE - 1)];
    // 部署区域按卡类别校验（法术全场/建筑己半场/部队 CR 推塔扩张规则）。
    // 单机模式不校验（点哪边半场就归哪方）
    if net.is_some() || bot_mode.is_some() {
        let tower_snaps: Vec<(Faction, bool, Vec3)> = towers
            .iter()
            .map(|(t, tr, k)| (t.faction, k.is_some(), tr.translation))
            .collect();
        if !cards::deploy_zone_ok(&CARDS[card as usize], faction, point, &tower_snaps) {
            return; // 区域不可部署：无效操作
        }
    }
    pending.0.push(GameCommand::Deploy {
        faction,
        card,
        x: point.x,
        z: point.z,
    });
}

/// 指令打帧号（FixedUpdate，每模拟帧最先运行）：
/// 本地指令打上 T+INPUT_DELAY 的执行帧号，存入缓冲并发给对手（联网时）
/// 追帧/回放状态下不处理（指令来自日志而非实时输入）
pub fn collect_inputs(
    state: Res<net::SimState>,
    mut pending: ResMut<PendingClicks>,
    mut buffer: ResMut<CommandBuffer>,
    tick: Res<Tick>,
    net: Option<Res<NetClient>>,
) {
    if !matches!(*state, net::SimState::Solo | net::SimState::Playing) {
        return;
    }
    let stamp = tick.0 + INPUT_DELAY;
    let cmds: Vec<GameCommand> = std::mem::take(&mut pending.0);
    if let Some(net) = net.as_ref() {
        net::send_commands(net, stamp, &cmds);
    }
    if !cmds.is_empty() {
        buffer.local.entry(stamp).or_default().extend(cmds);
    }
}

/// 指令执行（FixedUpdate）：消费本帧的本地+远端指令
/// 按阵营序号排序，保证两个客户端的执行顺序逐比特一致
pub fn apply_commands(
    mut commands: Commands,
    mut buffer: ResMut<CommandBuffer>,
    tick: Res<Tick>,
    mut decks: ResMut<Decks>,
    mut elixir: ResMut<Elixir>,
    mut log: ResMut<CommandLog>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    towers: Query<(&Tower, &Transform, Option<&KingTower>)>,
    mut spell_targets: Query<(
        Entity,
        &mut Health,
        &Transform,
        Option<&Monster>,
        Option<&BuildingCard>,
        Option<&mut Stun>,
        Option<&mut Buffs>,
    )>,
) {
    // 部署区域判定用的塔快照（faction, is_king, pos）
    let tower_snaps: Vec<(Faction, bool, Vec3)> = towers
        .iter()
        .map(|(t, tr, k)| (t.faction, k.is_some(), tr.translation))
        .collect();

    let mut exec: Vec<GameCommand> = buffer.local.remove(&tick.0).unwrap_or_default();
    exec.extend(buffer.remote.remove(&tick.0).unwrap_or_default());
    // 稳定排序：蓝方指令先执行，同阵营保持发送顺序
    exec.sort_by_key(|c| match c {
        GameCommand::Deploy { faction, .. } => faction.index(),
    });

    for cmd in exec {
        // 记录到指令日志（录像回放的数据源）
        log.0.push((tick.0, cmd));
        match cmd {
            GameCommand::Deploy {
                faction,
                card,
                x,
                z,
            } => cards::play_card(
                &mut commands,
                &mut decks,
                &mut elixir,
                &mut meshes,
                &mut materials,
                &mut spell_targets,
                faction,
                card,
                Vec3::new(x, 0.0, z),
                &tower_snaps,
            ),
        }
    }
}

/// 帧号推进（每条指令都绑定帧号，联网时按帧号对齐）
pub fn advance_tick(mut tick: ResMut<Tick>) {
    tick.0 += 1;
}

// ===== 对局结束与死亡清除 =====

/// 国王塔被摧毁 → 清除该阵营所有塔和怪物，对局结束，模拟停止
/// 在帧同步链内执行，双方客户端同一帧做出相同判定
pub fn check_game_over(
    mut commands: Commands,
    mut state: ResMut<net::SimState>,
    timer: Res<crate::match_flow::MatchTimer>,
    kings: Query<(&Tower, &Health), With<KingTower>>,
    towers: Query<(Entity, &Tower, &Health)>,
    monsters: Query<(Entity, &Monster)>,
    buildings: Query<(Entity, &BuildingCard)>,
    net: Option<Res<NetClient>>,
) {
    use crate::match_flow::MatchPhase;

    // 追帧/回放也必须判定：否则追帧会越过对局结束点继续模拟"垃圾帧"，
    // 期间另一座国王塔可能也被打死，从而判出与真实相反的胜负
    if !matches!(
        *state,
        net::SimState::Solo
            | net::SimState::Playing
            | net::SimState::CatchingUp
            | net::SimState::Replaying
    ) {
        return;
    }

    let other = |f: Faction| match f {
        Faction::Player => Faction::Enemy,
        Faction::Enemy => Faction::Player,
    };

    // 1) 国王塔死亡：任何阶段立即结束
    // 2) 加时/拼血：任意塔死亡 = 猝死；双方同帧掉塔 = 平局
    // 返回值：None = 对局继续；Some(None) = 平局；Some(Some(w)) = w 胜
    let outcome: Option<Option<Faction>> = if let Some(loser) = kings
        .iter()
        .find(|(_, hp)| hp.current <= 0.0)
        .map(|(t, _)| t.faction)
    {
        Some(Some(other(loser)))
    } else if matches!(timer.phase, MatchPhase::Overtime | MatchPhase::Drain) {
        let mut dead = (false, false);
        for (_, t, hp) in &towers {
            if hp.current <= 0.0 {
                match t.faction {
                    Faction::Player => dead.0 = true,
                    Faction::Enemy => dead.1 = true,
                }
            }
        }
        match dead {
            (true, true) => Some(None),
            (true, false) => Some(Some(Faction::Enemy)),
            (false, true) => Some(Some(Faction::Player)),
            (false, false) => None,
        }
    } else {
        None
    };
    let Some(result) = outcome else {
        return; // 对局继续
    };

    *state = net::SimState::GameOver(result);
    info!("对局结束：{:?}", result);

    // 清除失败方所有塔和怪物（平局则双方保留）
    if let Some(winner) = result {
        let loser = other(winner);
        for (e, t, _) in &towers {
            if t.faction == loser {
                commands.entity(e).despawn();
            }
        }
        for (e, m) in &monsters {
            if m.faction == loser {
                commands.entity(e).despawn();
            }
        }
        // 失败方的建筑卡也一并清除（否则靠寿命慢慢自毁，结算画面不干净）
        for (e, b) in &buildings {
            if b.faction == loser {
                commands.entity(e).despawn();
            }
        }
    }

    // 结算界面（本地表现层，不影响模拟）
    let my = net.and_then(|n| Faction::from_index(n.my_index));
    crate::match_flow::spawn_result_ui(&mut commands, result, my);
}

/// 血量归零的单位清除（血条是子节点，会随父节点一起销毁）
/// 注意：国王塔永远不归这里管（check_game_over 处理）；
/// 加时/拼血阶段的塔也不归这里管（留给 check_game_over 判定猝死）
pub fn despawn_dead(
    mut commands: Commands,
    timer: Res<crate::match_flow::MatchTimer>,
    units: Query<(Entity, &Health, Option<&Tower>), (Changed<Health>, Without<KingTower>)>,
) {
    use crate::match_flow::MatchPhase;

    let sudden_death_phase = matches!(timer.phase, MatchPhase::Overtime | MatchPhase::Drain);
    for (e, h, tower) in &units {
        if h.current <= 0.0 {
            if tower.is_some() && sudden_death_phase {
                continue;
            }
            commands.entity(e).despawn();
        }
    }
}

// ===== 法术施法特效（纯表现层，Update 调度，不进模拟链） =====
// 法术瞬发不出实体，没有任何视觉反馈会让人以为"没反应"——
// 这里从指令日志增量检测法术释放，在施法点放一个扩散光环。
// 不读写任何模拟状态（CommandLog 只读），VFX 实体不带模拟组件，
// 不影响帧同步确定性与训练环境（sim_env 不跑 Update）。

/// 扩散光环特效
#[derive(Component)]
pub struct SpellFx {
    /// 已播放秒数
    t: f32,
    /// 总时长
    duration: f32,
    /// 扩散终半径（= 法术作用半径，略放大）
    end_radius: f32,
}

/// 法术卡的特效颜色
fn spell_fx_color(card: u8) -> Color {
    match card {
        15 => Color::srgb(0.95, 0.95, 0.3),  // Zap 黄
        16 => Color::srgb(0.95, 0.6, 0.2),   // Arrows 橙
        17 => Color::srgb(0.95, 0.3, 0.15),  // Fireball 红
        18 => Color::srgb(0.75, 0.3, 0.95),  // Rage 紫
        _ => Color::WHITE,
    }
}

/// 从指令日志增量检测法术释放 → 生成光环（含回放/追帧，cursor 自动追平）
pub fn spell_fx_spawn(
    mut commands: Commands,
    log: Res<CommandLog>,
    mut cursor: Local<usize>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // 世界重置后日志清空：cursor 回退到 0 重新跟（seek 回退重追时特效会重放，无害）
    if *cursor > log.0.len() {
        *cursor = log.0.len();
    }
    while *cursor < log.0.len() {
        let (_, cmd) = log.0[*cursor];
        *cursor += 1;
        // GameCommand 目前只有 Deploy 一种，模式匹配保留扩展性
        let GameCommand::Deploy { card, x, z, .. } = cmd;
        let Some(spec) = CARDS.iter().find(|c| c.id == card) else {
            continue;
        };
        let CardKind::Spell(spell) = &spec.kind else {
            continue;
        };
        commands.spawn((
            SpellFx {
                t: 0.0,
                duration: 0.45,
                end_radius: spell.radius + 0.4,
            },
            Mesh3d(meshes.add(bevy::math::primitives::Torus::new(1.0, 0.06))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color: spell_fx_color(card),
                unlit: true,
                alpha_mode: AlphaMode::Blend,
                ..default()
            })),
            Transform::from_translation(Vec3::new(x, 0.25, z))
                .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
            NotShadowCaster,
        ));
    }
}

/// 光环动画：半径 0 → end_radius 扩散，透明度淡出，播完销毁
pub fn spell_fx_update(
    mut commands: Commands,
    time: Res<Time>,
    mut fx: Query<(
        Entity,
        &mut SpellFx,
        &mut Transform,
        &MeshMaterial3d<StandardMaterial>,
    )>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (e, mut s, mut transform, mat) in &mut fx {
        s.t += time.delta_secs();
        let k = (s.t / s.duration).min(1.0);
        let r = s.end_radius * k;
        transform.scale = Vec3::new(r, r, 1.0);
        if let Some(mut m) = materials.get_mut(&mat.0) {
            m.base_color.set_alpha((1.0 - k) * 0.9);
        }
        if s.t >= s.duration {
            commands.entity(e).despawn();
        }
    }
}

// ===== 测试辅助 =====

/// 测试用白板骑士 Monster（纯物理属性，机制全部由能力组件表达）
#[cfg(test)]
pub(crate) fn test_monster(faction: Faction) -> Monster {
    Monster {
        faction,
        card: 0,
        radius: 0.5,
        mass: 1.0,
    }
}

/// 测试用白板攻击能力（骑士数值锚：100 伤害 / 0.75 射程 / 1.0s 攻速）
#[cfg(test)]
pub(crate) fn test_attacker() -> Attacker {
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
    }
}

/// 测试用怪物索敌策略（aggro 5.0，非只攻建筑）
#[cfg(test)]
pub(crate) fn seek(aggro: f32) -> Targeting {
    Targeting(TargetPolicy::Seek {
        aggro_range: aggro,
        building_only: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_flow::{MatchPhase, MatchTimer};
    use crate::net::SimState;

    fn spawn_tower(world: &mut World, faction: Faction, king: bool, hp: f32) {
        let mut e = world.spawn((
            Tower {
                faction,
                radius: 1.2,
            },
            Attacker {
                damage: TOWER_ATTACK_DAMAGE,
                attack_range: 6.0,
                interval: 1.0,
                cooldown: 1.0,
                splash_radius: 0.0,
                hits_air: true,
                ranged: true,
                target: None,
                engaged: false,
            },
            Targeting(TargetPolicy::Guard),
            Health {
                current: hp,
                max: 100.0,
            },
        ));
        if king {
            e.insert(KingTower);
        }
    }

    fn spawn_monster(world: &mut World, faction: Faction) {
        world.spawn((
            test_monster(faction),
            test_attacker(),
            seek(5.0),
            Mover { speed: 1.5 },
            Health::new(100.0),
        ));
    }

    /// 国王塔血量归零 → 对局结束、失败方全灭、胜利方保留
    #[test]
    fn king_death_ends_game() {
        let mut app = App::new();
        app.insert_resource(SimState::Playing);
        app.init_resource::<Tick>();
        app.init_resource::<MatchTimer>();
        let world = app.world_mut();

        // 蓝方国王塔已死；双方各一只怪
        spawn_tower(world, Faction::Player, true, 0.0);
        spawn_monster(world, Faction::Player);
        spawn_monster(world, Faction::Enemy);

        let mut schedule = Schedule::default();
        schedule.add_systems(check_game_over);
        schedule.run(world);

        // 状态：红方胜利
        assert!(matches!(
            world.resource::<SimState>(),
            SimState::GameOver(Some(Faction::Enemy))
        ));
        // 蓝方（失败方）塔和怪都被清除
        let mut towers = world.query::<&Tower>();
        assert_eq!(towers.iter(world).count(), 0);
        let mut monsters = world.query::<&Monster>();
        let remaining: Vec<&Monster> = monsters.iter(world).collect();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].faction, Faction::Enemy);
    }

    /// 常规阶段公主塔被毁 → 对局继续；进入加时后任意掉塔 → 猝死
    #[test]
    fn overtime_tower_death_is_sudden_death() {
        // 常规阶段：公主塔掉了不结束
        let mut app = App::new();
        app.insert_resource(SimState::Playing);
        app.init_resource::<Tick>();
        app.init_resource::<MatchTimer>(); // Regular
        let world = app.world_mut();
        spawn_tower(world, Faction::Player, false, 0.0);

        let mut schedule = Schedule::default();
        schedule.add_systems(check_game_over);
        schedule.run(world);
        assert!(matches!(
            world.resource::<SimState>(),
            SimState::Playing
        ));

        // 加时阶段：任意塔掉 → 猝死
        let mut app = App::new();
        app.insert_resource(SimState::Playing);
        app.init_resource::<Tick>();
        app.insert_resource(MatchTimer {
            phase: MatchPhase::Overtime,
            ticks_left: 100,
        });
        let world = app.world_mut();
        spawn_tower(world, Faction::Player, false, 0.0);

        let mut schedule = Schedule::default();
        // despawn_dead 在加时阶段必须跳过塔，留给 check_game_over 判定
        schedule.add_systems((despawn_dead, check_game_over).chain());
        schedule.run(world);
        assert!(matches!(
            world.resource::<SimState>(),
            SimState::GameOver(Some(Faction::Enemy))
        ));
        let mut towers = world.query::<&Tower>();
        assert_eq!(towers.iter(world).count(), 0);
    }

    /// 拼血阶段：所有塔每帧扣 DRAIN_PER_TICK
    #[test]
    fn drain_phase_towers_lose_hp() {
        let mut app = App::new();
        app.insert_resource(SimState::Playing);
        app.init_resource::<Tick>();
        app.insert_resource(MatchTimer {
            phase: MatchPhase::Drain,
            ticks_left: 0,
        });
        let world = app.world_mut();
        let tower = world
            .spawn((
                Tower {
                    faction: Faction::Player,
                    radius: 1.0,
                },
                Health::new(1000.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(crate::match_flow::tick_timer);
        schedule.run(world);

        let hp = world.get::<Health>(tower).unwrap();
        assert_eq!(hp.current, 1000.0 - DRAIN_PER_TICK);
    }
}
