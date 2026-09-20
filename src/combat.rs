//! 战斗：出兵、怪物/塔 AI、子弹、碰撞阻挡、死亡清除

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::bot::BotMode;
use crate::cards::{self, SelectedCard};
use crate::components::*;
use crate::constants::*;
use crate::net::{self, NetClient};

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
        &mut Health,
        &Transform,
        Option<&mut Monster>,
        Option<&BuildingCard>,
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
        15 => Color::srgb(0.95, 0.95, 0.3),   // Zap 黄
        16 => Color::srgb(0.95, 0.6, 0.2),    // Arrows 橙
        17 => Color::srgb(0.95, 0.3, 0.15),   // Fireball 红
        18 => Color::srgb(0.75, 0.3, 0.95),   // Rage 紫
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

/// 可被攻击单位的快照，避免索敌时嵌套查询
struct UnitSnap {
    entity: Entity,
    faction: Faction,
    pos: Vec3,
    radius: f32,
    is_tower: bool,
    /// 建筑卡（与塔同属"建筑"类目标，只攻建筑单位的索敌目标）
    is_building: bool,
    flying: bool,
}

impl UnitSnap {
    /// 是否建筑类目标（塔或建筑卡）
    fn is_building_kind(&self) -> bool {
        self.is_tower || self.is_building
    }
}

/// 水平边缘距离（忽略 y，减去双方半径）
fn edge_dist(a_pos: Vec3, a_r: f32, b_pos: Vec3, b_r: f32) -> f32 {
    let mut d = a_pos - b_pos;
    d.y = 0.0;
    d.length() - a_r - b_r
}

/// 攻击者能否把该快照当作目标：
/// - 不能对空 → 打不了飞行单位
/// - 只攻建筑 → 只索塔/建筑卡，无视怪物（巨人/野猪）
fn can_target(attacker: &Monster, s: &UnitSnap) -> bool {
    if s.flying && !attacker.hits_air {
        return false;
    }
    if attacker.building_only {
        return s.is_building_kind();
    }
    true
}

