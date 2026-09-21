//! 追帧 / 重连 / 录像回放
//!
//! 核心原理：对局状态 = 初始状态 + 全部指令（确定性模拟）。
//! 追帧 = 以最高速度重放指令日志；录像回放 = 追帧 + 播放控制。

use std::collections::{BTreeMap, HashMap};

use bevy::ecs::schedule::ScheduleLabel;
use bevy::prelude::*;

use crate::components::*;
use crate::constants::*;
use crate::match_flow::{FireworksActive, MatchTimer};
use crate::net::{command_to_wire, wire_to_command, NetClient, OwnHashes, SimState};
use crate::protocol::{LogEntry, ReplayFile};

/// 回放模式标记资源（main 在 REPLAY 模式下插入；存在时不保存录像、不走联网流程）
#[derive(Resource)]
pub struct ReplayMode;

/// SimTick 调度标签：整条确定性模拟链挂在这里，只由驱动系统手动运行
#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimTick;

/// 追帧数据源：帧号 → 该帧的指令（来自重连 History 或录像文件）
#[derive(Resource, Default)]
pub struct ReplayLog {
    pub map: HashMap<u32, Vec<GameCommand>>,
    /// 追帧目标帧号（重连 = 对局当前帧，回放 = 录像结束帧）
    pub target: u32,
}

/// 回放播放控制（本地表现状态）
#[derive(Resource)]
pub struct ReplayControl {
    pub paused: bool,
    /// 每渲染帧推进的模拟帧数（倍速）
    pub speed: u32,
    /// seek 目标帧（回退 seek 会先重置世界再重追）
    pub seek_to: Option<u32>,
}

impl Default for ReplayControl {
    fn default() -> Self {
        Self {
            paused: false,
            speed: 1,
            seek_to: None,
        }
    }
}

/// 实时驱动（FixedUpdate，30Hz）：过屏障后运行一帧模拟
pub fn drive_sim(world: &mut World) {
    let ready = {
        let state = world.resource::<SimState>();
        let buffer = world.resource::<CommandBuffer>();
        let tick = world.resource::<Tick>();
        let net = world.get_resource::<NetClient>();
        crate::net::sim_ready(state, buffer, tick, net)
    };
    if ready {
        let _ = world.try_run_schedule(SimTick);
    }
}

/// 把日志中当前帧的指令喂进指令缓冲（喂到 remote 槽位，apply_commands 统一消费）
fn feed_log_tick(world: &mut World) {
    let tick = world.resource::<Tick>().0;
    let cmds = world
        .resource::<ReplayLog>()
        .map
        .get(&tick)
        .cloned()
        .unwrap_or_default();
    if !cmds.is_empty() {
        world
            .resource_mut::<CommandBuffer>()
            .remote
            .insert(tick, cmds);
    }
}

/// 追帧/回放驱动（Update）：
/// - CatchingUp：每渲染帧最多追 600 tick，追平目标后回到 Playing
/// - Replaying：按 speed 推进；seek 回退时先重置世界再重追
pub fn drive_replay(world: &mut World) {
    match *world.resource::<SimState>() {
        SimState::CatchingUp => {
            for _ in 0..600 {
                if world.resource::<Tick>().0 >= world.resource::<ReplayLog>().target {
                    break;
                }
                feed_log_tick(world);
                let _ = world.try_run_schedule(SimTick);
                // 追帧途中可能赶上对局结束
                if matches!(world.resource::<SimState>(), SimState::GameOver(_)) {
                    return;
                }
            }
            if world.resource::<Tick>().0 >= world.resource::<ReplayLog>().target {
                *world.resource_mut::<SimState>() = SimState::Playing;
                info!("追帧完成，恢复实时对局");
            }
        }
        SimState::Replaying => {
            // seek：目标帧在过去 → 重置世界；然后正常快进到目标帧
            if let Some(seek) = world.resource_mut::<ReplayControl>().seek_to.take() {
                if seek < world.resource::<Tick>().0 {
                    reset_world(world);
                }
                // 快进部分由下面的正常推进完成：临时把 paused 打开逐帧走完
                let end = world.resource::<ReplayLog>().target.min(seek);
                while world.resource::<Tick>().0 < end {
                    feed_log_tick(world);
                    let _ = world.try_run_schedule(SimTick);
                }
            }
            let (paused, speed) = {
                let c = world.resource::<ReplayControl>();
                (c.paused, c.speed)
            };
            if !paused {
                for _ in 0..speed {
                    if world.resource::<Tick>().0 >= world.resource::<ReplayLog>().target {
                        world.resource_mut::<ReplayControl>().paused = true;
                        info!("回放结束");
                        break;
                    }
                    feed_log_tick(world);
                    let _ = world.try_run_schedule(SimTick);
                    if matches!(world.resource::<SimState>(), SimState::GameOver(_)) {
                        world.resource_mut::<ReplayControl>().paused = true;
                        break;
                    }
                }
            }
        }
        _ => {}
    }
}

