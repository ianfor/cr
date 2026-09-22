//! 统一开火：怪物/塔/建筑卡共用一套攻击逻辑。
//!
//! - 近战：直接扣血 + 以自身为中心的溅射（瓦基丽 360°）
//! - 远程：发射追踪子弹（溅射参数随弹携带，命中点溅射见 projectile 模块）
//! - 冲锋首击：伤害×蓄力倍率，命中后蓄力清零
//! - 攻击冷却只在目标进入射程后流逝（行军途中不回复）；
//!   狂暴加速攻击节奏（冷却步长 ÷mult）
//! - 进攻击范围 → 置 engaged（交战）：锁定从此不被抢走，直到被打断

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

use super::{edge_dist, ProjectileAssets, UnitSnap, WorldSnaps};
use super::projectile::projectile_assets;

pub fn attacking(
    mut commands: Commands,
    snaps: Res<WorldSnaps>,
    mut units: Query<(
        &mut Attacker,
        &Transform,
        &Unit,
        Option<&mut Charge>,
        Option<&Buffs>,
    )>,
    mut healths: Query<&mut Health>,
    mut proj_assets: ResMut<ProjectileAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (mut attacker, transform, unit, mut charge, buffs) in &mut units {
        // 禁攻击（眩晕/缴械）：实时查询 buff 标志位，无派生缓存
        if buffs.map(|b| b.channels().cannot_attack).unwrap_or(false) {
            continue;
        }
        let Some(target_entity) = attacker.target else {
            continue;
        };
        // 点查表 O(1)（替代旧的 O(n) 线性 find）
        let Some(&i) = snaps.index.get(&target_entity) else {
            continue; // 目标不在快照中（本帧已被清除）
        };
        let target = &snaps.snaps[i as usize];
        let pos = transform.translation;
        let faction = unit.faction;
        let self_radius = unit.radius;

        let edge = edge_dist(pos, self_radius, target.pos, target.radius);
        if edge > attacker.attack_range + 0.05 {
            continue; // 不在射程：交给移动系统接近
        }
        attacker.engaged = true;

        // 攻速 = 属性修饰器合成（狂暴等数值 buff 都从这里进来）
        let rate = buffs
            .map(|b| b.stat(1.0, StatKind::AttackSpeed))
            .unwrap_or(1.0);
        let dt = TICK_DT / rate;
        attacker.cooldown -= dt;
        if attacker.cooldown > 0.0 {
            continue;
        }
        attacker.cooldown = attacker.interval;

        // 冲锋首击：伤害×蓄力倍率，命中后蓄力清零
        let mut damage = attacker.damage;
        if let Some(c) = charge.as_deref_mut() {
            if c.charged() {
                damage *= c.damage_mult;
                c.progress = 0.0;
            }
        }

        if attacker.ranged {
            // 远程：发射追踪子弹（溅射参数随弹携带）
            let (mesh, mat) =
                projectile_assets(&mut proj_assets, &mut meshes, &mut materials, faction);
            // 弹道起点高度按实体类别：怪 1.5 / 塔 3.5 / 建筑 1.2
            let muzzle_y = match unit.kind {
                UnitKind::Troop => 1.5,
                UnitKind::Tower => 3.5,
                UnitKind::Building => 1.2,
            };
            commands.spawn((
                Projectile {
                    target: target.entity,
                    damage,
                    splash_radius: attacker.splash_radius,
                    hits_air: attacker.hits_air,
                    attacker: faction,
                },
                Mesh3d(mesh),
                MeshMaterial3d(mat),
                Transform::from_translation(pos + Vec3::Y * muzzle_y),
                NotShadowCaster,
            ));
        } else if let Ok(mut health) = healths.get_mut(target.entity) {
            // 近战：直接扣血
            health.current -= damage;
        }

        // 近战溅射：以自身为中心的范围伤害（瓦基丽 360°）
        // 怪走网格圆域；塔/建筑走线性（旧版溅射同时波及建筑，必须保留）
        if !attacker.ranged && attacker.splash_radius > 0.0 {
            let mut splash = |s: &UnitSnap| {
                if s.faction == faction || s.entity == target.entity {
                    return;
                }
                if s.flying && !attacker.hits_air {
                    return; // 对地溅射打不到空军
                }
                let mut d = s.pos - pos;
                d.y = 0.0;
                if d.length() <= attacker.splash_radius + s.radius {
                    if let Ok(mut health) = healths.get_mut(s.entity) {
                        health.current -= damage;
                    }
                }
            };
            snaps.grid.for_each_in_circle(
                pos,
                attacker.splash_radius + MONSTER_RADIUS_MAX,
                &mut |i| splash(&snaps.snaps[i as usize]),
            );
            for s in snaps.snaps.iter().filter(|s| s.is_building_kind()) {
                splash(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{moving, seek, targeting, test_attacker, test_monster};
    use super::*;

    /// 近战溅射（瓦基丽）：攻击目标时波及身边的第二个敌人
    #[test]
    fn melee_splash_hits_nearby_enemy() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();

        let a = world
            .spawn((
                test_monster(Faction::Enemy),
                test_attacker(),
                seek(5.0),
                Mover { speed: 1.5 },
                Health::new(2000.0),
                Transform::from_xyz(0.9, 1.0, 0.0), // 贴脸（主目标）
            ))
            .id();
        let b = world
            .spawn((
                test_monster(Faction::Enemy),
                test_attacker(),
                seek(5.0),
                Mover { speed: 1.5 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 1.0), // 溅射半径内
            ))
            .id();
        let mut valk = test_attacker();
        valk.splash_radius = 1.5;
        world.spawn((
            test_monster(Faction::Player),
            valk,
            seek(5.0),
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, 0.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        for _ in 0..35 {
            schedule.run(world); // 35 tick > 1.0s 攻击间隔
        }
        assert!(world.get::<Health>(a).unwrap().current < 2000.0, "主目标掉血");
        assert!(
            world.get::<Health>(b).unwrap().current < 2000.0,
            "溅射半径内的第二个敌人也必须掉血"
        );
    }

    /// 两只敌对骑士：索敌→接近（简化为互贴）→互相扣血的端到端冒烟
    #[test]
    fn two_knights_fight() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();

        let a = world
            .spawn((
                test_monster(Faction::Player),
                test_attacker(),
                seek(5.0),
                Mover { speed: 3.0 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, -2.0),
            ))
            .id();
        let b = world
            .spawn((
                test_monster(Faction::Enemy),
                test_attacker(),
                seek(5.0),
                Mover { speed: 3.0 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 2.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking, moving).chain());
        // 第 1 帧：立即互相锁定
        schedule.run(world);
        assert_eq!(world.get::<Attacker>(a).unwrap().target, Some(b));
        assert_eq!(world.get::<Attacker>(b).unwrap().target, Some(a));
        // 跑 120 帧：接近到攻击距离并互相扣血
        for _ in 0..120 {
            schedule.run(world);
        }
        let pa = world.get::<Transform>(a).unwrap().translation;
        let pb = world.get::<Transform>(b).unwrap().translation;
        assert!((pa - pb).length() < 2.0, "两只怪没有接近");
        let ha = world.get::<Health>(a).unwrap().current;
        let hb = world.get::<Health>(b).unwrap().current;
        assert!(ha < 2000.0 && hb < 2000.0, "两只怪没有互相伤害");
    }
}