/// 怪物 AI（属性来自卡牌规格）：
/// - 索敌：aggro 范围内"最近目标"（塔/怪物/建筑一视同仁，修复塔沦为兜底的旧 bug）；
///   aggro 内没有目标 → 全场最近的敌方建筑（塔/建筑卡）作为行军方向
/// - 目标锁定：一旦锁定不切换。目标消失（死亡）解锁；
///   已交战（进过攻击范围）后被挤出攻击范围 = 被打断解锁；
///   未交战（走向远目标途中）不因距离解锁，也不被新进 aggro 的怪抢走目标
/// - 进入攻击范围 → 停下攻击：近战直接扣血（可溅射），远程发射子弹
/// - 冲锋（王子）：持续移动蓄力，蓄满移速×，首击伤害×，命中或被晕清零
/// - 晕眩：无法移动/攻击；狂暴：攻速/移速×rage_mult
/// - 未进入 → 朝目标移动（过河走桥；飞行单位直线）
pub fn monster_ai(
    mut commands: Commands,
    mut monsters: Query<(Entity, &mut Monster, &mut Transform, &mut AttackTimer), Without<Tower>>,
    towers: Query<(Entity, &Tower, &Transform), Without<Monster>>,
    buildings: Query<(Entity, &BuildingCard, &Transform), (Without<Monster>, Without<Tower>)>,
    mut healths: Query<&mut Health>,
    mut proj_assets: ResMut<ProjectileAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // 快照所有单位（怪物 + 塔 + 建筑卡）
    let mut snaps: Vec<UnitSnap> = monsters
        .iter()
        .map(|(e, m, t, _)| UnitSnap {
            entity: e,
            faction: m.faction,
            pos: t.translation,
            radius: m.radius,
            is_tower: false,
            is_building: false,
            flying: m.flying,
        })
        .collect();
    snaps.extend(towers.iter().map(|(e, t, tr)| UnitSnap {
        entity: e,
        faction: t.faction,
        pos: tr.translation,
        radius: t.radius,
        is_tower: true,
        is_building: false,
        flying: false,
    }));
    snaps.extend(buildings.iter().map(|(e, b, tr)| UnitSnap {
        entity: e,
        faction: b.faction,
        pos: tr.translation,
        radius: b.radius,
        is_tower: false,
        is_building: true,
        flying: false,
    }));

    for (entity, mut monster, mut transform, mut timer) in monsters.iter_mut() {
        let pos = transform.translation;

        // 晕眩：无法移动/攻击，冲锋清零；目标锁定保留（晕完继续打）
        if monster.stun_secs > 0.0 {
            monster.stun_secs -= TICK_DT;
            if let Some(c) = &mut monster.charge {
                c.progress = 0.0;
            }
            continue;
        }
        // 狂暴计时衰减
        if monster.rage_secs > 0.0 {
            monster.rage_secs -= TICK_DT;
        }

        // 锁定失效即解除：
        // 1) 目标消失（死亡）
        // 2) 已交战（进过攻击范围）后被挤出攻击范围 = 被打断（比如站桩输出时
        //    被新放置的怪挤开）。解除后下方立刻重新索敌，aggro 内最近的目标
        //    会被重新锁定（可能就是挤它的那只）。
        //    未交战的单位（走向远目标途中）不因距离解锁——否则任何进入
        //    aggro 的怪都会抢走目标（历史 bug：塔的优先级被压到怪物之下）
        if let Some(e) = monster.target {
            let invalid = match snaps
                .iter()
                .find(|s| s.entity == e && s.faction != monster.faction)
            {
                None => true, // 目标已消失
                Some(s) => {
                    monster.engaged
                        && edge_dist(pos, monster.radius, s.pos, s.radius)
                            > monster.attack_range + 0.05
                }
            };
            if invalid {
                monster.target = None;
                monster.engaged = false;
            }
        }
        // 无锁定 → 索敌：
        // a) aggro 内最近的合法目标（塔/怪物/建筑一视同仁）
        // b) 都没有 → 全场最近的敌方建筑（行军方向；只攻建筑单位同样适用）
        if monster.target.is_none() {
            let nearest = |filter: &dyn Fn(&UnitSnap) -> bool| {
                snaps
                    .iter()
                    .filter(|s| s.faction != monster.faction && s.entity != entity)
                    .filter(|s| filter(s))
                    .min_by(|a, b| {
                        pos.distance_squared(a.pos)
                            .partial_cmp(&pos.distance_squared(b.pos))
                            .unwrap()
                    })
            };
            let in_aggro = nearest(&|s| {
                can_target(&monster, s)
                    && edge_dist(pos, monster.radius, s.pos, s.radius) <= monster.aggro_range
            });
            monster.target = in_aggro
                .or_else(|| nearest(&|s| can_target(&monster, s) && s.is_building_kind()))
                .map(|t| t.entity);
        }
        let Some(target_entity) = monster.target else {
            continue;
        };
        let target = snaps
            .iter()
            .find(|s| s.entity == target_entity)
            .expect("锁定目标必然在快照中");

        // 攻击判定只看与目标的实际边缘距离，与导航路点无关
        // （否则隔着半个地图纵坐标符号不同时，贴脸也永远无法出手）
        let edge = edge_dist(pos, monster.radius, target.pos, target.radius);
        // 攻击停止距离（中心距）= 攻击边缘距离 + 双方半径
        let stop_dist = monster.attack_range + monster.radius + target.radius;
        let goal = steering_goal(pos, target.pos, monster.flying);
        let mut to_goal = goal - pos;
        to_goal.y = 0.0;
        let dist = to_goal.length();

        if edge <= monster.attack_range + 0.05 {
            // 在攻击范围内：停下攻击（固定步长 tick，保证确定性）
            monster.engaged = true;
            // 狂暴加速攻击节奏：计时器步长 ×rage_mult
            let tick = if monster.rage_secs > 0.0 {
                std::time::Duration::from_secs_f32(TICK_DT / monster.rage_mult)
            } else {
                TICK_DURATION
            };
            if timer.0.tick(tick).just_finished() {
                // 冲锋首击：伤害×蓄力倍率，命中后蓄力清零
                let mut damage = monster.damage;
                if let Some(c) = &mut monster.charge {
                    if c.charged() {
                        damage *= c.damage_mult;
                        c.progress = 0.0;
                    }
                }
                if monster.ranged {
                    // 远程：发射追踪子弹（溅射参数随弹携带）
                    let (mesh, mat) = projectile_assets(
                        &mut proj_assets,
                        &mut meshes,
                        &mut materials,
                        monster.faction,
                    );
                    commands.spawn((
                        Projectile {
                            target: target.entity,
                            damage,
                            splash_radius: monster.splash_radius,
                            hits_air: monster.hits_air,
                            attacker: monster.faction,
                        },
                        Mesh3d(mesh),
                        MeshMaterial3d(mat),
                        Transform::from_translation(pos + Vec3::Y * 1.5),
                        NotShadowCaster,
                    ));
                } else if let Ok(mut health) = healths.get_mut(target.entity) {
                    // 近战：直接扣血
                    health.current -= damage;
                }
                // 近战溅射：以自身为中心的范围伤害（瓦基丽 360°）
                if !monster.ranged && monster.splash_radius > 0.0 {
                    for s in snaps
                        .iter()
                        .filter(|s| s.faction != monster.faction && s.entity != target.entity)
                    {
                        if s.flying && !monster.hits_air {
                            continue; // 对地溅射打不到空军
                        }
                        let mut d = s.pos - pos;
                        d.y = 0.0;
                        if d.length() <= monster.splash_radius + s.radius {
                            if let Ok(mut health) = healths.get_mut(s.entity) {
                                health.current -= damage;
                            }
                        }
                    }
                }
            }
        } else if dist > 1e-4 {
            // 冲锋蓄力：持续移动累积，蓄满进入冲锋（移速×）
            let mut speed = monster.speed;
            if let Some(c) = &mut monster.charge {
                c.progress += TICK_DT;
                if c.charged() {
                    speed *= c.speed_mult;
                }
            }
            if monster.rage_secs > 0.0 {
                speed *= monster.rage_mult;
            }
            let step = speed * TICK_DT;
            // 朝最终目标移动时不要把步长走过停止距离
            let step = if goal == target.pos {
                step.min((dist - stop_dist).max(0.0))
            } else {
                step.min(dist)
            };
            transform.translation += to_goal.normalize() * step;
        }
    }
}

