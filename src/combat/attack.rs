//! 执行流程控制（攻击三权分立之二：何时打）：
//! 推进 AttackFlow 三态——目标选择由 TargetSelector 负责、
//! 结算执行由 Skill（效果）+ strike/detonate 负责，本系统只管计时。
//!
//! - Idle：冷却倒数（目标在射程内才流逝，行军不回复），归零 → 进前摇
//! - Windup（前摇）：被晕/缴械 → 取消回 Idle{0}（白摇）；
//!   归零 → 出手帧结算（resolve_release）
//! - Recover（后摇）：归零 → Idle{剩余冷却}
//!
//! 周期守恒：Release → Recover(R) → Idle(cycle−W−R) → Windup(W) → Release，
//! release→release = cycle = interval/攻速（DPS 与旧模型一致）。
//! 出手帧不重新判距离（已 commit 必中，CR 亦然）；目标已死 → 落空进后摇。

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

use super::strike::{detonate, projectile_assets};
use super::{edge_dist, ProjectileAssets, ReleaseLog, UnitSnap, WorldSnaps};

/// 攻速合成（狂暴等数值 buff 从这里进来）
fn attack_rate(buffs: Option<&Buffs>) -> f32 {
    buffs
        .map(|b| b.stat(1.0, StatKind::AttackSpeed))
        .unwrap_or(1.0)
}

/// 攻击周期（tick，攻速 buff 同步缩短）
fn cycle_ticks(flow: &AttackFlow, rate: f32) -> u32 {
    ticks_per_secs(flow.interval / rate)
}

/// 前摇时长（tick）
fn windup_ticks(flow: &AttackFlow, rate: f32) -> u32 {
    ticks_per_secs(flow.windup_secs / rate)
}

/// 后摇时长（tick）：周期 × RECOVER_FRAC（保底 ≥1，极端短周期卡）
fn recover_ticks(cycle: u32) -> u32 {
    (((cycle as f32) * RECOVER_FRAC).round() as u32).max(1)
}

