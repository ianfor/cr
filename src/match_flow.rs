//! 对局流程：倒计时、圣水倍数、加时/拼血阶段、结算界面（含特效）

use bevy::prelude::*;

use crate::components::{faction_color, Faction, Health, Tower};
use crate::constants::*;
use crate::net::{NetClient, SimState};

/// 对局阶段
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchPhase {
    /// 常规 3 分钟（最后 1 分钟双倍圣水）
    Regular,
    /// 加时 2 分钟（三倍圣水，任意掉塔即负）
    Overtime,
    /// 拼血：加时结束仍平局，所有塔持续掉血，先掉塔者输
    Drain,
}

/// 对局计时器（帧驱动，确定性）
#[derive(Resource)]
pub struct MatchTimer {
    pub phase: MatchPhase,
    pub ticks_left: u32,
}

impl Default for MatchTimer {
    fn default() -> Self {
        Self {
            phase: MatchPhase::Regular,
            ticks_left: REGULAR_TICKS,
        }
    }
}

/// 当前圣水回复倍数
pub fn elixir_multiplier(timer: &MatchTimer) -> f32 {
    match timer.phase {
        MatchPhase::Regular => {
            if timer.ticks_left <= DOUBLE_ELIXIR_TICKS {
                2.0
            } else {
                1.0
            }
        }
        MatchPhase::Overtime | MatchPhase::Drain => 3.0,
    }
}

/// 倒计时 UI 文本
#[derive(Component)]
pub struct CountdownText;

/// 结算文字弹出动画
#[derive(Component)]
pub struct ResultPop {
    t: f32,
}

/// 烟花粒子（结算界面持续燃放）
#[derive(Component)]
pub struct Firework {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    max_life: f32,
    color: Color,
}

/// 烟花发射器：insert 后每 0.6 秒自动放一发（一直放）
#[derive(Resource)]
pub struct FireworksActive {
    timer: f32,
    seed: u32,
}

/// 计时推进（FixedUpdate 链内，check_game_over 之前）：
/// 常规到点比塔数 → 加时 → 拼血（所有塔持续掉血）
pub fn tick_timer(
    mut commands: Commands,
    mut timer: ResMut<MatchTimer>,
    mut state: ResMut<SimState>,
    mut towers: Query<(&Tower, &mut Health)>,
    net: Option<Res<NetClient>>,
) {
    match timer.phase {
        MatchPhase::Regular => {
            if timer.ticks_left == DOUBLE_ELIXIR_TICKS {
                info!("双倍圣水时间");
            }
            timer.ticks_left -= 1;
            if timer.ticks_left == 0 {
                // 比剩余塔数，多者直接获胜
                let mut counts = (0u32, 0u32);
                for (t, _) in towers.iter() {
                    match t.faction {
                        Faction::Player => counts.0 += 1,
                        Faction::Enemy => counts.1 += 1,
                    }
                }
                if counts.0 != counts.1 {
                    let winner = if counts.0 > counts.1 {
                        Faction::Player
                    } else {
                        Faction::Enemy
                    };
                    info!("常规时间结束：{:?} 破塔数获胜", winner);
                    *state = SimState::GameOver(Some(winner));
                    let my = net.and_then(|n| Faction::from_index(n.my_index));
                    spawn_result_ui(&mut commands, Some(winner), my);
                } else {
                    timer.phase = MatchPhase::Overtime;
                    timer.ticks_left = OVERTIME_TICKS;
                    info!("加时赛！三倍圣水，任意掉塔即负");
                }
            }
        }
        MatchPhase::Overtime => {
            timer.ticks_left -= 1;
            if timer.ticks_left == 0 {
                timer.phase = MatchPhase::Drain;
                info!("拼血阶段：所有塔持续掉血，先掉塔者输！");
            }
        }
        MatchPhase::Drain => {
            for (_, mut hp) in &mut towers {
                hp.current -= DRAIN_PER_TICK;
            }
        }
    }
}

/// 结算界面：暗色遮罩 + 弹出大字 + 胜利烟花
/// winner=None 为平局；my=None 为单机（显示阵营名）
/// 注：内置字体无中文字形，UI 文字一律用英文
pub fn spawn_result_ui(commands: &mut Commands, winner: Option<Faction>, my: Option<Faction>) {
    let (msg, color, celebrate) = match (winner, my) {
        (None, _) => ("DRAW", Color::WHITE, false),
        (Some(w), Some(m)) => {
            if w == m {
                ("VICTORY", Color::srgb(1.0, 0.85, 0.2), true)
            } else {
                ("DEFEAT", Color::srgb(0.75, 0.75, 0.8), false)
            }
        }
        // 单机：显示阵营名
        (Some(w), None) => {
            let name = match w {
                Faction::Player => "BLUE WINS!",
                Faction::Enemy => "RED WINS!",
            };
            (name, faction_color(w), true)
        }
    };

    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.65)),
        ))
        .with_children(|p| {
            p.spawn((
                Text::new(msg),
                TextFont {
                    font_size: FontSize::Px(40.0), // 由 ResultPop 动画到 96→80
                    ..default()
                },
                TextColor(color),
                ResultPop { t: 0.0 },
            ));
        });

    // 胜利方开始放烟花（本地表现层，与模拟无关，持续到关游戏）
    if celebrate {
        commands.insert_resource(FireworksActive {
            timer: 0.0,
            seed: 0,
        });
    }
}

