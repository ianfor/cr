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
GATE_EPISODES = 60
GATE_THRESHOLD = 0.55


class MixedOpponentCallback(BaseCallback):
    """每 freq 步重掷一次对手池（70% 锚点 / 30% 随机）"""

    def __init__(self, env, mixer, freq=16384):
        super().__init__()
        self.env = env
        self.mixer = mixer
        self.freq = freq
        self._next = freq

    def _on_step(self):
        if self.num_timesteps >= self._next:
            self.mixer(self.env)
            self._next += self.freq
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
    mixer = MixedOpponent(prev)
    mixer(env.unwrapped)

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
        callback=MixedOpponentCallback(env.unwrapped, mixer),
    )
    secs = time.time() - t0

    wr_random = evaluate(model)
    wr_prev = evaluate_vs(model, prev)
    print(f"train {total} steps in {secs:.0f}s ({total/secs:.0f} steps/s)")
    print(f"vs random: {wr_random:.0%}   vs prev: {wr_prev:.0%}")

    # 无条件存档（门控只决定"是否当新师傅"，不再丢模型）
    path = MODEL_DIR / f"cand_r{wr_random:.2f}_p{wr_prev:.2f}.zip"
    model.save(path)
    if wr_prev >= GATE_THRESHOLD:
        print(f"GATE PASSED, new anchor: {path}")
    else:
        print(f"gate not passed ({wr_prev:.0%} < {GATE_THRESHOLD:.0%}), model kept at {path}")


if __name__ == "__main__":
    main()
