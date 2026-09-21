//! 状态效果计时：Stun / Rage 组件的生命周期管理。
//!
//! - Stun：倒计时归零移除；持有期间单位无法索敌/攻击/移动
//!   （各系统以 Without<Stun> 过滤），冲锋蓄力被清零，目标锁定保留
//! - Rage：倒计时归零移除；持有期间攻速/移速 ×mult

use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

pub fn status_effects(
    mut commands: Commands,
    mut stuns: Query<(Entity, &mut Stun, Option<&mut Charge>)>,
    mut rages: Query<(Entity, &mut Rage)>,
) {
    for (e, mut s, charge) in &mut stuns {
        s.secs -= TICK_DT;
        // 晕眩打断冲锋蓄力
        if let Some(mut c) = charge {
            c.progress = 0.0;
        }
        if s.secs <= 0.0 {
            commands.entity(e).remove::<Stun>();
        }
    }
    for (e, mut r) in &mut rages {
        r.secs -= TICK_DT;
        if r.secs <= 0.0 {
            commands.entity(e).remove::<Rage>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{moving, seek, targeting, test_attacker, test_monster, WorldSnaps};
    use super::*;

    /// 晕眩：完全无法行动（位置不动），晕完恢复移动
    #[test]
    fn stun_freezes_monster() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<super::super::ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();

        world.spawn((
            Tower {
                faction: Faction::Enemy,
                radius: 1.0,
            },
            test_attacker(),
            Targeting(TargetPolicy::Guard),
            Health::new(6000.0),
            Transform::from_xyz(0.0, 0.0, 12.5),
        ));
        let e = world
            .spawn((
                test_monster(Faction::Player),
                test_attacker(),
                seek(5.0),
                Mover { speed: 1.5 },
                Stun { secs: 1.0 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((status_effects, targeting, moving).chain());
        for _ in 0..29 {
            schedule.run(world); // 29 tick < 1.0s 晕眩
        }
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation,
            Vec3::new(0.0, 1.0, -5.0),
            "晕眩期间不得移动"
        );
        for _ in 0..10 {
            schedule.run(world); // 晕眩结束（组件被移除）
        }
        assert!(
            world.get::<Transform>(e).unwrap().translation.z > -5.0,
            "晕眩结束后必须恢复移动（走向塔）"
        );
        assert!(
            world.get::<Stun>(e).is_none(),
            "晕眩到期后 Stun 组件应被移除"
        );
    }
}
