//! 卡牌：牌库洗牌/循环、出牌、卡槽 UI 与选牌

use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;
use crate::health_bar;
use crate::net::NetClient;

/// 当前选中的手牌槽位（本地表现状态，不进模拟、不同步）
#[derive(Resource, Default)]
pub struct SelectedCard(pub usize);

/// 卡槽按钮
#[derive(Component)]
pub struct CardSlot {
    pub index: usize,
}

/// 卡槽文字（名称 + 费用）
#[derive(Component)]
pub struct CardSlotText {
    pub index: usize,
}

/// 下一张牌预览
#[derive(Component)]
pub struct NextCardText;

/// 确定性伪随机（洗牌是模拟状态，两端必须一致：只用位运算，平台无关）
fn prand(seed: u32) -> f32 {
    let h = seed.wrapping_mul(2654435761) ^ 0x9E3779B9;
    (h % 1000) as f32 / 1000.0
}

/// 确定性洗牌（Fisher-Yates + prand）
pub fn shuffled_deck(seed: u32) -> Vec<u8> {
    let mut deck: Vec<u8> = (0..DECK_SIZE as u8).map(|i| i % CARDS.len() as u8).collect();
    for i in (1..deck.len()).rev() {
        let j = (prand(seed.wrapping_add(i as u32)) * (i + 1) as f32) as usize % (i + 1);
        deck.swap(i, j);
    }
    deck
}

impl Decks {
    /// 双方牌库（固定种子，单机/测试用）
    pub fn shuffled() -> Self {
        Self::shuffled_with(42)
    }

    /// 双方牌库（指定种子；联网对局由中继在 Start 中下发，逐局变化）
    pub fn shuffled_with(seed: u32) -> Self {
        Self {
            player: shuffled_deck(seed),
            enemy: shuffled_deck(seed.wrapping_add(0x9E3779B9)),
        }
    }

    pub fn queue(&self, faction: Faction) -> &Vec<u8> {
        match faction {
            Faction::Player => &self.player,
            Faction::Enemy => &self.enemy,
        }
    }
}

/// 出兵单位的颜色：小骷髅白色、巨人深色、其余阵营色
fn unit_color(faction: Faction, spec: &MonsterSpec) -> Color {
    let base = faction_color(faction);
    if spec.radius < 0.4 {
        Color::srgb(0.92, 0.92, 0.85)
    } else if spec.radius > 0.6 {
        base.darker(0.15)
    } else {
        base
    }
}

/// 部署区域判定（CR 规则，帧同步两端各自判定结果一致）：
/// - 自己半场（不含河道）：任意部署
/// - 敌方半场：仅限该侧（左/右）公主塔已被推掉的区域，
///   且纵深不超过公主塔原来的位置（|z| <= PRINCESS_Z）
/// towers: (faction, is_king, pos) 快照
pub fn deploy_allowed(faction: Faction, pos: Vec3, towers: &[(Faction, bool, Vec3)]) -> bool {
    let own_sign = match faction {
        Faction::Player => -1.0,
        Faction::Enemy => 1.0,
    };
    if pos.z.abs() < RIVER_HALF_WIDTH {
        return false; // 河道不可部署
    }
    if pos.z.signum() == own_sign {
        return true; // 自己半场
    }
    // 敌方半场：纵深不得超过公主塔原位
    if pos.z.abs() > PRINCESS_Z {
        return false;
    }
    // 敌方半场：该侧公主塔必须已被推掉
    let side_left = pos.x < 0.0;
    !towers
        .iter()
        .any(|(f, is_king, t)| *f != faction && !*is_king && (t.x < 0.0) == side_left)
}

/// 出牌（帧同步链内执行）：校验部署区域、手牌与费用 → 扣费 → 牌循环 → 出兵
/// 任何一步不满足都丢弃指令（两端状态一致，判定结果必然相同）
#[allow(clippy::too_many_arguments)]
pub fn play_card(
    commands: &mut Commands,
    decks: &mut Decks,
    elixir: &mut Elixir,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
    card_id: u8,
    pos: Vec3,
    towers: &[(Faction, bool, Vec3)],
) {
    // 部署区域权威校验（防改版客户端在区域外下怪；两端判定一致）
    if !deploy_allowed(faction, pos, towers) {
        return;
    }
    let Some(spec) = CARDS.iter().find(|c| c.id == card_id) else {
        return;
    };
    let queue = match faction {
        Faction::Player => &mut decks.player,
        Faction::Enemy => &mut decks.enemy,
    };
    // 必须是当前手牌（确定性校验）
    let Some(hand_pos) = queue[..HAND_SIZE].iter().position(|&c| c == card_id) else {
        return;
    };
    let pool = match faction {
        Faction::Player => &mut elixir.player,
        Faction::Enemy => &mut elixir.enemy,
    };
    if *pool < spec.cost {
        return;
    }
    *pool -= spec.cost;
    // 循环：打出的牌排到队尾，补进下一张
    let played = queue.remove(hand_pos);
    queue.push(played);

    // 多单位围绕落点散开（固定偏移，确定性）
    // 先出虚影，放置时间结束才变成真兵（process_deploying 处理）
    const OFFSETS: [(f32, f32); 3] = [(0.0, 0.0), (-0.6, -0.5), (0.6, -0.5)];
    for k in 0..spec.count as usize {
        let (dx, dz) = OFFSETS[k % OFFSETS.len()];
        spawn_ghost(
            commands,
            meshes,
            materials,
            faction,
            spec,
            pos + Vec3::new(dx, 0.0, dz),
        );
    }
}

