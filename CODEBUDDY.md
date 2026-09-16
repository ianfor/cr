# CODEBUDDY.md

This file provides guidance to CodeBuddy Code when working with code in this repository.

## 项目简介

皇室战争（Clash Royale）like 的 1v1 实时对战游戏，Bevy 0.19 + Rust，**确定性帧同步（lockstep）联网**。单机可玩，也可双客户端 + 中继服务器对战。

## 常用命令

```bash
cargo build                  # 构建全部（lib + 游戏 + relay）
cargo test --lib             # 全部单元测试（无头 ECS 测试，不需要 GPU）
cargo test --lib replay      # 按名字过滤单个测试（如 replay_is_deterministic）
cargo test --lib aggro       # 另一个示例：怪物索敌/锁定测试

cargo run                    # 启动游戏客户端（默认连 127.0.0.1:9700；连不上自动进单机模式）
cargo run --bin relay        # 启动中继服务器（默认 0.0.0.0:9700）

# 环境变量
RELAY_ADDR=ip:9700 ROOM=2 cargo run     # 指定服务器和房间
PLAYER_TOKEN=222 cargo run              # 同机开两个客户端测试时必须区分 token（否则互相顶座）
RELAY_BIND=0.0.0.0:9701 cargo run --bin relay  # relay 监听地址
REPLAY=replays/xxx.cr cargo run         # 录像回放模式（空格暂停，1/2/4/8 倍速，左右键 ±10s seek）
```

注意：`cargo test` 只编译测试二进制，**不会更新游戏 exe**；改了代码要跑游戏必须 `cargo build`（本仓库历史上多次踩过旧二进制的坑）。

## 编译与平台坑

- bevy 特性已裁剪为 `default-features = false, features = ["3d", "ui"]`（砍了 audio/gltf/动画/纹理编解码），加新渲染功能前先查 `bevy-0.19.1/Cargo.toml` 的特性集合（`3d`/`ui`/`default_app` 等）
- **`bevy/dynamic_linking` 在本平台（Windows MSVC）不可用**：PE 导出符号上限 65535，bevy_dylib 需要 ~70 万，实测 link.exe 和 rust-lld 都过不去，不要开
- Windows 上 exe 被占用会导致 `cargo build` 报"拒绝访问"：先 `tasklist //FI "IMAGENAME eq bevy_hello.exe"` 查杀残留进程
- 首次全量编译 ~5 分钟，老机器（i7-6700）增量编译分钟级属正常

## 核心架构：帧同步如何运转

**对局状态 = 初始状态 + 全部指令输入**。两端各自跑完全相同的确定性模拟，网络只同步指令（每帧几十字节）。

### 双调度结构（理解本仓库的关键）

- **`SimTick` schedule**（`replay.rs`）：整条确定性模拟链挂在这里，**不由 bevy 自动驱动**，由两个驱动手动 `try_run_schedule`：
  - `drive_sim`（FixedUpdate，30Hz）：实时对局，每 tick 先过锁步屏障 `sim_ready`
  - `drive_replay`（Update）：追帧/回放模式，连续空转（每渲染帧最多 600 tick）
- **`Update`**：输入采集、网络收发、全部表现层（血条/UI/特效/部署区域显示）

### 确定性铁律（违反即失同步）

在 `SimTick` 链内的代码必须遵守：

1. **禁止 `delta_secs()`/真实时间**：一律用 `TICK_DT` / `TICK_DURATION` 常量（`constants.rs`）
2. **禁止随机数和非确定性遍历**：洗牌等用 `cards.rs` 里的位运算 `prand`（固定种子，牌库种子由 relay 在 Start 下发）；需要随机感的纯视觉表现可以用 sin/cos（它们在模拟外）
3. **输入只能走指令流**：点击 → `gather_input`（Update，只产生 `GameCommand` 放入 `PendingClicks`，不碰模拟状态）→ `collect_inputs`（打 `T+INPUT_DELAY` 帧号入 `CommandBuffer` 并发给对手）→ `apply_commands`（帧边界统一执行）。**权威校验（费用/手牌/部署区域）必须放在执行侧**（`play_card`），两端用同一模拟状态判定，结果必然一致；采集侧的校验只是体验优化
4. 模拟状态的实体顺序、指令执行顺序（`apply_commands` 里按阵营序号稳定排序）必须两端一致
5. **任何影响模拟的改动（数值/AI/地图/洗牌/帧率）必须给 `SIM_VERSION`（`constants.rs`）+1**——录像回放靠它校验版本，不匹配会警告结果失真

