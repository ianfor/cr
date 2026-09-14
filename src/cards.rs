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
    /// 双方牌库（不同固定种子，两端客户端生成结果一致）
    pub fn shuffled() -> Self {
        Self {
            player: shuffled_deck(100),
            enemy: shuffled_deck(200),
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

/// 出牌（帧同步链内执行）：校验手牌与费用 → 扣费 → 牌循环 → 出兵
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
) {
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
    const OFFSETS: [(f32, f32); 3] = [(0.0, 0.0), (-0.6, -0.5), (0.6, -0.5)];
    for k in 0..spec.count as usize {
        let (dx, dz) = OFFSETS[k % OFFSETS.len()];
        spawn_unit(
            commands,
            meshes,
            materials,
            faction,
            &spec.monster,
            pos + Vec3::new(dx, 0.0, dz),
        );
    }
}

fn spawn_unit(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
    spec: &MonsterSpec,
    pos: Vec3,
) {
    let r = spec.radius;
    // 胶囊按比例缩放：半径 r、圆柱段 2r，总高 4r
    let mut e = commands.spawn((
        Monster {
            faction,
            damage: spec.damage,
            attack_range: spec.attack_range,
            aggro_range: spec.aggro_range,
            speed: spec.speed,
            radius: r,
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
                        width: Val::Px(108.0),
                        height: Val::Px(84.0),
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
                            font_size: FontSize::Px(16.0),
                            ..default()
                        },
                        TextColor(Color::WHITE),
                    ));
                });
            }
            // 下一张预览
            p.spawn((
                NextCardText,
                Text::new(""),
                TextFont {
                    font_size: FontSize::Px(12.0),
                    ..default()
                },
                TextColor(Color::srgb(0.7, 0.7, 0.7)),
                Node {
                    align_self: AlignSelf::Center,
                    margin: UiRect::left(Val::Px(8.0)),
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
        // 按数量出兵，属性来自卡牌规格
        let mut monsters = world.query::<&Monster>();
        let spawned: Vec<&Monster> = monsters.iter(world).collect();
        assert_eq!(spawned.len(), spec.count as usize);
        assert_eq!(spawned[0].damage, spec.monster.damage);
    }
}
