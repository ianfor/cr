//! 部署区域可视化：可放区（绿）与不可放区（红）半透明覆盖层，
//! 以及法术卡的施法范围指示圈（选中时贴鼠标位置）。
//! 表现层：只读模拟状态，不影响帧同步

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::bot::BotMode;
use crate::cards::SelectedCard;
use crate::components::{Decks, Faction, KingTower, Tower};
use crate::constants::{CardKind, CARDS, HAND_SIZE, PRINCESS_Z, RIVER_HALF_WIDTH};
use crate::net::{NetClient, SimState};

#[derive(Component)]
pub struct DeployZone {
    kind: ZoneKind,
}

/// 法术施法范围指示圈：选中法术卡时贴鼠标位置显示作用半径
#[derive(Component)]
pub struct SpellRangeIndicator;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ZoneKind {
    /// 自己半场（绿）
    OwnHalf,
    /// 推掉敌左公主塔后开放的扩张区（绿）
    ExpandLeft,
    /// 推掉敌右公主塔后开放的扩张区（绿）
    ExpandRight,
    /// 敌方半场不可放区（红，垫在绿色扩张区下面）
    EnemyBlocked,
}

/// 生成 4 块区域覆盖层（初始隐藏，由 update 每帧刷新）
pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let plane = meshes.add(Plane3d::default().mesh().size(1.0, 1.0));
    let green = materials.add(StandardMaterial {
        base_color: Color::srgba(0.2, 0.85, 0.35, 0.3),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });
    let red = materials.add(StandardMaterial {
        base_color: Color::srgba(0.9, 0.2, 0.2, 0.22),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });

    for (kind, mat, y) in [
        (ZoneKind::EnemyBlocked, red, 0.015),
        (ZoneKind::OwnHalf, green.clone(), 0.025),
        (ZoneKind::ExpandLeft, green.clone(), 0.025),
        (ZoneKind::ExpandRight, green, 0.025),
    ] {
        commands.spawn((
            DeployZone { kind },
            Mesh3d(plane.clone()),
            MeshMaterial3d(mat),
            Transform::from_xyz(0.0, y, 0.0),
            Visibility::Hidden,
            NotShadowCaster,
        ));
    }

    // 法术范围指示圈：半径 1 的圆环，按法术半径缩放（x/z 缩放，环厚度随半径略变）
    let ring = meshes.add(bevy::math::primitives::Torus::new(1.0, 0.05));
    let ring_mat = materials.add(StandardMaterial {
        base_color: Color::srgba(1.0, 0.95, 0.6, 0.85),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });
    commands.spawn((
        SpellRangeIndicator,
        Mesh3d(ring),
        MeshMaterial3d(ring_mat),
        Transform::from_xyz(0.0, 0.1, 0.0)
            .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        Visibility::Hidden,
        NotShadowCaster,
    ));
}

