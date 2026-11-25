"""对话模型来源：本机 Ollama / 本地网关 / 任意 OpenAI 兼容 API。

为什么单独成模块：
  引导页（welcome）和中台设置抽屉要的是同一份事实 —— 现在用的是什么模型、
  这台机上还有哪些能选、换一个之后连不连得上。探测、校验、落盘这三件事只写一遍，
  否则两处各写一份，迟早对不上。

三种来源的区别只有一处是真的：
  - `gateway` 是中台自带的本地网关，除对话外还提供 /mcp 工具目录，
    模型能调本机的文件、检索、抓网页能力；
  - `ollama` / `openai` 都是标准 OpenAI 兼容端点，只能对话。
  所以 `kind` 只用来决定 UI 与提示语，请求路径统一按 OpenAI 兼容走，
  工具能不能用由 agent 运行时自己探（探不到就降级成纯对话，不报错）。
"""
from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

PROBE_TIMEOUT = 2.5

# 面板上直接可点的来源。前三条是本机，本机探不到就打「未运行」而不是藏起来 ——
# 用户装没装、开没开，得看得见才好判断。
PRESETS = [
    {"id": "gateway", "name": "中台本地网关", "kind": "gateway",
     "base": "http://127.0.0.1:8080",
     "hint": "自带工具，能用中台的文件、检索、抓网页"},
    {"id": "ollama", "name": "本机 Ollama", "kind": "ollama",
     "base": "http://127.0.0.1:11434",
     "hint": "模型跑在本机，不联网"},
    {"id": "lmstudio", "name": "LM Studio", "kind": "openai",
     "base": "http://127.0.0.1:1234/v1",
     "hint": "先在 LM Studio 里加载模型，这里才看得见"},
    {"id": "deepseek", "name": "DeepSeek", "kind": "openai",
     "base": "https://api.deepseek.com/v1",
     "hint": "填上 API Key 即可，按量计费"},
    {"id": "openai", "name": "OpenAI 官方", "kind": "openai",
     "base": "https://api.openai.com/v1",
     "hint": "需要能连出去的网络"},
    {"id": "custom", "name": "自定义端点", "kind": "openai", "base": "",
     "hint": "任何 OpenAI 兼容地址：vLLM、one-api、中转站都行"},
]

KINDS = {"gateway", "ollama", "openai"}


# ---------------------------------------------------------------- 地址归一

def norm_base(base: str) -> str:
    return str(base or "").strip().rstrip("/")


def openai_root(base: str) -> str:
    """把用户填的地址补成「OpenAI 兼容的根」。

    填 `http://127.0.0.1:11434` 和填 `http://127.0.0.1:11434/v1` 都算对 ——
    没人会记得住该不该带 /v1，两种都收。
    """
    b = norm_base(base)
    if not b:
        return ""
    return b if b.endswith("/v1") else b + "/v1"


def chat_url(base: str) -> str:
    return openai_root(base) + "/chat/completions"


def models_url(base: str) -> str:
    return openai_root(base) + "/models"


# ---------------------------------------------------------------- 探测

def _get_json(url: str, key: str = "", timeout: float = PROBE_TIMEOUT) -> dict:
    hdr = {"Accept": "application/json"}
    if key:
        hdr["Authorization"] = "Bearer " + key
    req = urllib.request.Request(url, headers=hdr)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8", "replace"))


def probe(base: str, key: str = "", timeout: float = PROBE_TIMEOUT) -> dict:
    """探一个端点能提供哪些模型。

    先按 OpenAI 兼容格式要 /v1/models；失败且地址像 Ollama 时再问 /api/tags ——
    Ollama 的 OpenAI 兼容层有时关着，但原生接口一直在。两条都不通才算不可用。
    """
    base = norm_base(base)
    if not base:
        return {"ok": False, "models": [], "reason": "没填地址"}
    try:
        data = _get_json(models_url(base), key, timeout)
        models = [m.get("id") for m in (data.get("data") or []) if m.get("id")]
        if models:
            return {"ok": True, "models": models[:200], "reason": "已连通"}
        reason = "连上了，但没列出模型"
    except (urllib.error.URLError, TimeoutError, OSError, ValueError) as exc:
        reason = _why(exc)

    try:
        data = _get_json(base + "/api/tags", key, timeout)
        models = [m.get("name") for m in (data.get("models") or []) if m.get("name")]
        if models:
            return {"ok": True, "models": models[:200], "reason": "Ollama 原生接口"}
        return {"ok": True, "models": [], "reason": "Ollama 在跑，但还没有模型"}
    except (urllib.error.URLError, TimeoutError, OSError, ValueError):
        return {"ok": False, "models": [], "reason": reason}