### 屏障与断线处理（net.rs / relay.rs）

- `sim_ready`：Playing 状态下必须等到对手该帧的指令包；`OpponentLeft` 后对手缺失帧按空指令放行；`live_resume_stamp` 覆盖重连衔接空隙
- relay（`src/bin/relay.rs`）：纯转发管道 + 房间管理，**不理解游戏逻辑**。token 认座、指令日志（只记非空帧）、双方离线房间冻结（TTL 600s，空帧不进日志，靠重连方追帧时自己模拟）
- 追帧（重连）与录像回放**共用同一套机制**：`ReplayLog`（帧号→指令）+ `drive_replay` + `reset_world`（seek 回退 = 重置世界后重新追帧）
- 每帧互发状态哈希（`send_hash`），不一致报"失同步"——改模拟代码后联网测试必看

### 模块地图（src/）

| 文件 | 职责 |
|---|---|
| `constants.rs` | 所有数值调参（地图/战斗/圣水/计时）+ `TowerSpec` + `CARDS` 卡牌规格 |
| `components.rs` | ECS 组件与资源（Faction/Health/Monster/Tower/Elixir/Decks/GameCommand 等） |
| `arena.rs` | 场景搭建：相机（正交斜视）、灯光、地面、塔生成；红方视角镜像 |
| `combat.rs` | 战斗核心：输入采集、指令执行、怪物 AI（目标锁定）、塔 AI、子弹、碰撞/河道约束、对局结束判定 |
| `cards.rs` | 牌库洗牌/循环、出牌与部署区域规则（推塔开放侧区）、卡槽 UI |
| `elixir.rs` | 圣水回复（倍数随对局阶段）与 UI |
| `match_flow.rs` | 对局计时（常规→加时→拼血）、结算界面（弹出大字+烟花特效） |
| `health_bar.rs` | 血条生成/刷新/面向相机 |
| `deploy_zone.rs` | 可放/不可放区域半透明覆盖层 |
| `net.rs` | 客户端网络：连接、收发、屏障、哈希、SimState |
| `protocol.rs` | 报文定义（不依赖 bevy，relay 也只引用它）+ `ReplayFile` 录像格式 |
| `replay.rs` | SimTick 调度、追帧/回放驱动、世界重置、录像存取 |
| `src/bin/relay.rs` | 中继服务器（独立二进制） |

## 测试模式

单元测试都是**无头 ECS 测试**（不需要 GPU/窗口）：`App::new()` + 手插资源 + `Schedule::run(world)` 跑单个系统或整条链。最重要的回归测试是 `replay::tests::replay_is_deterministic`——同一指令流跑两遍（第二遍先 `reset_world`），最终状态哈希必须逐比特一致；改任何模拟代码前先想它会过这个测试吗。测试用 `assets` 资源用 `init_resource::<Assets<Mesh>>()` 注入。

## Bevy 0.19 API 注意点（和旧教程不同的地方）

- `TextFont.font_size` 是 `FontSize` 枚举（`FontSize::Px(60.0)`）不是 f32
- `WindowResolution` 用 `(u32, u32)`；`DirectionalLight.shadow_maps_enabled`（不是 shadows_enabled）
- UI 渲染需要相机带 `IsDefaultUiCamera`（Camera3d 不再默认带）
- `ScalingMode` 在 `bevy::camera`，`NotShadowCaster` 在 `bevy::light`，均不在 prelude
- 资源寄存在内部实体上——清空世界时不能全量 despawn（参考 `replay::reset_world` 按 `Transform`/`Node` 过滤）

## 其他

- **中文注释/文档可以，但游戏内 UI 文字必须用英文**（bevy 内置字体无中文字形）
- `assets/cards.json`：121 张官方卡牌数值（`tools/scrape_cards.py` 抓取，带磁盘缓存）；游戏内 `CARDS` 目前仍是硬编码在 `constants.rs`
- `replays/`、`player_token.txt`、`tools/cache/` 已被 gitignore
- 提交信息用中文，详细描述改动语义（本仓库 commit 风格）
