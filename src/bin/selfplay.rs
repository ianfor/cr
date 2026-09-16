//! 自对弈冒烟测试：随机策略跑若干局，输出速度和对局结果
//! 运行：cargo run --bin selfplay [局数，默认 10]

use bevy_hello::sim_env::{EnvAction, SimWorld};

/// 简单伪随机（避免引入 rand 依赖）
fn prand(seed: u32) -> f32 {
    let h = seed.wrapping_mul(2654435761) ^ 0x9E3779B9;
    (h % 1000) as f32 / 1000.0
}

/// 随机策略：~25% 概率出一张随机手牌到随机位置（自己半场）
fn random_action(rng: &mut u32, z_sign: f32) -> Option<EnvAction> {
    *rng = rng.wrapping_add(1);
    if prand(*rng) < 0.25 {
        Some(EnvAction {
            slot: (prand(*rng + 11) * 4.0) as usize,
            x: prand(*rng + 23) * 12.0 - 6.0,
            z: z_sign * (2.0 + prand(*rng + 37) * 10.0),
        })
    } else {
        None
    }
}

fn main() {
    let episodes: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let mut w = SimWorld::new();
    let t0 = std::time::Instant::now();
    let mut total_ticks = 0u64;
    let mut wins = [0u32; 3]; // [blue, red, draw]

    for ep in 0..episodes {
        w.reset(ep.wrapping_add(1000));
        let mut rng = ep.wrapping_mul(7919) + 1;
        loop {
            let r = w.step(random_action(&mut rng, -1.0), random_action(&mut rng, 1.0));
            if r.done {
                let idx = match r.winner {
                    Some(bevy_hello::components::Faction::Player) => 0,
                    Some(bevy_hello::components::Faction::Enemy) => 1,
                    None => 2,
                };
                wins[idx] += 1;
                println!(
                    "ep {ep}: {} ticks, winner: {:?}, reward: {:+.3}",
                    w.tick(),
                    r.winner,
                    r.reward
                );
                break;
            }
        }
        total_ticks += w.tick() as u64;
    }

    let secs = t0.elapsed().as_secs_f32();
    println!(
        "\n{episodes} episodes, {total_ticks} ticks in {secs:.1}s → {:.0} ticks/s, {:.1}s/episode",
        total_ticks as f32 / secs,
        secs / episodes as f32
    );
    println!("blue {} / red {} / draw {}", wins[0], wins[1], wins[2]);
}