/// 每帧按本方阵营与塔存活状态刷新区域显示
pub fn update(
    net: Option<Res<NetClient>>,
    towers: Query<(&Tower, &Transform, Option<&KingTower>), Without<DeployZone>>,
    mut zones: Query<(&DeployZone, &mut Transform, &mut Visibility)>,
) {
    let half_depth = 14.0 - RIVER_HALF_WIDTH;

    // 单机模式：点哪边半场就属于哪方，所以两半都可下（绿），只有河道不可下（红）
    if net.is_none() {
        for (zone, mut transform, mut vis) in &mut zones {
            let (cx, cz, w, d) = match zone.kind {
                ZoneKind::OwnHalf => (0.0, -(RIVER_HALF_WIDTH + 14.0) / 2.0, 16.0, half_depth),
                ZoneKind::ExpandLeft => (-4.0, (RIVER_HALF_WIDTH + 14.0) / 2.0, 8.0, half_depth),
                ZoneKind::ExpandRight => (4.0, (RIVER_HALF_WIDTH + 14.0) / 2.0, 8.0, half_depth),
                ZoneKind::EnemyBlocked => (0.0, 0.0, 18.0, RIVER_HALF_WIDTH * 2.0),
            };
            let y = transform.translation.y;
            *transform = Transform::from_xyz(cx, y, cz).with_scale(Vec3::new(w, 1.0, d));
            *vis = Visibility::Visible;
        }
        return;
    }

    let Some(my) = net.and_then(|n| Faction::from_index(n.my_index)) else {
        for (_, _, mut vis) in &mut zones {
            *vis = Visibility::Hidden;
        }
        return;
    };
    let sign = match my {
        Faction::Player => -1.0_f32,
        Faction::Enemy => 1.0_f32,
    };

    // 敌方该侧公主塔是否还在（在则该侧敌半场不可下）
    let princess_alive = |x_sign: f32| {
        towers.iter().any(|(t, tr, k)| {
            t.faction != my && k.is_none() && tr.translation.x.signum() == x_sign
        })
    };
    let expand_left = !princess_alive(-1.0);
    let expand_right = !princess_alive(1.0);

    // 矩形：(center_x, center_z, width, depth)
    let own = (
        0.0,
        sign * (RIVER_HALF_WIDTH + 14.0) / 2.0,
        16.0,
        half_depth,
    );
    let expand = |x_sign: f32| {
        (
            x_sign * 4.0,
            -sign * (RIVER_HALF_WIDTH + PRINCESS_Z) / 2.0,
            8.0,
            PRINCESS_Z - RIVER_HALF_WIDTH,
        )
    };
    let blocked = (
        0.0,
        -sign * (RIVER_HALF_WIDTH + 14.0) / 2.0,
        16.0,
        half_depth,
    );

    for (zone, mut transform, mut vis) in &mut zones {
        let (show, (cx, cz, w, d)) = match zone.kind {
            ZoneKind::OwnHalf => (true, own),
            ZoneKind::EnemyBlocked => (true, blocked),
            ZoneKind::ExpandLeft => (expand_left, expand(-1.0)),
            ZoneKind::ExpandRight => (expand_right, expand(1.0)),
        };
        let y = transform.translation.y;
        *transform =
            Transform::from_xyz(cx, y, cz).with_scale(Vec3::new(w, 1.0, d));
        *vis = if show {
            Visibility::Visible
        } else {
            Visibility::Hidden
        };
    }
}

/// 法术施法范围指示圈：选中法术卡时贴鼠标位置显示作用半径（纯表现层）。
/// 阵营判定与 gather_input 同规则——联网取己方、PvE 锁蓝方、
/// 单机按悬停半场（指示的就是"此刻点击会放出的牌"）
pub fn spell_range_update(
    state: Res<SimState>,
    net: Option<Res<NetClient>>,
    bot_mode: Option<Res<BotMode>>,
    window: Single<&Window>,
    camera: Single<(&Camera, &GlobalTransform)>,
    decks: Res<Decks>,
    selected: Res<SelectedCard>,
    mut indicator: Query<(&mut Transform, &mut Visibility), With<SpellRangeIndicator>>,
) {
    let mut hide = || {
        for (_, mut v) in indicator.iter_mut() {
            *v = Visibility::Hidden;
        }
    };
    if !matches!(*state, SimState::Solo | SimState::Playing) {
        hide();
        return;
    }
    let Some(cursor) = window.cursor_position() else {
        hide();
        return;
    };
    let (camera, camera_transform) = *camera;
    let Ok(ray) = camera.viewport_to_world(camera_transform, cursor) else {
        hide();
        return;
    };
    let Some(t) = ray.intersect_plane(Vec3::ZERO, InfinitePlane3d::new(Vec3::Y)) else {
        hide();
        return;
    };
    let mut point = ray.get_point(t);
    // 与 gather_input 相同的竞技场钳制
    point.x = point.x.clamp(-8.0, 8.0);
    point.z = point.z.clamp(-14.0, 14.0);
    let faction = match net.as_ref() {
        Some(n) => match Faction::from_index(n.my_index) {
            Some(f) => f,
            None => {
                hide();
                return;
            }
        },
        None if bot_mode.is_some() => Faction::Player,
        None => {
            if point.z < 0.0 {
                Faction::Player
            } else {
                Faction::Enemy
            }
        }
    };
    let card = decks.queue(faction)[selected.0.min(HAND_SIZE - 1)];
    let CardKind::Spell(spell) = &CARDS[card as usize].kind else {
        hide();
        return; // 非法术卡：显示的是部署区域，不显示范围圈
    };
    for (mut transform, mut v) in &mut indicator {
        // 圆心 = 放置点（地面射线求交点），略抬避免与部署区覆盖层穿模
        transform.translation = Vec3::new(point.x, 0.1, point.z);
        // 环放平后在局部 XY 平面（X 旋转只是躺倒）：X/Y 缩放半径、
        // Z（管轴）保持 1——按世界轴缩放 (r,1,r) 会画出 Z 向恒为 1 的椭圆
        transform.scale = Vec3::new(spell.radius, spell.radius, 1.0);
        *v = Visibility::Visible;
    }
}