/// 子弹共享资源：mesh 和各阵营材质只建一次，避免每发子弹新建资产
#[derive(Resource, Default)]
pub struct ProjectileAssets {
    mesh: Option<Handle<Mesh>>,
    materials: [Option<Handle<StandardMaterial>>; 2],
}

fn projectile_assets(
    assets: &mut ProjectileAssets,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
) -> (Handle<Mesh>, Handle<StandardMaterial>) {
    let mesh = assets
        .mesh
        .get_or_insert_with(|| meshes.add(Sphere::new(PROJECTILE_RADIUS)))
        .clone();
    let idx = faction.index() as usize;
    let mat = assets.materials[idx]
        .get_or_insert_with(|| {
            materials.add(StandardMaterial {
                base_color: faction_color(faction),
                unlit: true,
                ..default()
            })
        })
        .clone();
    (mesh, mat)
}

/// 塔 AI：索敌范围内有敌方怪物时，按攻击间隔从塔顶发射追踪小球
/// 目标锁定：一旦锁定不切换，除非目标死亡（消失）或跑出攻击范围
pub fn tower_ai(
    mut commands: Commands,
    mut towers: Query<(&mut Tower, &Transform, &mut AttackTimer)>,
    monsters: Query<(Entity, &Monster, &Transform), Without<Tower>>,
    mut proj_assets: ResMut<ProjectileAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (mut tower, transform, mut timer) in &mut towers {
        let pos = transform.translation;

        // 锁定校验：目标存在、仍是敌人、且还在攻击范围内；否则解除
        if let Some(e) = tower.target {
            let valid = monsters
                .get(e)
                .map(|(_, m, t)| {
                    m.faction != tower.faction
                        && edge_dist(pos, tower.radius, t.translation, m.radius)
                            <= tower.attack_range
                })
                .unwrap_or(false);
            if !valid {
                tower.target = None;
            }
        }
        // 无锁定 → 索敌：攻击范围内最近的敌方怪物
        if tower.target.is_none() {
            tower.target = monsters
                .iter()
                .filter(|(_, m, _)| m.faction != tower.faction)
                .filter(|(_, m, t)| {
                    edge_dist(pos, tower.radius, t.translation, m.radius) <= tower.attack_range
                })
                .map(|(e, _, t)| (e, t.translation))
                .min_by(|a, b| {
                    pos.distance_squared(a.1)
                        .partial_cmp(&pos.distance_squared(b.1))
                        .unwrap()
                })
                .map(|(e, _)| e);
        }
        let Some(target_entity) = tower.target else {
            continue;
        };

        if timer.0.tick(TICK_DURATION).just_finished() {
            let (mesh, mat) =
                projectile_assets(&mut proj_assets, &mut meshes, &mut materials, tower.faction);
            commands.spawn((
                Projectile {
                    target: target_entity,
                    damage: TOWER_ATTACK_DAMAGE,
                    splash_radius: 0.0,
                    hits_air: true,
                    attacker: tower.faction,
                },
                Mesh3d(mesh),
                MeshMaterial3d(mat),
                Transform::from_translation(pos + Vec3::Y * 3.5),
                NotShadowCaster,
            ));
        }
    }
}

/// 子弹追踪目标：命中扣血（可溅射），目标已死则子弹消失
/// 目标可以是怪物（塔/建筑/远程怪的子弹）、塔或建筑卡（远程怪的子弹）
pub fn move_projectiles(
    mut commands: Commands,
    mut projectiles: Query<(Entity, &Projectile, &mut Transform), Without<Monster>>,
    monsters: Query<(Entity, &Transform, &Monster), Without<Projectile>>,
    towers: Query<(&Transform, &Tower), (Without<Monster>, Without<Projectile>)>,
    buildings: Query<(Entity, &BuildingCard, &Transform), (Without<Monster>, Without<Projectile>)>,
    mut healths: Query<&mut Health>,
) {
    for (e, proj, mut transform) in &mut projectiles {
        // 查目标位置和半径：先怪物，后塔/建筑卡
        let target = monsters
            .get(proj.target)
            .map(|(_, t, m)| (t.translation, m.radius))
            .or_else(|_| {
                towers
                    .get(proj.target)
                    .map(|(t, tw)| (t.translation, tw.radius))
            })
            .or_else(|_| {
                buildings
                    .get(proj.target)
                    .map(|(_, b, t)| (t.translation, b.radius))
            });
        let Ok((target_pos, target_radius)) = target else {
            commands.entity(e).despawn();
            continue;
        };
        let to_target = target_pos - transform.translation;
        let dist = to_target.length();
        let step = PROJECTILE_SPEED * TICK_DT;
        if dist <= step + target_radius * 0.5 {
            if let Ok(mut health) = healths.get_mut(proj.target) {
                health.current -= proj.damage;
            }
            // 溅射：以命中点为中心的范围伤害（对攻击方阵营的敌人）
            if proj.splash_radius > 0.0 {
                for (me, mt, m) in monsters.iter() {
                    if m.faction == proj.attacker || me == proj.target {
                        continue;
                    }
                    if m.flying && !proj.hits_air {
                        continue; // 对地溅射打不到空军
                    }
                    let mut d = mt.translation - target_pos;
                    d.y = 0.0;
                    if d.length() <= proj.splash_radius + m.radius {
                        if let Ok(mut health) = healths.get_mut(me) {
                            health.current -= proj.damage;
                        }
                    }
                }
                for (be, b, bt) in buildings.iter() {
                    if b.faction == proj.attacker || be == proj.target {
                        continue;
                    }
                    let mut d = bt.translation - target_pos;
                    d.y = 0.0;
                    if d.length() <= proj.splash_radius + b.radius {
                        if let Ok(mut health) = healths.get_mut(be) {
                            health.current -= proj.damage;
                        }
                    }
                }
            }
            commands.entity(e).despawn();
        } else {
            transform.translation += to_target.normalize() * step;
        }
    }
}

