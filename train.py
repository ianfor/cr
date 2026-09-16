#!/usr/bin/env python
# 自我对弈 PPO 训练（冒烟版）：MLP 小网络 + MaskablePPO + 快照对手 + 胜率门控
# 用法: .venv/Scripts/python.exe train.py [总步数, 默认 20000]

import sys
import time
from pathlib import Path

import gymnasium as gym
import numpy as np
from sb3_contrib import MaskablePPO
from sb3_contrib.common.wrappers import ActionMasker
from stable_baselines3.common.callbacks import BaseCallback

import cr_py

N_ACTIONS = 449
NOOP = N_ACTIONS - 1
OBS_SIZE = 117
MODEL_DIR = Path("models")


class SelfPlayEnv(gym.Env):
    """蓝方为学习方；红方由快照模型（或随机策略）扮演"""

    def __init__(self):
        super().__init__()
        self.inner = cr_py.CrEnv()
        self.observation_space = gym.spaces.Box(-2.0, 2.0, shape=(OBS_SIZE,), dtype=np.float32)
        self.action_space = gym.spaces.Discrete(N_ACTIONS)
        self.opp_model = None
        self.ep_seed = 0
        self._last_obs = None

    def set_opponent(self, model):
        self.opp_model = model

    def _opp_action(self):
        if self.opp_model is None:
            if np.random.rand() < 0.75:
                return NOOP
            return int(np.random.randint(N_ACTIONS))
        # 对手也用合法动作掩码（红方 faction=1），不然自对弈对手太水
        mask = np.array(self.inner.action_mask(1), dtype=bool)
        a, _ = self.opp_model.predict(self._last_obs, deterministic=False, action_masks=mask)
        return int(a)

    def reset(self, seed=None, options=None):
        super().reset(seed=seed)
        self.ep_seed += 1
        obs = self.inner.reset(1000 + self.ep_seed)
        self._last_obs = obs
        return np.array(obs, dtype=np.float32), {}

    def step(self, action):
        obs, reward, done, winner = self.inner.step(int(action), self._opp_action())
        self._last_obs = obs
        return np.array(obs, dtype=np.float32), reward, done, False, {"winner": winner}

    def action_masks(self):
        return np.array(self.inner.action_mask(0), dtype=bool)


def mask_fn(env):
    return env.action_masks()


class OpponentUpdateCallback(BaseCallback):
    """每 update_freq 步把当前模型设为对手（self-play 最新自我）"""

    def __init__(self, env, update_freq=8192):
        super().__init__()
        self.env = env
        self.update_freq = update_freq
        self._next = update_freq

    def _on_step(self):
        if self.num_timesteps >= self._next:
            self.env.set_opponent(self.model)
            self._next += self.update_freq
        return True


def evaluate(model, episodes=20):
    """模型 vs 随机策略胜率"""
    env = SelfPlayEnv()
    env.set_opponent(None)
    wins = 0
    for ep in range(episodes):
        obs, _ = env.reset()
        done = False
        while not done:
            mask = env.action_masks()
            a, _ = model.predict(obs, deterministic=True, action_masks=mask)
            obs, _, term, trunc, info = env.step(a)
            done = term or trunc
        if info["winner"] == 1:
            wins += 1
    return wins / episodes


def main():
    total = int(sys.argv[1]) if len(sys.argv) > 1 else 20000
    MODEL_DIR.mkdir(exist_ok=True)

    env = ActionMasker(SelfPlayEnv(), mask_fn)
    model = MaskablePPO(
        "MlpPolicy",
        env,
        n_steps=1024,
        batch_size=256,
        learning_rate=3e-4,
        gamma=0.995,
        ent_coef=0.01,
        policy_kwargs=dict(net_arch=[128, 128]),
        verbose=0,
        device="cpu",
    )

    t0 = time.time()
    model.learn(
        total_timesteps=total,
        callback=OpponentUpdateCallback(env.unwrapped),
    )
    secs = time.time() - t0

    wr = evaluate(model)
    print(f"train {total} steps in {secs:.0f}s ({total/secs:.0f} steps/s)")
    print(f"vs random winrate: {wr:.0%}")

    path = MODEL_DIR / f"gen0_wr{wr:.2f}.zip"
    model.save(path)
    print(f"saved: {path}")


if __name__ == "__main__":
    main()