pub fn attacking(
    mut commands: Commands,
    snaps: Res<WorldSnaps>,
    tick: Res<Tick>,
    mut release_log: ResMut<ReleaseLog>,
    mut units: Query<(
        Entity,
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
            &mut Health,
        ),
        Without<Strike>,
    >,
    mut proj_assets: ResMut<ProjectileAssets>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (entity, mut skill, transform, unit, mut charge, buffs) in &mut units {
        // 禁攻击（眩晕/缴械）：实时查询 buff 标志位，无派生缓存。
        // 前摇中被控 = 白摇取消（CR 正统）；Idle/Recover 静置不流逝
        if buffs.map(|b| b.channels().cannot_attack).unwrap_or(false) {
            if let SkillState::Windup { .. } = skill.flow.state {
                skill.flow.state = SkillState::Idle { left: 0 };
            }
            continue;
        }
        let pos = transform.translation;
        // flow 段本系统独占推进；select 段只读；effect 段出手帧读
        let Skill { select, flow, effect } = &mut *skill;

        match flow.state {
            // ===== 待机：冷却倒数（目标在射程内才流逝），归零 → 进前摇 =====
            SkillState::Idle { left } => {
                // 在射程内才倒数：edge 判定用选择器的 range 与锁定目标
                let Some(target_entity) = select.target else {
                    continue;
                };
                let Some(&i) = snaps.index.get(&target_entity) else {
                    continue; // 目标不在快照（已死）：交给索敌清锁
                };
                let target = &snaps.snaps[i as usize];
                let edge = edge_dist(pos, unit.radius, target.pos, target.radius);
                if edge > select.range + 0.05 {
                    continue; // 不在射程：冷却不流逝，交给移动系统接近
                }
                if left > 0 {
                    flow.state = SkillState::Idle { left: left - 1 };
                    continue;
                }
                // 冷却就绪 → 摇前摇（时长按当前攻速量化，链内冻结）
                let rate = attack_rate(buffs.as_deref());
                flow.state = SkillState::Windup {
                    left: windup_ticks(flow, rate),
                };
            }
            // ===== 前摇：倒数归零 → 出手帧结算 =====
            SkillState::Windup { left } => {
                let left = left - 1;
                if left > 0 {
                    flow.state = SkillState::Windup { left };
                    continue;
                }
                // 出手帧：先转后摇（落空也挥完动作），再执行效果
                let rate = attack_rate(buffs.as_deref());
                let cycle = cycle_ticks(flow, rate);
                let r = recover_ticks(cycle);
                flow.state = SkillState::Recover { left: r };

                let Some(target_entity) = select.target else {
                    continue; // 落空：无目标可打（挥空）
                };
                let Some(&i) = snaps.index.get(&target_entity) else {
                    continue; // 落空：目标已死（whiff）
                };
                let target = &snaps.snaps[i as usize];
                // 注意：不重新判定距离——出手已 commit，被推挤出射程照样命中

                // 冲锋首击：负载伤害×蓄力倍率，命中后蓄力清零
                // （直击与溅射共用同一份修改后的负载）
                let mut payload = effect.payload.clone();
                if let Some(c) = charge.as_deref_mut() {
                    if c.charged() {
                        payload.damage *= c.damage_mult;
                        c.progress = 0.0;
                    }
                }

                resolve_release(
                    &mut commands,
                    &mut targets,
                    &mut proj_assets,
                    &mut meshes,
                    &mut materials,
                    unit,
                    pos,
                    &payload,
                    &effect.delivery,
                    target,
                );
                // 出手记录（表现层 attack_action_fx 消费）
                release_log.0.push((tick.0, entity, target.pos));
            }
            // ===== 后摇：可移动（走A），归零 → 回待机冷却 =====
            SkillState::Recover { left } => {
                let left = left - 1;
                if left > 0 {
                    flow.state = SkillState::Recover { left };
                } else {
                    // 周期守恒：release→release = cycle，
                    // 后摇已耗 R、前摇将耗 W，冷却余量 = cycle − W − R
                    let rate = attack_rate(buffs.as_deref());
                    let cycle = cycle_ticks(flow, rate);
                    let w = windup_ticks(flow, rate);
                    let r = recover_ticks(cycle);
                    flow.state = SkillState::Idle {
                        left: cycle.saturating_sub(w + r),
                    };
                }
            }
        }
    }
}

