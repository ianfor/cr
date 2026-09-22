//! 统一索敌：怪物（Seek）与塔/建筑卡（Guard）共用一套目标锁定逻辑。
//!
//! Seek（怪物）：
//! - aggro 内"最近目标"，塔/怪物/建筑一视同仁（塔不是兜底——历史 bug 修复）
//! - 交战中锁定不换目标（防距离抖动 flip-flop，对齐 CR：打塔不理小怪）
//! - 未交战（行军中）每帧重评——对手进场立即回应（骷髅不会无视贴脸敌人）
//! - aggro 内无目标 → 全场最近敌方建筑为行军方向
//!
//! Guard（塔/建筑卡）：射程内最近敌方怪物，目标出射程即丢锁（原地守卫）
//!
//! 本系统还负责构建全场战场视图（WorldSnaps：快照 + 怪物空间网格 +
//! entity→下标点查表），供 attacking/moving/溅射复用。
//! 最近邻：怪走网格（扩环+早退），塔/建筑走线性（≤12 个且不动），
//! 等距平局怪优先（对齐旧全扫顺序：怪在快照前段）。

use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;

use super::{can_target, edge_dist, SpatialGrid, UnitSnap, WorldSnaps};

pub fn targeting(
    mut snaps_res: ResMut<WorldSnaps>,
    mut units: Query<(
        Entity,
        &mut Attacker,
        &Targeting,
        &Transform,
        &Unit,
        Option<&Buffs>,
    )>,
    all: Query<(Entity, &Unit, &Transform, Option<&Flying>)>,
) {
    // ===== 全场快照（怪+塔+建筑，顺序两端一致） =====
    let WorldSnaps { snaps, grid, index } = &mut *snaps_res;
    snaps.clear();
    snaps.extend(all.iter().map(|(e, u, t, f)| UnitSnap {
        entity: e,
        kind: u.kind,
        faction: u.faction,
        pos: t.translation,
        radius: u.radius,
        mass: u.mass,
        flying: f.is_some(),
    }));
    // 确定性铁律：统一 query 的迭代序是 archetype 序，不保证 怪→塔→建筑 分组。
    // sort_by_key 是稳定排序：同类保持 query 相对序（与旧单类 query 的相对序一致），
    // 类间固定 Troop→Tower→Building——逐比特复刻旧"三段拼接"的快照序列，
    // 保住 merge_nearest 等距怪优先 / nearest_static 塔先建筑后 的 tie-break 契约
    snaps.sort_by_key(|s| s.kind.rank());

    // ===== 空间索引：网格只装怪物；entity→下标点查表（只 get 不迭代） =====
    grid.clear();
    index.clear();
    for (i, s) in snaps.iter().enumerate() {
        if s.kind == UnitKind::Troop {
            grid.insert(s.pos, i as u32);
        }
        index.insert(s.entity, i as u32);
    }
    let (snaps, grid, index) = (&*snaps, &*grid, &*index);

    for (entity, mut attacker, targeting, transform, unit, buffs) in &mut units {
        // 禁索敌（眩晕/致盲）：实时查询 buff 标志位，无派生缓存
        if buffs.map(|b| b.channels().cannot_seek).unwrap_or(false) {
            continue;
        }
        let pos = transform.translation;
        let faction = unit.faction;
        let self_radius = unit.radius;

        match &targeting.0 {
            // ===== 守卫（塔/建筑卡）：只打怪，出射程丢锁 =====
            TargetPolicy::Guard => {
                if let Some(e) = attacker.target {
                    let invalid = match index.get(&e).map(|&i| &snaps[i as usize]) {
                        Some(s) if s.faction != faction => {
                            !can_target(attacker.hits_air, false, s)
                                || edge_dist(pos, self_radius, s.pos, s.radius)
                                    > attacker.attack_range
                        }
                        _ => true,
                    };
                    if invalid {
                        attacker.target = None;
                    }
                }
                if attacker.target.is_none() {
                    let found = nearest_monster(
                        grid,
                        snaps,
                        pos,
                        faction,
                        entity,
                        self_radius,
                        attacker.attack_range,
                        attacker.hits_air,
                    );
                    attacker.target = found.map(|(_, i)| snaps[i as usize].entity);
                }
            }
            // ===== 怪物：aggro 内最近 + 建筑兜底 + 交战锁定 =====
            TargetPolicy::Seek {
                aggro_range,
                building_only,
            } => {
                // 锁定失效即解除：
                // 1) 目标消失（死亡）
                // 2) 已交战（进过攻击范围）后被挤出攻击范围 = 被打断
                //    （站桩输出被新放置的怪挤开等）。未交战不因距离解锁。
                if let Some(e) = attacker.target {
                    let invalid = match index.get(&e).map(|&i| &snaps[i as usize]) {
                        Some(s) if s.faction != faction => {
                            attacker.engaged
                                && edge_dist(pos, self_radius, s.pos, s.radius)
                                    > attacker.attack_range + 0.05
                        }
                        _ => true,
                    };
                    if invalid {
                        attacker.target = None;
                        attacker.engaged = false;
                    }
                }
                // 索敌：未交战每帧重评（交战中锁定不换）
                if !attacker.engaged {
                    // aggro 内最近（含塔/建筑）。只攻建筑单位：怪物全被
                    // can_target 拒 → 跳过网格查询，只扫静态
                    let in_aggro = if *building_only {
                        nearest_static(snaps, pos, faction, entity, self_radius, *aggro_range)
                    } else {
                        merge_nearest(
                            nearest_monster(
                                grid,
                                snaps,
                                pos,
                                faction,
                                entity,
                                self_radius,
                                *aggro_range,
                                attacker.hits_air,
                            ),
                            nearest_static(snaps, pos, faction, entity, self_radius, *aggro_range),
                        )
                    };
                    // 建筑兜底：全场最近敌方建筑（无距离限制，行军方向）
                    let found = in_aggro.or_else(|| {
                        nearest_static(snaps, pos, faction, entity, self_radius, f32::INFINITY)
                    });
                    attacker.target = found.map(|(_, i)| snaps[i as usize].entity);
                }
            }
        }
    }
}

