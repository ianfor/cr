//! 状态效果：Buffs（属性修饰 + 控制标志）的统一生命周期。
//!
//! 数据源唯一：Buffs 容器（时长/叠加/标志位都在里面）。
//! 本系统每帧做三件事：
//! 1. 倒计时、过期移除、清空删容器
//! 2. 同步派生标记：任一活跃 buff 带 STUN 位 → 挂 Stun 标记组件
//!    （让索敌/攻击/移动的 Without<Stun> 过滤保持 archetype 级快查）
//! 3. 晕眩期间清冲锋蓄力

use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

pub fn status_effects(
    mut commands: Commands,
    mut buffed: Query<(Entity, &mut Buffs, Option<&mut Charge>)>,
) {
    for (e, mut buffs, mut charge) in &mut buffed {
        // 1) 倒计时 + 过期
        for b in buffs.0.iter_mut() {
            b.secs -= TICK_DT;
        }
        buffs.0.retain(|b| b.secs > 0.0);

        // 2) 晕眩：清冲锋蓄力 + 同步派生标记
        let stunned = buffs.has_cc(CCFlags::STUN);
        if stunned {
            if let Some(c) = charge.as_mut() {
                c.progress = 0.0;
            }
            commands.entity(e).insert(Stun);
        } else {
            commands.entity(e).remove::<Stun>();
        }

        // 3) 没有 buff 就删容器：消费方的 Option<&Buffs> 回到 None
        if buffs.0.is_empty() {
            commands.entity(e).remove::<Buffs>();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{moving, seek, targeting, test_attacker, test_monster, WorldSnaps};
    use super::*;

    fn stun_buff(secs: f32) -> ActiveBuff {
        ActiveBuff {
            name: "Stun",
            secs,
            stacks: 1,
            policy: StackPolicy::Longer,
            flags: CCFlags::STUN,
            effects: vec![],
        }
    }

    /// 晕眩：完全无法行动（位置不动），晕完恢复移动；
    /// Stun 标记与 Buffs 数据同步（出现/消失）
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
                Buffs(vec![stun_buff(1.0)]),
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((status_effects, targeting, moving).chain());
        schedule.run(world);
        // 晕眩期间：标记已同步、位置不动
        assert!(world.get::<Stun>(e).is_some(), "晕眩 buff 应同步出 Stun 标记");
        for _ in 0..28 {
            schedule.run(world); // 29 tick < 1.0s 晕眩
        }
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation,
            Vec3::new(0.0, 1.0, -5.0),
            "晕眩期间不得移动"
        );
        for _ in 0..10 {
            schedule.run(world); // 晕眩结束
        }
        assert!(
            world.get::<Transform>(e).unwrap().translation.z > -5.0,
            "晕眩结束后必须恢复移动（走向塔）"
        );
        assert!(
            world.get::<Stun>(e).is_none(),
            "晕眩到期后 Stun 标记应随 Buffs 移除"
        );
        assert!(
            world.get::<Buffs>(e).is_none(),
            "晕眩到期后 Buffs 容器应被移除"
        );
    }

    /// 晕眩叠加策略 Longer：重复施加取更久，短的不缩短长晕
    #[test]
    fn stun_reapply_takes_longer() {
        let mut buffs = Buffs(vec![stun_buff(2.0)]);
        buffs.apply(stun_buff(0.5)); // 更短：不生效
        assert_eq!(buffs.0[0].secs, 2.0);
        buffs.apply(stun_buff(3.0)); // 更长：取 3.0
        assert_eq!(buffs.0[0].secs, 3.0);
        assert_eq!(buffs.0.len(), 1, "同名晕眩不叠条目");
    }

    /// 属性修饰器管线：合成规则（Add 先加、Mul 后乘、Stack 按层数幂）
    #[test]
    fn buff_stat_composition() {
        let mut buffs = Buffs::default();
        // 狂暴：移速 ×1.35
        buffs.apply(ActiveBuff {
            name: "Rage",
            secs: 6.0,
            stacks: 1,
            policy: StackPolicy::Refresh,
            flags: CCFlags::NONE,
            effects: vec![StatMod {
                stat: StatKind::MoveSpeed,
                op: Op::Mul,
                value: 1.35,
            }],
        });
        // 疾跑：移速 +2（固定值）
        buffs.apply(ActiveBuff {
            name: "Sprint",
            secs: 3.0,
            stacks: 1,
            policy: StackPolicy::Refresh,
            flags: CCFlags::NONE,
            effects: vec![StatMod {
                stat: StatKind::MoveSpeed,
                op: Op::Add,
                value: 2.0,
            }],
        });
        // (1.5 + 2.0) × 1.35 = 4.725
        assert!((buffs.stat(1.5, StatKind::MoveSpeed) - 4.725).abs() < 1e-5);
        // 没碰的属性域不受影响
        assert_eq!(buffs.stat(1.0, StatKind::AttackSpeed), 1.0);
        // 空容器 = 基础值
        assert_eq!(Buffs::default().stat(2.0, StatKind::MoveSpeed), 2.0);
    }

    /// 叠加策略：Refresh 刷新不叠层，Stack 叠层到上限，层数进合成
    #[test]
    fn buff_stack_policies() {
        let mut buffs = Buffs::default();
        let mk = |policy| ActiveBuff {
            name: "Haste",
            secs: 2.0,
            stacks: 1,
            policy,
            flags: CCFlags::NONE,
            effects: vec![StatMod {
                stat: StatKind::AttackSpeed,
                op: Op::Mul,
                value: 1.2,
            }],
        };
        // Refresh：施三次仍是一层
        buffs.apply(mk(StackPolicy::Refresh));
        buffs.apply(mk(StackPolicy::Refresh));
        assert_eq!(buffs.0.len(), 1);
        assert_eq!(buffs.0[0].stacks, 1);
        assert!((buffs.stat(1.0, StatKind::AttackSpeed) - 1.2).abs() < 1e-6);

        // Stack(3)：叠三层 = 1.2³
        let mut stacked = Buffs::default();
        for _ in 0..5 {
            stacked.apply(mk(StackPolicy::Stack(3)));
        }
        assert_eq!(stacked.0[0].stacks, 3);
        assert!((stacked.stat(1.0, StatKind::AttackSpeed) - 1.2f32.powi(3)).abs() < 1e-6);

        // 独立共存：两个同名独立 buff 叠乘 1.2 × 1.2
        let mut indep = Buffs::default();
        indep.apply(mk(StackPolicy::Independent));
        indep.apply(mk(StackPolicy::Independent));
        assert_eq!(indep.0.len(), 2);
        assert!((indep.stat(1.0, StatKind::AttackSpeed) - 1.44).abs() < 1e-6);
    }

    /// 端到端：狂暴加速移动，到期后回基础移速、容器组件被移除
    #[test]
    fn rage_buff_speeds_then_expires() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<super::super::ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 敌方塔做行军目标
        world.spawn((
            Tower {
                faction: Faction::Enemy,
                radius: 1.0,
            },
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
                Mover { speed: 1.0 },
                Buffs(vec![ActiveBuff {
                    name: "Rage",
                    secs: 0.5,
                    stacks: 1,
                    policy: StackPolicy::Refresh,
                    flags: CCFlags::NONE,
                    effects: vec![
                        StatMod {
                            stat: StatKind::MoveSpeed,
                            op: Op::Mul,
                            value: 2.0,
                        },
                        StatMod {
                            stat: StatKind::AttackSpeed,
                            op: Op::Mul,
                            value: 2.0,
                        },
                    ],
                }]),
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((status_effects, targeting, moving).chain());
        // 15 tick = 0.5s：狂暴期间移速 2.0
        let mut boosted = None;
        for _ in 0..14 {
            schedule.run(world);
            let z = world.get::<Transform>(e).unwrap().translation.z;
            boosted = Some(z);
        }
        // 直线走桥方向 z 分量：狂暴 14 tick×2.0 速 ≈ z=-4.45，
        // 基础速 14 tick×1.0 ≈ z=-4.73——取 -4.55 区分两者
        assert!(
            boosted.unwrap() > -4.55,
            "狂暴期间移速应翻倍（z = {:.2}）",
            boosted.unwrap()
        );
        // 再跑到狂暴到期（0.5s + 余量）
        for _ in 0..10 {
            schedule.run(world);
        }
        assert!(
            world.get::<Buffs>(e).is_none(),
            "狂暴到期后 Buffs 容器应被移除"
        );
        // 到期后按基础移速走：接下来 10 tick（1/3s）z 位移 ≤ 1.0×(1/3)+0.05
        let z0 = world.get::<Transform>(e).unwrap().translation.z;
        for _ in 0..10 {
            schedule.run(world);
        }
        let delta = world.get::<Transform>(e).unwrap().translation.z - z0;
        assert!(
            delta < 0.45,
            "到期后应回到基础移速（0.33s 移动了 {delta:.2}）"
        );
    }
}
