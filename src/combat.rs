//! 战斗：出兵、怪物/塔 AI、子弹、碰撞阻挡、死亡清除

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

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
    decks: Res<Decks>,
    selected: Res<SelectedCard>,
    buttons: Query<&Interaction, With<Button>>,
    towers: Query<(&Tower, &Transform, Option<&KingTower>)>,
    mut pending: ResMut<PendingClicks>,
    net: Option<Res<NetClient>>,
) {
    // 等待/追帧/回放/对局结束期间不采集点击（防止恢复后指令倾泻）
    if !matches!(*state, net::SimState::Solo | net::SimState::Playing) {
        return;
    }
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    // 点在 UI（卡槽按钮）上：那是选牌操作，不在场景放怪
    if buttons.iter().any(|i| *i != Interaction::None) {
        return;
    }
    let Some(cursor) = window.cursor_position() else {
        return;
    };
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
        // 联网：阵营由服务器序号决定；部署区域按 CR 规则校验
        // （自己半场任意；推掉敌侧公主塔后可在该侧敌半场下怪）
        Some(n) => {
            let Some(f) = Faction::from_index(n.my_index) else {
                return; // 还没分配到序号（观战/等待中）
            };
            let tower_snaps: Vec<(Faction, bool, Vec3)> = towers
                .iter()
                .map(|(t, tr, k)| (t.faction, k.is_some(), tr.translation))
                .collect();
            if !cards::deploy_allowed(f, point, &tower_snaps) {
                return; // 区域不可部署：无效操作
            }
            f
        }
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

/// 可被攻击单位的快照，避免索敌时嵌套查询
struct UnitSnap {
    entity: Entity,
    faction: Faction,
    pos: Vec3,
    radius: f32,
    is_tower: bool,
}

/// 水平边缘距离（忽略 y，减去双方半径）
fn edge_dist(a_pos: Vec3, a_r: f32, b_pos: Vec3, b_r: f32) -> f32 {
    let mut d = a_pos - b_pos;
    d.y = 0.0;
    d.length() - a_r - b_r
}

/// aggro 范围内最近的敌方怪物
fn nearest_enemy_monster<'a>(
    snaps: &'a [UnitSnap],
    pos: Vec3,
    monster: &Monster,
    self_entity: Entity,
) -> Option<&'a UnitSnap> {
    snaps
        .iter()
        .filter(|s| !s.is_tower && s.faction != monster.faction && s.entity != self_entity)
        .filter(|s| edge_dist(pos, monster.radius, s.pos, s.radius) <= monster.aggro_range)
        .min_by(|a, b| {
            pos.distance_squared(a.pos)
                .partial_cmp(&pos.distance_squared(b.pos))
                .unwrap()
        })
}

/// 怪物 AI（属性来自卡牌规格）：
/// - 目标锁定：一旦锁定不切换，直到目标消失（死亡）才重新索敌
/// - 索敌范围内有敌方怪物 → 打最近的怪；否则 → 打最近的敌塔
/// - 进入攻击范围 → 停下攻击：近战直接扣血，远程发射子弹
/// - 未进入 → 朝目标移动（过河走桥）
pub fn monster_ai(
    mut commands: Commands,
    mut monsters: Query<(Entity, &mut Monster, &mut Transform, &mut AttackTimer), Without<Tower>>,
    towers: Query<(Entity, &Tower, &Transform), Without<Monster>>,
    mut healths: Query<&mut Health>,
    mut proj_assets: ResMut<ProjectileAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // 快照所有单位
    let mut snaps: Vec<UnitSnap> = monsters
        .iter()
        .map(|(e, m, t, _)| UnitSnap {
            entity: e,
            faction: m.faction,
            pos: t.translation,
            radius: m.radius,
            is_tower: false,
        })
        .collect();
    snaps.extend(towers.iter().map(|(e, t, tr)| UnitSnap {
        entity: e,
        faction: t.faction,
        pos: tr.translation,
        radius: t.radius,
        is_tower: true,
    }));

    for (entity, mut monster, mut transform, mut timer) in monsters.iter_mut() {
        let pos = transform.translation;

        // 锁定失效即解除：目标消失（死亡），或已脱离攻击范围（被打断——
        // 比如站桩输出时被新放置的怪挤开）。解除后下方立刻重新索敌，
        // 仍在 aggro 内最近的目标会被重新锁定（可能就是挤它的那只）。
        if let Some(e) = monster.target {
            let invalid = match snaps
                .iter()
                .find(|s| s.entity == e && s.faction != monster.faction)
            {
                None => true, // 目标已消失
                Some(s) => {
                    edge_dist(pos, monster.radius, s.pos, s.radius)
                        > monster.attack_range + 0.05
                }
            };
            if invalid {
                monster.target = None;
            }
        }
        // 无锁定 → 索敌：aggro 内最近的敌方怪物；没有怪可打 → 最近的敌塔
        if monster.target.is_none() {
            monster.target = nearest_enemy_monster(&snaps, pos, &monster, entity)
                .or_else(|| {
                    snaps
                        .iter()
                        .filter(|s| s.is_tower && s.faction != monster.faction)
                        .min_by(|a, b| {
                            pos.distance_squared(a.pos)
                                .partial_cmp(&pos.distance_squared(b.pos))
                                .unwrap()
                        })
                })
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
        let goal = steering_goal(pos, target.pos);
        let mut to_goal = goal - pos;
        to_goal.y = 0.0;
        let dist = to_goal.length();

        if edge <= monster.attack_range + 0.05 {
            // 在攻击范围内：停下攻击（固定步长 tick，保证确定性）
            if timer.0.tick(TICK_DURATION).just_finished() {
                if monster.ranged {
                    // 远程：发射追踪子弹
                    let (mesh, mat) = projectile_assets(
                        &mut proj_assets,
                        &mut meshes,
                        &mut materials,
                        monster.faction,
                    );
                    commands.spawn((
                        Projectile {
                            target: target.entity,
                            damage: monster.damage,
                        },
                        Mesh3d(mesh),
                        MeshMaterial3d(mat),
                        Transform::from_translation(pos + Vec3::Y * 1.5),
                        NotShadowCaster,
                    ));
                } else if let Ok(mut health) = healths.get_mut(target.entity) {
                    // 近战：直接扣血
                    health.current -= monster.damage;
                }
            }
        } else if dist > 1e-4 {
            let step = monster.speed * TICK_DT;
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
                },
                Mesh3d(mesh),
                MeshMaterial3d(mat),
                Transform::from_translation(pos + Vec3::Y * 3.5),
                NotShadowCaster,
            ));
        }
    }
}