def _why(exc: Exception) -> str:
    """把 urllib 的一堆异常翻成一句人话。"""
    if isinstance(exc, urllib.error.HTTPError):
        if exc.code in (401, 403):
            return "鉴权没过，检查 API Key"
        if exc.code == 404:
            return "这个地址没有 /v1/models"
        return f"HTTP {exc.code}"
    if isinstance(exc, TimeoutError):
        return "连接超时"
    if isinstance(exc, ValueError):
        return "返回的不是 JSON"
    text = str(exc)
    if "Connection refused" in text or "10061" in text:
        return "端口没人应答（服务没启动？）"
    return text[:80]


# ---------------------------------------------------------------- 来源清单

def _local_source(p: dict, cur_base: str) -> dict:
    r = probe(p["base"])
    return {**p, "models": r.get("models") or [], "running": bool(r.get("ok")),
            "note": r.get("reason") or "", "current": norm_base(p["base"]) == cur_base}


def sources(cur_base: str = "", cur_key: str = "") -> list[dict]:
    """给面板用的完整清单：每条都带「现在跑没跑、有哪些模型」。

    三条本机来源并发探，总耗时压在单条超时以内 —— 串行探三个最坏要 7.5 秒，
    设置面板打开要等这么久是不能接受的。
    """
    local = [p for p in PRESETS if p["id"] in ("gateway", "ollama", "lmstudio")]
    with ThreadPoolExecutor(max_workers=3) as pool:
        rows = list(pool.map(lambda p: _local_source(p, norm_base(cur_base)), local))
    remote = [dict(p, models=[], running=False, note="", current=False)
              for p in PRESETS if p["id"] not in ("gateway", "ollama", "lmstudio")]
    for p in remote:
        if norm_base(p["base"]) and norm_base(p["base"]) == norm_base(cur_base):
            p["current"] = True
    return rows + remote


def mask_key(key: str) -> str:
    key = str(key or "")
    if not key:
        return ""
    if len(key) <= 8:
        return "•" * len(key)
    return key[:4] + "…" + key[-4:]


def state(cfg: dict, cur_model: str = "") -> dict:
    kind = str(cfg.get("model_kind") or "gateway")
    base = norm_base(cfg.get("gateway_base") or "")
    key = str(cfg.get("gateway_api_key") or "")
    return {
        "current": {
            "kind": kind if kind in KINDS else "gateway",
            "base": base,
            "model": str(cur_model or cfg.get("model") or ""),
            "key_set": bool(key),
            "key_mask": mask_key(key),
        },
        "sources": sources(base, key),
    }


# ---------------------------------------------------------------- 落盘

class ModelConfigError(ValueError):
    """用户填的东西不能用。"""


def _write_atomic(path: Path, cfg: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(cfg, ensure_ascii=False, indent=2), encoding="utf-8")
    try:
        os.chmod(tmp, 0o600)
    except OSError:
        pass
    os.replace(tmp, path)


def save(cfg: dict, cfg_path: Path, body: dict) -> dict:
    """把选择写进 config.json，返回新的当前状态。

    只动四个键：model_kind / gateway_base / gateway_api_key / model。
    地址里别带 /v1 也行，读的时候统一归一，写回去的保持用户原样（少改人家东西）。
    """
    kind = str(body.get("kind") or "gateway").strip()
    if kind not in KINDS:
        raise ModelConfigError(f"不认识的来源类型：{kind}")
    base = norm_base(body.get("base") or "")
    model = str(body.get("model") or "").strip()
    if not base:
        raise ModelConfigError("地址不能为空")
    if not model:
        raise ModelConfigError("模型名不能为空")

    # 密钥留空 = 沿用原来那把（页面上看到的是打码串，不能当明文写回去）
    if "key" in body and body.get("key") is not None and str(body.get("key")).strip():
        key = str(body.get("key")).strip()
    else:
        key = str(cfg.get("gateway_api_key") or "")

    cfg["model_kind"] = kind
    cfg["gateway_base"] = base
    cfg["gateway_api_key"] = key
    cfg["model"] = model
    _write_atomic(cfg_path, cfg)
    return {"kind": kind, "base": base, "model": model,
            "key_set": bool(key), "key_mask": mask_key(key)}


def apply_to(gw, cfg: dict) -> None:
    """把新配置贴到正在跑的网关客户端上，不重启进程。

    读的是 cfg 本身而不是 save() 的返回值 —— 返回值里的密钥是打码的，
    拿它去贴会把真 key 冲掉。

    工具缓存必须清掉：换了端点还留着上一个网关的工具表，模型会去调一堆
    根本执行不了的工具。
    """
    gw.base = norm_base(cfg.get("gateway_base") or "")
    gw.api_key = str(cfg.get("gateway_api_key") or "")
    gw.model = str(cfg.get("model") or "")
    gw._tools = None                                        # noqa: SLF001
    gw._tools_at = 0.0                                      # noqa: SLF001
    gw._no_tools_until = 0.0                                # noqa: SLF001
