pub mod arena;
pub mod bot;
pub mod cards;
pub mod combat;
pub mod components;
pub mod constants;
pub mod deploy_zone;
pub mod elixir;
pub mod health_bar;
pub mod match_flow;
pub mod net;
pub mod protocol;
pub mod replay;
pub mod sim_env;

use bevy::prelude::*;

use crate::bot::{BotMode, BotPolicy, BotState};
use crate::cards::SelectedCard;
use crate::components::{CommandBuffer, CommandLog, Decks, Elixir, PendingClicks, Tick};
use crate::constants::{ELIXIR_START, TICKS_PER_SEC};
use crate::match_flow::MatchTimer;
use crate::net::{OwnHashes, SimState};
use crate::replay::{ReplayControl, ReplayLog, ReplayMode, SimTick};

/// 玩家身份 token：持久化到 player_token.txt，断线重连凭它认领座位
/// 同机开多个客户端测试时用 PLAYER_TOKEN 环境变量区分
fn player_token() -> u64 {
    if let Ok(t) = std::env::var("PLAYER_TOKEN") {
        if let Ok(t) = t.parse() {
            return t;
        }
    }
    if let Ok(s) = std::fs::read_to_string("player_token.txt") {
        if let Ok(t) = s.trim().parse() {
            return t;
        }
    }
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        ^ (std::process::id() as u64) << 32;
    let _ = std::fs::write("player_token.txt", t.to_string());
    t
}

/// 组装并启动游戏 App：PC 的 main 与 Android 的 example 共用此入口。
/// 环境变量（REPLAY/BOT_MODEL/RELAY_ADDR/ROOM）在 Android 上均不存在，
/// 走默认值 → 连不上中继自动进单机模式
pub fn run_app() {
    use bevy::prelude::*;


    // 回放模式：REPLAY=replays/xxx.cr 时加载录像，不联网
    let replay_path = std::env::var("REPLAY").ok();
    let replay_log = replay_path
        .as_deref()
        .and_then(replay::load_replay_file);

    // PvE 模式：BOT_MODEL=<导出的权重JSON> 时机器人执红（玩家锁蓝方）
    let bot_policy = std::env::var("BOT_MODEL")
        .ok()
        .and_then(|path| {
            let p = BotPolicy::load(&path);
            if p.is_some() {
                println!("PvE 模式：机器人已加载 {path}（你执蓝方）");
            } else {
                println!("BOT_MODEL={path} 加载失败，进入普通单机");
            }
            p
        });

    // 连接中继服务器：默认 127.0.0.1:9700、房间 1，用 RELAY_ADDR / ROOM 环境变量覆盖
    // 连不上就进入单机模式（无屏障，点哪边半场出哪边的怪）
    let (net_client, sim_state) = if replay_log.is_some() {
        println!("回放模式：{}", replay_path.as_deref().unwrap());
        (None, SimState::Replaying)
    } else if bot_policy.is_some() {
        // PvE 不联网
        (None, SimState::Solo)
    } else {
        let addr = std::env::var("RELAY_ADDR").unwrap_or_else(|_| "127.0.0.1:9700".into());
        let room: u32 = std::env::var("ROOM")
            .ok()
            .and_then(|r| r.parse().ok())
            .unwrap_or(1);
        let client = net::connect(&addr, room, player_token());
        let state = if client.is_some() {
            println!("已连接中继 {addr} 房间 {room}，等待对手…");
            SimState::Waiting
        } else {
            println!("未连上中继 {addr}，进入单机模式");
            SimState::Solo
        };
        (client, state)
    };

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Bevy Arena".into(),
            // 竖屏，接近皇室战争的构图
            resolution: (540, 960).into(),
            ..default()
        }),
        ..default()
    }))
    .insert_resource(Elixir {
        player: ELIXIR_START,
        enemy: ELIXIR_START,
    })
    .insert_resource(sim_state)
    .insert_resource(Decks::shuffled())
    .init_resource::<SelectedCard>()
    .init_resource::<Tick>()
    .init_resource::<PendingClicks>()
    .init_resource::<CommandBuffer>()
    .init_resource::<CommandLog>()
    .init_resource::<OwnHashes>()
    .init_resource::<MatchTimer>()
    .init_resource::<ReplayLog>()
    .init_resource::<ReplayControl>()
    .init_resource::<combat::ProjectileAssets>()
    // 全场单位快照：targeting 每帧写入，attacking/moving 读取
    .init_resource::<combat::WorldSnaps>()
    // 帧同步：固定 30Hz 模拟帧率
    .insert_resource(Time::<Fixed>::from_hz(TICKS_PER_SEC))
    .add_systems(
        Startup,
        (
            arena::setup,
            elixir::setup_ui,
            match_flow::setup_timer_ui,
            cards::setup_ui,
            deploy_zone::setup,
        ),
    )
    // 输入采集与网络收发（渲染帧率）
    .add_systems(Update, combat::gather_input)
    .add_systems(Update, net::receive)
    .add_systems(Update, arena::flip_camera_for_enemy)
    .add_systems(Update, cards::select_card_input)
    .add_systems(Update, bot::bot_think)
    .add_systems(Update, replay::replay_input)
    .add_systems(Update, replay::save_replay_on_game_over)
    // 法术施法特效（纯表现层）
    .add_systems(Update, (combat::spell_fx_spawn, combat::spell_fx_update).chain())
    // 确定性模拟链挂在 SimTick：实时由 drive_sim 按 30Hz 驱动，
    // 追帧/回放由 drive_replay 连续驱动
    .add_systems(
        SimTick,
        (
            combat::collect_inputs,
            combat::apply_commands,
            cards::process_deploying,
            combat::status_effects,
            combat::targeting,
            combat::attacking,
            combat::moving,
            combat::building_lifetime,
            combat::building_spawner,
            combat::move_projectiles,
            combat::separate_monsters,
            combat::separate_from_statics,
            combat::keep_out_of_river,
            combat::despawn_dead,
            match_flow::tick_timer,
            combat::check_game_over,
            net::notify_game_over,
            elixir::regen,
            net::send_hash,
            combat::advance_tick,
        )
            .chain(),
    )
    .add_systems(FixedUpdate, replay::drive_sim)
    .add_systems(Update, replay::drive_replay)
    // 表现层（渲染帧率）：血条、圣水 UI、卡牌 UI、部署区域、倒计时、结算特效
    .add_systems(
        Update,
        (
            health_bar::face_camera,
            health_bar::update,
            elixir::update_ui,
            cards::update_card_ui,
            deploy_zone::update,
            match_flow::update_countdown,
            match_flow::result_pop,
            match_flow::fireworks_spawn,
            match_flow::fireworks_fly,
        ),
    );

    if let Some(client) = net_client {
        app.insert_resource(client);
    }
    if let Some(policy) = bot_policy {
        app.insert_resource(BotMode)
            .insert_resource(policy)
            .init_resource::<BotState>();
    }
    if replay_log.is_some() {
        app.insert_resource(replay_log.unwrap())
            .insert_resource(ReplayMode);
    }

    app.run();
}
