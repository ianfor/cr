//! 客户端网络层：连接中继、收发报文、帧同步屏障、防失同步校验

use std::collections::HashMap;
use std::net::TcpStream;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread;

use bevy::prelude::*;

use crate::components::{
    CommandBuffer, Decks, Elixir, Faction, GameCommand, Health, Monster, Tick, Tower,
};
use crate::constants::INPUT_DELAY;
use crate::protocol::{read_msg, write_msg, ClientMsg, CommandWire, ServerMsg};

/// 对局状态
#[derive(Resource, PartialEq, Eq, Clone, Copy, Debug)]
pub enum SimState {
    /// 单机（未连中继）：无屏障，指令本地延迟执行
    Solo,
    /// 已连中继，等待对手进房
    Waiting,
    /// 对局进行中
    Playing,
    /// 断线重连后追帧中：以最高速度重放指令日志，追平后回到 Playing
    CatchingUp,
    /// 录像回放模式
    Replaying,
    /// 对局结束（Some = 胜利方，None = 平局）：模拟停止
    GameOver(Option<Faction>),
}

/// 网络会话（仅联网模式存在该资源）
#[derive(Resource)]
pub struct NetClient {
    /// 服务器分配的玩家序号：0 = 蓝方，1 = 红方
    pub my_index: u8,
    /// 对手是否在实时发报文。false 时其缺失的帧按空指令处理（对局不停）
    pub opponent_live: bool,
    /// 对手重连后恢复发报文的起始帧号：小于此帧号的缺失帧按空指令处理
    pub live_resume_stamp: Option<u32>,
    incoming: Mutex<Receiver<ServerMsg>>, // Receiver 不是 Sync，包一层
    outgoing: Sender<ClientMsg>,
}

/// 己方历史状态哈希（防失同步比对用）
#[derive(Resource, Default)]
pub struct OwnHashes(pub HashMap<u32, u32>);

