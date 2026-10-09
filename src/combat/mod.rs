//! 战斗模块：机制拆分为能力组件 + 小系统（组合优于配置）
//!
//! 攻击三权分立（各挂一个组件，互不牵连）：
//! - [`TargetSelector`]（选谁）：targeting 系统执行——锁定/保持/重锁规则，
//!   每帧构建全场快照（WorldSnaps）供流程/移动复用
//! - [`AttackFlow`]（何时打）：attacking 系统执行——前摇/出手帧/后摇三态机
//! - [`Skill`]（打到会怎样）：纯效果规格，出手帧经 resolve_release 执行
//!   （近战当场 detonate / 远程发射在途 Strike）
//!
//! 其他机制系统：
//! - [`movement`]（moving）：移动（桥道转向/飞行直线/冲锋蓄力/狂暴加速）
//! - [`status`]（status_effects）：晕眩/狂暴计时（Stun/Rage 组件生命周期）
//! - [`physics`]：推挤/河道禁入/静态阻挡（飞行单位全部跳过）
//! - [`strike`]（strike_tick/detonate）：在途打击推进与统一命中结算
//! - [`buildings`]：建筑寿命自毁/墓碑出兵
//!
//! 帧同步确定性：全部系统挂 SimTick 链（lib.rs / sim_env.rs / replay.rs 三处），
//! 链内顺序显式固定。快照（[WorldSnaps]）由 targeting 每帧构建一次，
//! attacking/moving 复用——既避免 Transform 读写冲突，也保证三个系统
//! 看到的是同一份战场视图。

mod attack;
mod buildings;
mod grid;
mod movement;
mod physics;
mod status;
mod strike;
mod targeting;

pub use attack::attacking;
pub use buildings::building_spawner;
pub use movement::moving;
pub use physics::{keep_out_of_river, separate_monsters, separate_from_statics};
pub(crate) use strike::detonate;
pub use strike::{strike_tick, ProjectileAssets};
pub use status::status_effects;
pub use targeting::targeting;

use bevy::light::NotShadowCaster;
use bevy::prelude::*;
use std::collections::HashMap;

use crate::bot::BotMode;
use crate::cards::{self, SelectedCard};
use crate::components::*;
use crate::constants::*;
use crate::net::{self, NetClient};

pub(crate) use grid::SpatialGrid;

// ===== 共享快照 =====

/// 可被攻击单位的快照（索敌/攻击/移动/推挤共用），避免嵌套查询与读写冲突
pub(crate) struct UnitSnap {
    pub entity: Entity,
    pub kind: UnitKind,
    pub faction: Faction,
    pub pos: Vec3,
    pub radius: f32,
    /// 质量（推挤力分配用；塔/建筑不参与推挤，填 0）
    pub mass: f32,
    pub flying: bool,
}

impl UnitSnap {
    /// 是否建筑类目标（塔或建筑卡；王塔在内——巨人/野猪的索敌目标）
    pub(crate) fn is_building_kind(&self) -> bool {
        self.kind.is_building_kind()
    }
}

/// 本帧战场视图：targeting 每帧构建一次，attacking/moving 读取
/// （帧同步链内顺序保证新鲜）。三件套：
/// - `snaps`：全场快照（怪→塔→建筑，顺序两端一致）
/// - `grid`：怪物空间网格（塔/建筑不入格——≤12 个且不动，最近邻走线性）
/// - `index`：entity → 快照下标。**只做点查（get），禁止迭代**
///   （HashMap RandomState 每进程随机种子，迭代序不同会失同步）
#[derive(Resource, Default)]
pub struct WorldSnaps {
    pub(crate) snaps: Vec<UnitSnap>,
    pub(crate) grid: SpatialGrid,
    pub(crate) index: HashMap<Entity, u32>,
}

/// 水平边缘距离（忽略 y，减去双方半径）
pub(crate) fn edge_dist(a_pos: Vec3, a_r: f32, b_pos: Vec3, b_r: f32) -> f32 {
    let mut d = a_pos - b_pos;
    d.y = 0.0;
    d.length() - a_r - b_r
}

