//! 统一开火：怪物/塔/建筑卡共用一套攻击逻辑。
//!
//! - 近战（Melee）：当场以自身位置为中心 detonate（直击 + 溅射一次结算）
//! - 远程（Homing）：发射在途 Strike（payload 随弹携带，命中结算见 strike 模块）
//! - 冲锋首击：伤害×蓄力倍率，命中后蓄力清零
//! - 攻击冷却只在目标进入射程后流逝（行军途中不回复）；
//!   狂暴加速攻击节奏（冷却步长 ÷mult）
//! - 进攻击范围 → 置 engaged（交战）：锁定从此不被抢走，直到被打断

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

use super::strike::{detonate, projectile_assets};
use super::{edge_dist, ProjectileAssets, WorldSnaps};

pub fn attacking(
    mut commands: Commands,
    snaps: Res<WorldSnaps>,
    mut units: Query<(
        &mut Skill,
        &Transform,
        &Unit,
        Option<&mut Charge>,
        Option<&Buffs>,
    )>,
    mut targets: Query<
        (
            Entity,
            &Unit,
            &Transform,
            Option<&Flying>,
            &mut Health,
        ),
        Without<Strike>,
    >,
    mut proj_assets: ResMut<ProjectileAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (mut skill, transform, unit, mut charge, buffs) in &mut units {
        // 禁攻击（眩晕/缴械）：实时查询 buff 标志位，无派生缓存
        if buffs.map(|b| b.channels().cannot_attack).unwrap_or(false) {
            continue;
        }
        let Some(target_entity) = skill.target else {
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
        if edge > skill.range + 0.05 {
            continue; // 不在射程：交给移动系统接近
        }
        skill.engaged = true;

        // 攻速 = 属性修饰器合成（狂暴等数值 buff 都从这里进来）
        let rate = buffs
            .map(|b| b.stat(1.0, StatKind::AttackSpeed))
            .unwrap_or(1.0);
        let dt = TICK_DT / rate;
        skill.cooldown -= dt;
        if skill.cooldown > 0.0 {
            continue;
        }
        skill.cooldown = skill.interval;

        // 冲锋首击：负载伤害×蓄力倍率，命中后蓄力清零
        // （直击与溅射共用同一份修改后的负载）
        let mut payload = skill.payload.clone();
        if let Some(c) = charge.as_deref_mut() {
            if c.charged() {
                payload.damage *= c.damage_mult;
                c.progress = 0.0;
            }
        }

        match skill.delivery {
            // 近战：当场以自身位置为中心结算（直击 primary + 360° 溅射）
            Delivery::Melee => {
                detonate(
                    &mut commands,
                    &mut targets,
                    &payload,
                    faction,
                    pos,
                    Some(target.entity),
                );
            }
            // 远程：发射在途追踪弹（结算延迟到贴身，见 strike 模块）
            Delivery::Homing => {
                let (mesh, mat) =
                    projectile_assets(&mut proj_assets, &mut meshes, &mut materials, faction);
                // 弹道起点高度按实体类别：怪 1.5 / 塔（含王塔）3.5 / 建筑 1.2
                let muzzle_y = match unit.kind {
                    UnitKind::Troop => 1.5,
                    UnitKind::Tower | UnitKind::KingTower => 3.5,
                    UnitKind::Building => 1.2,
                };
                commands.spawn((
                    Strike {
                        attacker: faction,
                        payload,
                        flight: Flight::Homing {
                            target: target.entity,
                        },
                    },
                    Mesh3d(mesh),
                    MeshMaterial3d(mat),
                    Transform::from_translation(pos + Vec3::Y * muzzle_y),
                    NotShadowCaster,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::strike_tick;
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
        valk.payload.splash_radius = 1.5;
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
        assert_eq!(world.get::<Skill>(a).unwrap().target, Some(b));
        assert_eq!(world.get::<Skill>(b).unwrap().target, Some(a));
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

    /// 近战溅射波及塔（瓦基丽溅塔）：hits_towers=true 的显式化回归
    #[test]
    fn melee_splash_hits_tower() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 敌方塔紧挨着敌怪（近战攻击怪时溅射半径覆盖塔）
        world.spawn((
            Unit::tower(Faction::Enemy, 1.0),
            Targeting(TargetPolicy::Guard),
            Health::new(6000.0),
            Transform::from_xyz(0.0, 0.0, 1.0),
        ));
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(20000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let mut valk = test_attacker();
        valk.payload.splash_radius = 1.5;
        // 近战负载 hits_towers = true（spawn_unit 对非远程卡的取值）
        valk.payload.hits_towers = true;
        world.spawn((
            test_monster(Faction::Player),
            valk,
            seek(5.0),
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, 0.5), // 贴脸敌怪
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        for _ in 0..35 {
            schedule.run(world);
        }
        assert!(
            world.get::<Health>(victim).unwrap().current < 20000.0,
            "主目标掉血"
        );
        let mut towers = world.query_filtered::<&Health, (
            With<Unit>,
            Without<Mover>,
        )>();
        let tower_hp: Vec<f32> = towers
            .iter(world)
            .map(|h| h.current)
            .filter(|&hp| hp < 6000.0)
            .collect();
        assert!(
            !tower_hp.is_empty(),
            "近战溅射必须波及塔（瓦基丽溅塔，hits_towers=true）"
        );
    }

    /// 弹溅不吃塔（hits_towers=false）：远程溅射单位攻击贴塔敌怪，塔不掉血
    #[test]
    fn projectile_splash_skips_tower() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 敌方塔紧挨着敌怪（弹着点溅射半径覆盖塔，但塔必须免疫）
        world.spawn((
            Unit::tower(Faction::Enemy, 1.0),
            Targeting(TargetPolicy::Guard),
            Health::new(6000.0),
            Transform::from_xyz(0.0, 0.0, 1.0),
        ));
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(20000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let mut archer = test_attacker();
        archer.range = 4.0;
        archer.delivery = Delivery::Homing;
        archer.payload.splash_radius = 1.5;
        archer.payload.hits_towers = false; // 远程弹溅不吃塔（spawn_unit 对远程卡的取值）
        world.spawn((
            test_monster(Faction::Player),
            archer,
            seek(5.0),
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, -2.0), // 射程内
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking, strike_tick).chain());
        for _ in 0..120 {
            schedule.run(world); // 追踪弹有飞行时间，跑足帧数确保命中
        }
        assert!(
            world.get::<Health>(victim).unwrap().current < 20000.0,
            "主目标必须被弹命中掉血"
        );
        let mut towers = world.query_filtered::<&Health, (
            With<Unit>,
            Without<Mover>,
        )>();
        let tower_damaged = towers.iter(world).any(|h| h.current < 6000.0);
        assert!(
            !tower_damaged,
            "弹溅不得波及塔（hits_towers=false，显式化旧规则）"
        );
    }
}