/// 子弹追踪目标：命中扣血，目标已死则子弹消失
/// 目标可以是怪物（塔的子弹）或塔（远程怪的子弹）
pub fn move_projectiles(
    mut commands: Commands,
    mut projectiles: Query<(Entity, &Projectile, &mut Transform), Without<Monster>>,
    monsters: Query<(&Transform, &Monster), Without<Projectile>>,
    towers: Query<(&Transform, &Tower), (Without<Monster>, Without<Projectile>)>,
    mut healths: Query<&mut Health>,
) {
    for (e, proj, mut transform) in &mut projectiles {
        // 查目标位置和半径：先怪物后塔
        let target = monsters
            .get(proj.target)
            .map(|(t, m)| (t.translation, m.radius))
            .or_else(|_| {
                towers
                    .get(proj.target)
                    .map(|(t, tw)| (t.translation, tw.radius))
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
            commands.entity(e).despawn();
        } else {
            transform.translation += to_target.normalize() * step;
        }
    }
}

/// 怪物推挤（转向力模型）：
/// - 两两碰撞时按 dir/distance 累积转向力（越近力越大）
/// - 力按质量分配：大质量怪物推开小质量怪物（轻的吃更多力）
/// - 总力钳制 MAX_STEERING_FORCE，以速度形式施加（不再硬改位置，防闪现）
pub fn separate_monsters(mut monsters: Query<(&Monster, &mut Transform)>) {
    // 快照 (pos, radius, mass)
    let snaps: Vec<(Vec3, f32, f32)> = monsters
        .iter()
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

    for (i, (_, mut transform)) in monsters.iter_mut().enumerate() {
        let mut f = forces[i];
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

/// 河道禁入（硬约束）：不在桥道上的怪物不允许停留在河面，挤下去立刻推回岸边
/// 转向逻辑管"走"，这个管"挤"
pub fn keep_out_of_river(mut monsters: Query<&mut Transform, With<Monster>>) {
    for mut transform in &mut monsters {
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

/// 怪物与塔的静态阻挡：不能穿过塔身
pub fn separate_from_towers(
    mut monsters: Query<(&Monster, &mut Transform)>,
    towers: Query<(&Tower, &Transform), Without<Monster>>,
) {
    for (m, mut transform) in &mut monsters {
        for (tower, tower_transform) in &towers {
            let min_dist = tower.radius + m.radius;
            let mut diff = transform.translation - tower_transform.translation;
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

/// 路点转向：需要过河时，先走向最近的桥口，进了桥道再直线过河
fn steering_goal(pos: Vec3, target: Vec3) -> Vec3 {
    if pos.z.signum() == target.z.signum() {
        return target; // 已过河（或本就同侧），直奔目标
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
            Monster {
                faction,
                damage: 100.0,
                attack_range: 0.75,
                aggro_range: 5.0,
                speed: 1.5,
                radius: 0.5,
                mass: 1.0,
                ranged: false,
                target: None,
            },
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

        let mk = |faction: Faction| Monster {
            faction,
            damage: 100.0,
            attack_range: 0.75,
            aggro_range: 5.0,
            speed: 3.0,
            radius: 0.5,
            mass: 1.0,
            ranged: false,
            target: None,
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
        Monster {
            faction,
            damage: 100.0,
            attack_range: 0.75,
            aggro_range: 5.0,
            speed: 1.5,
            radius: 0.5,
            mass: 1.0,
            ranged: false,
            target: None,
        }
    }

    /// 锁塔的怪在敌方怪物进入 aggro 后必须改锁怪物（塔只是兜底目标）
    #[test]
    fn tower_locked_monster_retargets_to_enemy_monster() {
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

        // 敌方怪物进入 aggro → 必须改锁怪物
        let e = world
            .spawn((
                mk(Faction::Enemy),
                Health::new(2000.0),
                AttackTimer(Timer::from_seconds(1.0, TimerMode::Repeating)),
                Transform::from_xyz(0.0, 1.0, -1.0),
            ))
            .id();
        schedule.run(world);
        assert_eq!(world.get::<Monster>(m).unwrap().target, Some(e));
    }
}

#[cfg(test)]
mod engaged_lock_tests {
    use super::*;

    fn mk(faction: Faction) -> Monster {
        Monster {
            faction,
            damage: 100.0,
            attack_range: 0.75,
            aggro_range: 5.0,
            speed: 1.5,
            radius: 0.5,
            mass: 1.0,
            ranged: false,
            target: None,
        }
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
        Monster {
            faction,
            damage: 100.0,
            attack_range: 0.75,
            aggro_range: 5.0,
            speed: 1.5,
            radius: 0.5,
            mass: 1.0,
            ranged: false,
            target: None,
        }
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
            faction,
            damage: 100.0,
            attack_range: 0.75,
            aggro_range: 5.0,
            speed: 1.5,
            radius: 0.5,
            mass,
            ranged: false,
            target: None,
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
