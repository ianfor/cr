//! 极简帧同步中继服务器
//! 职责：房间管理（每房 2 人）+ 报文转发 + 指令日志（重连追帧用）
//!
//! 运行：cargo run --bin relay
//! 监听地址用环境变量 RELAY_BIND 覆盖，默认 0.0.0.0:9700

use std::collections::HashMap;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use bevy_hello::constants::{INPUT_DELAY, TICKS_PER_SEC};
use bevy_hello::protocol::{read_msg, write_msg, ClientMsg, CommandWire, LogEntry, ReplayFile, ServerMsg};

/// 双方都离线后房间保留时长（超时清房）
const EMPTY_ROOM_TTL_SECS: u64 = 600;

struct Seat {
    token: u64,
    tx: Sender<ServerMsg>,
    connected: bool,
    /// 该座位发出过的最大执行帧号（含心跳包），用于估算对局当前帧
    last_stamp: u32,
    /// 离线期间的收件箱：重连时补发（只存指令包，哈希不用补）
    inbox: Vec<ServerMsg>,
}

struct Room {
    /// 座位，Vec 下标即玩家序号，最多 2 个
    seats: Vec<Seat>,
    started: bool,
    /// 本局牌库种子（开局时生成，重连时随 History 下发）
    seed: u32,
    /// 创建时间（清理长期未开局的房间用）
    created_at: Instant,
    /// 指令日志：只记非空帧（重连追帧 + 录像回放的数据源）
    log: Vec<LogEntry>,
    /// 双方都离线的冻结点：（离线时刻, 当时的帧号估算）
    /// 帧同步下对局是确定的：空帧不需要记录，重连方追帧时自己会模拟
    frozen_at: Option<(Instant, u32)>,
    /// 录像是否已落盘（防重复写）
    saved: bool,
}

/// 把房间日志落盘为录像文件（ReplayFile 格式，与客户端本地录像一致）
/// tag 用于文件名区分：final = 正常打完，partial = 弃局
fn save_replay(room: u32, room_state: &mut Room, end_tick: u32, tag: &str) {
    if room_state.saved {
        return;
    }
    room_state.saved = true;
    let file = ReplayFile {
        version: 1,
        end_tick,
        entries: room_state.log.clone(),
    };
    let _ = std::fs::create_dir_all("replays");
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = format!("replays/room{room}_{tag}_{millis}.cr");
    match bincode::serialize(&file) {
        Ok(bytes) => {
            if std::fs::write(&path, bytes).is_ok() {
                // 打印绝对路径，免得找不到文件（写在 relay 进程的当前工作目录下）
                let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone().into());
                println!(
                    "room {room} replay saved: {} ({} entries, end tick {end_tick})",
                    abs.display(),
                    file.entries.len()
                );
            }
        }
        Err(e) => eprintln!("room {room} replay serialize failed: {e}"),
    }
}

impl Room {
    fn other(&self, index: u8) -> Option<&Seat> {
        self.seats
            .iter()
            .enumerate()
            .find(|(i, _)| *i as u8 != index)
            .map(|(_, s)| s)
    }

    fn other_mut(&mut self, index: u8) -> Option<&mut Seat> {
        self.seats
            .iter_mut()
            .enumerate()
            .find(|(i, _)| *i as u8 != index)
            .map(|(_, s)| s)
    }

    /// 当前帧号估算：双方最近心跳的最大值减去输入延迟
    fn tick_estimate(&self) -> u32 {
        self.seats
            .iter()
            .map(|s| s.last_stamp)
            .max()
            .unwrap_or(0)
            .saturating_sub(INPUT_DELAY)
    }
}

type Rooms = Arc<Mutex<HashMap<u32, Room>>>;