/// 最近怪（网格扩环 + 早退）：filter 内做全部精确判定
/// （阵营/自身排除 + 对空限制 + edge_dist ≤ range）
fn nearest_monster(
    grid: &SpatialGrid,
    snaps: &[UnitSnap],
    pos: Vec3,
    faction: Faction,
    self_entity: Entity,
    self_radius: f32,
    range: f32,
    hits_air: bool,
) -> Option<(f32, u32)> {
    grid.query_nearest(
        snaps,
        pos,
        range + self_radius + MONSTER_RADIUS_MAX,
        &|s: &UnitSnap| {
            s.faction != faction
                && s.entity != self_entity
                && can_target(hits_air, false, s)
                && edge_dist(pos, self_radius, s.pos, s.radius) <= range
        },
    )
}

/// 最近静态（塔/建筑线性扫描，保持快照顺序：塔在前建筑在后）
fn nearest_static(
    snaps: &[UnitSnap],
    pos: Vec3,
    faction: Faction,
    self_entity: Entity,
    self_radius: f32,
    max_edge: f32,
) -> Option<(f32, u32)> {
    snaps
        .iter()
        .enumerate()
        .filter(|(_, s)| {
            s.is_building_kind()
                && s.faction != faction
                && s.entity != self_entity
                && edge_dist(pos, self_radius, s.pos, s.radius) <= max_edge
        })
        .map(|(i, s)| (pos.distance_squared(s.pos), i as u32))
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
}

