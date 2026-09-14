//! 圣水：回复、UI 生成与刷新

use bevy::prelude::*;

use crate::components::{Elixir, ElixirFill, ElixirMultiplier, ElixirText, Faction};
use crate::constants::{ELIXIR_MAX, ELIXIR_PER_SEC, ELIXIR_START, TICK_DT};
use crate::match_flow::{elixir_multiplier, MatchTimer};
use crate::net::NetClient;

/// 圣水自动回复（双方同速，固定步长保证确定性，倍数随对局阶段变化）
pub fn regen(mut elixir: ResMut<Elixir>, timer: Res<MatchTimer>) {
    let rate = ELIXIR_PER_SEC * elixir_multiplier(&timer) * TICK_DT;
    elixir.player = (elixir.player + rate).min(ELIXIR_MAX);
    elixir.enemy = (elixir.enemy + rate).min(ELIXIR_MAX);
}

/// 圣水条 UI：底部横条 + 数值
pub fn setup_ui(mut commands: Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                bottom: Val::Px(12.0),
                left: Val::Px(12.0),
                right: Val::Px(12.0),
                height: Val::Px(26.0),
                ..default()
            },
            BackgroundColor(Color::srgb(0.08, 0.08, 0.12)),
        ))
        .with_children(|p| {
            // 填充条
            p.spawn((
                ElixirFill,
                Node {
                    width: Val::Percent(ELIXIR_START / ELIXIR_MAX * 100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                BackgroundColor(Color::srgb(0.85, 0.25, 0.95)),
            ));
            // 数值文本
            p.spawn((
                ElixirText,
                Text::new(format!("{}", ELIXIR_START as i32)),
                TextFont {
                    font_size: FontSize::Px(18.0),
                    ..default()
                },
                TextColor(Color::WHITE),
                Node {
                    position_type: PositionType::Absolute,
                    right: Val::Px(8.0),
                    top: Val::Px(3.0),
                    ..default()
                },
            ));
            // 倍数指示（双倍/三倍圣水时显示）
            p.spawn((
                ElixirMultiplier,
                Text::new(""),
                TextFont {
                    font_size: FontSize::Px(14.0),
                    ..default()
                },
                TextColor(Color::srgb(1.0, 0.8, 0.2)),
                Node {
                    position_type: PositionType::Absolute,
                    left: Val::Px(6.0),
                    top: Val::Px(5.0),
                    ..default()
                },
            ));
        });
}

/// 圣水 UI 刷新：显示本方（联网按服务器序号，单机默认蓝方）的填充条 + 数值 + 倍数
pub fn update_ui(
    elixir: Res<Elixir>,
    timer: Res<MatchTimer>,
    net: Option<Res<NetClient>>,
    mut fills: Query<&mut Node, With<ElixirFill>>,
    mut texts: Query<&mut Text, With<ElixirText>>,
    mut mults: Query<&mut Text, (With<ElixirMultiplier>, Without<ElixirText>)>,
) {
    let mine = match net.and_then(|n| Faction::from_index(n.my_index)) {
        Some(Faction::Enemy) => elixir.enemy,
        _ => elixir.player,
    };
    for mut node in &mut fills {
        node.width = Val::Percent(mine / ELIXIR_MAX * 100.0);
    }
    for mut text in &mut texts {
        text.0 = format!("{}", mine.floor() as i32);
    }
    let mult = elixir_multiplier(&timer);
    for mut text in &mut mults {
        text.0 = if mult > 1.0 {
            format!("x{}", mult as i32)
        } else {
            String::new()
        };
    }
}
