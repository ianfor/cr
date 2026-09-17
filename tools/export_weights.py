#!/usr/bin/env python
# 把 SB3 MaskablePPO 的策略网络权重导出为 JSON（供游戏内 Rust 纯前向推理）
# 用法: .venv/Scripts/python.exe tools/export_weights.py models/xxx.zip models/xxx.json

import json
import sys

from sb3_contrib import MaskablePPO


def main():
    src, dst = sys.argv[1], sys.argv[2]
    model = MaskablePPO.load(src)
    sd = model.policy.state_dict()

    # SB3 MlpPolicy 结构：features_extractor(恒等) + mlp_extractor.policy_net(Linear,Tanh,Linear,Tanh) + action_net(Linear)
    w = {
        "w0": sd["mlp_extractor.policy_net.0.weight"].tolist(),
        "b0": sd["mlp_extractor.policy_net.0.bias"].tolist(),
        "w1": sd["mlp_extractor.policy_net.2.weight"].tolist(),
        "b1": sd["mlp_extractor.policy_net.2.bias"].tolist(),
        "w2": sd["action_net.weight"].tolist(),
        "b2": sd["action_net.bias"].tolist(),
    }
    with open(dst, "w") as f:
        json.dump(w, f)
    print(
        f"exported {src} -> {dst}: "
        f"{len(w['w0'])}x{len(w['w0'][0])} -> {len(w['w1'])}x{len(w['w1'][0])} -> {len(w['w2'])}x{len(w['w2'][0])}"
    )


if __name__ == "__main__":
    main()