/// 重置世界到初始状态（seek 回退用）：
/// 清空游戏实体与模拟资源，重跑 Startup 重建场景，然后由驱动重新追帧
/// 注意：不能全量 despawn——0.19 的资源寄存在内部实体上，只清带 Transform/Node 的
pub fn reset_world(world: &mut World) {
    let mut q = world.query_filtered::<Entity, Or<(With<Transform>, With<Node>)>>();
    let entities: Vec<Entity> = q.iter(world).collect();
    for e in entities {
        // 父实体递归销毁后子实体可能已不存在，先检查避免告警
        if world.get_entity(e).is_ok() {
            let _ = world.despawn(e);
        }
    }
    *world.resource_mut::<Tick>() = Tick(0);
    *world.resource_mut::<Elixir>() = Elixir {
        player: ELIXIR_START,
        enemy: ELIXIR_START,
    };
    *world.resource_mut::<Decks>() = Decks::shuffled();
    *world.resource_mut::<CommandBuffer>() = CommandBuffer::default();
    *world.resource_mut::<MatchTimer>() = MatchTimer::default();
    *world.resource_mut::<OwnHashes>() = OwnHashes::default();
    *world.resource_mut::<CommandLog>() = CommandLog::default();
    world.remove_resource::<FireworksActive>();
    let _ = world.try_run_schedule(Startup);
}

/// 回放按键：空格暂停/继续，1/2/4/8 倍速，左右方向键 ±10 秒 seek
pub fn replay_input(
    keys: Res<ButtonInput<KeyCode>>,
    state: Res<SimState>,
    tick: Res<Tick>,
    mut control: ResMut<ReplayControl>,
) {
    if !matches!(*state, SimState::Replaying) {
        return;
    }
    if keys.just_pressed(KeyCode::Space) {
        control.paused = !control.paused;
    }
    for (code, speed) in [
        (KeyCode::Digit1, 1),
        (KeyCode::Digit2, 2),
        (KeyCode::Digit4, 4),
        (KeyCode::Digit8, 8),
    ] {
        if keys.just_pressed(code) {
            control.speed = speed;
            control.paused = false;
        }
    }
    if keys.just_pressed(KeyCode::ArrowRight) {
        control.seek_to = Some(tick.0 + 300);
    }
    if keys.just_pressed(KeyCode::ArrowLeft) {
        control.seek_to = Some(tick.0.saturating_sub(300));
    }
}

/// 对局结束自动保存录像（回放模式下不重复保存）
pub fn save_replay_on_game_over(
    state: Res<SimState>,
    tick: Res<Tick>,
    log: Res<CommandLog>,
    replay_mode: Option<Res<ReplayMode>>,
    mut saved: Local<bool>,
) {
    if *saved || replay_mode.is_some() || !matches!(*state, SimState::GameOver(_)) {
        return;
    }
    *saved = true;

    // (tick, cmd) 流水按帧分组成 LogEntry
    let mut grouped: BTreeMap<u32, Vec<crate::protocol::CommandWire>> = BTreeMap::new();
    for (t, cmd) in &log.0 {
        grouped
            .entry(*t)
            .or_default()
            .push(command_to_wire(*cmd));
    }
    let entries: Vec<LogEntry> = grouped
        .into_iter()
        .map(|(t, cmds)| LogEntry {
            tick: t,
            from: 0, // 指令自身带阵营，from 仅作冗余
            cmds,
        })
        .collect();
    let file = ReplayFile {
        version: SIM_VERSION,
        end_tick: tick.0,
        entries,
    };
    let _ = std::fs::create_dir_all("replays");
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = format!("replays/replay_{millis}.cr");
    match bincode::serialize(&file) {
        Ok(bytes) => {
            if std::fs::write(&path, bytes).is_ok() {
                info!("录像已保存：{path}（{} 条指令）", file.entries.len());
            }
        }
        Err(e) => error!("录像保存失败：{e}"),
    }
}

