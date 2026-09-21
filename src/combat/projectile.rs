//! 子弹：共享弹体资产 + 追踪飞行 + 命中结算（单体伤害与命中点溅射）。
//! 目标可以是怪物（塔/建筑/远程怪的子弹）、塔或建筑卡（远程怪的子弹）

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

/// 子弹追踪目标：命中扣血（可溅射），目标已死则子弹消失
pub fn move_projectiles(
    mut commands: Commands,
    mut projectiles: Query<(Entity, &Projectile, &mut Transform), Without<Monster>>,
    monsters: Query<(Entity, &Transform, &Monster, Option<&Flying>), Without<Projectile>>,
    towers: Query<(Entity, &Transform, &Tower), (Without<Monster>, Without<Projectile>)>,
    buildings: Query<
        (Entity, &Transform, &BuildingCard),
        (Without<Monster>, Without<Projectile>),
    >,
    mut healths: Query<&mut Health>,
) {
    for (e, proj, mut transform) in &mut projectiles {
        // 查目标位置和半径：先怪物，后塔/建筑卡
        let target = monsters
            .get(proj.target)
            .map(|(_, t, m, _)| (t.translation, m.radius))
            .or_else(|_| {
                towers
                    .get(proj.target)
                    .map(|(_, t, tw)| (t.translation, tw.radius))
            })
            .or_else(|_| {
                buildings
                    .get(proj.target)
                    .map(|(_, t, b)| (t.translation, b.radius))
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
                for (me, mt, m, flying) in monsters.iter() {
                    if m.faction == proj.attacker || me == proj.target {
                        continue;
                    }
                    if flying.is_some() && !proj.hits_air {
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
                for (be, bt, b) in buildings.iter() {
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
