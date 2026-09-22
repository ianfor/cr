//! 移动：朝目标移动 + 桥道转向（飞行单位直线）+ 冲锋蓄力 + 狂暴加速。
//! 已在攻击范围内的单位不动（attacking 系统负责输出）。

use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

use super::{edge_dist, WorldSnaps};

pub fn moving(
    snaps: Res<WorldSnaps>,
    mut movers: Query<(
        &Unit,
        &Mover,
        &mut Transform,
        &Attacker,
        Option<&mut Charge>,
        Option<&Buffs>,
        Option<&Flying>,
    )>,
) {
    for (u, mover, mut transform, attacker, mut charge, buffs, flying) in &mut movers {
        // 禁移动（眩晕/缠绕）：实时查询 buff 标志位，无派生缓存；
        // 被控期间冲锋蓄力清零
        if buffs.map(|b| b.channels().cannot_move).unwrap_or(false) {
            if let Some(c) = charge.as_deref_mut() {
                c.progress = 0.0;
            }
            continue;
        }
        let Some(target_entity) = attacker.target else {
            continue;
        };
        // 点查表 O(1)（替代旧的 O(n) 线性 find）
        let Some(&i) = snaps.index.get(&target_entity) else {
            continue;
        };
        let target = &snaps.snaps[i as usize];
        let pos = transform.translation;
        let edge = edge_dist(pos, u.radius, target.pos, target.radius);
        if edge <= attacker.attack_range + 0.05 {
            continue; // 射程内：attacking 负责，原地输出
        }
        // 攻击停止距离（中心距）= 攻击边缘距离 + 双方半径
        let stop_dist = attacker.attack_range + u.radius + target.radius;
        let goal = steering_goal(pos, target.pos, flying.is_some());
        let mut to_goal = goal - pos;
        to_goal.y = 0.0;
        let dist = to_goal.length();
        if dist <= 1e-4 {
            continue;
        }
        // 移速 = 属性修饰器合成（狂暴等数值 buff 都从这里进来）
        let mut speed = buffs
            .map(|b| b.stat(mover.speed, StatKind::MoveSpeed))
            .unwrap_or(mover.speed);
        // 冲锋蓄力：移动中累积，蓄满移速×（作用在 buff 合成之后）
        if let Some(c) = charge.as_deref_mut() {
            c.progress += TICK_DT;
            if c.charged() {
                speed *= c.speed_mult;
            }
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

/// 路点转向：需要过河时，先走向最近的桥口，进了桥道再直线过河；
/// 飞行单位无视河道，直线飞向目标
pub(crate) fn steering_goal(pos: Vec3, target: Vec3, flying: bool) -> Vec3 {
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

#[cfg(test)]
mod tests {
    use super::super::{seek, targeting, test_attacker, test_monster};
    use super::*;

    fn setup() -> App {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<super::super::ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        app
    }

    /// 冲锋（王子）：持续移动蓄力，蓄满后移速倍增
    #[test]
    fn charge_accelerates_after_windup() {
        let mut app = setup();
        let world = app.world_mut();
        world.spawn((
            Unit::tower(Faction::Enemy, 1.0),
            test_attacker(),
            Targeting(TargetPolicy::Guard),
            Health::new(60000.0),
            Transform::from_xyz(0.0, 0.0, 12.5),
        ));
        let e = world
            .spawn((
                test_monster(Faction::Player),
                test_attacker(),
                seek(5.0),
                Mover { speed: 1.5 },
                Charge {
                    progress: 0.0,
                    windup: 0.5,
                    speed_mult: 3.0,
                    damage_mult: 2.0,
                },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, moving).chain());
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

    /// 飞行单位：直线飞向目标（不过桥、不绕路）
    #[test]
    fn flying_units_go_straight() {
        let mut app = setup();
        let world = app.world_mut();
        let target = world
            .spawn((
                Unit::tower(Faction::Enemy, 1.0),
                test_attacker(),
                Targeting(TargetPolicy::Guard),
                Health::new(60000.0),
                // 与怪隔河且不在桥道：地面单位要先绕桥，空军应直线
                Transform::from_xyz(0.0, 0.0, 8.5),
            ))
            .id();
        let e = world
            .spawn((
                test_monster(Faction::Player),
                test_attacker(),
                seek(5.0),
                Mover { speed: 1.0 },
                Flying,
                Health::new(320.0),
                Transform::from_xyz(0.0, 2.6, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, moving).chain());
        for _ in 0..60 {
            schedule.run(world);
        }
        // 直线接近：与目标的 x 恒为 0（若绕桥会先横移到 ±4.5）
        let x = world.get::<Transform>(e).unwrap().translation.x;
        let moved_z = world.get::<Transform>(e).unwrap().translation.z + 5.0;
        assert!(
            x.abs() < 0.05 && moved_z > 1.0,
            "飞行单位必须直线飞向目标：x = {x:.2}"
        );
        let _ = target;
    }
}
