//! 物理约束：单位推挤（质量加权转向力）、河道禁入、静态阻挡（塔/建筑卡）。
//! 飞行单位不参与地面物理（不推不挤、不被挡、无视河道）。

use bevy::prelude::*;

use super::WorldSnaps;
use crate::components::*;
use crate::constants::*;

/// 怪物推挤（转向力模型）：
/// - 两两碰撞时按 dir/distance 累积转向力（越近力越大）
/// - 力按质量分配：大质量怪物推开小质量怪物（轻的吃更多力）
/// - 总力钳制 MAX_STEERING_FORCE，以速度形式施加（不再硬改位置，防闪现）
/// - 飞行单位不参与地面推挤（也不互相推挤）
/// - 全程消费帧首快照（WorldSnaps）：配对用快照坐标互比——桶与坐标同源，
///   无过期无 padding；施力一遍统一写回，不碰查询
/// - 语义：本帧移动新产生的重叠下一帧才被解开（一帧 33ms 延迟）；
///   帧中出生的单位（墓碑骷髅）下一帧起参与推挤
pub fn separate_monsters(
    snaps: Res<WorldSnaps>,
    mut monsters: Query<(Entity, Option<&Flying>, &mut Transform), With<Monster>>,
) {    // 力累积：与快照下标平行
    let mut forces = vec![Vec3::ZERO; snaps.snaps.len()];
    // 配对：快照坐标互比（网格桶同样建自这份坐标）
    for i in 0..snaps.snaps.len() {
        let s_i = &snaps.snaps[i];
        if s_i.is_building_kind() || s_i.flying {
            continue; // 只推地面怪
        }
        // 邻域半径 = r_i + MONSTER_RADIUS_MAX：覆盖一切可能接触的对
        snaps
            .grid
            .for_each_in_circle(s_i.pos, s_i.radius + MONSTER_RADIUS_MAX, &mut |j| {
                let j = j as usize;
                if j <= i {
                    return; // 每对只处理一次（较小 i 的一侧）
                }
                let s_j = &snaps.snaps[j];
                if s_j.flying {
                    return; // 桶里只有怪，但飞行怪不参与地面推挤
                }
                let mut diff = s_i.pos - s_j.pos;
                diff.y = 0.0;
                let dist = diff.length();
                let min_dist = s_i.radius + s_j.radius;
                if dist < min_dist && dist > 1e-4 {
                    // dir / distance：越近力越大（参考算法）
                    let f = diff.normalize() / dist;
                    // 质量加权：i 吃的力 ∝ j 的质量占比，j 吃的力 ∝ i 的质量占比
                    let total_mass = s_i.mass + s_j.mass;
                    forces[i] += f * (s_j.mass / total_mass);
                    forces[j] -= f * (s_i.mass / total_mass);
                }
            });
    }
    // 施力：统一写回（entity → 快照下标点查；不在快照里的单位跳过）
    for (e, f, mut transform) in monsters.iter_mut() {
        if f.is_some() {
            continue;
        }
        let Some(&i) = snaps.index.get(&e) else {
            continue;
        };
        let mut d = forces[i as usize];
        d.y = 0.0;
        let mag = d.length();
        if mag > 1e-4 {
            // 总力钳制上限后以速度形式施加位移
            let capped = if mag > MAX_STEERING_FORCE {
                d * (MAX_STEERING_FORCE / mag)
            } else {
                d
            };
            transform.translation += capped * TICK_DT;
        }
    }
}

/// 河道禁入（硬约束）：不在桥道上的怪物不允许停留在河面，挤下去立刻推回岸边。
/// 飞行单位无视河道。转向逻辑管"走"，这个管"挤"
pub fn keep_out_of_river(mut monsters: Query<(&Monster, Option<&Flying>, &mut Transform)>) {
    for (_, f, mut transform) in &mut monsters {
        if f.is_some() {
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
pub fn separate_from_statics(
    mut monsters: Query<(&Monster, Option<&Flying>, &mut Transform)>,
    towers: Query<(&Tower, &Transform), Without<Monster>>,
    buildings: Query<(&BuildingCard, &Transform), (Without<Monster>, Without<Tower>)>,
) {
    for (m, f, mut transform) in &mut monsters {
        if f.is_some() {
            continue;
        }
        for (tower, tower_transform) in &towers {
            push_out(&mut transform, m.radius, tower_transform.translation, tower.radius);
        }
        for (building, building_transform) in &buildings {
            push_out(
                &mut transform,
                m.radius,
                building_transform.translation,
                building.radius,
            );
        }
    }
}

/// 把单位推出静态圆形碰撞体（重叠时沿连线推到刚好不重叠）
fn push_out(transform: &mut Transform, self_radius: f32, center: Vec3, static_radius: f32) {
    let min_dist = static_radius + self_radius;
    let mut diff = transform.translation - center;
    diff.y = 0.0;
    let dist = diff.length();
    if dist < min_dist && dist > 1e-4 {
        transform.translation += diff.normalize() * (min_dist - dist);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{targeting, WorldSnaps};
    use super::*;

    /// 质量加权推挤：重叠时小质量位移远大于大质量。
    /// 推挤消费帧首快照：必须先跑 targeting 构建 WorldSnaps
    #[test]
    fn heavy_pushes_light_more() {
        let mut app = App::new();
        app.init_resource::<WorldSnaps>();
        let world = app.world_mut();
        let mut heavy = test_monster(Faction::Player);
        heavy.mass = 3.0;
        let heavy = world
            .spawn((
                heavy,
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let mut light = test_monster(Faction::Player);
        light.mass = 0.3;
        let light = world
            .spawn((
                light,
                Transform::from_xyz(0.6, 1.0, 0.0), // 重叠（0.6 < 1.0）
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, separate_monsters).chain());
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

    fn test_monster(faction: Faction) -> crate::components::Monster {
        crate::components::Monster {
            faction,
            card: 0,
            radius: 0.5,
            mass: 1.0,
        }
    }
}
