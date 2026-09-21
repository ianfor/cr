//! 竞技场场景搭建：相机、灯光、地面、装饰、河道、桥、塔

use bevy::camera::ScalingMode;
use bevy::prelude::*;

use crate::components::{
    faction_color, Attacker, Faction, Health, KingTower, Targeting, TargetPolicy, Tower,
};
use crate::constants::*;
use crate::health_bar;
use crate::net::NetClient;

/// 联网对战中红方（序号 1）的视角镜像：让自己半场始终显示在屏幕下方
/// 相机和输入都是世界坐标，AI/部署逻辑无需任何改动
pub fn flip_camera_for_enemy(
    net: Option<Res<NetClient>>,
    mut camera: Single<&mut Transform, With<Camera>>,
    mut flipped: Local<bool>,
) {
    if *flipped {
        return;
    }
    let Some(net) = net else { return };
    if net.my_index == 1 {
        **camera = Transform::from_xyz(0.0, 34.0, 9.0).looking_at(Vec3::new(0.0, 0.0, 2.0), Vec3::Y);
        info!("红方视角：相机已镜像（己方半场在屏幕下方）");
        *flipped = true;
    }
}

pub fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // 斜视 + 正交投影：全图无近大远小，CR 式视角
    // IsDefaultUiCamera：让这台相机同时负责渲染 UI（圣水条）
    // look_at 下移 2.0：底部留出卡牌/圣水 UI 的空间，不盖住玩家底线
    commands.spawn((
        Camera3d::default(),
        IsDefaultUiCamera,
        Projection::Orthographic(OrthographicProjection {
            // AutoMin：宽、高都不小于下限，比例不变。
            // PC 540×960 时等价于原 FixedVertical(34)；手机长窄屏（逻辑宽 ~405）
            // 若仍固定高度，横向视野只剩 ~15 世界单位 < 场地宽（~18），左右被裁
            scaling_mode: ScalingMode::AutoMin {
                min_width: 19.0,
                min_height: 34.0,
            },
            ..OrthographicProjection::default_3d()
        }),
        Transform::from_xyz(0.0, 34.0, -9.0).looking_at(Vec3::new(0.0, 0.0, -2.0), Vec3::Y),
    ));

    // 平行光
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(8.0, 16.0, -6.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    // 地面 18 x 30
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(18.0, 30.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.35, 0.6, 0.3))),
    ));

    // 外围大地面，盖住竞技场外的空白
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(80.0, 80.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.22, 0.42, 0.2))),
        Transform::from_xyz(0.0, -0.02, 0.0),
    ));

    // 两侧和两端的装饰树（用三角函数做确定性的位置抖动，避免引入随机数库）
    for i in 0..6 {
        let t = i as f32;
        let z = -13.0 + t * 5.0;
        for side in [-1.0_f32, 1.0] {
            let x = side * (10.5 + (t * 3.7).sin() * 1.2);
            let scale = 0.8 + (t * 5.1 + side).cos().abs() * 0.4;
            spawn_tree(
                &mut commands,
                &mut meshes,
                &mut materials,
                Vec3::new(x, 0.0, z),
                scale,
            );
        }
    }
    for i in 0..3 {
        let t = i as f32;
        let x = -6.0 + t * 6.0;
        for end in [-1.0_f32, 1.0] {
            let z = end * (16.5 + (t * 2.9).cos() * 1.0);
            let scale = 0.9 + (t * 4.3 + end).sin().abs() * 0.3;
            spawn_tree(
                &mut commands,
                &mut meshes,
                &mut materials,
                Vec3::new(x, 0.0, z),
                scale,
            );
        }
    }

    // 中间的河道
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(18.0, 2.5))),
        MeshMaterial3d(materials.add(Color::srgb(0.2, 0.45, 0.8))),
        Transform::from_xyz(0.0, 0.01, 0.0),
    ));

    // 两座桥
    for x in BRIDGES {
        commands.spawn((
            Mesh3d(meshes.add(Cuboid::new(3.0, 0.4, 3.5))),
            MeshMaterial3d(materials.add(Color::srgb(0.5, 0.35, 0.2))),
            Transform::from_xyz(x, 0.2, 0.0),
        ));
    }

    // 双方塔：蓝方在下（玩家侧），红方在上
    // 国王塔居中靠边界，两座公主塔靠边（射程互不覆盖）
    for (faction, sign) in [(Faction::Player, -1.0_f32), (Faction::Enemy, 1.0_f32)] {
        spawn_tower(
            &mut commands,
            &mut meshes,
            &mut materials,
            Vec3::new(0.0, 0.0, sign * TOWER_Z),
            faction,
            &KING_TOWER,
        );
        for x in [-PRINCESS_X, PRINCESS_X] {
            spawn_tower(
                &mut commands,
                &mut meshes,
                &mut materials,
                Vec3::new(x, 0.0, sign * PRINCESS_Z),
                faction,
                &PRINCESS_TOWER,
            );
        }
    }
}

fn spawn_tree(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    position: Vec3,
    scale: f32,
) {
    // 树干
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(0.15, 1.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.4, 0.28, 0.15))),
        Transform::from_translation(position + Vec3::Y * 0.5).with_scale(Vec3::splat(scale)),
    ));
    // 树冠
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(0.9))),
        MeshMaterial3d(materials.add(Color::srgb(0.18, 0.48, 0.22))),
        Transform::from_translation(position + Vec3::Y * 1.6).with_scale(Vec3::splat(scale)),
    ));
}

fn spawn_tower(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    position: Vec3,
    faction: Faction,
    spec: &TowerSpec,
) {
    let color = faction_color(faction);
    let body_mesh = meshes.add(Cylinder::new(spec.body_radius, spec.body_height));
    let roof_mesh = meshes.add(Cone::new(spec.roof_radius, spec.roof_height));
    let body_mat = materials.add(color);
    let roof_mat = materials.add(color.darker(0.15));

    let mut root = commands.spawn((
        Tower {
            faction,
            radius: spec.body_radius,
        },
        // 塔的攻击能力：统一走 targeting(Guard)/attacking 系统
        Attacker {
            damage: TOWER_ATTACK_DAMAGE,
            attack_range: spec.attack_range,
            interval: ATTACK_INTERVAL,
            cooldown: ATTACK_INTERVAL,
            splash_radius: 0.0,
            hits_air: true,
            ranged: true,
            target: None,
            engaged: false,
        },
        Targeting(TargetPolicy::Guard),
        Health::new(spec.hp),
        Transform::from_translation(position),
    ));
    if spec.is_king {
        root.insert(KingTower);
    }
    root.with_children(|p| {
        // 塔身
        p.spawn((
            Mesh3d(body_mesh),
            MeshMaterial3d(body_mat),
            Transform::from_xyz(0.0, spec.body_height * 0.5, 0.0),
        ));
        // 塔顶
        p.spawn((
            Mesh3d(roof_mesh),
            MeshMaterial3d(roof_mat),
            Transform::from_xyz(0.0, spec.body_height + spec.roof_height * 0.5, 0.0),
        ));
    });
    health_bar::spawn(&mut root, meshes, materials, spec.bar_width, spec.bar_y, color);
}
