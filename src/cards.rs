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

/// 从全部卡种里确定性抽 DECK_SIZE 张组成牌池（部分 Fisher-Yates 取前缀）
/// 双方共用同一牌池（各自洗牌序不同）：消除"一方没抽到法术/建筑"的结构性差距
fn deck_pool(seed: u32) -> Vec<u8> {
    let n = CARDS.len();
    let mut all: Vec<u8> = (0..n as u8).collect();
    for i in 0..DECK_SIZE {
        let r = prand(seed.wrapping_add((i as u32).wrapping_mul(0x9E37)));
        let j = i + (r * ((n - i) as f32)) as usize % (n - i);
        all.swap(i, j);
    }
    all.truncate(DECK_SIZE);
    all
}

/// 确定性洗牌（Fisher-Yates + prand）
pub fn shuffled_deck(seed: u32) -> Vec<u8> {
    let mut deck = deck_pool(seed);
    for i in (1..deck.len()).rev() {
        let j = (prand(seed.wrapping_add((i as u32).wrapping_mul(31))) * (i + 1) as f32) as usize
            % (i + 1);
        deck.swap(i, j);
    }
    deck
}

/// 对给定牌池做确定性洗牌（双方共用池、不同序）
fn shuffled_order(pool: &[u8], seed: u32) -> Vec<u8> {
    let mut deck = pool.to_vec();
    for i in (1..deck.len()).rev() {
        let j = (prand(seed.wrapping_add((i as u32).wrapping_mul(31))) * (i + 1) as f32) as usize
            % (i + 1);
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
    /// 双方共用同一 8 张牌池（消除结构性卡池差距），各自独立洗牌
    pub fn shuffled_with(seed: u32) -> Self {
        let pool = deck_pool(seed);
        Self {
            player: shuffled_order(&pool, seed),
            enemy: shuffled_order(&pool, seed.wrapping_add(0x9E3779B9)),
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

/// 按卡类别的部署区域判定（采集侧与执行侧共用同一套规则）：
/// - 部队：CR 规则（deploy_allowed）
/// - 法术：瞬发，全场任意位置（含河道，AoE 打桥上的单位）
/// - 建筑：仅己方半场（推塔也不开放敌半场），不含河道
pub fn deploy_zone_ok(
    spec: &CardSpec,
    faction: Faction,
    pos: Vec3,
    towers: &[(Faction, bool, Vec3)],
) -> bool {
    match &spec.kind {
        CardKind::Spell(_) => pos.x.abs() <= 8.0 && pos.z.abs() <= 14.0,
        CardKind::Building(_) => {
            let own_sign = match faction {
                Faction::Player => -1.0,
                Faction::Enemy => 1.0,
            };
            pos.z.signum() == own_sign && pos.z.abs() >= RIVER_HALF_WIDTH && pos.z.abs() <= 14.0
        }
        CardKind::Troop(_) => deploy_allowed(faction, pos, towers),
    }
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
    spell_targets: &mut Query<(
        Entity,
        &mut Health,
        &Transform,
        Option<&Monster>,
        Option<&BuildingCard>,
        Option<&mut Buffs>,
    )>,
    faction: Faction,
    card_id: u8,
    pos: Vec3,
    towers: &[(Faction, bool, Vec3)],
) {
    // 部署区域权威校验（防改版客户端在区域外下怪；两端判定一致）
    let Some(spec) = CARDS.iter().find(|c| c.id == card_id) else {
        return;
    };
    if !deploy_zone_ok(spec, faction, pos, towers) {
        return;
    }
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

    match &spec.kind {
        // 部队：先出虚影，放置时间结束才变成真兵（process_deploying 处理）
        CardKind::Troop(_) => {
            // 多单位围绕落点散开（固定偏移，确定性）
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
        // 法术：瞬发结算（伤害对敌，狂暴/晕眩对己/敌——都打包成 buff）；
        // 多段法术（waves > 1）改为 SpellVolley 分波延迟结算
        CardKind::Spell(spell) => {
            if spell.waves > 1 {
                // 万箭齐发类：伤害按波落地（首波 SPELL_WAVE_FIRST_TICKS 帧、
                // 波隔 SPELL_WAVE_INTERVAL_TICKS 帧），按落波时刻的位置判定。
                // 狂暴/晕眩仍属瞬发效果——多段卡目前不带这些（带了也只该
                // 在首波生效，届时再扩展）
                commands.spawn((
                    SpellVolley {
                        faction,
                        damage: spell.damage / spell.waves as f32,
                        radius: spell.radius,
                        x: pos.x,
                        z: pos.z,
                        waves_left: spell.waves,
                        next_in: SPELL_WAVE_FIRST_TICKS,
                        interval: SPELL_WAVE_INTERVAL_TICKS,
                    },
                    // 带 Transform：reset_world 按 Transform/Node 清场景实体
                    Transform::default(),
                ));
                return;
            }
            for (e, mut hp, tr, monster, building, mut buffs) in spell_targets.iter_mut() {
                let target_faction = match (&monster, &building) {
                    (Some(m), _) => m.faction,
                    (None, Some(b)) => b.faction,
                    _ => continue, // 塔等其余实体不吃法术
                };
                let mut d = tr.translation - pos;
                d.y = 0.0;
                if d.length() > spell.radius {
                    continue;
                }
                if target_faction == faction {
                    // 己方单位：狂暴 buff（仅怪物）
                    if let (Some(r), Some(_)) = (&spell.rage, monster) {
                        let buff = ActiveBuff {
                            name: "Rage",
                            secs: r.secs,
                            stacks: 1,
                            policy: StackPolicy::Refresh,
                            flags: CCFlags::NONE,
                            effects: vec![
                                StatMod {
                                    stat: StatKind::MoveSpeed,
                                    op: Op::Pct,
                                    value: r.pct,
                                },
                                StatMod {
                                    stat: StatKind::AttackSpeed,
                                    op: Op::Pct,
                                    value: r.pct,
                                },
                            ],
                        };
                        match buffs.as_mut() {
                            Some(existing) => existing.apply(buff),
                            None => {
                                commands.entity(e).insert(Buffs::new(buff));
                            }
                        }
                    }
                } else {
                    // 敌方单位：伤害 + 晕眩 buff（仅怪物；Stun 标记由 status 同步）
                    hp.current -= spell.damage;
                    if spell.stun_secs > 0.0 && monster.is_some() {
                        let buff = ActiveBuff {
                            name: "Stun",
                            secs: spell.stun_secs,
                            stacks: 1,
                            policy: StackPolicy::Longer,
                            flags: CCFlags::STUN,
                            effects: vec![],
                        };
                        match buffs.as_mut() {
                            Some(existing) => existing.apply(buff),
                            None => {
                                commands.entity(e).insert(Buffs::new(buff));
                            }
                        }
                    }
                }
            }
        }
        // 建筑：先出虚影，放置时间结束生成建筑实体（process_deploying 处理）
        CardKind::Building(_) => {
            spawn_ghost(commands, meshes, materials, faction, spec, pos);
        }
    }
}

/// 多段法术逐帧推进（帧同步链内，紧随 apply_commands）：
/// 到点结算一波——目标规则与瞬发法术完全一致（敌怪 + 敌建筑卡，
/// 中心距 ≤ 半径，塔不吃法术）。波间倒数，波数耗尽销毁。
/// 按落波时刻的位置判定：期间走位可以躲出圈（对齐 CR 万箭手感）
pub fn spell_volley_tick(
    mut commands: Commands,
    mut volleys: Query<(Entity, &mut SpellVolley)>,
    mut targets: Query<(
        &mut Health,
        &Transform,
        Option<&Monster>,
        Option<&BuildingCard>,
    )>,
) {
    for (ve, mut v) in &mut volleys {
        if v.next_in > 0 {
            v.next_in -= 1;
            continue;
        }
        let pos = Vec3::new(v.x, 0.0, v.z);
        for (mut hp, tr, monster, building) in targets.iter_mut() {
            let target_faction = match (&monster, &building) {
                (Some(m), _) => m.faction,
                (None, Some(b)) => b.faction,
                _ => continue, // 塔等其余实体不吃法术（与瞬发分支一致）
            };
            if target_faction == v.faction {
                continue;
            }
            let mut d = tr.translation - pos;
            d.y = 0.0;
            if d.length() <= v.radius {
                hp.current -= v.damage;
            }
        }
        v.waves_left -= 1;
        if v.waves_left == 0 {
            commands.entity(ve).despawn();
        } else {
            // −1 补栅栏：本帧已结算（next_in 从 0 起数），
            // 重置 interval−1 使波间隔恰为 interval 帧
            v.next_in = v.interval - 1;
        }
    }
}

/// 放置虚影：半透明胶囊 + Deploying 组件（不参与战斗/碰撞/索敌）
/// 半径按卡类别取：部队=体型，建筑=固定小方块
fn spawn_ghost(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
    spec: &CardSpec,
    pos: Vec3,
) {
    let r = match &spec.kind {
        CardKind::Troop(m) => m.radius,
        _ => BUILDING_RADIUS,
    };
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

/// 放置倒计时（帧同步链内）：虚影倒计时结束 → 按卡类别生成真实体
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
                let ground = Vec3::new(pos.x, 0.0, pos.z);
                match &spec.kind {
                    CardKind::Troop(ms) => spawn_unit(
                        &mut commands,
                        &mut meshes,
                        &mut materials,
                        d.faction,
                        d.card,
                        ms,
                        ground,
                    ),
                    CardKind::Building(bs) => spawn_building(
                        &mut commands,
                        &mut meshes,
                        &mut materials,
                        d.faction,
                        d.card,
                        bs,
                        ground,
                    ),
                    // 法术瞬发不产生虚影（play_card 直接结算），不会走到这里
                    CardKind::Spell(_) => {}
                }
            }
            commands.entity(e).despawn();
        }
    }
}

/// 生成怪物实体（play_card 部队落地 / 墓碑出兵共用）。
/// 机制全部由能力组件表达：Attacker/Targeting/Mover 必备，Charge/Flying 按卡挂
pub fn spawn_unit(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
    card: u8,
    spec: &MonsterSpec,
    pos: Vec3,
) {
    let r = spec.radius;
    // 飞行单位抬高（纯表现；模拟逻辑只在 xz 平面，y 不参与任何判定）
    let lift = if spec.flying { FLY_HEIGHT } else { 0.0 };
    // 胶囊按比例缩放：半径 r、圆柱段 2r，总高 4r
    let mut e = commands.spawn((
        Monster {
            faction,
            card,
            radius: r,
            mass: spec.mass,
        },
        Attacker {
            damage: spec.damage,
            attack_range: spec.attack_range,
            interval: spec.attack_interval,
            cooldown: spec.attack_interval,
            splash_radius: spec.splash_radius,
            hits_air: spec.hits_air,
            ranged: spec.ranged,
            target: None,
            engaged: false,
        },
        Targeting(TargetPolicy::Seek {
            aggro_range: spec.aggro_range,
            building_only: spec.building_only,
        }),
        Mover { speed: spec.speed },
        Health::new(spec.hp),
        Mesh3d(meshes.add(Capsule3d::new(r, 2.0 * r))),
        MeshMaterial3d(materials.add(unit_color(faction, spec))),
        Transform::from_translation(pos + Vec3::Y * (2.0 * r + lift)),
    ));
    // 冲锋（王子）
    if let Some(c) = &spec.charge {
        e.insert(Charge {
            progress: 0.0,
            windup: c.windup_secs,
            speed_mult: c.speed_mult,
            damage_mult: c.damage_mult,
        });
    }
    // 飞行
    if spec.flying {
        e.insert(Flying);
    }
    health_bar::spawn(
        &mut e,
        meshes,
        materials,
        2.0 * r,
        2.0 * r + 0.35 + lift,
        faction_color(faction),
    );
}

/// 建筑卡实体的碰撞半径（加农炮/墓碑共用小方块）
pub const BUILDING_RADIUS: f32 = 0.6;

/// 生成建筑实体（速度为 0 的特殊单位：可被索敌、有寿命，
/// 攻击/出兵/寿命分别由 Attacker/Spawner/Lifetime 能力组件表达）
fn spawn_building(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    faction: Faction,
    card: u8,
    spec: &BuildingSpec,
    pos: Vec3,
) {
    let r = BUILDING_RADIUS;
    let mut e = commands.spawn((
        BuildingCard {
            faction,
            card,
            radius: r,
        },
        Lifetime {
            secs: spec.lifetime_secs,
        },
        Targeting(TargetPolicy::Guard),
        Health::new(spec.hp),
        Mesh3d(meshes.add(Cuboid::new(2.0 * r, 1.4, 2.0 * r))),
        MeshMaterial3d(materials.add(faction_color(faction).darker(0.15))),
        Transform::from_translation(pos + Vec3::Y * 0.7),
    ));
    // 加农炮类攻击能力（冷却从 0 起：有敌即开火，之后按间隔）
    if let Some(a) = &spec.attack {
        e.insert(Attacker {
            damage: a.damage,
            attack_range: a.range,
            interval: a.interval,
            cooldown: 0.0,
            splash_radius: 0.0,
            hits_air: a.hits_air,
            ranged: true,
            target: None,
            engaged: false,
        });
    }
    // 墓碑类出兵能力（冷却从间隔起：落地 interval 秒后出第一只）
    if let Some(s) = &spec.spawner {
        e.insert(Spawner {
            interval: s.interval_secs,
            card_id: s.card_id,
            cooldown: s.interval_secs,
        });
    }
    health_bar::spawn(&mut e, meshes, materials, 1.4, 1.9, faction_color(faction));
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
        // 强制手牌 0 为骑士（部队）：牌池可能抽到法术/建筑，测试要确定性
        app.world_mut().resource_mut::<Decks>().player[0] = 0;
        app.init_resource::<Tick>();
        app.init_resource::<CommandBuffer>();
        app.init_resource::<CommandLog>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        let card_id = 0u8;
        let spec = &CARDS[card_id as usize];
        let CardKind::Troop(ms) = &spec.kind else {
            unreachable!("卡 0 是骑士")
        };
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
        let mut monsters = world.query::<(Entity, &Monster)>();
        let spawned: Vec<Entity> = monsters.iter(world).map(|(e, _m)| e).collect();
        assert_eq!(spawned.len(), spec.count as usize);
        let attacker = world.get::<Attacker>(spawned[0]).unwrap();
        assert_eq!(attacker.damage, ms.damage);
        assert_eq!(attacker.interval, ms.attack_interval);
        assert!(world.get::<Targeting>(spawned[0]).is_some());
        assert!(world.get::<Mover>(spawned[0]).is_some());
    }

    /// 牌池：21 选 8、同种子双方同池不同序、卡种不重复
    #[test]
    fn deck_pool_selects_distinct_shared_cards() {
        let pool = deck_pool(42);
        assert_eq!(pool.len(), DECK_SIZE);
        // 无重复
        let mut sorted = pool.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), DECK_SIZE);
        // 双方同池
        let decks = Decks::shuffled_with(42);
        let mut p = decks.player.clone();
        let mut e = decks.enemy.clone();
        p.sort();
        e.sort();
        assert_eq!(p, e, "双方必须共享同一 8 张牌池");
        // 同种子结果稳定
        assert_eq!(Decks::shuffled_with(42).player, decks.player);
        // 不同种子通常不同（固定一对已知不同的种子做回归锚）
        assert_ne!(Decks::shuffled_with(42).player, Decks::shuffled_with(7).player);
    }

    /// 法术出牌：瞬发结算伤害+晕眩，塔不吃法术，扣费+牌循环正常
    #[test]
    fn spell_card_damages_and_stuns_instantly() {
        let mut app = App::new();
        app.insert_resource(Elixir {
            player: ELIXIR_START,
            enemy: ELIXIR_START,
        });
        app.insert_resource(Decks::shuffled());
        // 强制手牌 0 为电击（Zap，id 15）：2 费、半径 1.2、伤 160、晕 0.5s
        app.world_mut().resource_mut::<Decks>().player[0] = 15;
        app.init_resource::<Tick>();
        app.init_resource::<CommandBuffer>();
        app.init_resource::<CommandLog>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        let world = app.world_mut();
        // 敌方怪在法术范围内（距离 1.0 < 1.2）
        let victim = world
            .spawn((
                Monster {
                    faction: Faction::Enemy,
                    card: 0,
                    radius: 0.5,
                    mass: 1.0,
                },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 5.0),
            ))
            .id();
        // 敌方塔在范围内：不吃法术
        let tower = world
            .spawn((
                Tower {
                    faction: Faction::Enemy,
                    radius: 1.0,
                },
                Health::new(6000.0),
                Transform::from_xyz(1.0, 0.0, 5.0),
            ))
            .id();
        // 范围外的己方怪（距离 5 > 1.2）：不掉血
        let bystander = world
            .spawn((
                Monster {
                    faction: Faction::Player,
                    card: 0,
                    radius: 0.5,
                    mass: 1.0,
                },
                Health::new(2000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        world.resource_mut::<CommandBuffer>().local.insert(
            0,
            vec![GameCommand::Deploy {
                faction: Faction::Player,
                card: 15,
                x: 0.0,
                z: 5.0, // 敌半场：法术全场可放
            }],
        );

        let mut schedule = Schedule::default();
        schedule.add_systems(crate::combat::apply_commands);
        schedule.run(world);

        // 扣 2 费 + 牌循环
        assert_eq!(world.resource::<Elixir>().player, ELIXIR_START - 2.0);
        assert_eq!(world.resource::<Decks>().player[DECK_SIZE - 1], 15);
        // 敌怪：掉血 + 被晕（晕眩晕打包进 Buffs，Stun 标记由 status_effects 同步）
        assert_eq!(world.get::<Health>(victim).unwrap().current, 2000.0 - 160.0);
        let buffs = world.get::<Buffs>(victim).unwrap();
        assert!(buffs.has_cc(CCFlags::STUN), "电击必须附带晕眩标志位");
        assert_eq!(buffs.list[0].secs, 0.5);
        assert!(matches!(buffs.list[0].policy, StackPolicy::Longer));
        drop(buffs);
        // 塔：不吃法术
        assert_eq!(world.get::<Health>(tower).unwrap().current, 6000.0);
        // 范围外：无伤
        assert_eq!(
            world.get::<Health>(bystander).unwrap().current,
            2000.0
        );
        // 法术瞬发：无虚影、无实体
        let mut deployers = world.query::<&Deploying>();
        assert_eq!(deployers.iter(world).count(), 0);
        let mut monsters = world.query::<&Monster>();
        assert_eq!(monsters.iter(world).count(), 2, "法术不应产生新单位");
    }

    /// 建筑卡出牌：己方半场生成虚影，落成建筑实体（可被索敌、有寿命）
    #[test]
    fn building_card_spawns_building_entity() {
        let mut app = App::new();
        app.insert_resource(Elixir {
            player: ELIXIR_START,
            enemy: ELIXIR_START,
        });
        app.insert_resource(Decks::shuffled());
        app.world_mut().resource_mut::<Decks>().player[0] = 19; // 加农炮
        app.init_resource::<Tick>();
        app.init_resource::<CommandBuffer>();
        app.init_resource::<CommandLog>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        let world = app.world_mut();
        world.resource_mut::<CommandBuffer>().local.insert(
            0,
            vec![GameCommand::Deploy {
                faction: Faction::Player,
                card: 19,
                x: 0.0,
                z: -5.0, // 己方半场
            }],
        );
        let mut schedule = Schedule::default();
        schedule.add_systems(crate::combat::apply_commands);
        schedule.run(world);
        let mut deployers = world.query::<&Deploying>();
        assert_eq!(deployers.iter(world).count(), 1);

        let mut schedule = Schedule::default();
        schedule.add_systems(process_deploying);
        for _ in 0..CARDS[19].deploy_ticks {
            schedule.run(world);
        }
        let mut buildings = world.query::<&BuildingCard>();
        let n = buildings.iter(world).count();
        assert_eq!(n, 1, "建筑落地应生成 BuildingCard 实体");
    }
}

#[cfg(test)]
mod zone_tests {
    use super::*;

    /// CR 部署区域规则：推掉哪侧公主塔，开放哪侧敌半场
    #[test]
    fn deploy_zone_expands_after_princess_falls() {
        // 敌方左公主塔活着、右塔已掉（快照里没有右塔）
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

    /// 按卡类别的区域规则：法术全场、建筑仅己方半场（不含河道）
    #[test]
    fn deploy_zone_depends_on_card_kind() {
        let towers: Vec<(Faction, bool, Vec3)> = vec![
            (Faction::Enemy, false, Vec3::new(-6.5, 0.0, 8.5)),
            (Faction::Enemy, true, Vec3::new(0.0, 0.0, 12.5)),
        ];
        let zap = &CARDS[15];
        let cannon = &CARDS[19];
        // 法术：全场任意（含河道与敌方半场）
        assert!(deploy_zone_ok(zap, Faction::Player, Vec3::new(2.0, 0.0, 5.0), &towers));
        assert!(deploy_zone_ok(zap, Faction::Player, Vec3::new(0.0, 0.0, 0.0), &towers));
        assert!(deploy_zone_ok(zap, Faction::Player, Vec3::new(0.0, 0.0, 13.5), &towers));
        // 建筑：己方半场可，河道/敌半场不可
        assert!(deploy_zone_ok(cannon, Faction::Player, Vec3::new(0.0, 0.0, -5.0), &towers));
        assert!(!deploy_zone_ok(cannon, Faction::Player, Vec3::new(0.0, 0.0, -1.0), &towers));
        assert!(!deploy_zone_ok(cannon, Faction::Player, Vec3::new(0.0, 0.0, 5.0), &towers));
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
        // 强制手牌 0 为骑士（部队）：法术全场可放，会绕过区域拒绝
        app.world_mut().resource_mut::<Decks>().player[0] = 0;
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
            },
            Health::new(6000.0),
            Transform::from_xyz(-6.5, 0.0, 8.5),
        ));

        let card_id = 0u8;
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

    /// 万箭出牌链路：扣 3 圣水、手牌循环离手、生成 SpellVolley 多波实体
    #[test]
    fn arrows_cost_elixir_and_spawn_volley() {
        let mut app = App::new();
        app.insert_resource(Elixir {
            player: 10.0,
            enemy: 10.0,
        });
        app.insert_resource(Decks::shuffled());
        // 手牌 0 强制为万箭（id 16），其余槽位用固定卡避免随机池重复干扰断言
        app.world_mut().resource_mut::<Decks>().player = vec![16, 0, 1, 2, 3, 4, 5, 6];
        app.init_resource::<Tick>();
        app.init_resource::<CommandBuffer>();
        app.init_resource::<CommandLog>();
        app.init_resource::<Assets<Mesh>>();
        app.init_resource::<Assets<StandardMaterial>>();

        app.world_mut()
            .resource_mut::<CommandBuffer>()
            .local
            .insert(
                0,
                vec![GameCommand::Deploy {
                    faction: Faction::Player,
                    card: 16,
                    x: 0.0,
                    z: -5.0,
                }],
            );

        let mut schedule = Schedule::default();
        schedule.add_systems(crate::combat::apply_commands);
        schedule.run(app.world_mut());

        assert_eq!(app.world().resource::<Elixir>().player, 7.0, "万箭应扣 3 圣水");
        let mut q = app.world_mut().query::<&SpellVolley>();
        assert_eq!(q.iter(app.world()).count(), 1, "应生成多波结算实体");
        assert!(
            !app.world().resource::<Decks>().player[..HAND_SIZE].contains(&16),
            "打出的牌应离开手牌（循环到队尾）"
        );
    }

    /// 万箭多波结算：施放后第 15/27/39 tick 各落一波（每波 1/3 伤害），
    /// 波数耗尽销毁；只打敌怪/敌建筑（塔不吃），己方与圈外不受影响
    #[test]
    fn arrows_volley_deals_three_waves_on_schedule() {
        let mut app = App::new();
        let world = app.world_mut();
        // 敌怪圈内 / 敌怪圈外 / 己方怪圈内
        let inside = world
            .spawn((
                crate::combat::test_monster(Faction::Enemy),
                Health::new(1000.0),
                Transform::from_xyz(1.0, 1.0, 0.0),
            ))
            .id();
        let outside = world
            .spawn((
                crate::combat::test_monster(Faction::Enemy),
                Health::new(1000.0),
                Transform::from_xyz(9.0, 1.0, 0.0),
            ))
            .id();
        let ally = world
            .spawn((
                crate::combat::test_monster(Faction::Player),
                Health::new(1000.0),
                Transform::from_xyz(0.0, 1.0, 0.0),
            ))
            .id();
        // 3 波 × 100，半径 2（模拟 play_card 的多波分支）
        world.spawn((
            SpellVolley {
                faction: Faction::Player,
                damage: 100.0,
                radius: 2.0,
                x: 0.0,
                z: 0.0,
                waves_left: 3,
                next_in: SPELL_WAVE_FIRST_TICKS,
                interval: SPELL_WAVE_INTERVAL_TICKS,
            },
            Transform::default(),
        ));

        let mut schedule = Schedule::default();
        schedule.add_systems(spell_volley_tick);
        // run k = 施放后第 k-1 tick（run 1 = tick 0，同帧首跑）
        let hp = |w: &mut World, e: Entity| w.get::<Health>(e).unwrap().current;
        let mut wave_tick = |n: usize| {
            // 三波分别在 tick 15 / 27 / 39（= run 16 / 28 / 40）
            for _ in 0..n {
                schedule.run(world);
            }
            hp(world, inside)
        };
        assert_eq!(wave_tick(15), 1000.0, "首波前（0..14 tick）不应掉血");
        assert_eq!(wave_tick(1), 900.0, "首波在施放后第 15 tick 落地");
        assert_eq!(wave_tick(11), 900.0, "波隔期间不应掉血");
        assert_eq!(wave_tick(1), 800.0, "第二波在 27 tick 落地");
        assert_eq!(wave_tick(11), 800.0, "波隔期间不应掉血");
        assert_eq!(wave_tick(1), 700.0, "第三波在 39 tick 落地");
        // 总量守恒：3 × 100 = 300
        assert_eq!(hp(world, outside), 1000.0, "圈外不吃伤害");
        assert_eq!(hp(world, ally), 1000.0, "己方不吃伤害");
        // 波数耗尽：实体销毁（命令在 run 结束后应用）
        schedule.run(world);
        let mut q = world.query::<&SpellVolley>();
        assert_eq!(q.iter(world).count(), 0, "波数耗尽后实体应销毁");
    }
}