/// 加载录像文件 → 追帧数据源
/// 版本不匹配的录像仍然加载，但明确警告结果可能失真
pub fn load_replay_file(path: &str) -> Option<ReplayLog> {
    let bytes = std::fs::read(path).ok()?;
    let file: ReplayFile = bincode::deserialize(&bytes).ok()?;
    if file.version != SIM_VERSION {
        eprintln!(
            "警告：录像版本不匹配（文件 v{}，当前模拟 v{}），回放结果可能失真！",
            file.version, SIM_VERSION
        );
    }
    let mut map = HashMap::new();
    for e in file.entries {
        let cmds: Vec<GameCommand> = e.cmds.into_iter().filter_map(wire_to_command).collect();
        map.insert(e.tick, cmds);
    }
    Some(ReplayLog {
        map,
        target: file.end_tick,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{arena, cards, combat, elixir, match_flow, net};

    /// 搭一个和 main 相同装配的无头 App
    fn build_app() -> App {
        let mut app = App::new();
        app.insert_resource(SimState::Replaying)
            .insert_resource(Elixir {
                player: ELIXIR_START,
                enemy: ELIXIR_START,
            })
            .insert_resource(Decks::shuffled())
            .init_resource::<Tick>()
            .init_resource::<PendingClicks>()
            .init_resource::<CommandBuffer>()
            .init_resource::<CommandLog>()
            .init_resource::<OwnHashes>()
            .init_resource::<MatchTimer>()
            .init_resource::<ReplayLog>()
            .init_resource::<ReplayControl>()
            .init_resource::<combat::ProjectileAssets>()
            .init_resource::<combat::WorldSnaps>()
            .init_resource::<Assets<Mesh>>()
            .init_resource::<Assets<StandardMaterial>>()
            .add_systems(
                Startup,
                (
                    arena::setup,
                    elixir::setup_ui,
                    match_flow::setup_timer_ui,
                    cards::setup_ui,
                ),
            )
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
            );
        app
    }

    fn run_ticks(app: &mut App, n: u32) {
        for _ in 0..n {
            feed_log_tick(app.world_mut());
            let _ = app.world_mut().try_run_schedule(SimTick);
        }
    }

    /// 同一指令流跑两遍（第二遍先 reset_world），最终状态哈希必须一致
    #[test]
    fn replay_is_deterministic() {
        let mut app = build_app();
        let _ = app.world_mut().try_run_schedule(Startup);

        // 第一遍：指令放日志里喂入，模拟同时记录 CommandLog
        {
            let mut rl = app.world_mut().resource_mut::<ReplayLog>();
            rl.map.insert(
                5,
                vec![GameCommand::Deploy {
                    faction: Faction::Player,
                    card: 0,
                    x: 0.0,
                    z: -5.0,
                }],
            );
            rl.map.insert(
                10,
                vec![GameCommand::Deploy {
                    faction: Faction::Enemy,
                    card: 0,
                    x: 0.0,
                    z: 5.0,
                }],
            );
            rl.map.insert(
                50,
                vec![GameCommand::Deploy {
                    faction: Faction::Player,
                    card: 1,
                    x: -3.0,
                    z: -6.0,
                }],
            );
        }
        run_ticks(&mut app, 300);
        let h1 = app.world().resource::<OwnHashes>().0[&299];

        // CommandLog → 回放日志（必须在 reset 前取，reset 会清空）
        {
            let mut map = HashMap::new();
            for (t, cmd) in &app.world().resource::<CommandLog>().0 {
                map.entry(*t).or_insert_with(Vec::new).push(*cmd);
            }
            let mut rl = app.world_mut().resource_mut::<ReplayLog>();
            rl.map = map;
            rl.target = 300;
        }

        // 重置世界并重放
        reset_world(app.world_mut());
        run_ticks(&mut app, 300);
        let h2 = app.world().resource::<OwnHashes>().0[&299];

        assert_eq!(h1, h2, "回放结果与实时模拟不一致：帧同步确定性被破坏");
    }
}