/// 放置虚影：半透明胶囊 + Deploying 组件（不参与战斗/碰撞/索敌）
fn spawn_ghost(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
    spec: &CardSpec,
    pos: Vec3,
) {
    let r = spec.monster.radius;
    commands.spawn((
        Deploying {
            card: spec.id,
            faction,
            ticks_left: spec.deploy_ticks,
        },
        Mesh3d(meshes.add(Capsule3d::new(r, 2.0 * r))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: faction_color(faction).with_alpha(0.45),
            alpha_mode: AlphaMode::Blend,
            unlit: true,
            ..default()
        })),
        Transform::from_translation(pos + Vec3::Y * 2.0 * r),
        bevy::light::NotShadowCaster,
    ));
}

/// 放置倒计时（帧同步链内）：虚影倒计时结束 → 变成真兵
pub fn process_deploying(
    mut commands: Commands,
    mut deployers: Query<(Entity, &mut Deploying, &Transform)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (e, mut d, transform) in &mut deployers {
        d.ticks_left -= 1;
        if d.ticks_left == 0 {
            let pos = transform.translation;
            if let Some(spec) = CARDS.iter().find(|c| c.id == d.card) {
                spawn_unit(
                    &mut commands,
                    &mut meshes,
                    &mut materials,
                    d.faction,
                    d.card,
                    &spec.monster,
                    Vec3::new(pos.x, 0.0, pos.z),
                );
            }
            commands.entity(e).despawn();
        }
    }
}

fn spawn_unit(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
    card: u8,
    spec: &MonsterSpec,
    pos: Vec3,
) {
    let r = spec.radius;
    // 胶囊按比例缩放：半径 r、圆柱段 2r，总高 4r
    let mut e = commands.spawn((
        Monster {
            faction,
            card,
            damage: spec.damage,
            attack_range: spec.attack_range,
            aggro_range: spec.aggro_range,
            speed: spec.speed,
            radius: r,
            mass: spec.mass,
            ranged: spec.ranged,
            target: None,
        },
        Health::new(spec.hp),
        AttackTimer(Timer::from_seconds(ATTACK_INTERVAL, TimerMode::Repeating)),
        Mesh3d(meshes.add(Capsule3d::new(r, 2.0 * r))),
        MeshMaterial3d(materials.add(unit_color(faction, spec))),
        Transform::from_translation(pos + Vec3::Y * 2.0 * r),
    ));
    health_bar::spawn(
        &mut e,
        meshes,
        materials,
        2.0 * r,
        2.0 * r + 0.35,
        faction_color(faction),
    );
}

/// 卡槽 UI：底部 4 张手牌 + 右侧下一张预览（在圣水条上方）
///
/// 弹性布局适配任意屏宽（手机逻辑宽 ~380px 塞不下 4×108 固定宽 + 预览）：
/// 卡槽 flex_grow 均分剩余空间，预览固定窄列不许压缩——
/// 否则 flexbox 按文字宽度挤压按钮，尺寸随卡名变化抖动
pub fn setup_ui(mut commands: Commands) {
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            bottom: Val::Px(46.0),
            left: Val::Px(12.0),
            right: Val::Px(12.0),
            height: Val::Px(84.0),
            justify_content: JustifyContent::Center,
            column_gap: Val::Px(8.0),
            ..default()
        })
        .with_children(|p| {
            for i in 0..HAND_SIZE {
                p.spawn((
                    Button,
                    CardSlot { index: i },
                    Node {
                        // 均分剩余宽度：窄屏自动变窄，宽屏自动变宽，四张永远等大
                        flex_grow: 1.0,
                        flex_basis: Val::Px(0.0),
                        flex_shrink: 0.0,
                        height: Val::Percent(100.0),
                        justify_content: JustifyContent::Center,
                        align_items: AlignItems::Center,
                        border: UiRect::all(Val::Px(3.0)),
                        ..default()
                    },
                    BorderColor::all(Color::NONE),
                    BackgroundColor(Color::srgb(0.15, 0.18, 0.3)),
                ))
                .with_children(|p| {
                    p.spawn((
                        CardSlotText { index: i },
                        Text::new(""),
                        TextFont {
                            font_size: FontSize::Px(14.0),
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    ));
                });
            }
            // 下一张预览：固定窄列，不参与伸缩
            p.spawn((
                NextCardText,
                Text::new(""),
                TextFont {
                    font_size: FontSize::Px(11.0),
                    ..default()
                },
                TextColor(Color::srgb(0.7, 0.7, 0.7)),
                Node {
                    width: Val::Px(52.0),
                    flex_shrink: 0.0,
                    align_self: AlignSelf::Center,
                    ..default()
                },
            ));
        });
}