fn main() -> io::Result<()> {
    let addr = std::env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:9700".into());
    let listener = TcpListener::bind(&addr)?;
    println!("relay listening on {addr}");

    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));

    // 看门线程：定期清理
    // - 冻结超时的房间（先落盘弃局录像）
    // - 长期未开局的房间（有人进房但一直凑不齐人，防泄漏）
    {
        let rooms = rooms.clone();
        thread::spawn(move || loop {
            thread::sleep(std::time::Duration::from_secs(60));
            let mut rooms = rooms.lock().unwrap();
            let expired: Vec<u32> = rooms
                .iter()
                .filter(|(_, r)| {
                    match r.frozen_at {
                        Some((t, _)) => t.elapsed().as_secs() > EMPTY_ROOM_TTL_SECS,
                        // 未开局房间超过 TTL 还没凑齐人
                        None => {
                            !r.started
                                && r.created_at.elapsed().as_secs() > EMPTY_ROOM_TTL_SECS
                        }
                    }
                })
                .map(|(id, _)| *id)
                .collect();
            for id in expired {
                if let Some(mut room_state) = rooms.remove(&id) {
                    if room_state.started {
                        let end = room_state.tick_estimate();
                        save_replay(id, &mut room_state, end, "partial");
                    }
                    println!("room {id} expired by janitor");
                }
            }
        });
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let rooms = rooms.clone();
                thread::spawn(move || handle_client(stream, rooms));
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

fn handle_client(mut stream: TcpStream, rooms: Rooms) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    println!("[{peer}] connected");

    // 每个连接一个写线程，从通道取报文发出
    let (tx, rx) = channel::<ServerMsg>();
    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    thread::spawn(move || {
        while let Ok(msg) = rx.recv() {
            if write_msg(&mut writer, &msg).is_err() {
                break;
            }
        }
    });

    let mut my_room: Option<u32> = None;
    let mut my_index: u8 = u8::MAX;

    loop {
        let msg: ClientMsg = match read_msg(&mut stream) {
            Ok(m) => m,
            Err(_) => break, // 掉线
        };
        match msg {
            ClientMsg::Join { room, token } => {
                let mut rooms = rooms.lock().unwrap();

                // 双方都离线的超时房间：先把日志落盘为弃局录像，再清掉
                let expired = rooms.get(&room).is_some_and(|r| {
                    r.frozen_at.is_some_and(|(t, _)| {
                        t.elapsed().as_secs() > EMPTY_ROOM_TTL_SECS
                    })
                });
                if expired {
                    let mut state = rooms.remove(&room).unwrap();
                    let end = state.tick_estimate();
                    save_replay(room, &mut state, end, "partial");
                    println!("room {room} expired (empty too long)");
                }

                let room_state = rooms.entry(room).or_insert_with(|| Room {
                    seats: Vec::new(),
                    started: false,
                    seed: 0,
                    created_at: Instant::now(),
                    log: Vec::new(),
                    frozen_at: None,
                    saved: false,
                });

                // 1) token 匹配已有座位 → 断线重连，认领座位
                if let Some(idx) = room_state.seats.iter().position(|s| s.token == token) {
                    my_index = idx as u8;
                    my_room = Some(room);
                    let _ = tx.send(ServerMsg::Joined { index: my_index });
                    println!("[{peer}] rejoined room {room} as player {my_index}");

                    if room_state.started {
                        // 下发指令日志帮助追帧。当前帧估算：
                        // - 双方离线过：冻结帧号 + 离线墙钟时间换算的帧数
                        //   （空帧不进日志，追帧方自己模拟，对局照常在追帧中打完）
                        // - 否则：双方最近心跳估算
                        let current_tick = match room_state.frozen_at {
                            Some((t0, base)) => {
                                base + t0.elapsed().as_secs() as u32 * TICKS_PER_SEC as u32
                            }
                            None => room_state.tick_estimate(),
                        };
                        room_state.frozen_at = None;
                        let _ = tx.send(ServerMsg::History {
                            entries: room_state.log.clone(),
                            current_tick,
                            seed: room_state.seed,
                        });
                        // 补发离线期间错过的对手指令包
                        let seat = &mut room_state.seats[idx];
                        seat.tx = tx.clone();
                        seat.connected = true;
                        for m in seat.inbox.drain(..) {
                            let _ = tx.send(m);
                        }
                        // 通知在线方：对手回来了
                        if let Some(other) = room_state.other(my_index) {
                            if other.connected {
                                let _ = other.tx.send(ServerMsg::OpponentBack);
                            }
                        }
                    } else {
                        let seat = &mut room_state.seats[idx];
                        seat.tx = tx.clone();
                        seat.connected = true;
                    }
                    continue;
                }

                // 2) 新玩家入座
                if room_state.seats.len() >= 2 {
                    println!("[{peer}] room {room} full, rejected");
                    continue;
                }
                my_index = room_state.seats.len() as u8;
                room_state.seats.push(Seat {
                    token,
                    tx: tx.clone(),
                    connected: true,
                    last_stamp: 0,
                    inbox: Vec::new(),
                });
                my_room = Some(room);
                let _ = tx.send(ServerMsg::Joined { index: my_index });
                println!("[{peer}] joined room {room} as player {my_index}");
                if room_state.seats.len() == 2 {
                    room_state.started = true;
                    // 本局牌库种子：两端一致但逐局变化（防牌序被预知）
                    let seed = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(1)
                        ^ room;
                    room_state.seed = seed;
                    for seat in &room_state.seats {
                        let _ = seat.tx.send(ServerMsg::Start { seed });
                    }
                    println!("room {room} game start (seed {seed})");
                }
            }
            ClientMsg::Commands { tick, mut cmds } => {
                let mut rooms = rooms.lock().unwrap();
                if let Some(room_state) = my_room.and_then(|r| rooms.get_mut(&r)) {
                    if let Some(seat) = room_state.seats.get_mut(my_index as usize) {
                        seat.last_stamp = seat.last_stamp.max(tick);
                    }
                    // 服务端权威校验：防改版客户端作弊
                    // 阵营强制为发送方；坐标只做粗边界钳制——
                    // 精细部署区域规则（公主塔侧区）由两端 play_card 一致判定
                    for c in &mut cmds {
                        let CommandWire::Deploy { faction, x, z, .. } = c;
                        *faction = my_index;
                        *x = x.clamp(-8.0, 8.0);
                        *z = z.clamp(-14.0, 14.0);
                    }
                    // 指令日志只记非空帧（空帧是屏障心跳，追帧时隐式处理）
                    if !cmds.is_empty() {
                        room_state.log.push(LogEntry {
                            tick,
                            from: my_index,
                            cmds: cmds.clone(),
                        });
                    }
                    forward(room_state, my_index, ServerMsg::Commands { tick, cmds });
                }
            }
            ClientMsg::Hash { tick, hash } => {
                let mut rooms = rooms.lock().unwrap();
                if let Some(room_state) = my_room.and_then(|r| rooms.get_mut(&r)) {
                    forward(room_state, my_index, ServerMsg::Hash { tick, hash });
                }
            }
            ClientMsg::GameOver { end_tick } => {
                // 客户端判定对局结束：把本房日志落盘为录像（双方都会发，saved 去重）
                let mut rooms = rooms.lock().unwrap();
                if let Some(r) = my_room {
                    if let Some(room_state) = rooms.get_mut(&r) {
                        save_replay(r, room_state, end_tick, "final");
                    }
                }
            }
        }
    }

    // 掉线：标记座位离线并通知对手；双方都离线则冻结房间（保留日志，TTL 内可续）
    if let Some(room) = my_room {
        let mut rooms = rooms.lock().unwrap();
        if let Some(room_state) = rooms.get_mut(&room) {
            if let Some(seat) = room_state.seats.get_mut(my_index as usize) {
                seat.connected = false;
            }
            // 通知在线的对手：对方掉线了，后续帧按空指令继续
            if let Some(other) = room_state.other(my_index) {
                if other.connected {
                    let _ = other.tx.send(ServerMsg::OpponentLeft);
                }
            }
            if room_state.seats.iter().all(|s| !s.connected)
                && room_state.frozen_at.is_none()
            {
                let base = room_state.tick_estimate();
                room_state.frozen_at = Some((Instant::now(), base));
                println!("room {room} frozen at tick ~{base} (all offline, kept {EMPTY_ROOM_TTL_SECS}s)");
            }
        }
        println!("[{peer}] left room {room}");
    }
    println!("[{peer}] disconnected");
}

/// 把报文转发给同房间的其他人：在线直接发，离线存进其收件箱（仅指令包）
fn forward(room: &mut Room, from: u8, msg: ServerMsg) {
    if let Some(seat) = room.other_mut(from) {
        if seat.connected {
            let _ = seat.tx.send(msg);
        } else if matches!(msg, ServerMsg::Commands { .. }) {
            seat.inbox.push(msg);
        }
    }
}
