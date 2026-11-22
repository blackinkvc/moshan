#!/usr/bin/env python3
"""图片文章 OCR 的定时调度：把 08-阅读 里「该转还没转」的图转成文字。

为什么单独一个 runner 而不是塞进 heartbeat：心跳是「服务是否活着」的巡检，
180 秒一轮；OCR 是「内容处理」，几分钟一轮、单篇可能跑几秒，节奏完全不同。
但两者同一个宿主——pocket 的 daemon 线程（launchd 常驻，本机装不了新 LaunchAgent）。

只做调度，识别逻辑全在 ~/Documents/cuntu/ocr.py：
  python ocr.py --sweep --json
"""
from __future__ import annotations

import json
import os
import subprocess
import time
from datetime import datetime
from pathlib import Path

# OCR 是可选增强：识别逻辑在 cuntu 仓库里。别人机器上没有这个目录时，
# 调度器会静默跳过（下方 exists 判断），不影响中台其余功能。
CUNTU = Path(os.environ.get("MOSHAN_CUNTU", "") or (Path.home() / "Documents" / "cuntu"))
DEFAULT_PY = str(CUNTU / ".venv/bin/python")
DEFAULT_SCRIPT = str(CUNTU / "ocr.py")

import paths  # 交付版：阅读目录走资料库，不写死开发机的 ~/Documents/AI项目
READ_ROOT = paths.data_root() / "08-阅读"
ARTICLE_PY = Path(__file__).with_name("article.py")
SUMMARY_PY = CUNTU / "summary.py"
UNCATEGORIZED = "未分类"

STATE: dict = {"last_at": "", "scanned": 0, "done": 0, "failed": 0,
               "classified": 0, "summarized": 0, "post_errors": [],
               "running": False, "last_error": "", "last_output": None}


def cfg_of(cfg: dict) -> dict:
    c = (cfg or {}).get("ocr") or {}
    return {
        "enabled": c.get("enabled", True),
        "interval": int(c.get("interval_sec", 300)),
        "python": c.get("python") or DEFAULT_PY,
        "script": c.get("script") or DEFAULT_SCRIPT,
        "limit": int(c.get("limit", 20)),
        "timeout": int(c.get("timeout_sec", 420)),
        "post": bool(c.get("post_process", True)),
        "post_timeout": int(c.get("post_timeout_sec", 600)),
    }


# ---------------------------------------------------------------- 收尾

