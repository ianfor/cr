//! 建筑卡通用能力：寿命倒计时自毁（Lifetime）、定时出兵（Spawner）。
//! 建筑的攻击走统一 targeting/attacking（TargetPolicy::Guard + Attacker）

use bevy::prelude::*;

use crate::cards;
use crate::components::*;
use crate::constants::*;

/// 建筑寿命：归零自毁（不返圣水）
pub fn building_lifetime(mut commands: Commands, mut buildings: Query<(Entity, &mut Lifetime)>) {
    for (e, mut l) in &mut buildings {
        l.secs -= TICK_DT;
        if l.secs <= 0.0 {
            commands.entity(e).despawn();
        }
    }
}

/// 出兵建筑（墓碑）：倒计时出一只 card_id 对应的小兵（在建筑位置直接落地，
/// 不走虚影——出兵是建筑行为而非玩家指令）
pub fn building_spawner(
    mut commands: Commands,
    mut spawners: Query<(&BuildingCard, &mut Spawner, &Transform)>,
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
        attacking, moving, seek, targeting, test_attacker, test_monster, WorldSnaps,
    };
    use super::*;

    /// 墓碑定时出兵、加农炮索敌开火（统一索敌/开火系统的建筑侧验证）
    #[test]
    fn building_spawns_and_fires() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<super::super::ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();

        // 墓碑：4s 一只骷髅（card 1）
        world.spawn((
            BuildingCard {
                faction: Faction::Player,
                card: 20,
                radius: 0.6,
            },
            Lifetime { secs: 100.0 },
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
            BuildingCard {
                faction: Faction::Enemy,
                card: 19,
                radius: 0.6,
            },
            Lifetime { secs: 100.0 },
            Attacker {
                damage: 90.0,
                attack_range: 5.0,
                interval: 0.9,
                cooldown: 0.0,
                splash_radius: 0.0,
                hits_air: false,
                ranged: true,
                target: None,
                engaged: false,
            },
            Targeting(TargetPolicy::Guard),
            Health::new(1400.0),
            Transform::from_xyz(4.0, 0.7, 5.0),
        ));
        // 蓝方怪走进红方加农炮射程（距离 < 5）
        world.spawn((
            test_monster(Faction::Player),
            test_attacker(),
            seek(5.0),
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(4.0, 1.0, 1.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((
            targeting,
            attacking,
            moving,
            building_lifetime,
            building_spawner,
        )
            .chain());
        for _ in 0..125 {
            schedule.run(world); // 4.17s
        }
        // 墓碑出了 1 只骷髅（4s 时），第 2 只要 8s
        let mut monsters = world.query::<&Monster>();
        let skeletons = monsters.iter(world).filter(|m| m.card == 1).count();
        assert_eq!(skeletons, 1, "墓碑 4s 应出 1 只骷髅");
        // 加农炮已开火：场上存在追踪子弹
        let mut projectiles = world.query::<&Projectile>();
        let fired = projectiles
            .iter(world)
            .filter(|p| p.attacker == Faction::Enemy)
            .count();
        assert!(fired > 0, "加农炮必须对射程内敌人开火");
    }

    /// 建筑寿命：归零自毁
    #[test]
    fn building_expires_after_lifetime() {
        let mut app = App::new();
        let world = app.world_mut();
        world.spawn((
            BuildingCard {
                faction: Faction::Player,
                card: 19,
                radius: 0.6,
            },
            Lifetime { secs: 1.0 },
            Health::new(1400.0),
            Transform::from_xyz(0.0, 0.7, -5.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(building_lifetime);
        for _ in 0..35 {
            schedule.run(world); // 1.17s > 1.0s 寿命
        }
        let mut buildings = world.query::<&BuildingCard>();
        assert_eq!(buildings.iter(world).count(), 0, "寿命到必须自毁");
    }
}
