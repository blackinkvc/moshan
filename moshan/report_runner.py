#!/usr/bin/env python3
"""使用报告的常驻调度：采样 → 日聚合 → 出报告 → 归档。

挂在 pocket 的 daemon 线程里，跟 heartbeat 并列但完全独立：
  - 采集自己跑（120 秒一次），不往 heartbeat 里塞代码
    （heartbeat 会被 syscheck 用 importlib 单独导入，多一个 import
    就可能把「巡检判定源只有一处」搞坏）
  - 出报告每小时看一次，幂等：已有文件的周期直接跳过

为什么每小时都跑一遍而不是卡在 00:05：
  机器会关机。卡点跑的调度一旦错过那个时刻就永久漏一期，
  所以这里只看「已结束的周期里有没有缺文件」，开机后自动补齐。
  代价是每小时几个 glob，可以忽略。
"""
from __future__ import annotations

import threading
import time
import traceback
from datetime import date

import metrics
import report

SAMPLE_EVERY = 120          # 与心跳同频，两个线程各跑各的
REPORT_EVERY = 3600
FIRST_DELAY = 20            # 启动时先让服务把端口、路由挂完

STATE = {"started": 0.0, "samples": 0, "last_sample": "", "last_run": 0.0,
         "last_made": [], "last_removed": [], "error": "", "runs": 0}
_LOCK = threading.Lock()
LOGF = None


def _log(entry: dict) -> None:
    if LOGF:
        try:
            LOGF(entry)
        except Exception:
            pass


def tick(now: float | None = None) -> dict:
    now = now or time.time()
    out = {"sampled": False, "made": [], "removed": []}

    # 采样
    try:
        rec = metrics.append_sample()
        with _LOCK:
            STATE["samples"] += 1
            STATE["last_sample"] = rec.get("at") or ""
        out["sampled"] = True
    except Exception as e:                      # 采样失败绝不能把线程弄死
        STATE["error"] = f"sample: {e}"[:200]
        _log({"event": "report_sample_error", "error": str(e)[:200]})

    # 出报告
    if now - STATE["last_run"] >= REPORT_EVERY:
        try:
            res = report.run()
            STATE["last_run"] = now
            STATE["runs"] += 1
            STATE["last_made"] = res.get("made") or []
            STATE["last_removed"] = res.get("removed") or []
            out["made"] = STATE["last_made"]
            out["removed"] = STATE["last_removed"]
            if out["made"] or out["removed"]:
                _log({"event": "report_done",
                      "made": [f"{m['kind']}:{m['key']}" for m in out["made"]],
                      "removed": out["removed"]})
        except Exception as e:
            STATE["error"] = f"report: {e}"[:200]
            _log({"event": "report_error", "error": str(e)[:200],
                  "trace": traceback.format_exc()[-600:]})
        # 顺手做两件低频维护
        try:
            if date.today().day == 1:           # 每月 1 号裁一次快照
                metrics.trim_snapshot()
        except Exception:
            pass
    return out


def _loop() -> None:
    time.sleep(FIRST_DELAY)
    STATE["started"] = time.time()
    while True:
        try:
            tick()
        except Exception:
            pass
        time.sleep(SAMPLE_EVERY)


def start(log=None) -> None:
    global LOGF
    LOGF = log
    if STATE["started"]:
        return
    threading.Thread(target=_loop, name="report-runner", daemon=True).start()
    STATE["started"] = time.time()


def state() -> dict:
    return dict(STATE)


if __name__ == "__main__":
    import json
    print(json.dumps(tick(now=time.time()), ensure_ascii=False, indent=2))
    print(json.dumps(state(), ensure_ascii=False, indent=2))
