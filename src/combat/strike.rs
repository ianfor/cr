//! 统一打击：在途 Strike 推进（追踪弹飞行 / 法术分波）+ 统一结算 detonate。
//!
//! detonate 是全部命中效果的唯一结算口（近战直击/追踪弹命中/法术瞬发/
//! 法术波共用）：
//! - primary 必中（近战直击/追踪弹贴身），AOE 循环排除 primary（防双份）
//! - AOE 半径语义统一为 splash_radius + 目标半径
//! - buff 延迟施加（commands.queue 世界闭包）：调用方系统可能同时持有
//!   攻击方 Buffs 的读引用，同帧直接写会撞 borrow；chain 的自动 sync point
//!   保证同 tick 下游系统即可见
//! - AOE 早退守卫：纯伤害单体负载（无溅射无 buff）不进全量遍历，
//!   防 300 单位压测 O(n²)

use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

/// 子弹共享资源：mesh 和各阵营材质只建一次，避免每发子弹新建资产
#[derive(Resource, Default)]
pub struct ProjectileAssets {
    mesh: Option<Handle<Mesh>>,
    materials: [Option<Handle<StandardMaterial>>; 2],
}

pub(crate) fn projectile_assets(
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

/// 统一命中结算（近战直击/追踪弹命中/法术瞬发/法术波共用）。
/// - primary：必中直击（近战目标/追踪弹目标），无视半径判定；
///   enemy_buffs 对 primary 同样生效（近战附带晕眩等未来扩展）
/// - AOE：以 center 为中心，半径 = splash_radius + 目标半径；
///   敌方吃伤害 + enemy_buffs（仅部队），己方吃 ally_buffs（仅部队）；
///   !hits_towers 时塔（公主塔/王塔）不吃 AOE，建筑卡不受影响
/// - 纯单体直击（splash=0 且无 buff）早退，不进全量遍历
pub(crate) fn detonate(
    commands: &mut Commands,
    targets: &mut Query<
        (
            Entity,
            &Unit,
            &Transform,
            &mut Health,
        ),
        Without<Strike>,
    >,
    payload: &Payload,
    attacker: Faction,
    center: Vec3,
    primary: Option<Entity>,
) {
    // 直击：必中（伤害直接结算；buff 与 AOE 同规则——仅部队）
    if let Some(pe) = primary {
        if let Ok((e, u, _, mut hp)) = targets.get_mut(pe) {
            hp.current -= payload.damage;
            if u.kind == UnitKind::Troop && !payload.enemy_buffs.is_empty() {
                queue_buffs(commands, e, payload.enemy_buffs.clone());
            }
        }
    }
    // AOE 早退守卫：无溅射无 buff 的单体负载到此为止
    if payload.splash_radius <= 0.0 && !payload.has_buffs() {
        return;
    }
    for (e, u, t, mut hp) in targets.iter_mut() {
        if Some(e) == primary {
            continue; // primary 已在直击结算，排除防双份
        }
        let mut d = t.translation - center;
        d.y = 0.0;
        if u.faction == attacker {
            // 己方：仅 ally_buffs（狂暴），仅部队，不吃伤害
            if u.kind != UnitKind::Troop || payload.ally_buffs.is_empty() {
                continue;
            }
            if d.length() <= payload.splash_radius + u.radius {
                queue_buffs(commands, e, payload.ally_buffs.clone());
            }
        } else {
            // 敌方：伤害 + enemy_buffs（仅部队）
            if !payload.hits_towers && u.kind.is_tower() {
                continue; // 塔不吃弹溅/法术（显式化旧规则）
            }
            if u.flying && !payload.hits_air {
                continue; // 对地攻击打不到空军
            }
            if d.length() <= payload.splash_radius + u.radius {
                hp.current -= payload.damage;
                if u.kind == UnitKind::Troop && !payload.enemy_buffs.is_empty() {
                    queue_buffs(commands, e, payload.enemy_buffs.clone());
                }
            }
        }
    }
}

/// buff 延迟施加：排队到命令队列（世界闭包），调用方系统的
/// Buffs 读引用不会被同帧写撞坏；chain 的自动 sync point 保证同 tick 下游可见
fn queue_buffs(commands: &mut Commands, entity: Entity, incoming: Vec<ActiveBuff>) {
    commands.queue(move |world: &mut World| apply_buffs(world, entity, incoming));
}

/// 世界侧 buff 施加：已有容器走 apply 合并，否则插入新容器
fn apply_buffs(world: &mut World, entity: Entity, incoming: Vec<ActiveBuff>) {
    if incoming.is_empty() {
        return;
    }
    if world.get_entity_mut(entity).is_err() {
        return; // 实体已被更早的命令销毁（死亡清除等）
    }
    if let Some(mut existing) = world.get_mut::<Buffs>(entity) {
        for b in incoming {
            existing.apply(b);
        }
        return;
    }
    let mut iter = incoming.into_iter();
    if let Some(first) = iter.next() {
        let mut container = Buffs::new(first);
        for b in iter {
            container.apply(b);
        }
        world.entity_mut(entity).insert(container);
    }
}

/// 在途打击推进（帧同步链内，链位 = 旧 move_projectiles 位）：
/// - Homing：查目标 → 追踪移动 → 贴身以目标位置为中心 detonate（primary 必中）；
///   目标已死则 Strike 消失
/// - Volley：倒数到 0 → 以落点为中心 detonate 一波 → 波数耗尽销毁 /
///   重置 next_in；首波后清空 buff（多波卡目前无 buff，机制休眠但完整）
pub fn strike_tick(
    mut commands: Commands,
    mut strikes: Query<(Entity, &mut Strike, &mut Transform), Without<Unit>>,
    mut targets: Query<
        (
            Entity,
            &Unit,
            &Transform,
            &mut Health,
        ),
        Without<Strike>,
    >,
) {
    for (e, mut strike, mut transform) in &mut strikes {
        // 先把飞行参数拷出（Flight 两分支全是 Copy 数据）：
        // match 对 flight 的可变借用不能贯穿分支体（分支内还要写 payload）
        enum Step {
            Homing(Entity),
            Volley {
                x: f32,
                z: f32,
                waves_left: u32,
                next_in: u32,
                interval: u32,
            },
        }
        let step = match &strike.flight {
            Flight::Homing { target } => Step::Homing(*target),
            Flight::Volley {
                x,
                z,
                waves_left,
                next_in,
                interval,
            } => Step::Volley {
                x: *x,
                z: *z,
                waves_left: *waves_left,
                next_in: *next_in,
                interval: *interval,
            },
        };
        match step {
            Step::Homing(target) => {
                // 查目标位置和半径（怪/塔/建筑统一）；目标已死则打击消失
                let Ok((_, u, t, _)) = targets.get_mut(target) else {
                    commands.entity(e).despawn();
                    continue;
                };
                let target_pos = t.translation;
                let target_radius = u.radius;
                let to_target = target_pos - transform.translation;
                let dist = to_target.length();
                let step = PROJECTILE_SPEED * TICK_DT;
                if dist <= step + target_radius * 0.5 {
                    // 贴身结算：目标位置为中心，primary 必中
                    let payload = strike.payload.clone();
                    let attacker = strike.attacker;
                    detonate(
                        &mut commands,
                        &mut targets,
                        &payload,
                        attacker,
                        target_pos,
                        Some(target),
                    );
                    commands.entity(e).despawn();
                } else {
                    transform.translation += to_target.normalize() * step;
                }
            }
            Step::Volley {
                x,
                z,
                waves_left,
                next_in,
                interval,
            } => {
                if next_in > 0 {
                    strike.flight = Flight::Volley {
                        x,
                        z,
                        waves_left,
                        next_in: next_in - 1,
                        interval,
                    };
                    continue;
                }
                let center = Vec3::new(x, 0.0, z);
                let payload = strike.payload.clone();
                let attacker = strike.attacker;
                detonate(&mut commands, &mut targets, &payload, attacker, center, None);
                // 首波后清空 buff：多波卡目前无 buff（带了也只该首波生效），
                // 机制休眠但完整
                strike.payload.enemy_buffs.clear();
                strike.payload.ally_buffs.clear();
                let waves_left = waves_left - 1;
                if waves_left == 0 {
                    commands.entity(e).despawn();
                } else {
                    // −1 补栅栏：本帧已结算（next_in 从 0 起数），
                    // 重置 interval−1 使波间隔恰为 interval 帧
                    strike.flight = Flight::Volley {
                        x,
                        z,
                        waves_left,
                        next_in: interval - 1,
                        interval,
                    };
                }
            }
        }
    }
}
