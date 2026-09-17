#!/usr/bin/env python
# gen0' 冷启动训练：MLP 小网络 + MaskablePPO + 课程对手（70% 脚本 / 30% 随机）
# 奖励：胜负 ±1 + 塔血差 shaping(0.0005/HP) + 出牌引导(0.004) + 囤水罚 + 击杀交换
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
# 观测维度从 Rust 侧取，跟 compute_obs 布局保持单一事实来源
OBS_SIZE = cr_py.CrEnv.short(2700).obs_size
MODEL_DIR = Path("models")


class SelfPlayEnv(gym.Env):
    """蓝方为学习方；红方由脚本/随机/快照模型扮演"""

    def __init__(self):
        super().__init__()
        # 训练用 90 秒短局：单位步数内样本更多、信用分配更容易
        self.inner = cr_py.CrEnv.short(2700)
        self.observation_space = gym.spaces.Box(-2.0, 2.0, shape=(OBS_SIZE,), dtype=np.float32)
        self.action_space = gym.spaces.Discrete(N_ACTIONS)
        self.opp_model = None
        # 对手模式：mix=训练课程（70% 脚本+30% 随机）/ random / scripted / model
        self.opp_mode = "mix"
        self.ep_seed = 0
        self._last_obs = None

    def set_opponent(self, model):
        self.opp_model = model

    def _opp_action(self):
        if self.opp_model is not None:
            # 对手用红方镜像视角观测 + 红方合法掩码（之前错用蓝方视角，等于半瞎）
            obs = np.array(self.inner.obs_for(1), dtype=np.float32)
            mask = np.array(self.inner.action_mask(1), dtype=bool)
            a, _ = self.opp_model.predict(obs, deterministic=False, action_masks=mask)
            return int(a)
        if self.opp_mode == "scripted":
            return int(self.inner.scripted_action(1))
        if self.opp_mode == "random":
            if np.random.rand() < 0.75:
                return NOOP
            return int(np.random.randint(N_ACTIONS))
        # mix：课程对手（50% 脚本组合拳结构性压力 + 50% 随机节奏多样性）
        if np.random.rand() < 0.5:
            return int(self.inner.scripted_action(1))
        if np.random.rand() < 0.75:
            return NOOP
        return int(np.random.randint(N_ACTIONS))

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


def evaluate(model, episodes=60, mode="random"):
    """模型 vs 指定对手（random/scripted）胜率"""
    env = SelfPlayEnv()
    env.opp_mode = mode
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
        ent_coef=0.02,
        policy_kwargs=dict(net_arch=[128, 128]),
        verbose=0,
        device="cpu",
    )

    t0 = time.time()
    # 课程阶段 1：全程打随机对手，先把"赢随机"学会，再谈自我对弈
    model.learn(total_timesteps=total)
    secs = time.time() - t0

    wr = evaluate(model)
    wr_script = evaluate(model, mode="scripted")
    print(f"train {total} steps in {secs:.0f}s ({total/secs:.0f} steps/s)")
    print(f"vs random winrate: {wr:.0%}")
    print(f"vs scripted winrate: {wr_script:.0%}")

    path = MODEL_DIR / f"gen0_wr{wr:.2f}.zip"
    model.save(path)
    print(f"saved: {path}")


if __name__ == "__main__":
    main()