/// 结算文字弹出动画：40 → 96（过冲）→ 80
pub fn result_pop(time: Res<Time>, mut q: Query<(&mut TextFont, &mut ResultPop)>) {
    const DURATION: f32 = 0.35;
    for (mut font, mut pop) in &mut q {
        pop.t += time.delta_secs();
        let s = (pop.t / DURATION).min(1.0);
        let size = if s < 0.7 {
            40.0 + (96.0 - 40.0) * (s / 0.7)
        } else {
            96.0 - (96.0 - 80.0) * ((s - 0.7) / 0.3)
        };
        font.font_size = FontSize::Px(size);
    }
}

/// 烟花发射：每 0.6 秒在随机位置放一发（16 颗粒子径向爆开）
pub fn fireworks_spawn(
    mut commands: Commands,
    time: Res<Time>,
    spawner: Option<ResMut<FireworksActive>>,
) {
    let Some(mut spawner) = spawner else { return };
    spawner.timer -= time.delta_secs();
    if spawner.timer > 0.0 {
        return;
    }
    spawner.timer = 0.6;
    spawner.seed += 1;
    let seed = spawner.seed;

    let cx = 60.0 + prand(seed) * 420.0;
    let cy = 60.0 + prand(seed.wrapping_mul(7)) * 420.0;
    let color = firework_color(seed);
    const N: u32 = 16;
    for i in 0..N {
        let angle = i as f32 / N as f32 * std::f32::consts::TAU;
        let speed = 150.0 * (0.7 + 0.6 * prand(seed.wrapping_mul(31).wrapping_add(i)));
        commands.spawn((
            Firework {
                x: cx,
                y: cy,
                vx: angle.cos() * speed,
                vy: angle.sin() * speed,
                life: 1.2,
                max_life: 1.2,
                color,
            },
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(cx),
                top: Val::Px(cy),
                width: Val::Px(6.0),
                height: Val::Px(6.0),
                ..default()
            },
            BackgroundColor(color),
        ));
    }
}

/// 烟花粒子飞行：重力 + 空气阻力 + 渐隐，寿命结束销毁
pub fn fireworks_fly(
    mut commands: Commands,
    time: Res<Time>,
    mut q: Query<(Entity, &mut Node, &mut BackgroundColor, &mut Firework)>,
) {
    for (e, mut node, mut bg, mut f) in &mut q {
        f.life -= time.delta_secs();
        if f.life <= 0.0 {
            commands.entity(e).despawn();
            continue;
        }
        let dt = time.delta_secs();
        f.vy += 260.0 * dt; // 重力
        f.vx *= 1.0 - 0.6 * dt; // 阻力
        f.vy *= 1.0 - 0.4 * dt;
        f.x += f.vx * dt;
        f.y += f.vy * dt;
        node.left = Val::Px(f.x);
        node.top = Val::Px(f.y);
        let alpha = (f.life / f.max_life).clamp(0.0, 1.0);
        *bg = BackgroundColor(f.color.with_alpha(alpha));
    }
}

/// 顶部中央倒计时 UI
pub fn setup_timer_ui(mut commands: Commands) {
    commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            top: Val::Px(8.0),
            left: Val::Px(0.0),
            width: Val::Percent(100.0),
            justify_content: JustifyContent::Center,
            ..default()
        })
        .with_children(|p| {
            p.spawn((
                CountdownText,
                Text::new("3:00"),
                TextFont {
                    font_size: FontSize::Px(26.0),
                    ..default()
                },
                TextColor(Color::WHITE),
            ));
        });
}

/// 倒计时刷新（表现层）
pub fn update_countdown(timer: Res<MatchTimer>, mut q: Query<&mut Text, With<CountdownText>>) {
    for mut t in &mut q {
        t.0 = match timer.phase {
            MatchPhase::Drain => "DRAIN!".into(),
            _ => {
                let secs = timer.ticks_left / TICKS_PER_SEC as u32;
                format!("{}:{:02}", secs / 60, secs % 60)
            }
        };
    }
}

/// 确定性伪随机（烟花用，表现层无需联网一致）
fn prand(seed: u32) -> f32 {
    let h = seed.wrapping_mul(2654435761) ^ 0x9E3779B9;
    (h % 1000) as f32 / 1000.0
}

fn firework_color(seed: u32) -> Color {
    match seed % 4 {
        0 => Color::srgb(1.0, 0.85, 0.2),  // 金
        1 => Color::srgb(1.0, 1.0, 1.0),   // 白
        2 => Color::srgb(0.4, 0.7, 1.0),   // 蓝
        _ => Color::srgb(1.0, 0.45, 0.45), // 红
    }
}
