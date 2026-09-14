#!/usr/bin/env python
# 从 clashroyaledeckbuilder.net 抓取全部皇室战争卡牌数值，输出 cards.json
# 用法: python scrape_cards.py [输出路径，默认 assets/cards.json]

import json
import os
import re
import sys
import time
import urllib.request

BASE = "https://www.clashroyaledeckbuilder.net"
LIST_URL = BASE + "/zh/clash-royale-cards"
CACHE_DIR = os.path.join(os.path.dirname(__file__), "cache")

UA = {"User-Agent": "Mozilla/5.0 (card stats scraper for personal game dev)"}


def fetch(url: str) -> str:
    req = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read().decode("utf-8")


def fetch_cached(url: str, key: str) -> str:
    """带磁盘缓存的抓取：二次运行不重复请求"""
    os.makedirs(CACHE_DIR, exist_ok=True)
    path = os.path.join(CACHE_DIR, key + ".html")
    if os.path.exists(path):
        with open(path, encoding="utf-8") as f:
            return f.read()
    html = fetch(url)
    with open(path, "w", encoding="utf-8") as f:
        f.write(html)
    time.sleep(0.4)  # 礼貌限速（只在真实请求时等待）
    return html


def text(html: str) -> str:
    return re.sub(r"<[^>]+>", "", html).strip()


def parse_list(html: str) -> list[dict]:
    """列表页是服务端渲染的卡片格子：h3 名称 + 费用圆点 + 稀有度徽章"""
    cards = []
    for m in re.finditer(
        r'<h3[^>]*>([^<]+)</h3><div class="flex justify-center">'
        r'<div class="w-6 h-6 [^"]*">(\d+)</div></div>'
        r'<div class="flex justify-center"><span data-slot="badge"[^>]*>([^<]+)</span>',
        html,
        re.S,
    ):
        cards.append(
            {
                "name": m.group(1).strip(),
                "cost": int(m.group(2)),
                "rarity": m.group(3).strip().lower(),
            }
        )
    return cards


def slug_of(name: str) -> str:
    """名称 → URL slug：Knight -> knight, P.E.K.K.A -> pekka, The Log -> the_log"""
    s = name.lower().replace(".", "").replace("-", " ").replace("'", "")
    return re.sub(r"\s+", "_", s.strip())


def norm_key(header: str) -> str:
    """表头 → json key：Area Damage -> area_damage"""
    return re.sub(r"\s+", "_", header.strip().lower())


def parse_num(s: str):
    s = s.replace(",", "").strip()
    if re.fullmatch(r"-?\d+", s):
        return int(s)
    if re.fullmatch(r"-?\d+\.\d+", s):
        return float(s)
    return s


def parse_card_detail(html: str) -> dict:
    # 统计项两种版式：
    # 数值型 <span>LABEL</span>...</div><div class="text-lg|sm font-bold">VALUE</div>
    stats = {}
    for m in re.finditer(
        r"<span>([^<]+)</span></div><div class=\"text-(?:lg|sm) font-bold\">(.*?)</div>",
        html,
        re.S,
    ):
        stats[m.group(1).strip()] = text(m.group(2))
    # 徽章型（Type / Rarity）：<span>LABEL</span></div><span data-slot="badge"...>VALUE</span>
    for m in re.finditer(
        r'<span>(Type|Rarity)</span></div><span data-slot="badge"[^>]*>(.*?)</span>',
        html,
        re.S,
    ):
        stats[m.group(1).strip()] = text(m.group(2))

    # 等级表：按表头列名解析（兵种是 HP/Damage/DPS，法术是 Area Damage 等，列数不固定）
    levels = {}
    i = html.find("Level Statistics")
    if i >= 0:
        seg = html[i : i + 20000]
        heads = [
            h.strip()
            for h in re.findall(
                r'data-slot="table-head"[^>]*>.*?<span>([^<]+)</span>', seg, re.S
            )
        ]
        cells = [
            text(c)
            for c in re.findall(r'data-slot="table-cell"[^>]*>(.*?)</td>', seg, re.S)
        ]
        ncols = len(heads)
        if ncols >= 2:
            for k in range(0, len(cells) - ncols + 1, ncols):
                lv = cells[k].replace(",", "")
                if not lv.isdigit():
                    break  # 等级行结束（后面是别的表格）
                levels[lv] = {
                    norm_key(heads[c]): parse_num(cells[k + c]) for c in range(1, ncols)
                }

    def get(key):
        v = stats.get(key)
        return v if v else None

    return {
        "type": (get("Type") or "").lower() or None,
        "count": get("Count"),
        "hit_speed": get("Hit Speed"),
        "deploy_time": get("Deploy Time"),
        "speed": get("Speed"),
        "range": get("Range"),
        "target": get("Target"),
        "transport": get("Transport"),
        "levels": levels,
    }


def main():
    out_path = sys.argv[1] if len(sys.argv) > 1 else "assets/cards.json"

    list_html = fetch_cached(LIST_URL, "_list")
    base_cards = parse_list(list_html)
    print(f"found {len(base_cards)} cards on list page")

    cards, failed = [], []
    for n, base in enumerate(base_cards, 1):
        slug = slug_of(base["name"])
        card = {"slug": slug, **base}
        try:
            html = fetch_cached(f"{BASE}/zh/clash-royale-cards/{slug}", slug)
            if "Card Not Found" in html:
                raise ValueError("slug 404")
            card.update(parse_card_detail(html))
            lv11 = card["levels"].get("11", {})
            print(f"[{n}/{len(base_cards)}] {base['name']}: cost={base['cost']} "
                  f"{base['rarity']} lv11={lv11}")
        except Exception as e:
            failed.append(base["name"])
            print(f"[{n}/{len(base_cards)}] {base['name']}: FAILED {e}", file=sys.stderr)
        cards.append(card)

    with open(out_path, "w", encoding="utf-8") as f:
        json.dump(cards, f, ensure_ascii=False, indent=2)
    print(f"\nwrote {len(cards)} cards -> {out_path}")
    if failed:
        print(f"failed ({len(failed)}): {', '.join(failed)}")


if __name__ == "__main__":
    main()