/// 攻击者能否把该快照当作目标：
/// - 不能对空 → 打不了飞行单位
/// - 只攻建筑 → 只索塔/建筑卡，无视怪物（巨人/野猪）
pub(crate) fn can_target(hits_air: bool, building_only: bool, s: &UnitSnap) -> bool {
    if s.flying && !hits_air {
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
    towers: Query<(&Unit, &Transform)>,
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
            .filter(|(u, _)| u.kind.is_tower())
            .map(|(u, tr)| (u.faction, u.kind == UnitKind::KingTower, tr.translation))
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
    towers: Query<(&Unit, &Transform)>,
    mut spell_targets: Query<
        (
            Entity,
            &Unit,
            &Transform,
            Option<&Flying>,
            &mut Health,
        ),
        Without<Strike>,
    >,
) {
    // 部署区域判定用的塔快照（faction, is_king, pos）
    let tower_snaps: Vec<(Faction, bool, Vec3)> = towers
        .iter()
        .filter(|(u, _)| u.kind.is_tower())
        .map(|(u, tr)| (u.faction, u.kind == UnitKind::KingTower, tr.translation))
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
    units: Query<(Entity, &Unit, &Health)>,
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
    let outcome: Option<Option<Faction>> = if let Some(loser) = units
        .iter()
        .find(|(_, u, hp)| u.kind == UnitKind::KingTower && hp.current <= 0.0)
        .map(|(_, u, _)| u.faction)
    {
        Some(Some(other(loser)))
    } else if matches!(timer.phase, MatchPhase::Overtime | MatchPhase::Drain) {
        let mut dead = (false, false);
        for (_, u, hp) in &units {
            // 只看塔（含王塔——王塔已死会先进上面的分支）：
            // 加时拆掉建筑卡不算猝死
            if u.kind.is_tower() && hp.current <= 0.0 {
                match u.faction {
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

    // 清除失败方所有单位（塔/怪/建筑卡；平局则双方保留）
    if let Some(winner) = result {
        let loser = other(winner);
        for (e, u, _) in &units {
            if u.faction == loser {
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
    units: Query<(Entity, &Health, &Unit), Changed<Health>>,
) {
    use crate::match_flow::MatchPhase;

    let sudden_death_phase = matches!(timer.phase, MatchPhase::Overtime | MatchPhase::Drain);
    for (e, h, u) in &units {
        if h.current <= 0.0 {
            // 王塔永远不归这里管（check_game_over 处理）
            if u.kind == UnitKind::KingTower {
                continue;
            }
            // 加时/拼血阶段的塔也不归这里管（留给 check_game_over 判定猝死）
            if u.kind.is_tower() && sudden_death_phase {
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

/// 扩散光环特效（delay > 0 时等待到点才扩散——多段法术每波一个）
#[derive(Component)]
pub struct SpellFx {
    /// 已播放秒数
    t: f32,
    /// 起播延迟（秒；0 = 立即）
    delay: f32,
    /// 总时长
    duration: f32,
    /// 扩散终半径（= 法术作用半径，略放大）
    end_radius: f32,
}

/// 万箭齐发的箭矢实体（纯表现层）：从施法方国王塔顶抛物线飞向圈内散布落点。
/// **落点时刻 = 对应波数的结算帧**（发射延迟/飞行时长从 SPELL_WAVE 时间表
/// 反推，特效与伤害逐帧对齐）；法术伤害由 Strike(Volley) 在模拟链内结算
#[derive(Component)]
pub struct SpellArrow {
    from: Vec3,
    to: Vec3,
    /// 已流逝时间（未到 delay 前停在塔顶）
    t: f32,
    delay: f32,
    duration: f32,
    /// 抛物线峰值高度
    arc: f32,
}

/// 万箭齐发每一波的箭矢数量
const ARROWS_PER_WAVE: u32 = 6;

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
    towers: Query<(&Unit, &Transform)>,
) {
    // 世界重置后日志清空：cursor 回退到 0 重新跟（seek 回退重追时特效会重放，无害）
    if *cursor > log.0.len() {
        *cursor = log.0.len();
    }
    while *cursor < log.0.len() {
        let (_, cmd) = log.0[*cursor];
        *cursor += 1;
        // GameCommand 目前只有 Deploy 一种，模式匹配保留扩展性
        let GameCommand::Deploy {
            faction,
            card,
            x,
            z,
            ..
        } = cmd;
        let Some(spec) = CARDS.iter().find(|c| c.id == card) else {
            continue;
        };
        let CardKind::Spell(spell) = &spec.kind else {
            continue;
        };
        // 万箭齐发（Arrows，id 16）：多波结算，特效吃同一张时间表——
        // 每波：落地光环（delay = 波结算帧）+ 一批箭矢（飞行时长恰好
        // 在波结算帧落地）。其余法术瞬发单光环
        if card == 16 {
            let ring_mesh = meshes.add(bevy::math::primitives::Torus::new(0.94, 1.06));
            let ring_mat = materials.add(StandardMaterial {
                base_color: spell_fx_color(card),
                unlit: true,
                alpha_mode: AlphaMode::Blend,
                ..default()
            });
            for wave in 0..spell.waves {
                // 本波落地时刻（秒）——与模拟侧 Strike(Volley) 的波帧一致
                let land = (SPELL_WAVE_FIRST_TICKS + wave * SPELL_WAVE_INTERVAL_TICKS) as f32
                    * TICK_DT;
                // 本波光环：到点扩散
                commands.spawn((
                    SpellFx {
                        t: 0.0,
                        delay: land,
                        duration: 0.45,
                        end_radius: spell.radius + 0.4,
                    },
                    Mesh3d(ring_mesh.clone()),
                    MeshMaterial3d(ring_mat.clone()),
                    Transform::from_translation(Vec3::new(x, 0.25, z))
                        .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
                    NotShadowCaster,
                ));
                // 本波箭矢：从施法方王塔顶射出（王塔已毁则跳过，对局将终）
                let Some(king_top) = towers
                    .iter()
                    .find(|(u, _)| u.kind == UnitKind::KingTower && u.faction == faction)
                    .map(|(_, tr)| tr.translation + Vec3::Y * 4.2)
                else {
                    continue;
                };
                let shaft = meshes.add(Cylinder::new(0.04, 0.55));
                let mat = materials.add(StandardMaterial {
                    base_color: Color::srgb(0.95, 0.7, 0.35),
                    unlit: true,
                    ..default()
                });
                for i in 0..ARROWS_PER_WAVE {
                    // 确定性散布：环形分布（每波旋转错开）+ 拱高错落
                    let total = ARROWS_PER_WAVE * spell.waves;
                    let angle = (i + wave * ARROWS_PER_WAVE) as f32 / total as f32
                        * std::f32::consts::TAU
                        + 0.3;
                    let rr = ((i * 7 + wave * 3) % 13) as f32 / 13.0;
                    let dist = spell.radius * 0.85 * (0.25 + 0.75 * rr);
                    let jitter = ((i * 11 + wave) % 5) as f32 / 5.0;
                    // 发射时刻：波间隔 × 波序 + 小错落；飞行时长 = 落地帧 - 发射
                    // （保证箭矢恰好在波结算帧落地）
                    let launch = wave as f32 * SPELL_WAVE_INTERVAL_TICKS as f32 * TICK_DT
                        + (i % 3) as f32 * 0.03;
                    commands.spawn((
                        SpellArrow {
                            from: king_top
                                + Vec3::new(angle.cos() * 0.5, 0.0, angle.sin() * 0.5),
                            to: Vec3::new(
                                x + angle.cos() * dist,
                                0.1,
                                z + angle.sin() * dist,
                            ),
                            t: 0.0,
                            delay: launch,
                            duration: land - launch,
                            arc: 3.0 + 1.8 * jitter,
                        },
                        Mesh3d(shaft.clone()),
                        MeshMaterial3d(mat.clone()),
                        Transform::from_translation(king_top),
                        NotShadowCaster,
                    ));
                }
            }
            continue;
        }
        // 其余法术：瞬发单光环
        commands.spawn((
            SpellFx {
                t: 0.0,
                delay: 0.0,
                duration: 0.45,
                end_radius: spell.radius + 0.4,
            },
            Mesh3d(meshes.add(bevy::math::primitives::Torus::new(0.94, 1.06))),
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

/// 万箭飞行：抛物线插值 + 朝向轨迹切线（圆柱默认沿 Y），落地销毁
pub fn spell_arrows_fly(
    mut commands: Commands,
    time: Res<Time>,
    mut arrows: Query<(Entity, &mut SpellArrow, &mut Transform)>,
) {
    for (e, mut a, mut tr) in &mut arrows {
        a.t += time.delta_secs();
        if a.t < a.delay {
            continue; // 未发射：停在塔顶
        }
        let k = ((a.t - a.delay) / a.duration).min(1.0);
        // 轨迹点：水平插值 + 4k(1-k) 拱高
        let p = |k: f32| {
            a.from.lerp(a.to, k) + Vec3::Y * (a.arc * 4.0 * k * (1.0 - k))
        };
        tr.translation = p(k);
        // 朝向轨迹切线（k 逼近 1 时切线退化，兜底直指落点）
        let k2 = (k + 0.02).min(1.0);
        let mut dir = p(k2) - p(k);
        if dir.length_squared() < 1e-6 {
            dir = a.to - a.from;
        }
        tr.rotation = Quat::from_rotation_arc(Vec3::Y, dir.normalize());
        if k >= 1.0 {
            commands.entity(e).despawn();
        }
    }
}

/// 多段法术落点危险圈（纯表现层）：法术结算期间在施法点持续显示作用范围。
/// 生命周期与模拟侧 Strike(Volley) 实体严格一致——放置时生成、最后一波
/// 落地后销毁，圈也随之消失（模拟实体是唯一权威，回放模式自动正确）
pub fn spell_volley_indicator(
    strikes: Query<&Strike>,
    mut gizmos: bevy::gizmos::prelude::Gizmos,
) {
    for s in &strikes {
        let Flight::Volley { x, z, .. } = s.flight else {
            continue; // 追踪弹不画危险圈
        };
        gizmos
            .circle(
                Isometry3d::new(
                    Vec3::new(x, 0.1, z),
                    Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
                ),
                s.payload.splash_radius,
                // 万箭橙（与卡色一致）：危险区，和瞄准时的黄圈区分
                Color::srgb(0.95, 0.6, 0.2),
            )
            .resolution(64);
    }
}

/// 出手闪光特效（纯表现层）：出手帧在攻击者处生成、0.12s 缩放淡出。
/// 近战=攻击者与目标之间的劈砍闪光；远程=枪口闪光（弹体本身已是表现）
#[derive(Component)]
pub struct AttackFx {
    /// 已播放秒数
    t: f32,
    /// 总时长
    duration: f32,
    /// 起始缩放
    start_scale: f32,
}

/// 从出手记录增量检测攻击出手 → 生成出手闪光
/// （含回放/追帧，cursor 自动追平；世界重置后日志清空，cursor 回退重跟）
pub fn attack_action_fx(
    mut commands: Commands,
    log: Res<ReleaseLog>,
    mut cursor: Local<usize>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    attackers: Query<(&Unit, &Transform, &Skill)>,
) {
    // 世界重置后日志清空：cursor 回退到 0 重新跟（seek 回退重追时特效重放，无害）
    if *cursor > log.0.len() {
        *cursor = log.0.len();
    }
    while *cursor < log.0.len() {
        let (_, entity, target_pos) = log.0[*cursor];
        *cursor += 1;
        let Ok((unit, transform, skill)) = attackers.get(entity) else {
            continue; // 攻击者已死（同帧阵亡等）——跳过，不播特效
        };
        let color = faction_color(unit.faction);
        let mesh = meshes.add(Sphere::new(0.22));
        let mat = materials.add(StandardMaterial {
            base_color: color.with_alpha(0.9),
            unlit: true,
            ..default()
        });
        // 光源位置：近战=攻击者朝目标方向顶进 0.5（劈砍点）；
        // 远程=枪口高度（与弹道起点一致）
        let (fx_pos, scale) = match skill.effect.delivery {
            Delivery::Melee => {
                let mut dir = target_pos - transform.translation;
                dir.y = 0.0;
                let dir = if dir.length_squared() < 1e-6 {
                    Vec3::ZERO
                } else {
                    dir.normalize() * 0.5
                };
                (transform.translation + Vec3::Y * 1.0 + dir, 1.6)
            }
            Delivery::Homing => {
                let muzzle_y = match unit.kind {
                    UnitKind::Troop => 1.5,
                    UnitKind::Tower | UnitKind::KingTower => 3.5,
                    UnitKind::Building => 1.2,
                };
                (transform.translation + Vec3::Y * muzzle_y, 1.0)
            }
        };
        commands.spawn((
            AttackFx {
                t: 0.0,
                duration: 0.12,
                start_scale: scale,
            },
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_translation(fx_pos),
            NotShadowCaster,
        ));
    }
}

/// 出手闪光动画：快速胀大淡出，播完销毁
pub fn attack_fx_update(
    mut commands: Commands,
    time: Res<Time>,
    mut fx: Query<(
        Entity,
        &mut AttackFx,
        &mut Transform,
        &MeshMaterial3d<StandardMaterial>,
    )>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (e, mut s, mut transform, mat) in &mut fx {
        s.t += time.delta_secs();
        let k = (s.t / s.duration).clamp(0.0, 1.0);
        // 弹性胀大（sin 拱形）+ 淡出
        let pop = (std::f32::consts::PI * k).sin();
        let r = s.start_scale * (0.5 + pop);
        transform.scale = Vec3::splat(r);
        if let Some(mut m) = materials.get_mut(&mat.0) {
            m.base_color.set_alpha((1.0 - k) * 0.9);
        }
        if s.t >= s.duration {
            commands.entity(e).despawn();
        }
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
        // delay 期间 k 钳在 0（半径 0 不可见），到点才开始扩散
        let k = ((s.t - s.delay) / s.duration).clamp(0.0, 1.0);
        let r = s.end_radius * k;
        transform.scale = Vec3::new(r, r, 1.0);
        if let Some(mut m) = materials.get_mut(&mat.0) {
            m.base_color.set_alpha((1.0 - k) * 0.9);
        }
        if s.t >= s.delay + s.duration {
            commands.entity(e).despawn();
        }
    }
}

/// 出手记录（模拟侧确定性追加）：(帧号, 攻击者实体, 目标位置)。
/// 纯表现层 attack_action_fx 增量消费播放出手闪光（cursor 模式，
/// 回放/追帧自动正确）；reset_world 清空。不进哈希、不影响模拟
#[derive(Resource, Default)]
pub struct ReleaseLog(pub Vec<(u32, Entity, Vec3)>);

// ===== 测试辅助 =====

/// 测试用白板骑士部队 Unit（纯物理属性，机制全部由能力组件表达）
#[cfg(test)]
pub(crate) fn test_monster(faction: Faction) -> Unit {
    Unit::troop(faction, 0, 0.5, 1.0)
}

/// 测试用白板塔 Unit
#[cfg(test)]
pub(crate) fn test_tower(faction: Faction) -> Unit {
    Unit::tower(faction, 1.0)
}

/// 测试用白板骑士技能（数值锚：100 伤害 / 0.75 射程 / 1.0s 攻速 /
/// 0.3s 前摇 / 近战 / aggro 5）。需要自定义策略/数值时先改字段再入 bundle
#[cfg(test)]
pub(crate) fn test_skill() -> Skill {
    Skill {
        select: TargetSelector {
            policy: TargetPolicy::Seek {
                aggro_range: 5.0,
                building_only: false,
            },
            range: 0.75,
            hits_air: false,
            target: None,
            engaged: false,
        },
        flow: AttackFlow {
            interval: 1.0,
            windup_secs: 0.3,
            state: SkillState::Idle {
                left: initial_cooldown_ticks(1.0, 0.3),
            },
        },
        effect: SkillEffect {
            payload: Payload::damage_only(100.0, 0.0, false, true),
            delivery: Delivery::Melee,
        },
    }
}

/// 测试用塔技能（守卫 / 射程 6 / 塔伤 / 对空 / 远程追踪弹）
#[cfg(test)]
pub(crate) fn tower_skill() -> Skill {
    Skill {
        select: TargetSelector {
            policy: TargetPolicy::Guard,
            range: 6.0,
            hits_air: true,
            target: None,
            engaged: false,
        },
        flow: AttackFlow {
            interval: 1.0,
            windup_secs: 0.35,
            state: SkillState::Idle {
                left: initial_cooldown_ticks(1.0, 0.35),
            },
        },
        effect: SkillEffect {
            payload: Payload::damage_only(TOWER_ATTACK_DAMAGE, 0.0, true, false),
            delivery: Delivery::Homing,
        },
    }
}

/// 测试用加农炮技能（守卫 / 射程 5 / 0.9s 周期 / 90 伤 / 仅对地 /
/// 首冷却 0 有敌即摇）
#[cfg(test)]
pub(crate) fn cannon_skill() -> Skill {
    Skill {
        select: TargetSelector {
            policy: TargetPolicy::Guard,
            range: 5.0,
            hits_air: false,
            target: None,
            engaged: false,
        },
        flow: AttackFlow {
            interval: 0.9,
            windup_secs: 0.35,
            state: SkillState::Idle { left: 0 },
        },
        effect: SkillEffect {
            payload: Payload::damage_only(90.0, 0.0, false, false),
            delivery: Delivery::Homing,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_flow::{MatchPhase, MatchTimer};
    use crate::net::SimState;

    fn spawn_tower(world: &mut World, faction: Faction, king: bool, hp: f32) {
        let unit = if king {
            Unit::king(faction, 1.2)
        } else {
            Unit::tower(faction, 1.2)
        };
        world.spawn((
            unit,
            tower_skill(),
            Health {
                current: hp,
                max: 100.0,
            },
        ));
    }

    fn spawn_monster(world: &mut World, faction: Faction) {
        world.spawn((
            test_monster(faction),
            test_skill(),
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
        let mut towers = world.query::<&Unit>();
        assert_eq!(
            towers.iter(world).filter(|u| u.kind.is_tower()).count(),
            0
        );
        let mut monsters = world.query_filtered::<&Unit, With<Mover>>();
        let remaining: Vec<&Unit> = monsters.iter(world).collect();
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
        let mut towers = world.query::<&Unit>();
        assert_eq!(
            towers.iter(world).filter(|u| u.kind.is_tower()).count(),
            0
        );
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
                Unit::tower(Faction::Player, 1.0),
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
