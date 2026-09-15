//! 部署区域可视化：可放区（绿）与不可放区（红）半透明覆盖层
//! 表现层：只读模拟状态，不影响帧同步

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::components::{Faction, KingTower, Tower};
use crate::constants::{PRINCESS_Z, RIVER_HALF_WIDTH};
use crate::net::NetClient;

#[derive(Component)]
pub struct DeployZone {
    kind: ZoneKind,
}

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
        base_color: Color::srgba(0.2, 0.85, 0.35, 0.22),
        alpha_mode: AlphaMode::Blend,
        unlit: true,
        ..default()
    });
    let red = materials.add(StandardMaterial {
        base_color: Color::srgba(0.9, 0.2, 0.2, 0.15),
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
}

/// 每帧按本方阵营与塔存活状态刷新区域显示
/// 单机模式（两方都可下）不显示
pub fn update(
    net: Option<Res<NetClient>>,
    towers: Query<(&Tower, &Transform, Option<&KingTower>), Without<DeployZone>>,
    mut zones: Query<(&DeployZone, &mut Transform, &mut Visibility)>,
) {
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
    let half_depth = 14.0 - RIVER_HALF_WIDTH;
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