/// 建筑卡 AI（帧同步链内）：寿命倒计时自毁；加农炮索敌开火；墓碑出兵
pub fn building_ai(
    mut commands: Commands,
    mut buildings: Query<(Entity, &mut BuildingCard, &Transform)>,
    monsters: Query<(Entity, &Monster, &Transform), Without<BuildingCard>>,
    mut proj_assets: ResMut<ProjectileAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (e, mut building, transform) in &mut buildings {
        // 寿命到 → 自毁（不返圣水）
        building.lifetime -= TICK_DT;
        if building.lifetime <= 0.0 {
            commands.entity(e).despawn();
            continue;
        }
        let faction = building.faction;
        let pos = transform.translation;
        let radius = building.radius;

        // 攻击（加农炮类）：锁定 aggro = 攻击范围内最近的敌方怪物
        if let Some(attack) = &mut building.attack {
            if let Some(t) = attack.target {
                let valid = monsters
                    .get(t)
                    .map(|(_, m, mt)| {
                        m.faction != faction
                            && (attack.hits_air || !m.flying)
                            && edge_dist(pos, radius, mt.translation, m.radius) <= attack.range
                    })
                    .unwrap_or(false);
                if !valid {
                    attack.target = None;
                }
            }
            if attack.target.is_none() {
                attack.target = monsters
                    .iter()
                    .filter(|(_, m, _)| m.faction != faction)
                    .filter(|(_, m, _)| attack.hits_air || !m.flying)
                    .filter(|(_, m, mt)| {
                        edge_dist(pos, radius, mt.translation, m.radius) <= attack.range
                    })
                    .min_by(|a, b| {
                        pos.distance_squared(a.2.translation)
                            .partial_cmp(&pos.distance_squared(b.2.translation))
                            .unwrap()
                    })
                    .map(|(me, _, _)| me);
            }
            attack.cooldown -= TICK_DT;
            if let Some(target) = attack.target {
                if attack.cooldown <= 0.0 {
                    attack.cooldown = attack.interval;
                    let (mesh, mat) =
                        projectile_assets(&mut proj_assets, &mut meshes, &mut materials, faction);
                    commands.spawn((
                        Projectile {
                            target,
                            damage: attack.damage,
                            splash_radius: 0.0,
                            hits_air: attack.hits_air,
                            attacker: faction,
                        },
                        Mesh3d(mesh),
                        MeshMaterial3d(mat),
                        Transform::from_translation(pos + Vec3::Y * 1.2),
                        NotShadowCaster,
                    ));
                }
            }
        }

        // 出兵（墓碑类）：倒计时出一只对应卡的小兵
        if let Some(spawner) = &mut building.spawner {
            spawner.cooldown -= TICK_DT;
            if spawner.cooldown <= 0.0 {
                spawner.cooldown = spawner.interval_secs;
                if let Some(spec) = CARDS.iter().find(|c| c.id == spawner.card_id) {
                    if let CardKind::Troop(ms) = &spec.kind {
                        cards::spawn_unit(
                            &mut commands,
                            &mut meshes,
                            &mut materials,
                            faction,
                            spawner.card_id,
                            ms,
                            Vec3::new(pos.x, 0.0, pos.z),
                        );
                    }
                }
            }
        }
    }
}

