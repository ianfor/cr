#!/usr/bin/env python
# 联盟自我对弈进化：从上一代模型出发，自我对弈训练 + 55% 胜率门控换代
# 用法: .venv/Scripts/python.exe league.py <上一代模型.zip> [总步数, 默认 300000]

import sys
import time
from pathlib import Path

import numpy as np
from sb3_contrib import MaskablePPO
from sb3_contrib.common.wrappers import ActionMasker
from stable_baselines3.common.callbacks import BaseCallback

from train import SelfPlayEnv, mask_fn, evaluate

MODEL_DIR = Path("models")
# 门控局数：200 局 σ≈±3.5%，60 局的 ±6.5% 纯抛硬币（gen1 过门有运气成分的教训）
GATE_EPISODES = 200
GATE_THRESHOLD = 0.55


class AnnealedOpponentCallback(BaseCallback):
    """对手退火：先多打非锚点练基本功，再逐步加重锚点比例
    （冷启动模型开局太弱，70% 锚点会把基础打崩——gen2 的教训）。
    阈值按总步数等比缩放（原始 150K/300K 是按 30 万步总长写的）"""

    def __init__(self, env, anchor, total_steps, check_freq=8192):
        super().__init__()
        self.env = env
        self.anchor = anchor
        self.check_freq = check_freq
        self._next = check_freq
        # (总步数比例, 锚点概率)
        self.schedule = [
            (0.0, 0.2),
            (0.5, 0.5),
            (1.0, 0.7),
        ]
        self.total_steps = total_steps

    def _anchor_prob(self, steps):
        frac = steps / max(self.total_steps, 1)
        p = self.schedule[0][1]
        for threshold, prob in self.schedule:
            if frac >= threshold:
                p = prob
        return p

    def _on_step(self):
        if self.num_timesteps >= self._next:
            if np.random.rand() < self._anchor_prob(self.num_timesteps):
                self.env.set_opponent(self.anchor)
            else:
                self.env.set_opponent(None)  # 随机对手
            self._next += self.check_freq
        return True


def evaluate_vs(model_a, model_b, episodes=GATE_EPISODES):
    """model_a（蓝）vs model_b（红，带掩码）胜率"""
    env = SelfPlayEnv()
    env.set_opponent(model_b)
    wins = 0
    for _ in range(episodes):
        obs, _ = env.reset()
        done = False
        while not done:
            mask = env.action_masks()
            a, _ = model_a.predict(obs, deterministic=True, action_masks=mask)
            obs, _, term, trunc, info = env.step(a)
            done = term or trunc
        if info["winner"] == 1:
            wins += 1
    return wins / episodes


class MixedOpponent:
    """对手池：70% 冻结锚点模型 + 30% 随机
    （不用"最新快照"——移动靶会导致自我对弈漂移/遗忘）"""

    def __init__(self, anchor):
        self.anchor = anchor

    def __call__(self, env_selfplay):
        if np.random.rand() < 0.7:
            env_selfplay.set_opponent(self.anchor)
        else:
            env_selfplay.set_opponent(None)


def main():
    prev_path = sys.argv[1]
    total = int(sys.argv[2]) if len(sys.argv) > 2 else 300000
    fresh = len(sys.argv) > 3 and sys.argv[3] == "fresh"
    MODEL_DIR.mkdir(exist_ok=True)

    prev = MaskablePPO.load(prev_path)
    env = ActionMasker(SelfPlayEnv(), mask_fn)

    if fresh:
        # exploiter 从随机权重冷启动打冻结池（联盟标准做法，避免热启动漂移）
        model = MaskablePPO(
            "MlpPolicy",
            env,
            n_steps=1024,
            batch_size=256,
            learning_rate=3e-4,
            gamma=0.995,
            ent_coef=0.02,
            policy_kwargs=dict(net_arch=[128, 128]),
            verbose=0,
            device="cpu",
        )
    else:
        model = MaskablePPO.load(prev_path, env=env)  # 从上一代热启动
    t0 = time.time()
    model.learn(
        total_timesteps=total,
        callback=AnnealedOpponentCallback(env.unwrapped, prev, total),
    )
    secs = time.time() - t0

    wr_random = evaluate(model, episodes=60)
    wr_script = evaluate(model, mode="scripted", episodes=60)
    wr_prev = evaluate_vs(model, prev)
    print(f"train {total} steps in {secs:.0f}s ({total/secs:.0f} steps/s)")
    print(f"vs random: {wr_random:.0%}   vs script: {wr_script:.0%}   vs prev: {wr_prev:.0%}")

    # 无条件存档（门控只决定"是否当新师傅"，不再丢模型）
    path = MODEL_DIR / f"cand_r{wr_random:.2f}_p{wr_prev:.2f}.zip"
    model.save(path)
    if wr_prev >= GATE_THRESHOLD:
        print(f"GATE PASSED, new anchor: {path}")
    else:
        print(f"gate not passed ({wr_prev:.0%} < {GATE_THRESHOLD:.0%}), model kept at {path}")


if __name__ == "__main__":
    main()
