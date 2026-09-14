//! 网络协议：客户端与中继服务器之间的报文定义与读写
//! 本模块不依赖 bevy，中继服务器只引用它

use std::io::{self, Read, Write};
use std::net::TcpStream;

use serde::{Deserialize, Serialize};

/// 指令的网络传输格式（与游戏内 GameCommand 一一对应，不依赖 bevy 类型）
#[derive(Clone, Copy, Serialize, Deserialize)]
pub enum CommandWire {
    /// 用手牌 card（CARDS 中的 id）在地面 (x, z) 部署；faction: 0 = Player, 1 = Enemy
    Deploy {
        faction: u8,
        card: u8,
        x: f32,
        z: f32,
    },
}

/// 指令日志条目：某帧某玩家发出的指令（只记录非空帧）
/// 重连追帧和录像回放共用的数据格式
#[derive(Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub tick: u32,
    /// 发送方玩家序号（0/1）
    pub from: u8,
    pub cmds: Vec<CommandWire>,
}

/// 录像文件格式
#[derive(Serialize, Deserialize)]
pub struct ReplayFile {
    pub version: u32,
    /// 对局结束时的帧号
    pub end_tick: u32,
    pub entries: Vec<LogEntry>,
}

/// 客户端 → 服务器
#[derive(Serialize, Deserialize)]
pub enum ClientMsg {
    /// 加入房间；token 是客户端持久化的身份标识，断线重连凭它认领座位
    Join { room: u32, token: u64 },
    /// 指令包：tick = 要执行的帧号；空 vec 表示该帧无操作（屏障心跳）
    Commands { tick: u32, cmds: Vec<CommandWire> },
    /// 状态哈希：防失同步校验
    Hash { tick: u32, hash: u32 },
    /// 对局结束通知（end_tick = 结束帧号）：中继收到后把本房日志落盘为录像
    GameOver { end_tick: u32 },
}

/// 服务器 → 客户端
#[derive(Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// 分配玩家序号：0 = 先到的（蓝方 Player），1 = 后到的（红方 Enemy）
    Joined { index: u8 },
    /// 房间满两人，对局开始；seed 为本局牌库洗牌种子（双方一致，逐局变化）
    Start { seed: u32 },
    /// 重连时下发：对局开始到 current_tick 的全部指令日志，以及本局牌库种子
    History {
        entries: Vec<LogEntry>,
        current_tick: u32,
        seed: u32,
    },
    /// 对手掉线（其后续帧按空指令处理，对局继续）
    OpponentLeft,
    /// 对手重新连上（其真实指令包恢复到达前可能仍在追帧）
    OpponentBack,
    /// 转发对手的指令包
    Commands { tick: u32, cmds: Vec<CommandWire> },
    /// 转发对手的状态哈希
    Hash { tick: u32, hash: u32 },
}

/// 写一条报文：4 字节小端长度前缀 + bincode 负载
pub fn write_msg<T: Serialize>(stream: &mut TcpStream, msg: &T) -> io::Result<()> {
    let payload =
        bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(&payload)
}

/// 读一条报文（阻塞）
pub fn read_msg<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 1_000_000 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}