/// 怪物推挤（转向力模型）：
/// - 两两碰撞时按 dir/distance 累积转向力（越近力越大）
/// - 力按质量分配：大质量怪物推开小质量怪物（轻的吃更多力）
/// - 总力钳制 MAX_STEERING_FORCE，以速度形式施加（不再硬改位置，防闪现）
/// - 飞行单位不参与地面推挤（也不互相推挤）
pub fn separate_monsters(mut monsters: Query<(&Monster, &mut Transform)>) {
    // 快照 (pos, radius, mass)：只收地面单位
    let snaps: Vec<(Vec3, f32, f32)> = monsters
        .iter()
        .filter(|(m, _)| !m.flying)
        .map(|(m, t)| (t.translation, m.radius, m.mass))
        .collect();
    let mut forces: Vec<Vec3> = vec![Vec3::ZERO; snaps.len()];

    for i in 0..snaps.len() {
        for j in (i + 1)..snaps.len() {
            let mut diff = snaps[i].0 - snaps[j].0;
            diff.y = 0.0;
            let dist = diff.length();
            let min_dist = snaps[i].1 + snaps[j].1;
            if dist < min_dist && dist > 1e-4 {
                // dir / distance：越近力越大（参考算法）
                let f = diff.normalize() / dist;
                // 质量加权：i 吃的力 ∝ j 的质量占比，j 吃的力 ∝ i 的质量占比
                let total_mass = snaps[i].2 + snaps[j].2;
                forces[i] += f * (snaps[j].2 / total_mass);
                forces[j] -= f * (snaps[i].2 / total_mass);
            }
        }
    }

    // 力的施加顺序与快照一致（iter 顺序稳定，无结构性变更）
    let mut idx = 0;
    for (m, mut transform) in monsters.iter_mut() {
        if m.flying {
            continue;
        }
        let mut f = forces[idx];
        idx += 1;
        f.y = 0.0;
        let mag = f.length();
        if mag > 1e-4 {
            // 总力钳制上限后以速度形式施加位移
            let capped = if mag > MAX_STEERING_FORCE {
                f * (MAX_STEERING_FORCE / mag)
            } else {
                f
            };
            transform.translation += capped * TICK_DT;
        }
    }
}

/// 河道禁入（硬约束）：不在桥道上的怪物不允许停留在河面，挤下去立刻推回岸边。
/// 飞行单位无视河道。转向逻辑管"走"，这个管"挤"
pub fn keep_out_of_river(mut monsters: Query<(&Monster, &mut Transform)>) {
    for (m, mut transform) in &mut monsters {
        if m.flying {
            continue;
        }
        let p = &mut transform.translation;
        if p.z.abs() < RIVER_HALF_WIDTH {
            let on_bridge = BRIDGES
                .iter()
                .any(|bx| (p.x - bx).abs() < BRIDGE_HALF_WIDTH);
            if !on_bridge {
                // 推回最近一侧岸边；正好在 z=0 时推向北岸
                let sign = if p.z >= 0.0 { 1.0 } else { -1.0 };
                p.z = sign * RIVER_HALF_WIDTH;
            }
        }
    }
}

/// 怪物与静态建筑（塔/建筑卡）的阻挡：不能穿过；飞行单位无视
pub fn separate_from_towers(
    mut monsters: Query<(&Monster, &mut Transform)>,
    towers: Query<(&Tower, &Transform), Without<Monster>>,
    buildings: Query<(&BuildingCard, &Transform), (Without<Monster>, Without<Tower>)>,
) {
    for (m, mut transform) in &mut monsters {
        if m.flying {
            continue;
        }
        for (tower, tower_transform) in &towers {
            let min_dist = tower.radius + m.radius;
            let mut diff = transform.translation - tower_transform.translation;
            diff.y = 0.0;
            let dist = diff.length();
            if dist < min_dist && dist > 1e-4 {
                transform.translation += diff.normalize() * (min_dist - dist);
            }
        }
        for (building, building_transform) in &buildings {
            let min_dist = building.radius + m.radius;
            let mut diff = transform.translation - building_transform.translation;
            diff.y = 0.0;
            let dist = diff.length();
            if dist < min_dist && dist > 1e-4 {
                transform.translation += diff.normalize() * (min_dist - dist);
            }
        }
    }
}

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

    // 追帧/回放也必须判定：否则追帧会越过对局结束点继续模拟“垃圾帧”，
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

/// 路点转向：需要过河时，先走向最近的桥口，进了桥道再直线过河；
/// 飞行单位无视河道，直线飞向目标
fn steering_goal(pos: Vec3, target: Vec3, flying: bool) -> Vec3 {
    if flying || pos.z.signum() == target.z.signum() {
        return target; // 飞行直线 / 已过河（或本就同侧），直奔目标
    }
    // 选最近的桥
    let bridge_x = BRIDGES
        .iter()
        .min_by(|a, b| {
            (pos.x - **a)
                .abs()
                .partial_cmp(&(pos.x - **b).abs())
                .unwrap()
        })
        .copied()
        .unwrap();

    if (pos.x - bridge_x).abs() < BRIDGE_HALF_WIDTH {
        // 已在桥道上：沿桥直线过河
        Vec3::new(pos.x, 0.0, target.z)
    } else {
        // 斜走向本方一侧的桥口
        Vec3::new(bridge_x, 0.0, pos.z.signum() * (RIVER_HALF_WIDTH + 0.5))
    }
}

/// 测试用白板骑士（所有新机制字段取默认关闭值）
#[cfg(test)]
pub(crate) fn test_monster(faction: Faction) -> Monster {
    Monster {
        faction,
        card: 0,
        damage: 100.0,
        attack_range: 0.75,
        aggro_range: 5.0,
        speed: 1.5,
        radius: 0.5,
        mass: 1.0,
        ranged: false,
        splash_radius: 0.0,
        hits_air: false,
        flying: false,
        building_only: false,
        engaged: false,
        target: None,
        charge: None,
        stun_secs: 0.0,
        rage_secs: 0.0,
        rage_mult: 1.0,
    }
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
                attack_range: 6.0,
                target: None,
            },
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
                    attack_range: 8.0,
                    target: None,
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

