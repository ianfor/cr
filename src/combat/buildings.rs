//! 建筑卡通用能力：定时出兵（Spawner）。建筑的攻击走统一
//! targeting/attacking（TargetPolicy::Guard + Attacker）；
//! 寿命 = Decay 扣血 buff（components/cards），死亡走 despawn_dead 通用路径

use bevy::prelude::*;

use crate::cards;
use crate::components::*;
use crate::constants::*;

/// 出兵建筑（墓碑）：倒计时出一只 card_id 对应的小兵（在建筑位置直接落地，
/// 不走虚影——出兵是建筑行为而非玩家指令）
pub fn building_spawner(
    mut commands: Commands,
    mut spawners: Query<(&Unit, &mut Spawner, &Transform)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (building, mut spawner, transform) in &mut spawners {
        spawner.cooldown -= TICK_DT;
        if spawner.cooldown > 0.0 {
            continue;
        }
        spawner.cooldown = spawner.interval;
        let pos = transform.translation;
        if let Some(spec) = CARDS.iter().find(|c| c.id == spawner.card_id) {
            if let CardKind::Troop(ms) = &spec.kind {
                cards::spawn_unit(
                    &mut commands,
                    &mut meshes,
                    &mut materials,
                    building.faction,
                    spawner.card_id,
                    ms,
                    Vec3::new(pos.x, 0.0, pos.z),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        attacking, despawn_dead, moving, status_effects, targeting, test_monster, test_skill,
        WorldSnaps,
    };
    use crate::cards::decay_buff;
    use crate::match_flow::MatchTimer;
    use super::*;

    /// 测试用 Decay：寿命长到测试期内不死（只验证出兵/开火，不验证寿命）
    fn long_decay() -> Buffs {
        Buffs::new(decay_buff(800.0, 100.0))
    }

    /// 墓碑定时出兵、加农炮索敌开火（统一索敌/开火系统的建筑侧验证）
    #[test]
    fn building_spawns_and_fires() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<super::super::ProjectileAssets>()
            .init_resource::<super::super::ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>()
            .init_resource::<MatchTimer>();
        let world = app.world_mut();

        // 墓碑：4s 一只骷髅（card 1）
        world.spawn((
            Unit::building(Faction::Player, 20, 0.6),
            long_decay(),
            Spawner {
                interval: 4.0,
                card_id: 1,
                cooldown: 4.0,
            },
            Health::new(800.0),
            Transform::from_xyz(-4.0, 0.7, -5.0),
        ));
        // 加农炮：0.9s 一发，打不到空军
        world.spawn((
            Unit::building(Faction::Enemy, 19, 0.6),
            long_decay(),
            TargetSelector {
                policy: TargetPolicy::Guard,
                range: 5.0,
                hits_air: false,
                target: None,
                engaged: false,
            },
            AttackFlow {
                interval: 0.9,
                windup_secs: 0.35,
                state: SkillState::Idle { left: 0 },
            },
            Skill {
                payload: Payload::damage_only(90.0, 0.0, false, false),
                delivery: Delivery::Homing,
            },
            Health::new(1400.0),
            Transform::from_xyz(4.0, 0.7, 5.0),
        ));
        // 蓝方怪走进红方加农炮射程（距离 < 5）
        world.spawn((
            test_monster(Faction::Player),
            test_skill(),
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(4.0, 1.0, 1.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((
            status_effects,
            targeting,
            attacking,
            moving,
            building_spawner,
            despawn_dead,
        )
            .chain());
        for _ in 0..125 {
            schedule.run(world); // 4.17s
        }
        // 墓碑出了 1 只骷髅（4s 时），第 2 只要 8s
        let mut monsters = world.query::<&Unit>();
        let skeletons = monsters
            .iter(world)
            .filter(|u| u.kind == UnitKind::Troop && u.card == Some(1))
            .count();
        assert_eq!(skeletons, 1, "墓碑 4s 应出 1 只骷髅");
        // 加农炮已开火：场上存在在途打击（追踪弹）
        let mut projectiles = world.query::<&Strike>();
        let fired = projectiles
            .iter(world)
            .filter(|p| p.attacker == Faction::Enemy)
            .count();
        assert!(fired > 0, "加农炮必须对射程内敌人开火");
    }

    /// 建筑寿命（Decay buff）：总掉血 = hp，寿命尽恰好归零，
    /// 走 despawn_dead 通用死亡路径
    #[test]
    fn building_expires_after_lifetime() {
        let mut app = App::new();
        app.init_resource::<MatchTimer>();
        let world = app.world_mut();
        world.spawn((
            Unit::building(Faction::Player, 19, 0.6),
            Buffs::new(decay_buff(1400.0, 1.0)),
            Health::new(1400.0),
            Transform::from_xyz(0.0, 0.7, -5.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((status_effects, despawn_dead).chain());
        for _ in 0..35 {
            schedule.run(world); // 1.17s > 1.0s 寿命
        }
        let mut buildings = world.query::<&Unit>();
        assert_eq!(
            buildings
                .iter(world)
                .filter(|u| u.kind == UnitKind::Building)
                .count(),
            0,
            "寿命到必须自毁"
        );
    }

    /// Decay 末跳回归：先 drain 后 tick——最后一跳必须落上。
    /// 反序（先 tick 清容器）末跳 drain 会随容器蒸发，建筑剩 1/30 血永生；
    /// 另一坑是 FP 欠扣（见 decay_buff 的 0.1% 放大兜底注释）
    #[test]
    fn decay_last_tick_drains_fully() {
        let mut app = App::new();
        app.init_resource::<MatchTimer>();
        let world = app.world_mut();
        let e = world
            .spawn((
                Unit::building(Faction::Player, 19, 0.6),
                Buffs::new(decay_buff(300.0, 1.0)),
                Health::new(300.0),
                Transform::from_xyz(0.0, 0.7, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((status_effects, despawn_dead).chain());
        for _ in 0..29 {
            schedule.run(world); // 29/30 tick：还差最后一跳
        }
        assert!(
            world.get::<Health>(e).is_some(),
            "29 tick（寿命 1s）建筑必须还在"
        );
        schedule.run(world); // 第 30 tick：末跳 drain 与容器过期同帧
        assert!(
            world.get::<Health>(e).is_none(),
            "30 tick 恰好归零并 despawn（末跳 drain 不得丢失）"
        );
    }
}
