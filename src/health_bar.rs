//! 血条：生成与刷新

use bevy::light::NotShadowCaster;
use bevy::prelude::*;

use crate::components::{Health, HealthBar, HealthBarFill};

/// 给一个单位挂血条：背景条 + 前景条，满血时隐藏
pub fn spawn(
    parent: &mut EntityCommands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    width: f32,
    y_offset: f32,
    color: Color,
) {
    // 相机固定不动，血条只要与相机同向就始终正对镜头（初始值，face_camera 每帧会校正）
    let rot = Transform::from_xyz(0.0, 34.0, -9.0)
        .looking_at(Vec3::ZERO, Vec3::Y)
        .rotation;
    let bg_mat = materials.add(StandardMaterial {
        base_color: Color::srgb(0.1, 0.1, 0.1),
        unlit: true,
        ..default()
    });
    let fill_mat = materials.add(StandardMaterial {
        base_color: color,
        unlit: true,
        ..default()
    });

    parent.with_children(|p| {
        p.spawn((
            HealthBar,
            Transform::from_xyz(0.0, y_offset, 0.0).with_rotation(rot),
            Visibility::Hidden,
        ))
        .with_children(|p| {
            // 背景
            p.spawn((
                Mesh3d(meshes.add(Cuboid::new(width + 0.08, 0.16, 0.02))),
                MeshMaterial3d(bg_mat),
                NotShadowCaster,
            ));
            // 前景
            p.spawn((
                HealthBarFill { width },
                Mesh3d(meshes.add(Cuboid::new(width, 0.12, 0.03))),
                MeshMaterial3d(fill_mat),
                NotShadowCaster,
            ));
        });
    });
}

/// 血条始终面向当前相机（真 billboard；联网红方相机镜像后也能正对镜头）
pub fn face_camera(
    camera: Single<&GlobalTransform, With<Camera>>,
    mut bars: Query<&mut Transform, (With<HealthBar>, Without<Camera>)>,
) {
    let rot = camera.rotation();
    for mut t in &mut bars {
        t.rotation = rot;
    }
}

/// 血条刷新：满血隐藏；不满血显示并按比例缩放前景条
pub fn update(
    units: Query<(&Health, &Children)>,
    bar_roots: Query<&Children, With<HealthBar>>,
    mut bar_vis: Query<&mut Visibility, With<HealthBar>>,
    mut fills: Query<(&HealthBarFill, &mut Transform)>,
) {
    for (health, children) in &units {
        let pct = (health.current / health.max).clamp(0.0, 1.0);
        for child in children.iter() {
            let Ok(mut vis) = bar_vis.get_mut(child) else {
                continue;
            };
            *vis = if pct >= 1.0 {
                Visibility::Hidden
            } else {
                Visibility::Visible
            };
            if let Ok(bar_children) = bar_roots.get(child) {
                for bar_child in bar_children.iter() {
                    if let Ok((fill, mut t)) = fills.get_mut(bar_child) {
                        t.scale.x = pct;
                        // 左对齐锚定：缩放时向左缩短
                        t.translation.x = -(1.0 - pct) * fill.width * 0.5;
                    }
                }
            }
        }
    }
}
