# -*- coding: utf-8 -*-
"""Cherry Studio 的用量账本：把 ai_usage_record 折成人民币。

为什么不用它自己算的钱
    Cherry 每条记录都带 cost，但那套价是**美元单价**（deepseek-v4-flash 记
    $0.14/百万输入、$0.0028 缓存、$0.28 输出），跟百炼现在的人民币价
    （忙时 ¥3 / 缓存 ¥0.3 / 输出 ¥9，闲时减半）差一到两倍。直接采信 = 少算一半，
    所以这里按官方人民币价重算；只有价表里没有的模型才退回 Cherry 的美元值
    （按 USD_CNY 折算），并且标注清楚是「Cherry 自算」。

百炼的分时口径（官方）
    忙时 08:00–22:00，闲时 22:00–次日 08:00，东八区。缓存命中按输入价的 10%
    计（deepseek 系列），qwen 系列是固定价、不分时。

数据来源
    ~/Library/Application Support/CherryStudio/Data/cherrystudio.sqlite
    表 ai_usage_record：一次请求一行，input_tokens 已含缓存部分，
    cache_read_tokens 是其中命中缓存的那一段。
"""
from __future__ import annotations

import datetime as dt
import sqlite3
import time
from pathlib import Path

DB = (Path.home() / "Library" / "Application Support" / "CherryStudio"
      / "Data" / "cherrystudio.sqlite")

USD_CNY = 7.10          # Cherry 那套美元价折人民币用的比价，只为兜底
BUSY_FROM, BUSY_TO = 8, 22      # 百炼忙时窗口 [8, 22)

# 价表：元/百万 token。in/cache/out 都是 (闲时, 忙时)；不分时的模型两档写一样。
# 来源：阿里云百炼「模型推理计费」与各模型详情页（2026-11-21 取）。
PRICES: dict[str, dict] = {
    "deepseek-v4-flash-0731": {"in": (1.5, 3.0), "cache": (0.15, 0.3), "out": (4.5, 9.0)},
    "deepseek-v4.1-flash": {"in": (1.0, 2.0), "cache": (0.02, 0.04), "out": (4.0, 8.0)},
    "deepseek-v4.1-flash-1017": {"in": (1.0, 2.0), "cache": (0.02, 0.04), "out": (4.0, 8.0)},
    "deepseek-v4-pro-0813": {"in": (4.5, 9.0), "cache": (0.45, 0.9), "out": (13.5, 27.0)},
    "deepseek-v4-pro": {"in": (12.0, 12.0), "cache": (1.2, 1.2), "out": (24.0, 24.0)},
    "qwen3.5-flash": {"in": (0.2, 0.2), "cache": (0.02, 0.02), "out": (2.0, 2.0)},
    "qwen3.5-plus": {"in": (0.8, 0.8), "cache": (0.08, 0.08), "out": (8.0, 8.0)},
    "qwen3.7-flash": {"in": (0.2, 0.2), "cache": (0.04, 0.04), "out": (0.8, 0.8)},
    "qwen3.8-flash": {"in": (0.8, 0.8), "cache": (0.1, 0.1), "out": (2.7, 2.7)},
    # 硅基流动（真有调用时会用到；价格来自 siliconflow.cn/pricing）
    "deepseek-ai/deepseek-v3.2": {"in": (4.0, 4.0), "cache": (0.4, 0.4), "out": (6.0, 6.0)},
    "deepseek-ai/deepseek-v4-flash": {"in": (1.5, 3.0), "cache": (0.15, 0.3), "out": (4.5, 9.0)},
    "deepseek-ai/deepseek-v4.1-flash": {"in": (1.0, 2.0), "cache": (0.02, 0.04), "out": (4.0, 8.0)},
}

# 显示的厂商名（账上要写清这钱付给了谁）
VENDOR = {"dashscope": "阿里百炼", "silicon": "硅基流动"}

# Cherry 里这几家跑在本机，不花钱，压根不进账
LOCAL = {"lmstudio", "ollama", "cherryai", "72a4aecc-4dc3-43f2-82b2-82c7b4b73915"}

_CACHE: dict[str, object] = {"at": 0.0, "sig": None, "data": None}


def _sig(path: Path) -> tuple:
    try:
        st = path.stat()
        return (st.st_mtime_ns, st.st_size)
    except OSError:
        return (0, 0)