/// 选牌：点卡槽或按 1-4（本地操作，不进指令流）
pub fn select_card_input(
    keys: Res<ButtonInput<KeyCode>>,
    interactions: Query<(&Interaction, &CardSlot), Changed<Interaction>>,
    mut selected: ResMut<SelectedCard>,
) {
    for (interaction, slot) in &interactions {
        if *interaction == Interaction::Pressed {
            selected.0 = slot.index;
        }
    }
    for (code, idx) in [
        (KeyCode::Digit1, 0),
        (KeyCode::Digit2, 1),
        (KeyCode::Digit3, 2),
        (KeyCode::Digit4, 3),
    ] {
        if keys.just_pressed(code) {
            selected.0 = idx;
        }
    }
}

/// 卡槽 UI 刷新：手牌名称/费用、选中金框、圣水不足置灰、下一张预览
pub fn update_card_ui(
    decks: Res<Decks>,
    elixir: Res<Elixir>,
    selected: Res<SelectedCard>,
    net: Option<Res<NetClient>>,
    mut slots: Query<(&CardSlot, &mut BackgroundColor, &mut BorderColor)>,
    mut texts: Query<(&CardSlotText, &mut Text)>,
    mut nexts: Query<&mut Text, (With<NextCardText>, Without<CardSlotText>)>,
) {
    let (queue, my_elixir) = match net.and_then(|n| Faction::from_index(n.my_index)) {
        Some(Faction::Enemy) => (&decks.enemy, elixir.enemy),
        _ => (&decks.player, elixir.player),
    };

    for (slot, mut bg, mut border) in &mut slots {
        let spec = &CARDS[queue[slot.index] as usize];
        if slot.index == selected.0 {
            *bg = Color::srgb(0.25, 0.35, 0.55).into();
            *border = BorderColor::all(Color::srgb(1.0, 0.85, 0.2));
        } else if my_elixir < spec.cost {
            *bg = Color::srgb(0.1, 0.1, 0.12).into();
            *border = BorderColor::all(Color::NONE);
        } else {
            *bg = Color::srgb(0.15, 0.18, 0.3).into();
            *border = BorderColor::all(Color::NONE);
        }
    }
    for (ct, mut text) in &mut texts {
        let spec = &CARDS[queue[ct.index] as usize];
        text.0 = format!("{}\n{}", spec.name, spec.cost as i32);
    }
    for mut text in &mut nexts {
        let spec = &CARDS[queue[HAND_SIZE] as usize];
        text.0 = format!("NEXT\n{}", spec.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 出牌：扣费、牌循环到队尾、按数量出兵
    #[test]
    fn play_card_deducts_cycles_and_spawns() {
        let mut app = App::new();
        app.insert_resource(Elixir {
            player: ELIXIR_START,
            enemy: ELIXIR_START,
        });
        app.insert_resource(Decks::shuffled());
        app.init_resource::<Tick>();
        app.init_resource::<CommandBuffer>();
        app.init_resource::<CommandLog>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        let card_id = app.world().resource::<Decks>().player[0];
        let spec = &CARDS[card_id as usize];
        let world = app.world_mut();
        world.resource_mut::<CommandBuffer>().local.insert(
            0,
            vec![GameCommand::Deploy {
                faction: Faction::Player,
                card: card_id,
                x: 0.0,
                z: -5.0,
            }],
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(crate::combat::apply_commands);
        schedule.run(world);

        // 扣费
        assert_eq!(
            world.resource::<Elixir>().player,
            ELIXIR_START - spec.cost
        );
        // 打出的牌到队尾
        let decks = world.resource::<Decks>();
        assert_eq!(decks.player[DECK_SIZE - 1], card_id);
        // 放置时间未到：只有虚影，没有真兵
        {
            let mut monsters = world.query::<&Monster>();
            assert_eq!(monsters.iter(world).count(), 0);
            let mut deployers = world.query::<&Deploying>();
            assert_eq!(deployers.iter(world).count(), spec.count as usize);
        }
        // 跑满放置时间 → 变成真兵，属性来自卡牌规格
        let mut schedule = Schedule::default();
        schedule.add_systems(process_deploying);
        for _ in 0..spec.deploy_ticks {
            schedule.run(world);
        }
        let mut monsters = world.query::<&Monster>();
        let spawned: Vec<&Monster> = monsters.iter(world).collect();
        assert_eq!(spawned.len(), spec.count as usize);
        assert_eq!(spawned[0].damage, spec.monster.damage);
    }
}

#[cfg(test)]
mod zone_tests {
    use super::*;

    fn princess(faction: Faction, x: f32, z: f32) -> (Faction, bool, Vec3) {
        (faction, false, Vec3::new(x, 0.0, z))
    }

    /// CR 部署区域规则：推掉哪侧公主塔，开放哪侧敌半场
    #[test]
    fn deploy_zone_expands_after_princess_falls() {
        // 敌方左塔活着、右塔已掉（快照里没有右塔）
        let towers = vec![
            princess(Faction::Enemy, -6.5, 8.5),  // 左公主塔（活）
            princess(Faction::Enemy, 0.0, 12.5),  // 占位：实际国王塔是 is_king，不影响
        ];
        // 把第二座标记为国王塔
        let towers: Vec<(Faction, bool, Vec3)> = vec![
            (Faction::Enemy, false, Vec3::new(-6.5, 0.0, 8.5)),
            (Faction::Enemy, true, Vec3::new(0.0, 0.0, 12.5)),
        ];

        // 自己半场：任意可下
        assert!(deploy_allowed(Faction::Player, Vec3::new(-2.0, 0.0, -5.0), &towers));
        assert!(deploy_allowed(Faction::Player, Vec3::new(2.0, 0.0, -5.0), &towers));
        // 河道：不可下
        assert!(!deploy_allowed(Faction::Player, Vec3::new(0.0, 0.0, 0.0), &towers));
        // 敌半场左侧（左塔活着）：不可下
        assert!(!deploy_allowed(Faction::Player, Vec3::new(-2.0, 0.0, 5.0), &towers));
        // 敌半场右侧（右塔已掉）：可下
        assert!(deploy_allowed(Faction::Player, Vec3::new(2.0, 0.0, 5.0), &towers));
        // 敌半场右侧但纵深超过公主塔原位（8.5）：不可下
        assert!(!deploy_allowed(Faction::Player, Vec3::new(2.0, 0.0, 10.0), &towers));
        // 红方镜像
        assert!(!deploy_allowed(Faction::Enemy, Vec3::new(0.0, 0.0, 0.0), &towers));
        assert!(deploy_allowed(Faction::Enemy, Vec3::new(2.0, 0.0, -5.0), &towers));
    }

    /// 端到端：区域外部署指令被 play_card 丢弃（不出兵、不扣费）
    #[test]
    fn play_card_rejects_out_of_zone_deploy() {
        let mut app = App::new();
        app.insert_resource(Elixir {
            player: ELIXIR_START,
            enemy: ELIXIR_START,
        });
        app.insert_resource(Decks::shuffled());
        app.init_resource::<Tick>();
        app.init_resource::<CommandBuffer>();
        app.init_resource::<CommandLog>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        // 敌方左公主塔活着
        app.world_mut().spawn((
            Tower {
                faction: Faction::Enemy,
                radius: 1.0,
                attack_range: 8.0,
                target: None,
            },
            Health::new(6000.0),
            Transform::from_xyz(-6.5, 0.0, 8.5),
        ));

        let card_id = app.world().resource::<Decks>().player[0];
        app.world_mut().resource_mut::<CommandBuffer>().local.insert(
            0,
            vec![GameCommand::Deploy {
                faction: Faction::Player,
                card: card_id,
                x: -2.0,
                z: 5.0, // 敌半场左侧：左塔还在，不可部署
            }],
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(crate::combat::apply_commands);
        schedule.run(app.world_mut());

        assert_eq!(
            app.world().resource::<Elixir>().player,
            ELIXIR_START,
            "区域外部署不应扣费"
        );
        let mut q = app.world_mut().query::<&Monster>();
        assert_eq!(q.iter(app.world()).count(), 0, "区域外部署不应出兵");
    }
}
