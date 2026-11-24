"""首启引导的后端能力：探测环境、拉取模型、检查网关连通。

设计说明：
  模型不走「自己写 GGUF 下载器」，而是调用 Ollama 的 pull —— 原因有三个：
    1. Ollama 自带断点续传、分片校验、多源回退，比手写下载器可靠；
    2. 模型文件放 Ollama 自己的目录，卸载重装 Moshan 不会重复下载 2.4GB；
    3. 用户若已装 Ollama，直接复用，不产生第二份副本。
  代价是依赖 Ollama。所以同时提供「自定义 OpenAI 兼容端点」分支，
  让已有 LM Studio / vLLM / 云 API 的用户不必装 Ollama。
  全程不阻塞：任何一步失败都只降级对应能力，中台的文件、分享、传输照常可用。
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Callable, Optional

import paths

OLLAMA_DEFAULT = "http://127.0.0.1:11434"
PROBE_TIMEOUT = 2.5
PULL_LOG_LIMIT = 400  # 内存里最多留多少行 pull 输出


class BootstrapError(Exception):
    """引导过程中的可预期失败。"""


# ---------------------------------------------------------------- 状态容器

_STATE: dict = {
    "pull": {"running": False, "model": "", "lines": [], "ok": None, "error": ""},
    "lock": threading.Lock(),
}


def _append(line: str) -> None:
    with _STATE["lock"]:
        buf = _STATE["pull"]["lines"]
        buf.append(line.rstrip())
        if len(buf) > PULL_LOG_LIMIT:
            del buf[: len(buf) - PULL_LOG_LIMIT]


# ---------------------------------------------------------------- Ollama 探测

def detect_ollama(base: str = OLLAMA_DEFAULT) -> dict:
    """返回 {installed, running, base, models, version}。

    installed 看命令是否存在；running 看 HTTP 端口是否应答。两者独立，
    因为「装了没启动」是最常见的情况，提示语完全不同。
    """
    exe = shutil.which("ollama")
    result = {"installed": bool(exe), "running": False, "base": base,
              "models": [], "version": "", "exe": exe or ""}

    try:
        with urllib.request.urlopen(f"{base.rstrip('/')}/api/tags", timeout=PROBE_TIMEOUT) as resp:
            data = json.loads(resp.read().decode("utf-8"))
        result["running"] = True
        result["models"] = [m.get("name") for m in (data.get("models") or []) if m.get("name")]
    except (urllib.error.URLError, TimeoutError, OSError, ValueError):
        pass

    if exe:
        try:
            out = subprocess.run([exe, "--version"], capture_output=True, text=True,
                                 timeout=5, **paths.hide_console())
            result["version"] = (out.stdout or out.stderr or "").strip().splitlines()[0][:60]
        except (OSError, subprocess.SubprocessError, IndexError):
            pass
    return result


def has_model(model: str, base: str = OLLAMA_DEFAULT) -> bool:
    """模型名可能带或不带 :latest 后缀，两种都比一次。"""
    info = detect_ollama(base)
    want = model.strip()
    if want in info["models"]:
        return True
    if ":" not in want and f"{want}:latest" in info["models"]:
        return True
    return False


# ---------------------------------------------------------------- 拉取模型

def pull_async(model: str, base: str = OLLAMA_DEFAULT) -> dict:
    """后台拉取模型，进度写进 _STATE，前端轮询 /welcome/status 取。

    故意用 subprocess 调 ollama CLI 而不是 HTTP 的 /api/pull：
    CLI 已经处理了「Ollama 没启动就自己拉起」这类边界，输出也更规整。
    """
    with _STATE["lock"]:
        if _STATE["pull"]["running"]:
            return {"started": False, "reason": "已有拉取任务在进行"}
        _STATE["pull"] = {"running": True, "model": model, "lines": [], "ok": None, "error": ""}

    exe = shutil.which("ollama")
    if not exe:
        with _STATE["lock"]:
            _STATE["pull"].update(running=False, ok=False, error="未安装 Ollama")
        raise BootstrapError("未检测到 ollama 命令，请先安装 Ollama 再拉取模型")

    def worker() -> None:
        try:
            env = dict(os.environ)
            env.setdefault("OLLAMA_HOST", base.replace("http://", "").replace("https://", ""))
            proc = subprocess.Popen([exe, "pull", model], stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, text=True,
                                    bufsize=1, env=env)
            _append(f"$ ollama pull {model}")
            assert proc.stdout is not None
            for line in proc.stdout:
                _append(line)
            code = proc.wait()
            with _STATE["lock"]:
                _STATE["pull"].update(running=False, ok=(code == 0),
                                      error="" if code == 0 else f"ollama pull 退出码 {code}")
        except Exception as exc:  # noqa: BLE001 —— 后台线程不能把异常抛给用户界面
            with _STATE["lock"]:
                _STATE["pull"].update(running=False, ok=False, error=str(exc))
            _append(f"[错误] {exc}")

    threading.Thread(target=worker, name="moshan-model-pull", daemon=True).start()
    return {"started": True, "model": model}


def pull_status() -> dict:
    with _STATE["lock"]:
        return {
            "running": _STATE["pull"]["running"],
            "model": _STATE["pull"]["model"],
            "ok": _STATE["pull"]["ok"],
            "error": _STATE["pull"]["error"],
            "tail": _STATE["pull"]["lines"][-12:],
            "lines": len(_STATE["pull"]["lines"]),
        }


# ---------------------------------------------------------------- 网关连通

def probe_gateway(base: str, api_key: str = "", timeout: float = 4.0) -> dict:
    """探一下 OpenAI 兼容端点能否列出模型。失败不算错误，只是「不可用」。"""
    if not base:
        return {"ok": False, "reason": "未配置网关地址", "models": []}
    url = base.rstrip("/") + "/v1/models"
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {api_key or 'none'}"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.loads(resp.read().decode("utf-8"))
        models = [m.get("id") for m in (data.get("data") or []) if m.get("id")]
        return {"ok": True, "reason": "已连通", "models": models[:50]}
    except (urllib.error.URLError, TimeoutError, OSError, ValueError) as exc:
        return {"ok": False, "reason": f"连接失败：{exc}", "models": []}


# ---------------------------------------------------------------- 首启标记

def marker_file() -> Path:
    return paths.state_dir() / "welcome.done"


def is_first_run() -> bool:
    return not marker_file().exists()


def mark_done() -> None:
    paths.state_dir().mkdir(parents=True, exist_ok=True)
    marker_file().write_text(
        json.dumps({"at": time.strftime("%Y-%m-%dT%H:%M:%S%z")}, ensure_ascii=False),
        encoding="utf-8",
    )


def summary() -> dict:
    """引导页首屏要展示的环境快照。"""
    ollama = detect_ollama()
    return {
        "first_run": is_first_run(),
        "ollama": ollama,
        "platform": os.name,
        "dirs": paths.describe(),
    }
