#!/usr/bin/env python
# 过夜联盟连跑编排：等当前代结束 → 按门控结果选锚点 → 逐代 fresh 训练
# 用法: .venv/Scripts/python.exe overnight_league.py
# 停止条件：累计 10.5 小时 / 跑满 4 代 / 剩余时间不足一代（2.5h）

import re
import subprocess
import sys
import time
from pathlib import Path

MODELS = Path("models")
GATE = 0.55
MAX_GENS = 4
TOTAL_BUDGET_H = 10.5
GEN_COST_H = 2.5  # 一代预估耗时（训练+评估），剩余不足时不再开新代


def newest_cand():
    files = list(MODELS.glob("cand_*.zip"))
    return max(files, key=lambda p: p.stat().st_mtime) if files else None


def gate_wr(path):
    m = re.search(r"_p(\d+\.\d+)\.zip$", path.name)
    return float(m.group(1)) if m else 0.0


def wait_current_gen(timeout_s):
    """等正在跑的一代产出新 cand（按 mtime 轮询）"""
    base = newest_cand()
    base_m = base.stat().st_mtime if base else 0.0
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        n = newest_cand()
        if n and n.stat().st_mtime > base_m:
            return n
        time.sleep(60)
    return None


def run_gen(anchor):
    before = time.time()
    r = subprocess.run(
        [sys.executable, "league.py", str(anchor), "2000000", "fresh"]
    )
    cands = [
        p for p in MODELS.glob("cand_*.zip") if p.stat().st_mtime > before
    ]
    if r.returncode != 0:
        print(f"!! league.py 异常退出 code={r.returncode}", flush=True)
    return max(cands, key=lambda p: p.stat().st_mtime) if cands else None


def main():
    stop_at = time.time() + TOTAL_BUDGET_H * 3600
    anchor = MODELS / "gen0_wr0.43.zip"
    log = lambda s: print(time.strftime("[%H:%M] ") + s, flush=True)

    # 1. 等当前正在跑的 gen1' 结束
    log(f"等待当前代结束，锚点 = {anchor.name}")
    cur = wait_current_gen(3.2 * 3600)
    if cur:
        log(f"当前代产出: {cur.name}（vs 锚点 {gate_wr(cur):.0%}）")
        if gate_wr(cur) >= GATE:
            anchor = cur
            log("过门 → 锚点更新")
        else:
            log("未过门 → 锚点保持")
    else:
        log("等待超时（3.2h 无产出），直接以原锚点开跑")

    # 2. 逐代连跑
    for g in range(1, MAX_GENS + 1):
        remaining_h = (stop_at - time.time()) / 3600
        if remaining_h < GEN_COST_H:
            log(f"剩余 {remaining_h:.1f}h 不足一代，收工")
            break
        log(f"===== 第 {g} 代（fresh），锚点 = {anchor.name}，剩余预算 {remaining_h:.1f}h =====")
        cand = run_gen(anchor)
        if not cand:
            log("本轮无产出，跳过")
            continue
        wr = gate_wr(cand)
        log(f"产出: {cand.name}（vs 锚点 {wr:.0%}）")
        if wr >= GATE:
            anchor = cand
            log("过门 → 锚点更新")
        else:
            log("未过门 → 锚点保持，下代重试")
    log(f"过夜连跑结束，最终锚点: {anchor.name}")


if __name__ == "__main__":
    main()