/// 连接中继服务器并加入房间；失败返回 None（单机模式）
/// token：客户端身份标识，断线重连时凭它认领座位
pub fn connect(addr: &str, room: u32, token: u64) -> Option<NetClient> {
    let mut stream = TcpStream::connect(addr).ok()?;
    stream.set_nodelay(true).ok()?;
    write_msg(&mut stream, &ClientMsg::Join { room, token }).ok()?;

    // 读线程：报文 → incoming 通道
    let (tx_in, rx_in) = channel::<ServerMsg>();
    let mut reader = stream.try_clone().ok()?;
    thread::spawn(move || loop {
        match read_msg::<ServerMsg>(&mut reader) {
            Ok(msg) => {
                if tx_in.send(msg).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    });

    // 写线程：outgoing 通道 → 报文
    let (tx_out, rx_out) = channel::<ClientMsg>();
    let mut writer = stream;
    thread::spawn(move || {
        while let Ok(msg) = rx_out.recv() {
            if write_msg(&mut writer, &msg).is_err() {
                break;
            }
        }
    });

    Some(NetClient {
        my_index: u8::MAX, // 未分配
        // 开局默认对手在线：严格屏障（等不到包就停），防止开局竞态丢指令
        // 只有收到 OpponentLeft 才放宽为空帧放行
        opponent_live: true,
        live_resume_stamp: None,
        incoming: Mutex::new(rx_in),
        outgoing: tx_out,
    })
}

/// 收包（Update）：分配序号、对局开始、重连日志、对手指令入缓冲、哈希比对
pub fn receive(
    net: Option<ResMut<NetClient>>,
    mut state: ResMut<SimState>,
    mut buffer: ResMut<CommandBuffer>,
    mut replay_log: ResMut<crate::replay::ReplayLog>,
    mut decks: ResMut<Decks>,
    hashes: Res<OwnHashes>,
    mut conn_dead: Local<bool>,
) {
    let Some(mut net) = net else { return };
    // 先把报文全部取出（锁随作用域结束释放），并探测连接是否已断
    let msgs: Vec<ServerMsg> = {
        let incoming = net.incoming.lock().unwrap();
        let mut msgs = Vec::new();
        loop {
            match incoming.try_recv() {
                Ok(m) => msgs.push(m),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if !*conn_dead {
                        *conn_dead = true;
                        error!("与中继的连接已断开：对局已冻结，请检查网络后重连");
                    }
                    break;
                }
            }
        }
        msgs
    };
    for msg in msgs {
        match msg {
            ServerMsg::Joined { index } => {
                info!("已加入房间，玩家序号 {index}（0=蓝方 1=红方）");
                net.my_index = index;
            }
            ServerMsg::Start { seed } => {
                // 追帧中的重连客户端忽略（追完自动进 Playing）
                if matches!(*state, SimState::Waiting) {
                    info!("对手已就位，对局开始（牌库种子 {seed}）");
                    // 用中继下发的种子洗牌：两端一致但逐局变化
                    *decks = Decks::shuffled_with(seed);
                    *state = SimState::Playing;
                }
            }
            ServerMsg::History {
                entries,
                current_tick,
                seed,
            } => {
                info!(
                    "收到对局日志：{} 条指令，追帧至 tick {}",
                    entries.len(),
                    current_tick
                );
                // 重连方是全新进程：必须先用本局种子重建牌库，再追帧
                *decks = Decks::shuffled_with(seed);
                let mut map = std::collections::HashMap::new();
                for e in entries {
                    let cmds: Vec<GameCommand> =
                        e.cmds.into_iter().filter_map(wire_to_command).collect();
                    map.entry(e.tick).or_insert_with(Vec::new).extend(cmds);
                }
                *replay_log = crate::replay::ReplayLog {
                    map,
                    target: current_tick,
                };
                *state = SimState::CatchingUp;
            }
            ServerMsg::OpponentLeft => {
                info!("对手掉线：其后续帧按空指令处理，对局继续");
                net.opponent_live = false;
                net.live_resume_stamp = None;
            }
            ServerMsg::OpponentBack => {
                info!("对手已重连（可能仍在追帧）");
            }
            ServerMsg::Commands { tick, cmds } => {
                // 收到真实指令包 = 对手恢复实时发送；记录恢复帧号覆盖衔接空隙
                if !net.opponent_live {
                    net.opponent_live = true;
                    net.live_resume_stamp = Some(tick);
                }
                let cmds = cmds.into_iter().filter_map(wire_to_command).collect();
                buffer.remote.insert(tick, cmds);
            }
            ServerMsg::Hash { tick, hash } => {
                if let Some(&own) = hashes.0.get(&tick) {
                    if own != hash {
                        error!("失同步！帧 {tick}：己方 {own:#010x} != 对方 {hash:#010x}");
                    }
                }
            }
        }
    }
}

/// 帧同步屏障（实时驱动的门控函数）：
/// 单机直接跑；对局中必须等到对手该帧的指令包；其余状态（等待/追帧/回放/结束）不跑
pub fn sim_ready(
    state: &SimState,
    buffer: &CommandBuffer,
    tick: &Tick,
    net: Option<&NetClient>,
) -> bool {
    match state {
        SimState::Solo => true,
        SimState::Playing => {
            // 前 INPUT_DELAY 帧不可能存在指令，无需等待
            tick.0 < INPUT_DELAY
                || buffer.remote.contains_key(&tick.0)
                // 对手离线/追帧中：其帧按空指令处理，对局不停
                // 对手重连恢复发报文的起始帧号之前：其从未发过的帧同样按空指令处理
                || net.is_some_and(|n| {
                    !n.opponent_live || n.live_resume_stamp.is_some_and(|s| tick.0 < s)
                })
        }
        _ => false,
    }
}

/// 每帧结束：计算状态哈希并发送给对方比对
pub fn send_hash(
    tick: Res<Tick>,
    monsters: Query<(&Transform, &Health, &Monster)>,
    towers: Query<&Health, With<Tower>>,
    elixir: Res<Elixir>,
    decks: Res<Decks>,
    mut hashes: ResMut<OwnHashes>,
    net: Option<Res<NetClient>>,
) {
    // 位运算折叠，与遍历顺序无关（wrapping/XOR 可交换）
    let mut h = 0u32;
    for (t, hp, m) in &monsters {
        h = h.wrapping_add(
            t.translation.x.to_bits()
                ^ t.translation.z.to_bits()
                ^ hp.current.to_bits()
                ^ (m.faction.index() as u32),
        );
    }
    for hp in &towers {
        h = h.wrapping_add(hp.current.to_bits().rotate_left(7));
    }
    h = h.wrapping_add(elixir.player.to_bits() ^ elixir.enemy.to_bits().rotate_left(11));
    // 牌库牌序也是模拟状态
    for (i, c) in decks.player.iter().chain(decks.enemy.iter()).enumerate() {
        h = h.wrapping_add((*c as u32) << (i % 16));
    }

    hashes.0.insert(tick.0, h);
    hashes.0.retain(|t, _| *t + 3000 > tick.0); // 保留近期 100 秒（覆盖大延迟/追帧场景）

    if let Some(net) = net {
        let _ = net.outgoing.send(ClientMsg::Hash { tick: tick.0, hash: h });
    }
}

pub fn command_to_wire(cmd: GameCommand) -> CommandWire {
    match cmd {
        GameCommand::Deploy {
            faction,
            card,
            x,
            z,
        } => CommandWire::Deploy {
            faction: faction.index(),
            card,
            x,
            z,
        },
    }
}

pub fn wire_to_command(wire: CommandWire) -> Option<GameCommand> {
    match wire {
        CommandWire::Deploy {
            faction,
            card,
            x,
            z,
        } => Some(GameCommand::Deploy {
            faction: Faction::from_index(faction)?,
            card,
            x,
            z,
        }),
    }
}

pub fn send_commands(net: &NetClient, tick: u32, cmds: &[GameCommand]) {
    let wire: Vec<CommandWire> = cmds.iter().map(|c| command_to_wire(*c)).collect();
    let _ = net.outgoing.send(ClientMsg::Commands { tick, cmds: wire });
}

/// 通知中继对局结束（中继据此把房间日志落盘为录像文件）
pub fn send_game_over(net: &NetClient, end_tick: u32) {
    let _ = net.outgoing.send(ClientMsg::GameOver { end_tick });
}

/// 对局结束统一上报（覆盖所有结束路径：爆塔/猝死/常规比塔数/平局）
/// 放在 SimTick 链尾，状态翻到 GameOver 的那帧发一次
pub fn notify_game_over(
    state: Res<SimState>,
    tick: Res<Tick>,
    net: Option<Res<NetClient>>,
    mut fired: Local<bool>,
) {
    if *fired || !matches!(*state, SimState::GameOver(_)) {
        return;
    }
    *fired = true;
    if let Some(net) = net.as_ref() {
        send_game_over(net, tick.0);
    }
}