#[cfg(test)]
mod aggro_tests {
    use super::*;

    /// 两只敌对骑士在索敌范围内：必须互相锁定、接近并交战
    #[test]
    fn monsters_aggro_and_fight_each_other() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        let mk = |faction: Faction| {
            let mut m = test_monster(faction);
            m.speed = 3.0;
            m
        };
        let a = world
            .spawn((
                mk(Faction::Player),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, -2.0),
            ))
            .id();
        let b = world
            .spawn((
                mk(Faction::Enemy),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, 2.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);

        // 第 1 帧：应立即互相锁定
        schedule.run(world);
        assert_eq!(world.get::<Monster>(a).unwrap().target, Some(b));
        assert_eq!(world.get::<Monster>(b).unwrap().target, Some(a));

        // 跑 120 帧：应接近到攻击距离并互相扣血
        for _ in 0..120 {
            schedule.run(world);
        }
        let pa = world.get::<Transform>(a).unwrap().translation;
        let pb = world.get::<Transform>(b).unwrap().translation;
        let gap = (pa - pb).length();
        assert!(gap < 2.0, "两只怪没有接近：gap = {gap}");
        let ha = world.get::<Health>(a).unwrap().current;
        let hb = world.get::<Health>(b).unwrap().current;
        assert!(ha < 2000.0 && hb < 2000.0, "两只怪没有互相伤害");
    }
}

#[cfg(test)]
mod lock_retarget_tests {
    use super::*;

    fn mk(faction: Faction) -> Monster {
        test_monster(faction)
    }

    /// 走向塔途中的怪（未交战）保持目标：不被新进 aggro 的敌怪抢走锁定。
    /// 这是索敌修复的核心：旧逻辑塔是"兜底目标"，任何进 aggro 的怪都能抢锁，
    /// 导致单位反复横跳；新逻辑锁定只在目标死亡或交战后被打断时解除
    #[test]
    fn walking_monster_keeps_tower_target() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        let tower = world
            .spawn((
                Tower {
                    faction: Faction::Enemy,
                    radius: 1.0,
                    attack_range: 6.0,
                    target: None,
                },
                Health::new(6000.0),
                Transform::from_xyz(0.0, 0.0, 12.5),
            ))
            .id();
        let m = world
            .spawn((
                mk(Faction::Player),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        schedule.run(world);
        // 出生时无怪可打 → 锁塔
        assert_eq!(world.get::<Monster>(m).unwrap().target, Some(tower));
        assert!(!world.get::<Monster>(m).unwrap().engaged);

        // 敌方怪物进入 aggro：未交战的单位必须保持锁塔（不被抢锁）
        world.spawn((
            mk(Faction::Enemy),
            Health::new(2000.0),
            AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
            Transform::from_xyz(0.0, 1.0, -1.0),
        ));
        schedule.run(world);
        assert_eq!(
            world.get::<Monster>(m).unwrap().target,
            Some(tower),
            "未交战单位的锁定不应被新进 aggro 的怪抢走"
        );
    }
}

#[cfg(test)]
mod engaged_lock_tests {
    use super::*;

    fn mk(faction: Faction) -> Monster {
        test_monster(faction)
    }

    /// 已在攻击塔的怪（交战状态）绝不改目标，即使敌方怪物进入 aggro
    #[test]
    fn engaged_on_tower_never_retargets() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        let tower = world
            .spawn((
                Tower {
                    faction: Faction::Enemy,
                    radius: 1.0,
                    attack_range: 6.0,
                    target: None,
                },
                Health::new(6000.0),
                Transform::from_xyz(0.0, 0.0, 12.5),
            ))
            .id();
        // 贴着塔放（已在攻击范围内）
        let m = world
            .spawn((
                mk(Faction::Player),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, 11.3),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        schedule.run(world);
        assert_eq!(world.get::<Monster>(m).unwrap().target, Some(tower));

        // 敌方怪物进入 aggro：已进入攻击状态的怪不得改目标
        world.spawn((
            mk(Faction::Enemy),
            Health::new(2000.0),
            AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
            Transform::from_xyz(0.0, 1.0, 9.0),
        ));
        schedule.run(world);
        assert_eq!(world.get::<Monster>(m).unwrap().target, Some(tower));
    }
}

#[cfg(test)]
mod interrupt_tests {
    use super::*;

    fn mk(faction: Faction) -> Monster {
        test_monster(faction)
    }

    /// 站桩输出被挤到脱离攻击范围 = 被打断：锁定必须解除并改锁挤它的怪
    #[test]
    fn pushed_out_of_range_breaks_lock() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        let tower = world
            .spawn((
                Tower {
                    faction: Faction::Enemy,
                    radius: 1.0,
                    attack_range: 6.0,
                    target: None,
                },
                Health::new(6000.0),
                Transform::from_xyz(0.0, 0.0, 12.5),
            ))
            .id();
        let m = world
            .spawn((
                mk(Faction::Player),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, 11.3), // 贴塔，处于交战状态
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        schedule.run(world);
        assert_eq!(world.get::<Monster>(m).unwrap().target, Some(tower));

        // 模拟被挤开：挪到塔的攻击范围外，同时挤它的敌怪就在 aggro 内
        world.get_mut::<Transform>(m).unwrap().translation = Vec3::new(0.0, 1.0, 9.0);
        let e = world
            .spawn((
                mk(Faction::Enemy),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, 8.0),
            ))
            .id();