/// 合并怪（网格）与静态（线性）的最近结果：
/// 等距时怪优先——对齐旧全扫顺序（怪在快照前段，min_by 先见者优先）
fn merge_nearest(
    monster: Option<(f32, u32)>,
    statc: Option<(f32, u32)>,
) -> Option<(f32, u32)> {
    match (monster, statc) {
        (Some(m), None) => Some(m),
        (None, Some(s)) => Some(s),
        (Some((dm, im)), Some((ds, is))) => Some(if dm <= ds { (dm, im) } else { (ds, is) }),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::attacking;
    use super::super::{seek, test_attacker, test_monster, WorldSnaps};
    use super::*;
    use crate::components::Health;
    use crate::constants::TOWER_ATTACK_DAMAGE;

    /// 搭建跑索敌系统的最小 App（含快照资源与资产）
    fn setup() -> App {
        let mut app = App::new();
        app.init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<crate::combat::ProjectileAssets>()
            .init_resource::<WorldSnaps>();
        app
    }

    fn spawn_seek_monster(world: &mut World, faction: Faction, pos: Vec3) -> Entity {
        world
            .spawn((
                test_monster(faction),
                test_attacker(),
                seek(5.0),
                Mover { speed: 1.5 },
                Health::new(2000.0),
                Transform::from_translation(pos),
            ))
            .id()
    }

    fn spawn_guard_tower(world: &mut World, faction: Faction, pos: Vec3) -> Entity {
        world
            .spawn((
                Unit::tower(faction, 1.0),
                Attacker {
                    damage: TOWER_ATTACK_DAMAGE,
                    attack_range: 6.0,
                    interval: 1.0,
                    cooldown: 1.0,
                    splash_radius: 0.0,
                    hits_air: true,
                    ranged: true,
                    target: None,
                    engaged: false,
                },
                Targeting(TargetPolicy::Guard),
                Health::new(6000.0),
                Transform::from_translation(pos),
            ))
            .id()
    }

    /// 两只敌对骑士在索敌范围内：必须互相锁定
    #[test]
    fn monsters_aggro_each_other() {
        let mut app = setup();
        let world = app.world_mut();
        let a = spawn_seek_monster(world, Faction::Player, Vec3::new(0.0, 1.0, -2.0));
        let b = spawn_seek_monster(world, Faction::Enemy, Vec3::new(0.0, 1.0, 2.0));

        let mut schedule = Schedule::default();
        schedule.add_systems(targeting);
        schedule.run(world);
        assert_eq!(world.get::<Attacker>(a).unwrap().target, Some(b));
        assert_eq!(world.get::<Attacker>(b).unwrap().target, Some(a));
    }

    /// 行军中的怪（未交战）必须回应进入 aggro 的敌人：改锁更近的怪。
    /// 墓碑骷髅/防守怪"无视贴脸敌人只走塔"的 bug 回归
    #[test]
    fn marching_monster_retargets_to_enemy_entering_aggro() {
        let mut app = setup();
        let world = app.world_mut();
        let tower = spawn_guard_tower(world, Faction::Enemy, Vec3::new(0.0, 0.0, 12.5));
        let m = spawn_seek_monster(world, Faction::Player, Vec3::new(0.0, 1.0, -5.0));

        let mut schedule = Schedule::default();
        schedule.add_systems(targeting);
        schedule.run(world);
        // 出生时无怪可打 → 锁塔（行军方向）
        assert_eq!(world.get::<Attacker>(m).unwrap().target, Some(tower));
        assert!(!world.get::<Attacker>(m).unwrap().engaged);

        // 敌方怪物进入 aggro（距离 4 < 塔 17.5）：未交战必须改锁更近的怪
        let e = spawn_seek_monster(world, Faction::Enemy, Vec3::new(0.0, 1.0, -1.0));
        schedule.run(world);
        assert_eq!(
            world.get::<Attacker>(m).unwrap().target,
            Some(e),
            "行军中的单位必须回应进入 aggro 的更近敌人"
        );
    }

    /// 优先级一视同仁：塔比怪更近时锁塔（历史 bug 是怪物永远优先于塔）
    #[test]
    fn nearer_tower_beats_farther_monster() {
        let mut app = setup();
        let world = app.world_mut();
        let tower = spawn_guard_tower(world, Faction::Enemy, Vec3::new(0.0, 0.0, -1.0)); // 距我 4
        spawn_seek_monster(world, Faction::Enemy, Vec3::new(4.5, 1.0, -5.0)); // 距我 4.5
        let m = spawn_seek_monster(world, Faction::Player, Vec3::new(0.0, 1.0, -5.0));

        let mut schedule = Schedule::default();
        schedule.add_systems(targeting);
        schedule.run(world);
        assert_eq!(
            world.get::<Attacker>(m).unwrap().target,
            Some(tower),
            "aggro 内塔更近时必须锁塔（塔不是兜底目标）"
        );
    }

    /// 已在攻击塔的怪（交战状态）绝不改目标，即使敌方怪物进入 aggro
    #[test]
    fn engaged_on_tower_never_retargets() {
        let mut app = setup();
        let world = app.world_mut();
        let tower = spawn_guard_tower(world, Faction::Enemy, Vec3::new(0.0, 0.0, 12.5));
        // 贴着塔放（已在攻击范围内）：先跑一帧索敌+攻击置 engaged
        let m = spawn_seek_monster(world, Faction::Player, Vec3::new(0.0, 1.0, 11.3));

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        schedule.run(world);
        assert_eq!(world.get::<Attacker>(m).unwrap().target, Some(tower));
        assert!(world.get::<Attacker>(m).unwrap().engaged, "贴塔单位应已交战");

        // 敌方怪物进入 aggro：已交战的怪不得改目标
        spawn_seek_monster(world, Faction::Enemy, Vec3::new(0.0, 1.0, 9.0));
        schedule.run(world);
        assert_eq!(
            world.get::<Attacker>(m).unwrap().target,
            Some(tower),
            "交战中的单位不得改目标"
        );
    }

    /// 站桩输出被挤到脱离攻击范围 = 被打断：锁定必须解除并改锁挤它的怪
    #[test]
    fn pushed_out_of_range_breaks_lock() {
        let mut app = setup();
        let world = app.world_mut();
        let tower = spawn_guard_tower(world, Faction::Enemy, Vec3::new(0.0, 0.0, 12.5));
        let m = spawn_seek_monster(world, Faction::Player, Vec3::new(0.0, 1.0, 11.3)); // 贴塔

        let mut schedule = Schedule::default();
        schedule.add_systems((targeting, attacking).chain());
        schedule.run(world);
        assert_eq!(world.get::<Attacker>(m).unwrap().target, Some(tower));
        assert!(world.get::<Attacker>(m).unwrap().engaged);

        // 模拟被挤开：挪到塔的攻击范围外，同时挤它的敌怪就在 aggro 内
        world.get_mut::<Transform>(m).unwrap().translation = Vec3::new(0.0, 1.0, 9.0);
        let e = spawn_seek_monster(world, Faction::Enemy, Vec3::new(0.0, 1.0, 8.0));

        schedule.run(world);
        assert_eq!(
            world.get::<Attacker>(m).unwrap().target,
            Some(e),
            "被打断后必须改锁 aggro 内最近的敌人（挤它的那只）"
        );
    }

    /// 只攻建筑单位（巨人/野猪）：无视 aggro 内的敌怪，直奔塔/建筑卡
    #[test]
    fn building_only_ignores_monsters() {
        let mut app = setup();
        let world = app.world_mut();
        let tower = spawn_guard_tower(world, Faction::Enemy, Vec3::new(0.0, 0.0, 12.5));
        let mut giant = test_monster(Faction::Player);
        giant.mass = 3.0;
        let g = world
            .spawn((
                giant,
                test_attacker(),
                Targeting(TargetPolicy::Seek {
                    aggro_range: 5.0,
                    building_only: true,
                }),
                Mover { speed: 1.0 },
                Health::new(5000.0),
                Transform::from_xyz(0.0, 1.0, -5.0),
            ))
            .id();
        // 敌方骷髅进 aggro（距离 4，边缘距 3 ≤ 5）
        spawn_seek_monster(world, Faction::Enemy, Vec3::new(0.0, 1.0, -1.0));

        let mut schedule = Schedule::default();
        schedule.add_systems(targeting);
        schedule.run(world);
        assert_eq!(
            world.get::<Attacker>(g).unwrap().target,
            Some(tower),
            "只攻建筑单位必须无视怪物直奔塔"
        );
    }

    /// 不能对空的地面单位：打不了飞行单位，索敌跳过空军
    #[test]
    fn ground_unit_cannot_target_flying() {
        let mut app = setup();
        let world = app.world_mut();
        let tower = spawn_guard_tower(world, Faction::Enemy, Vec3::new(0.0, 0.0, 12.5));
        let knight = spawn_seek_monster(world, Faction::Player, Vec3::new(0.0, 1.0, -5.0));
        // 敌方飞行单位（亡灵）贴脸
        world.spawn((
            test_monster(Faction::Enemy),
            test_attacker(),
            seek(3.0),
            Mover { speed: 2.0 },
            Flying,
            Health::new(320.0),
            Transform::from_xyz(0.0, 2.6, -5.4),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(targeting);
        schedule.run(world);
        assert_eq!(
            world.get::<Attacker>(knight).unwrap().target,
            Some(tower),
            "不能对空的单位必须跳过飞行单位"
        );
    }

    /// 守卫（塔）：射程内最近敌方怪物，出射程丢锁
    #[test]
    fn guard_tower_targets_and_releases() {
        let mut app = setup();
        let world = app.world_mut();
        let tower = spawn_guard_tower(world, Faction::Enemy, Vec3::new(0.0, 0.0, 8.5));
        // 怪进入射程（边缘距 = 12.5-8.5-1.0-0.5 = 2.5 ≤ 6）
        let m = spawn_seek_monster(world, Faction::Player, Vec3::new(0.0, 1.0, 12.5));

        let mut schedule = Schedule::default();
        schedule.add_systems(targeting);
        schedule.run(world);
        assert_eq!(world.get::<Attacker>(tower).unwrap().target, Some(m));

        // 怪跑出射程：丢锁
        world.get_mut::<Transform>(m).unwrap().translation = Vec3::new(0.0, 1.0, 16.5);
        schedule.run(world);
        assert_eq!(
            world.get::<Attacker>(tower).unwrap().target,
            None,
            "守卫目标出射程必须丢锁"
        );
    }
}