def _price_for(model_id: str, model_name: str) -> dict | None:
    for key in (model_id, model_name, (model_id or "").lower(), (model_name or "").lower()):
        if key in PRICES:
            return PRICES[key]
    # 名字里带版本后缀的（DeepSeek-V4-Flash-0731 之类）按前缀再找一遍
    low = (model_id or model_name or "").lower()
    for key in PRICES:
        if key and key in low:
            return PRICES[key]
    return None


def _hour(ms: int) -> int:
    return dt.datetime.fromtimestamp(ms / 1000).hour


def _load(days: int) -> dict:
    if not DB.exists():
        return {"found": False, "db": str(DB), "reason": "这台机器上没装 Cherry Studio",
                "cny": 0.0, "calls": 0, "models": [], "by_day": [], "unpriced": []}
    # 一次把全表读进来，再在内存里按时间窗分层：全期合计和「这段范围」合计要同时
    # 出（页面默认停在 7 天，只报 7 天会让人以为一共才花了那么点）。1871 行，很轻。
    sql = ("SELECT provider_id, provider_name, model_id, model_name, input_tokens,"
           " output_tokens, cache_read_tokens, cost, created_at FROM ai_usage_record")
    con = sqlite3.connect(f"file:{DB}?mode=ro", uri=True, timeout=4)
    try:
        rows = con.execute(sql).fetchall()
    finally:
        con.close()
    lo = int((time.time() - days * 86400) * 1000) if days else 0

    models: dict[tuple, dict] = {}
    by_day: dict[str, dict] = {}
    unpriced: dict[str, int] = {}
    tot_cny = tot_cherry = 0.0
    t_in = t_out = t_cache = 0
    n = 0
    all_cny = all_calls = 0             # 全期合计（不受 range 影响）
    first_ts = last_ts = 0
    # 近四个月、按供应商拆 —— 计费账号那张卡拿它跟官方账单对账，
    # 所以必须跟 range 无关（页面上停在 7 天，也得给出整月的估算）。
    # 给四个月是因为账单经常是「上个月才出」，那个月才有数可对。
    now = dt.datetime.now()
    ym_list = []
    _d = now
    for _ in range(4):
        ym_list.append(_d.strftime("%Y-%m"))
        _d = (_d.replace(day=1) - dt.timedelta(days=1))
    cur_ym = ym_list[0]
    months: dict[str, dict[str, dict]] = {y: {} for y in ym_list}

    for pid, pname, mid, mname, ti, to, tc, cost, ts in rows:
        if (pid or "") in LOCAL:
            continue
        ti, to, tc = int(ti or 0), int(to or 0), int(tc or 0)
        if not (ti or to):
            continue
        pr = _price_for(mid or "", mname or "")
        busy = BUSY_FROM <= _hour(ts or 0) < BUSY_TO
        k = 1 if busy else 0
        unc = max(ti - tc, 0)
        cny = None
        if pr:
            cny = (unc * pr["in"][k] + tc * pr["cache"][k] + to * pr["out"][k]) / 1e6
        cherry = (float(cost) * USD_CNY) if cost is not None else None
        if cny is None:
            if cherry is None:
                unpriced[str(mname or mid)] = unpriced.get(str(mname or mid), 0) + ti + to
                continue
            cny = cherry

        all_cny += cny
        all_calls += 1
        if ts and (not first_ts or ts < first_ts):
            first_ts = ts
        if ts and ts > last_ts:
            last_ts = ts
        if ts:
            _ym = dt.datetime.fromtimestamp(ts / 1000).strftime("%Y-%m")
            if _ym in months:
                mv = months[_ym].setdefault(pid or "", {"cny": 0.0, "calls": 0, "in": 0,
                                                        "out": 0, "cache": 0})
                mv["cny"] += cny
                mv["calls"] += 1
                mv["in"] += ti
                mv["out"] += to
                mv["cache"] += tc
        if lo and (ts or 0) < lo:
            continue                    # 这段范围之前的：只进全期合计

        key = (pid or "", mid or mname or "?")
        m = models.setdefault(key, {"provider": VENDOR.get(pid or "", pname or pid or "?"),
                                    "vendor_id": pid or "", "model": mname or mid or "?",
                                    "model_id": mid or "", "n": 0, "in": 0, "out": 0,
                                    "cache": 0, "cny": 0.0, "cny_cherry": 0.0,
                                    "priced": bool(pr), "busy_n": 0})
        m["n"] += 1
        m["in"] += ti
        m["out"] += to
        m["cache"] += tc
        m["cny"] += cny
        if cherry is not None:
            m["cny_cherry"] += cherry
        if busy:
            m["busy_n"] += 1

        d = dt.datetime.fromtimestamp(ts / 1000).strftime("%Y-%m-%d")
        b = by_day.setdefault(d, {"d": d, "cny": 0.0, "n": 0})
        b["cny"] += cny
        b["n"] += 1

        tot_cny += cny
        tot_cherry += cherry or 0.0
        t_in += ti
        t_out += to
        t_cache += tc
        n += 1

    out_models = []
    for m in sorted(models.values(), key=lambda x: -x["cny"]):
        m["hit"] = round(m["cache"] / m["in"], 3) if m["in"] else None
        m["cny"] = round(m["cny"], 2)
        m["cny_cherry"] = round(m["cny_cherry"], 2)
        out_models.append(m)

    return {
        "found": True, "db": str(DB), "days": days,
        "calls": n,
        "tokens": {"in": t_in, "out": t_out, "cache": t_cache,
                   "uncached": max(t_in - t_cache, 0)},
        "cny": round(tot_cny, 2),
        "cny_cherry": round(tot_cherry, 2),      # Cherry 自己按美元价算的，仅作对照
        "usd_cny": USD_CNY,
        "models": out_models,
        "by_day": [by_day[k] for k in sorted(by_day)],
        "unpriced": sorted(unpriced.items(), key=lambda kv: -kv[1])[:6],
        "busy_from": BUSY_FROM, "busy_to": BUSY_TO,
        "vendor_n": len({m["vendor_id"] for m in out_models}),
        # 全期（跟 range 无关）：页面上「这段 ¥x · 全期 ¥y」两个数都要
        "all": {"cny": round(all_cny, 2), "calls": all_calls,
                "since": (dt.datetime.fromtimestamp(first_ts / 1000).strftime("%Y-%m-%d")
                          if first_ts else ""),
                "until": (dt.datetime.fromtimestamp(last_ts / 1000).strftime("%Y-%m-%d")
                          if last_ts else "")},
        # 近四个月按供应商拆（跟 range 无关）：计费账号那张卡拿它对官方账单
        "months": [{"ym": y,
                    "cny": round(sum(v["cny"] for v in months[y].values()), 2),
                    "by_vendor": {k: {**v, "cny": round(v["cny"], 2)}
                                  for k, v in months[y].items()}}
                   for y in ym_list],
        # 头两个月单独给一份：前端老代码 / 对账默认都看这两个
        "month": {"ym": cur_ym,
                  "cny": round(sum(v["cny"] for v in months[cur_ym].values()), 2),
                  "by_vendor": {k: {**v, "cny": round(v["cny"], 2)}
                                for k, v in months[cur_ym].items()}},
        "prev": {"ym": ym_list[1],
                 "cny": round(sum(v["cny"] for v in months[ym_list[1]].values()), 2),
                 "by_vendor": {k: {**v, "cny": round(v["cny"], 2)}
                               for k, v in months[ym_list[1]].items()}},
        "at": dt.datetime.now().strftime("%H:%M:%S"),
    }


def view(days: int = 7) -> dict:
    """days=0 表示全部。库文件没变就复用上次结果（20 秒内）。"""
    sig = (_sig(DB), days)
    now = time.time()
    if _CACHE.get("data") and _CACHE.get("sig") == sig and now - float(_CACHE["at"] or 0) < 20:
        return dict(_CACHE["data"])            # type: ignore[arg-type]
    try:
        data = _load(days)
    except Exception as e:                      # noqa: BLE001
        data = {"found": DB.exists(), "db": str(DB), "days": days, "calls": 0,
                "tokens": {"in": 0, "out": 0, "cache": 0, "uncached": 0},
                "cny": 0.0, "cny_cherry": 0.0, "models": [], "by_day": [],
                "unpriced": [], "err": str(e)}
    _CACHE.update(at=now, sig=sig, data=data)
    return dict(data)


if __name__ == "__main__":
    import json
    import sys
    d = view(int(sys.argv[1]) if len(sys.argv) > 1 else 0)
    print(json.dumps(d, ensure_ascii=False, indent=1))