        schedule.run(world);
        // 被打断 → 改锁挤它的怪，而不是走回塔
        assert_eq!(world.get::<Monster>(m).unwrap().target, Some(e));
    }
}

#[cfg(test)]
mod steering_tests {
    use super::*;

    fn mk_with_mass(faction: Faction, mass: f32) -> Monster {
        Monster {
            mass,
            ..test_monster(faction)
        }
    }

    /// 质量加权推挤：重叠时小质量位移远大于大质量
    #[test]
    fn heavy_pushes_light_more() {
        let mut app = App::new();
        let world = app.world_mut();
        let heavy = world
            .spawn((
                mk_with_mass(Faction::Player, 3.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let light = world
            .spawn((
                mk_with_mass(Faction::Player, 0.3),
                Transform::from_xyz(0.6, 1.0, 0.0), // 重叠（0.6 < 1.0）
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(separate_monsters);
        schedule.run(world);

        let heavy_move = world
            .get::<Transform>(heavy)
            .unwrap()
            .translation
            .distance(Vec3::new(0.0, 1.0, 0.0));
        let light_move = world
            .get::<Transform>(light)
            .unwrap()
            .translation
            .distance(Vec3::new(0.6, 1.0, 0.0));
        assert!(
            light_move > heavy_move * 3.0,
            "小质量位移({light_move})应远大于大质量({heavy_move})"
        );
        // 单帧位移不得超过力上限（防闪现）
        assert!(light_move <= MAX_STEERING_FORCE * TICK_DT + 1e-6);
        assert!(heavy_move <= MAX_STEERING_FORCE * TICK_DT + 1e-6);
    }
}

#[cfg(test)]
mod new_mechanics_tests {
    use super::*;

    /// 只攻建筑单位（巨人/野猪）：无视 aggro 内的敌怪，直奔塔/建筑卡
    #[test]
    fn building_only_ignores_monsters() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        let tower = world
            .spawn((
                Tower {
                    faction: Faction::Enemy,
                    radius: 1.0,
                    attack_range: 6.0,
                    target: None,
                },
                Health::new(6000.0),
                Transform::from_xyz(0.0, 0.0, 12.5),
            ))
            .id();
        let mut giant = test_monster(Faction::Player);
        giant.building_only = true;
        let g = world
            .spawn((
                giant,
                Health::new(5000.0),
                AttackTimer(Timer::from_seconds(1.5, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();
        // 敌方骷髅进 aggro（距离 4，边缘距 3 ≤ 5）
        world.spawn((
            test_monster(Faction::Enemy),
            Health::new(300.0),
            AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
            Transform::from_xyz(0.0, 1.0, -1.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        schedule.run(world);
        assert_eq!(
            world.get::<Monster>(g).unwrap().target,
            Some(tower),
            "只攻建筑单位必须无视怪物直奔塔"
        );
    }

    /// 不能对空的地面单位：打不了飞行单位，索敌跳过空军
    #[test]
    fn ground_unit_cannot_target_flying() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        let tower = world
            .spawn((
                Tower {
                    faction: Faction::Enemy,
                    radius: 1.0,
                    attack_range: 6.0,
                    target: None,
                },
                Health::new(6000.0),
                Transform::from_xyz(0.0, 0.0, 12.5),
            ))
            .id();
        let knight = world
            .spawn((
                test_monster(Faction::Player),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();
        // 敌方飞行单位（亡灵）贴脸
        let mut minion = test_monster(Faction::Enemy);
        minion.flying = true;
        world.spawn((
            minion,
            Health::new(320.0),
            AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
            Transform::from_xyz(0.0, 2.6, -5.4),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        schedule.run(world);
        assert_eq!(
            world.get::<Monster>(knight).unwrap().target,
            Some(tower),
            "不能对空的单位必须跳过飞行单位"
        );
    }

    /// 近战溅射（瓦基丽）：攻击目标时波及身边的第二个敌人
    #[test]
    fn melee_splash_hits_nearby_enemy() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        let mut valk = test_monster(Faction::Player);
        valk.splash_radius = 1.5;
        let a = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.9, 1.0, 0.0), // 贴脸（主目标）
            ))
            .id();
        let b = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, 1.0), // 溅射半径内
            ))
            .id();
        world.spawn((
            valk,
            Health::new(2000.0),
            AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
            Transform::from_xyz(0.0, 1.0, 0.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        for _ in 0..35 {
            schedule.run(world); // 35 tick > 1.0s 攻击间隔
        }
        assert!(world.get::<Health>(a).unwrap().current < 2000.0, "主目标掉血");
        assert!(
            world.get::<Health>(b).unwrap().current < 2000.0,
            "溅射半径内的第二个敌人也必须掉血"
        );
    }

    /// 晕眩：完全无法行动（位置不动），晕完恢复移动
    #[test]
    fn stun_freezes_monster() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        world.spawn((
            Tower {
                faction: Faction::Enemy,
                radius: 1.0,
                attack_range: 6.0,
                target: None,
            },
            Health::new(6000.0),
            Transform::from_xyz(0.0, 0.0, 12.5),
        ));
        let mut m = test_monster(Faction::Player);
        m.stun_secs = 1.0;
        let e = world
            .spawn((
                m,
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        for _ in 0..29 {
            schedule.run(world); // 29 tick < 1.0s 晕眩
        }
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation,
            Vec3::new(0.0, 1.0, -5.0),
            "晕眩期间不得移动"
        );
        for _ in 0..10 {
            schedule.run(world); // 晕眩结束
        }
        assert!(
            world.get::<Transform>(e).unwrap().translation.z > -5.0,
            "晕眩结束后必须恢复移动（走向塔）"
        );
    }

    /// 冲锋（王子）：持续移动蓄力，蓄满后移速倍增
    #[test]
    fn charge_accelerates_after_windup() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        world.spawn((
            Tower {
                faction: Faction::Enemy,
                radius: 1.0,
                attack_range: 6.0,
                target: None,
            },
            Health::new(60000.0),
            Transform::from_xyz(0.0, 0.0, 12.5),
        ));
        let mut prince = test_monster(Faction::Player);
        prince.charge = Some(ChargeState {
            progress: 0.0,
            windup: 0.5,
            speed_mult: 3.0,
            damage_mult: 2.0,
        });
        let e = world
            .spawn((
                prince,
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems(monster_ai);
        let start = world.get::<Transform>(e).unwrap().translation;
        for _ in 0..20 {
            schedule.run(world);
        }
        // 20 tick = 0.667s：前 0.5s 常速 1.5（走 0.75），后 0.167s 冲锋 4.5（走 0.75）
        // 合计 ~1.4-1.5 > 常速上限 1.0 —— 冲锋必须显著加快（走桥是斜向，量总位移）
        let moved = world.get::<Transform>(e).unwrap().translation.distance(start);
        assert!(
            moved > 1.25,
            "蓄满冲锋后移速必须倍增：实际移动 {moved:.2}"
        );
    }

    /// 建筑 AI：墓碑定时出兵、加农炮索敌开火
    #[test]
    fn building_ai_spawns_and_fires() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        // 墓碑：4s 一只骷髅（card 1）
        world.spawn((
            BuildingCard {
                faction: Faction::Player,
                card: 20,
                radius: 0.6,
                lifetime: 100.0,
                attack: None,
                spawner: Some(BuildingSpawnerState {
                    interval_secs: 4.0,
                    card_id: 1,
                    cooldown: 4.0,
                }),
            },
            Health::new(800.0),
            Transform::from_xyz(-4.0, 0.7, -5.0),
        ));
        // 加农炮：0.9s 一发，打不到空军
        world.spawn((
            BuildingCard {
                faction: Faction::Enemy,
                card: 19,
                radius: 0.6,
                lifetime: 100.0,
                attack: Some(BuildingAttackState {
                    damage: 90.0,
                    range: 5.0,
                    interval: 0.9,
                    hits_air: false,
                    cooldown: 0.0,
                    target: None,
                }),
                spawner: None,
            },
            Health::new(1400.0),
            Transform::from_xyz(4.0, 0.7, 5.0),
        ));
        // 蓝方怪走进红方加农炮射程（距离 < 5）
        world.spawn((
            test_monster(Faction::Player),
            Health::new(2000.0),
            AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
            Transform::from_xyz(4.0, 1.0, 1.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(building_ai);
        for _ in 0..125 {
            schedule.run(world); // 4.17s
        }
        // 墓碑出了 1 只骷髅（4s 时），第 2 只要 8s
        let mut monsters = world.query::<&Monster>();
        let skeletons = monsters
            .iter(world)
            .filter(|m| m.card == 1)
            .count();
        assert_eq!(skeletons, 1, "墓碑 4s 应出 1 只骷髅");
        // 加农炮已开火：场上存在追踪子弹
        let mut projectiles = world.query::<&Projectile>();
        let fired = projectiles
            .iter(world)
            .filter(|p| p.attacker == Faction::Enemy)
            .count();
        assert!(fired > 0, "加农炮必须对射程内敌人开火");
    }

    /// 建筑 AI：寿命归零自毁
    #[test]
    fn building_expires_after_lifetime() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>();
        let world = app.world_mut();

        world.spawn((
            BuildingCard {
                faction: Faction::Player,
                card: 19,
                radius: 0.6,
                lifetime: 1.0,
                attack: None,
                spawner: None,
            },
            Health::new(1400.0),
            Transform::from_xyz(0.0, 0.7, -5.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(building_ai);
        for _ in 0..35 {
            schedule.run(world); // 1.17s > 1.0s 寿命
        }
        let mut buildings = world.query::<&BuildingCard>();
        assert_eq!(buildings.iter(world).count(), 0, "寿命到必须自毁");
    }
}