/// 结算执行（攻击三权分立之三的入口）：按投放方式把效果打出去——
/// 近战当场以自身位置为中心 detonate（直击 + 溅射一次结算）；
/// 远程发射在途追踪 Strike（结算延迟到贴身，见 strike 模块）
#[allow(clippy::too_many_arguments)]
fn resolve_release(
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
    proj_assets: &mut ProjectileAssets,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    unit: &Unit,
    pos: Vec3,
    payload: &Payload,
    delivery: &Delivery,
    target: &UnitSnap,
) {
    match delivery {
        // 近战：当场以自身位置为中心结算（直击 primary + 360° 溅射）
        Delivery::Melee => {
            detonate(
                commands,
                targets,
                payload,
                unit.faction,
                pos,
                Some(target.entity),
            );
        }
        // 远程：发射在途追踪弹（结算延迟到贴身，见 strike 模块）
        Delivery::Homing => {
            let (mesh, mat) = projectile_assets(proj_assets, meshes, materials, unit.faction);
            // 弹道起点高度按实体类别：怪 1.5 / 塔（含王塔）3.5 / 建筑 1.2
            let muzzle_y = match unit.kind {
                UnitKind::Troop => 1.5,
                UnitKind::Tower | UnitKind::KingTower => 3.5,
                UnitKind::Building => 1.2,
            };
            commands.spawn((
                Strike {
                    attacker: unit.faction,
                    payload: payload.clone(),
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

#[cfg(test)]
mod tests {
    use super::super::strike_tick;
    use super::super::{moving, status_effects, targeting, test_monster, test_skill, tower_skill};
    use super::*;

    /// 近战溅射（瓦基丽）：攻击目标时波及身边的第二个敌人
    #[test]
    fn melee_splash_hits_nearby_enemy() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();

        let a = world
            .spawn((
                test_monster(Faction::Enemy),
                test_skill(),
                Mover { speed: 1.5 },
                Health::new(2000.0),
                Transform::from_xyz(0.9, 1.0, 0.0), // 贴脸（主目标）
            ))
            .id();
        let b = world
            .spawn((
                test_monster(Faction::Enemy),
                test_skill(),
                Mover { speed: 1.5 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 1.0), // 溅射半径内
            ))
            .id();
        let mut skill = test_skill();
        skill.effect.payload.splash_radius = 1.5;
        world.spawn((
            test_monster(Faction::Player),
            skill,
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, 0.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        // 40 tick：21 冷却 + 9 前摇 + 裕量 > 1.0s 周期
        for _ in 0..40 {
            schedule.run(world);
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
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();

        let a = world
            .spawn((
                test_monster(Faction::Player),
                test_skill(),
                Mover { speed: 3.0 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, -2.0),
            ))
            .id();
        let b = world
            .spawn((
                test_monster(Faction::Enemy),
                test_skill(),
                Mover { speed: 3.0 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 2.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking, moving).chain());
        // 第 1 帧：立即互相锁定
        schedule.run(world);
        assert_eq!(
            world.get::<Skill>(a).unwrap().select.target,
            Some(b),
        );
        assert_eq!(
            world.get::<Skill>(b).unwrap().select.target,
            Some(a),
        );
        // 跑 130 帧：接近（含前摇）到攻击距离并互相扣血
        for _ in 0..130 {
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
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 敌方塔紧挨着敌怪（近战攻击怪时溅射半径覆盖塔）
        world.spawn((
            Unit::tower(Faction::Enemy, 1.0),
            tower_skill(),
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
        let mut skill = test_skill();
        skill.effect.payload.splash_radius = 1.5;
        // 近战负载 hits_towers = true（spawn_unit 对非远程卡的取值）
        skill.effect.payload.hits_towers = true;
        world.spawn((
            test_monster(Faction::Player),
            skill,
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, 0.5), // 贴脸敌怪
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        for _ in 0..40 {
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
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 敌方塔紧挨着敌怪（弹着点溅射半径覆盖塔，但塔必须免疫）
        world.spawn((
            Unit::tower(Faction::Enemy, 1.0),
            tower_skill(),
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
        let mut skill = test_skill();
        skill.select.range = 4.0;
        skill.effect.delivery = Delivery::Homing;
        skill.effect.payload.splash_radius = 1.5;
        // 远程弹溅不吃塔（spawn_unit 对远程卡的取值）
        skill.effect.payload.hits_towers = false;
        world.spawn((
            test_monster(Faction::Player),
            skill,
            Mover { speed: 1.5 },
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, -2.0), // 射程内
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking, strike_tick).chain());
        for _ in 0..130 {
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

    // ===== SkillState 状态机守卫测试 =====

    /// 骑士白板的周期参数（与 test_skill 的数值锚一致）
    const KNIGHT_CYCLE: u32 = 30; // 1.0s / TICK_DT
    const KNIGHT_WINDUP: u32 = 9; // 0.3s / TICK_DT
    const KNIGHT_RECOVER: u32 = 5; // round(30 × 0.15)

    /// 首击时序：贴脸放置后，命中发生在 (cycle−W) 冷却 + W 前摇 ≈ cycle 帧
    #[test]
    fn first_strike_waits_windup() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 静止靶子（无 Skill 不还手）：与攻击者贴脸
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(100000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        world.spawn((
            test_monster(Faction::Player),
            test_skill(),
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, 0.0), // 贴脸
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        let hp = |w: &mut World| w.get::<Health>(victim).unwrap().current;
        // 冷却段（cycle−W 帧）内不掉血
        for _ in 0..KNIGHT_CYCLE - KNIGHT_WINDUP {
            schedule.run(world);
        }
        assert_eq!(hp(world), 100000.0, "冷却段不得命中");
        // 前摇 W 帧：期间也不掉血（最后 1 帧留到命中窗口）
        for _ in 0..KNIGHT_WINDUP - 1 {
            schedule.run(world);
        }
        assert_eq!(hp(world), 100000.0, "前摇期间不得命中");
        // 到点命中（release ≈ 第 cycle+1 帧，量化容差内）
        for _ in 0..4 {
            schedule.run(world);
        }
        assert!(
            hp(world) < 100000.0,
            "冷却+前摇走完后必须命中（cycle 帧附近，±1 tick 容差）"
        );
    }

    /// 周期守恒：站桩连续输出，两次命中的帧间隔 = cycle（±1 tick 量化容差）
    #[test]
    fn attack_cycle_conserves_dps() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(100000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        world.spawn((
            test_monster(Faction::Player),
            test_skill(),
            Health::new(2000.0),
            Transform::from_xyz(0.0, 1.0, 0.0),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        // 跑 3 个完整周期，记录命中帧（血量变化的帧号）
        let mut hits: Vec<u32> = vec![];
        let mut prev = 100000.0f32;
        for f in 0..KNIGHT_CYCLE * 3 + 4 {
            schedule.run(world);
            let cur = world.get::<Health>(victim).unwrap().current;
            if cur < prev {
                hits.push(f);
                prev = cur;
            }
        }
        assert!(hits.len() >= 3, "3 个周期至少 3 次命中，实际 {}", hits.len());
        for w in hits.windows(2) {
            let gap = w[1] - w[0];
            assert!(
                (gap as i32 - KNIGHT_CYCLE as i32).abs() <= 1,
                "命中间隔 {gap} 必须等于周期 {KNIGHT_CYCLE}（±1 tick 容差）"
            );
        }
    }

    /// 前摇锁移动：前摇期间位置逐帧不变
    #[test]
    fn windup_locks_movement() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 靶子贴脸（射程内），单位有移速验证锁定
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(100000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let atk = world
            .spawn((
                test_monster(Faction::Player),
                test_skill(),
                Mover { speed: 3.0 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking, moving).chain());
        // 跑到进入前摇（21 帧倒数 + 1 帧转换）
        for _ in 0..KNIGHT_CYCLE - KNIGHT_WINDUP + 1 {
            schedule.run(world);
        }
        let in_windup = matches!(
            world.get::<Skill>(atk).unwrap().flow.state,
            SkillState::Windup { .. }
        );
        assert!(in_windup, "冷却结束后应进入前摇");
        // 前摇期间：位置逐帧不变
        let frozen = world.get::<Transform>(atk).unwrap().translation;
        for _ in 0..KNIGHT_WINDUP {
            schedule.run(world);
            let p = world.get::<Transform>(atk).unwrap().translation;
            assert_eq!(p, frozen, "前摇期间必须锁移动");
        }
        let _ = victim;
    }

    /// 前摇中被晕 = 白摇取消：不结算；晕结束后重新摇前摇命中
    #[test]
    fn stun_cancels_windup() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(100000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let atk = world
            .spawn((
                test_monster(Faction::Player),
                test_skill(),
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking, status_effects).chain());
        // 进前摇（21 冷却 + 1 转换 = 22 帧）
        for _ in 0..KNIGHT_CYCLE - KNIGHT_WINDUP + 1 {
            schedule.run(world);
        }
        assert!(matches!(
            world.get::<Skill>(atk).unwrap().flow.state,
            SkillState::Windup { .. }
        ));
        // 前摇中途被晕（Stun buff 直接施加——Zap 的模拟侧效果）
        world.entity_mut(atk).insert(Buffs::new(ActiveBuff {
            name: "Stun",
            secs: 0.5,
            policy: StackPolicy::Longer,
            flags: CCFlags::STUN,
            ..Default::default()
        }));
        schedule.run(world);
        // 白摇：回 Idle{0}，且本帧不结算
        assert!(
            matches!(
                world.get::<Skill>(atk).unwrap().flow.state,
                SkillState::Idle { left: 0 }
            ),
            "前摇中被晕必须取消回 Idle（白摇）"
        );
        assert_eq!(
            world.get::<Health>(victim).unwrap().current,
            100000.0,
            "被取消的出手不得结算"
        );
        // 晕 15 tick（0.5s）+ 重新前摇 9 tick 后命中
        for _ in 0..15 + KNIGHT_WINDUP + 2 {
            schedule.run(world);
        }
        assert!(
            world.get::<Health>(victim).unwrap().current < 100000.0,
            "晕结束后必须重新摇前摇并命中"
        );
    }

    /// 前摇中目标被杀 → 出手帧落空（whiff）：不结算、不 panic、进后摇。
    /// 同时验证索敌冻结：Windup 中 targeting 不得清死锁（否则测不到 whiff 路径）
    #[test]
    fn windup_whiffs_on_dead_target() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(100.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let atk = world
            .spawn((
                test_monster(Faction::Player),
                test_skill(),
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        // 冷却就绪：下一帧直接进前摇
        world.get_mut::<Skill>(atk).unwrap().flow.state = SkillState::Idle { left: 0 };

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        // 第 1 帧：targeting 构建快照并锁目标；attacking 进前摇
        schedule.run(world);
        assert!(matches!(
            world.get::<Skill>(atk).unwrap().flow.state,
            SkillState::Windup { .. }
        ));
        assert_eq!(
            world.get::<Skill>(atk).unwrap().select.target,
            Some(victim)
        );
        // 前摇中目标被杀（despawn 立即生效）
        world.despawn(victim);
        // 前摇余下帧：targeting 对出手过程中的单位冻结（不得清锁）
        schedule.run(world);
        assert_eq!(
            world.get::<Skill>(atk).unwrap().select.target,
            Some(victim),
            "前摇中索敌必须冻结（死锁不清，出手帧自行落空）"
        );
        // 跑满前摇 → 出手帧：快照查不到目标 → 落空进后摇
        for _ in 0..KNIGHT_WINDUP + 2 {
            schedule.run(world);
        }
        assert!(
            matches!(
                world.get::<Skill>(atk).unwrap().flow.state,
                SkillState::Recover { .. }
            ),
            "落空后仍要进后摇（挥完动作）"
        );
    }

    /// 后摇可移动（走A）：出手后后摇中/后摇结束，单位必须能恢复追击
    #[test]
    fn recover_allows_movement() {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<ProjectileAssets>()
            .init_resource::<ReleaseLog>()
            .init_resource::<Tick>()
            .init_resource::<WorldSnaps>();
        let world = app.world_mut();
        // 靶子贴脸：命中一次后挪远，验证后摇结束单位恢复追击（走A）
        let victim = world
            .spawn((
                test_monster(Faction::Enemy),
                Health::new(100000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        let atk = world
            .spawn((
                test_monster(Faction::Player),
                test_skill(),
                Mover { speed: 3.0 },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking, moving).chain());
        // 跑到出手完成（帧 31 release → Recover{5}）
        for _ in 0..KNIGHT_CYCLE + 2 {
            schedule.run(world);
        }
        assert!(
            matches!(
                world.get::<Skill>(atk).unwrap().flow.state,
                SkillState::Recover { .. }
            ),
            "出手帧后应进后摇"
        );
        assert!(
            world.get::<Health>(victim).unwrap().current < 100000.0,
            "应已完成一次命中"
        );
        // 目标挪远（后摇中索敌冻结不清锁，后摇结束进 Idle 后恢复追击）
        world.get_mut::<Transform>(victim).unwrap().translation = Vec3::new(0.0, 1.0, 6.0);
        let before = world.get::<Transform>(atk).unwrap().translation;
        // 后摇 5 帧 + 追击 15 帧（移速 3.0，理论走 1.5）
        for _ in 0..KNIGHT_RECOVER + 15 {
            schedule.run(world);
        }
        let after = world.get::<Transform>(atk).unwrap().translation;
        let moved = (after - before).length();
        assert!(
            moved > 0.5,
            "后摇结束进 Idle 后必须恢复移动（走A追击）：实际移动 {moved:.2}"
        );
    }
}