def _meta_of(d: Path) -> dict:
    try:
        return json.loads((d / "article.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}


# 收尾失败（4B 没起来、模型没应答）的目录，半小时后再试。
# 不记这个的话，OCR 只在第一次完成时触发收尾，一次失败就永久停在未分类。
_RETRY: dict[str, float] = {}
RETRY_AFTER = 1800


def stuck_articles(limit: int = 2) -> list[dict]:
    """已经 OCR 完、但还挂在未分类的（上次收尾没成功）。"""
    root = READ_ROOT / UNCATEGORIZED
    out: list[dict] = []
    if not root.is_dir():
        return out
    now = time.time()
    for d in sorted(root.iterdir(), key=lambda p: p.stat().st_mtime if p.exists() else 0):
        if not d.is_dir() or d.name.startswith("."):
            continue
        meta = _meta_of(d)
        if meta.get("ocr_status") != "done":
            continue
        if (meta.get("topic") or UNCATEGORIZED) != UNCATEGORIZED:
            continue
        if _RETRY.get(str(d), 0) > now:
            continue
        out.append({"dir": str(d), "chars": int(meta.get("ocr_chars") or 0)})
        if len(out) >= limit:
            break
    return out


# ---------------------------------------------------------------- 补配图
#
# 公众号有文章是「文字 + 十张图」交替的：只抓文字等于丢一半内容，而且丢得毫无痕迹
# —— 正文看着完整，界面上跟「这篇本来没图」一模一样（用户原话「图就没有摘录」）。
# 抓取侧已经能拿到图了，但历史遗留的篇和偶发的 CDN 失败还要有人回头补，所以这里
# 每轮挑一两篇重抓：按 URL 重新拉一次，图落盘、正文按原位置插入，再交给 OCR。

IMG_RETRY: dict[str, float] = {}
IMG_RETRY_AFTER = 6 * 3600      # 一篇失败了 6 小时后再试，别一轮一轮地啃
IMG_MAX_TRIES = 3               # 试三次还是没有图，就当它本来就是纯文字文章


def missing_image_articles(limit: int = 2) -> list[dict]:
    """公众号文章里「正文一张图都没落盘」的，按最老的先补。

    优先补有 `images_failed` 记录的（确定丢过图）；没记录的也扫，
    因为老版本代码根本不记失败 —— 那篇「10 张图讲透 Agent」就是这么丢的。
    """
    out: list[dict] = []
    now = time.time()
    if not READ_ROOT.is_dir():
        return out
    cands: list[tuple[float, float, Path, dict]] = []
    for topic in sorted(READ_ROOT.iterdir()):
        if not topic.is_dir() or topic.name.startswith(("_", ".")):
            continue
        for d in topic.iterdir():
            if not d.is_dir() or not (d / "article.json").exists():
                continue
            meta = _meta_of(d)
            if (meta.get("platform") or "") != "公众号":
                continue
            if not meta.get("url"):
                continue
            if int(meta.get("inline_images") or 0):
                continue
            if int(meta.get("img_retry_count") or 0) >= IMG_MAX_TRIES:
                continue
            if IMG_RETRY.get(str(d), 0) > now:
                continue
            pri = 0.0 if meta.get("images_failed") else 1.0
            cands.append((pri, d.stat().st_mtime if d.exists() else 0, d, meta))
    for _pri, _mt, d, meta in sorted(cands, key=lambda x: (x[0], x[1]))[:limit]:
        out.append({"dir": str(d), "url": meta.get("url"), "chars": int(meta.get("chars") or 0)})
    return out


def refetch_missing(cfg: dict, limit: int = 2) -> dict:
    """把缺图的那几篇重抓一遍。成功后顺手把 OCR 结果作废，让它重新转。"""
    out = {"tried": 0, "fixed": 0, "errors": []}
    if not ARTICLE_PY.exists():
        return out
    c = cfg_of(cfg)
    for item in missing_image_articles(limit):
        d = Path(item["dir"])
        out["tried"] += 1
        r = _run_json(c["python"], ARTICLE_PY, ["refetch", str(d)],
                      c["post_timeout"], ARTICLE_PY.parent)
        meta = _meta_of(d)
        meta["img_retry_count"] = int(meta.get("img_retry_count") or 0) + 1
        meta["img_retry_at"] = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        got = int(r.get("images") or 0)
        if r.get("ok") and got:
            out["fixed"] += 1
            IMG_RETRY.pop(str(d), None)
            (d / "article.json").write_text(
                json.dumps(meta, ensure_ascii=False, indent=1), encoding="utf-8")
        else:
            IMG_RETRY[str(d)] = time.time() + IMG_RETRY_AFTER
            out["errors"].append(f"{d.name}：{(r.get('error') or '还是没图')[:80]}")
            try:
                (d / "article.json").write_text(
                    json.dumps(meta, ensure_ascii=False, indent=1), encoding="utf-8")
            except OSError:
                pass
    return out


def _run_json(py: str, script: Path, args: list[str], timeout: int,
              cwd: Path) -> dict:
    """跑一个会打印 JSON 的脚本（article.py 的约定：最后一个 {…} 是结果）。"""
    try:
        r = subprocess.run([py, str(script)] + args, capture_output=True, text=True,
                           timeout=timeout, cwd=str(cwd))
    except subprocess.TimeoutExpired:
        return {"ok": False, "error": f"超时（>{timeout} 秒）"}
    except OSError as e:
        return {"ok": False, "error": f"启动失败：{e}"}
    s = (r.stdout or "").strip()
    i, j = s.find("{"), s.rfind("}")
    if i < 0 or j <= i:
        tail = (r.stderr or "").strip().splitlines()
        return {"ok": False, "error": (tail[-1][:200] if tail else "无输出")}
    try:
        return json.loads(s[i:j + 1])
    except ValueError:
        return {"ok": False, "error": "输出不是 JSON"}


def post_process(cfg: dict, done: list) -> dict:
    """OCR 只把字写进正文，但「属于哪个主题、要不要摘要」是抓取那一刻定的事。

    那一刻正文只有一两句（微博长图典型就 14 个字），分类器判定「正文太短」
    直接拒了，文章就永远停在 未分类、没标签、没摘要——看起来像整条流水线失败，
    而实际上 OCR 是成功的。所以 OCR 完成后必须回头把这个尾巴补上：

      1. 还在 未分类 的 → 重新分类（分类会连目录一起挪到新主题下）
      2. 还没有摘要的 → 生成摘要

    只在主题仍是 未分类 时才重分类：用户手工分好的不覆盖。
    任何一步失败都只记错误、不抛异常——OCR 本身成功就该算成功。
    """
    c = cfg_of(cfg)
    out = {"classified": 0, "summarized": 0, "errors": []}
    if not c["post"]:
        return out
    if not ARTICLE_PY.exists():
        out["errors"].append(f"找不到 {ARTICLE_PY.name}")
        return out

    for item in done or []:
        if not isinstance(item, dict) or not item.get("dir"):
            continue
        if not int(item.get("chars") or 0):      # 图里没字，分类也无从谈起
            continue
        d = Path(item["dir"])
        meta = _meta_of(d)
        if not meta:
            continue

        if (meta.get("topic") or UNCATEGORIZED) == UNCATEGORIZED:
            r = _run_json(c["python"], ARTICLE_PY, ["classify", str(d)],
                          c["post_timeout"], ARTICLE_PY.parent)
            if r.get("ok"):
                out["classified"] += 1
                d = READ_ROOT / str(r.get("rel") or f"{UNCATEGORIZED}/{d.name}")
                meta = _meta_of(d)
            else:
                _RETRY[str(d)] = time.time() + RETRY_AFTER
                out["errors"].append(f"分类 {d.name}：{r.get('error', '失败')}")

        if not str(meta.get("summary") or "").strip():
            if not SUMMARY_PY.exists():
                out["errors"].append("找不到 summary.py，跳过摘要")
                continue
            try:
                subprocess.run([c["python"], str(SUMMARY_PY), str(d)],
                               capture_output=True, text=True,
                               timeout=c["post_timeout"], cwd=str(CUNTU))
            except (subprocess.TimeoutExpired, OSError) as e:
                out["errors"].append(f"摘要 {d.name}：{type(e).__name__}")
                continue
            if str(_meta_of(d).get("summary") or "").strip():
                out["summarized"] += 1
            else:
                out["errors"].append(f"摘要 {d.name}：没生成出来")
    return out


def run_once(cfg: dict) -> dict:
    c = cfg_of(cfg)
    if not c["enabled"]:
        return {"ok": True, "skipped": "disabled"}
    if not Path(c["script"]).exists():
        return {"ok": False, "error": f"找不到 {c['script']}"}
    STATE["running"] = True
    t0 = time.time()
    try:
        # 先补图再 OCR：公众号的图没落盘时，正文里连图都没有，
        # 这一轮就算转也没东西可转。补完图当轮就能顺带转出来。
        try:
            imgfix = refetch_missing(cfg)
        except Exception as e:  # noqa: BLE001      补图失败不能拖垮整轮
            imgfix = {"tried": 0, "fixed": 0, "errors": [str(e)[:120]]}
        r = subprocess.run(
            [c["python"], c["script"], "--sweep", "--json"],
            capture_output=True, text=True, timeout=c["timeout"],
            cwd=str(CUNTU))
        out = {}
        for line in reversed((r.stdout or "").strip().splitlines()):
            line = line.strip()
            if line.startswith("{"):
                try:
                    out = json.loads(line)
                    break
                except ValueError:
                    continue
        done = out.get("done") or []
        failed = out.get("failed") or []
        # OCR 成功 ≠ 处理完成：主题、标签、摘要要跟着补，否则文章永远停在「未分类」
        post = post_process(cfg, done)
        # 上一轮收尾没成功的（比如 4B 当时没应答）顺手再试一次，别让它永久卡住
        left = stuck_articles()
        if left:
            more = post_process(cfg, left)
            post["classified"] += more["classified"]
            post["summarized"] += more["summarized"]
            post["errors"] += more["errors"]
        rec = {
            "ok": r.returncode == 0,
            "at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
            "scanned": int(out.get("scanned") or 0),
            "done": len(done), "failed": len(failed),
            "chars": sum(int(x.get("chars") or 0) for x in done if isinstance(x, dict)),
            "classified": post["classified"], "summarized": post["summarized"],
            "img_fixed": imgfix.get("fixed", 0),
            "post_errors": (post["errors"] + imgfix.get("errors", []))[:5],
            "seconds": round(time.time() - t0, 1),
            "error": "" if r.returncode == 0
                     else ((r.stderr or "").strip().splitlines() or ["失败"])[-1][:200],
        }
        STATE.update({"last_at": rec["at"], "scanned": rec["scanned"],
                      "done": rec["done"], "failed": rec["failed"],
                      "classified": rec["classified"],
                      "summarized": rec["summarized"],
                      "img_fixed": rec["img_fixed"],
                      "post_errors": rec["post_errors"],
                      "last_error": rec["error"], "last_output": rec})
        return rec
        return rec
    except subprocess.TimeoutExpired:
        STATE["last_error"] = "OCR 超时"
        return {"ok": False, "error": "OCR 超时", "at": STATE["last_at"]}
    except Exception as e:  # noqa: BLE001
        STATE["last_error"] = str(e)[:200]
        return {"ok": False, "error": str(e)[:200], "at": STATE["last_at"]}
    finally:
        STATE["running"] = False


def scheduler_loop(cfg: dict, on_done=None) -> None:
    """与 dedupe/heartbeat 同构的 daemon 循环；启动先跑一次，异常不死。"""
    c = cfg_of(cfg)
    while True:
        try:
            out = run_once(cfg)
            if on_done:
                on_done(out)
        except Exception as e:               # 定时线程不能因为一次异常就退出
            if on_done:
                on_done({"ok": False, "error": str(e)})
        time.sleep(c["interval"])


def _main() -> None:
    import argparse
    ap = argparse.ArgumentParser(description="图片文章 OCR 定时调度")
    ap.add_argument("--once", action="store_true", help="只跑一次")
    ap.add_argument("--json", action="store_true", help="输出 JSON")
    args = ap.parse_args()
    cfg = {}
    try:
        cfg = json.loads((Path(__file__).with_name("config.json"))
                         .read_text(encoding="utf-8"))
    except (OSError, ValueError):
        pass
    if args.once:
        out = run_once(cfg)
        print(json.dumps(out, ensure_ascii=False, indent=2) if args.json
              else f"OCR 扫描 {out.get('scanned', 0)} 篇："
                   f"成功 {out.get('done', 0)}，失败 {out.get('failed', 0)}")
        return
    scheduler_loop(cfg)


if __name__ == "__main__":
    _main()
