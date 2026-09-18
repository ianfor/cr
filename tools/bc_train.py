#!/usr/bin/env python
# 行为克隆（BC）：用玩家录像的 (obs, action) 决策点样本监督训练策略网络
# 产物与 RL 模型同构（MaskablePPO .zip），可直接导出进游戏或接联盟微调
# 用法: .venv/Scripts/python.exe tools/bc_train.py

import glob
import sys
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
from sb3_contrib import MaskablePPO
from sb3_contrib.common.wrappers import ActionMasker

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from train import SelfPlayEnv, evaluate, mask_fn

NOOP = 448


def collect_samples():
    import cr_py

    env = cr_py.CrEnv()
    X, y = [], []
    skipped = 0
    for f in sorted(glob.glob("replays/replay_*.cr")):
        r = env.bc_replay(f, 0)
        if r is None:
            skipped += 1
            continue
        samples, injected, executed = r
        if injected != executed:
            print(f"跳过（牌库偏离）: {f} {injected}/{executed}")
            skipped += 1
            continue
        for obs, action in samples:
            X.append(obs)
            y.append(action)
    return np.array(X, dtype=np.float32), np.array(y, dtype=np.int64), skipped


def main():
    X, y, skipped = collect_samples()
    n_deploy = int((y != NOOP).sum())
    n_noop = int((y == NOOP).sum())
    print(f"样本: {len(X)}（出牌 {n_deploy} / 挂机 {n_noop}，跳过 {skipped} 局）")
    if len(X) < 500:
        print("样本太少（<500），先攒录像再跑")
        return

    # 类别均衡：挂机样本下采样到出牌样本的 1.5 倍（节奏直觉重要但别淹死动作）
    rng = np.random.RandomState(0)
    dep_idx = np.flatnonzero(y != NOOP)
    noop_idx = np.flatnonzero(y == NOOP)
    keep_noop = rng.choice(noop_idx, size=min(n_noop, int(n_deploy * 1.5)), replace=False)
    idx = np.concatenate([dep_idx, keep_noop])
    rng.shuffle(idx)
    X, y = X[idx], y[idx]
    print(f"均衡后: {len(X)}（出牌 {int((y != NOOP).sum())} / 挂机 {int((y == NOOP).sum())}）")

    # 与 SB3 MlpPolicy 同构：Linear(2718,128) Tanh Linear(128,128) Tanh + action_net
    net = nn.Sequential(
        nn.Linear(X.shape[1], 128),
        nn.Tanh(),
        nn.Linear(128, 128),
        nn.Tanh(),
    )
    head = nn.Linear(128, 449)

    # 90/10 训练/验证切分（按局切分近似：样本顺序内切即可，BC 无泄漏担忧级别高）
    n_val = max(1, len(X) // 10)
    Xtr, ytr, Xva, yva = X[:-n_val], y[:-n_val], X[-n_val:], y[-n_val:]
    xt = torch.from_numpy(Xtr)
    yt = torch.from_numpy(ytr)
    xv = torch.from_numpy(Xva)
    yv = torch.from_numpy(yva)

    opt = torch.optim.Adam(list(net.parameters()) + list(head.parameters()), lr=1e-3)
    lossf = nn.CrossEntropyLoss()
    best_val = 1e9
    best_state = None
    batch = 512
    for epoch in range(300):
        net.train(); head.train()
        perm = torch.randperm(len(xt))
        tot = 0.0
        for i in range(0, len(xt), batch):
            b = perm[i : i + batch]
            logits = head(net(xt[b]))
            loss = lossf(logits, yt[b])
            opt.zero_grad()
            loss.backward()
            opt.step()
            tot += loss.item() * len(b)
        net.eval(); head.eval()
        with torch.no_grad():
            vl = lossf(head(net(xv)), yv).item()
        if vl < best_val:
            best_val = vl
            best_state = (
                net.state_dict().copy(),
                head.state_dict().copy(),
            )
        if (epoch + 1) % 50 == 0:
            acc = (head(net(xv)).argmax(1) == yv).float().mean().item()
            print(f"epoch {epoch+1}: train {tot/len(xt):.4f}  val {vl:.4f}  acc {acc:.1%}")
        if epoch > 50 and vl > best_val * 1.5:  # 早停
            break
    net.load_state_dict(best_state[0])
    head.load_state_dict(best_state[1])

    # 权重灌进 MaskablePPO（结构与 RL 模型一致，可 save/load/导出/联盟微调）
    env = ActionMasker(SelfPlayEnv(), mask_fn)
    model = MaskablePPO(
        "MlpPolicy",
        env,
        n_steps=1024,
        policy_kwargs=dict(net_arch=[128, 128]),
        verbose=0,
        device="cpu",
    )
    sd = model.policy.state_dict()
    sd["mlp_extractor.policy_net.0.weight"] = net[0].weight
    sd["mlp_extractor.policy_net.0.bias"] = net[0].bias
    sd["mlp_extractor.policy_net.2.weight"] = net[2].weight
    sd["mlp_extractor.policy_net.2.bias"] = net[2].bias
    sd["action_net.weight"] = head.weight
    sd["action_net.bias"] = head.bias
    model.policy.load_state_dict(sd)
    path = Path("models/bc_player.zip")
    model.save(path)
    print(f"saved: {path}")

    wr_r = evaluate(model, episodes=60)
    wr_s = evaluate(model, mode="scripted", episodes=60)
    print(f"BC 模型评估: vs random {wr_r:.0%}   vs script {wr_s:.0%}")


if __name__ == "__main__":
    main()
