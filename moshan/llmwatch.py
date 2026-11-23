#!/usr/bin/env python3
"""本机大模型探测 —— 「这台机器现在跑着什么模型」。

为什么单独一个模块：进程卡能告诉你「llama-server.exe 吃了 20 GB」，但说不出
它跑的是哪个模型。而 20 GB 内存这件事，**知道跑的是什么**才算解释得清：
Qwen3-30B-A3B 的 Q4 量化占 20 GB 是正常的，同一个 20 GB 如果跑的是 4B 模型，
那就是配错了。所以「谁在吃内存」和「跑的是什么」必须一起看。

三层取数，逐层更准，前一层失败不影响后一层：

1. **进程层**（一定有）：扫进程名/命令行，认 llama.cpp / Ollama / LM Studio /
   vLLM / SGLang / KoboldCpp / MLX / GPT4All / Jan 这些推理服务。
2. **命令行层**：从 argv 抠模型路径、端口、上下文长度、GPU 层数。
   模型名就在路径里（`Qwen3-30B-A3B-Instruct-Q4_K_M.gguf`），量化等级和参数量
   一并解析出来。
3. **服务层**（最准）：直接问那个端口自己。llama.cpp 的 `/props` 给
   `model_path` + `n_ctx`；OpenAI 兼容的 `/v1/models` 给模型 id；
   Ollama 的 `/api/ps` 给「已加载的模型 + 显存占用」。

约定：
- 只探 127.0.0.1，超时 0.8 秒，且**必须绕开系统代理** —— macOS 上开着
  `http_proxy` 时 urllib 会把本机请求也发给代理，结果是「探不到」而不是「没有」。
- 探不到不是错误。空列表就是这台机器上没在跑本地模型，卡片上照实说。
- 结果缓存 20 秒：`/api/overview` 30 秒刷一轮，跟着它走就够，不必每次都探。
"""
from __future__ import annotations

import json
import os
import re
import socket
import threading
import time
import urllib.error
import urllib.request

CACHE_TTL = 20.0
PROBE_TIMEOUT = 0.8

# 认得的推理服务：进程名（小写）→ 给人看的族名。
# 只认「服务端」形态：这些程序跑起来就是在对外提供推理，和普通进程好区分。
_FAMILY_BY_NAME = {
    "llama-server": "llama.cpp",
    "llama-server.exe": "llama.cpp",
    "llama_server": "llama.cpp",
    "server.exe": "llama.cpp",              # llama.cpp 的 Windows 产物也这么叫
    "ollama": "Ollama",
    "ollama.exe": "Ollama",
    "ollama_llama_server": "Ollama",
    "ollama_llama_server.exe": "Ollama",
    "lm studio.exe": "LM Studio",
    "lmstudio": "LM Studio",
    "koboldcpp.exe": "KoboldCpp",
    "koboldcpp": "KoboldCpp",
    "llama-box.exe": "llama-box",
    "llama-box": "llama-box",
    "jan.exe": "Jan",
    "gpt4all": "GPT4All",
    "localai": "LocalAI",
}

# 命令行里出现这些模块名，说明这个 python 进程在跑推理服务。
_FAMILY_BY_ARGV = (
    ("vllm.entrypoints", "vLLM"),
    ("vllm serve", "vLLM"),
    ("sglang.launch_server", "SGLang"),
    ("mlx_lm.server", "MLX-LM"),
    ("mlx_lm", "MLX-LM"),
    ("text_generation.launch", "text-generation-webui"),
    ("oobabooga", "text-generation-webui"),
)

# argv 里的取值型开关：前者是模型，后者是运行参数。
_MODEL_FLAGS = {"-m", "--model", "--model-path", "--model_file",
                "--gguf", "-hf", "--hf-repo", "-hfr", "--repo-id"}
_PORT_FLAGS = {"--port", "-p", "--api-port", "--serve-port"}
_CTX_FLAGS = {"-c", "--ctx-size", "--ctx", "--max-model-len",
              "--context-length", "-contextlength"}
_NGL_FLAGS = {"-ngl", "--n-gpu-layers", "--n_gpu_layers"}
_ALIAS_FLAGS = {"-a", "--alias", "--model-alias"}

# 没写 --port 时挨个试这几个。都是各家默认值，不含中台自己的端口。
_DEFAULT_PORTS = (8080, 11434, 1234, 8000, 5001, 8081)

_QUANT = re.compile(r"(IQ\d[\w]*|Q\d[\w]*|BF16|FP16|F16|F32|FP32)", re.I)
_SIZE = re.compile(r"(?<![\d.])(\d{1,3}(?:\.\d)?)\s*([BM])(?![a-z])", re.I)

_LOCK = threading.Lock()
_CACHE: dict = {"at": 0.0, "val": None}


# ------------------------------------------------------------------ 进程
def _iter_procs():
    """产出 (pid, ppid, name, argv, rss_mb, age_s)。

    Windows 与 macOS 都优先走 psutil（`requirements.txt` 里已经装了：Windows 的
    进程采样靠它）。psutil 不在时 POSIX 退回 `ps` —— 这行命令每台 mac 都有，
    而 Windows 上没有 `ps`（PowerShell 里 `ps` 是 Get-Process 的别名，不是同一回事）。
    """
    try:
        import psutil
    except ImportError:
        psutil = None

    now = time.time()
    if psutil is not None:
        for p in psutil.process_iter(["pid", "ppid", "name", "cmdline",
                                      "memory_info", "create_time"]):
            info = p.info or {}
            name = (info.get("name") or "")
            argv = info.get("cmdline") or []
            if not name and not argv:
                continue
            mi = info.get("memory_info")
            rss = float(getattr(mi, "rss", 0)) / 1048576.0 if mi is not None else 0.0
            ct = info.get("create_time") or 0
            yield (int(info.get("pid") or 0), int(info.get("ppid") or 0),
                   name, list(argv), rss, round(now - ct) if ct else None)
        return

    if os.name == "nt":                    # 没有 psutil 又没有 ps：认了
        return
    try:
        import subprocess
        out = subprocess.run(["ps", "-Ao", "pid=,ppid=,rss=,command="],
                             capture_output=True, text=True, timeout=8).stdout
    except (OSError, subprocess.SubprocessError):
        return
    for line in out.splitlines():
        parts = line.strip().split(None, 3)
        if len(parts) < 4:
            continue
        try:
            pid = int(parts[0])
            ppid = int(parts[1])
            rss = float(parts[2]) / 1024.0
        except ValueError:
            continue
        yield (pid, ppid, "", parts[3].split(), rss, None)


def _family(name: str, argv: list[str]) -> str:
    low = (name or "").strip().lower()
    if low in _FAMILY_BY_NAME:
        return _FAMILY_BY_NAME[low]
    # 子串兜底：`llama-server-3.2`、`Ollama.exe` 之类带版本号的产物名
    for key, fam in _FAMILY_BY_NAME.items():
        if key.endswith(".exe"):
            continue
        if key in low:
            return fam
    hay = " ".join(argv).lower()
    for key, fam in _FAMILY_BY_ARGV:
        if key in hay:
            return fam
    return ""


def _take(argv: list[str], flags: set[str]) -> str:
    for i, a in enumerate(argv):
        low = a.lower()
        if low in flags and i + 1 < len(argv):
            nxt = argv[i + 1]
            if not nxt.startswith("-"):
                return nxt
        for f in flags:
            if low.startswith(f + "="):
                return a[len(f) + 1:]
    return ""


def _num(s: str) -> int | None:
    try:
        return int(str(s).strip())
    except (TypeError, ValueError):
        try:
            return int(float(str(s).strip()))
        except (TypeError, ValueError):
            return None


def _pretty(path: str) -> dict:
    """从一个模型路径/ID 里读出「叫什么、多大、什么量化」。"""
    base = re.split(r"[\\/]", path.strip())[-1] or path.strip()
    stem = re.sub(r"\.(gguf|safetensors|bin|pt|pth|mlx)$", "", base, flags=re.I)
    q = _QUANT.search(stem)
    s = _SIZE.search(stem)
    return {
        "name": stem or path,
        "quant": q.group(1).upper() if q else "",
        "size": (s.group(1) + s.group(2).upper()) if s else "",
    }


# ------------------------------------------------------------------ 探端口
def _get_json(port: int, path: str) -> dict | None:
    url = f"http://127.0.0.1:{port}{path}"
    # 关键：HTTP_PROXY 会把 127.0.0.1 的请求也送进代理，代理当然不认识这个端口。
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    try:
        with opener.open(url, timeout=PROBE_TIMEOUT) as r:
            if r.status != 200:
                return None
            raw = r.read(262144)
    except (urllib.error.URLError, OSError, ValueError):
        return None
    try:
        d = json.loads(raw.decode("utf-8", "replace"))
    except (ValueError, UnicodeDecodeError):
        return None
    return d if isinstance(d, dict) else None


def _probe_llama_cpp(port: int) -> dict | None:
    """llama.cpp 的 /props —— 最准的一份：模型绝对路径 + 实际上下文长度。"""
    d = _get_json(port, "/props")
    if not d:
        return None
    path = str(d.get("model_path") or d.get("model_alias") or "")
    if not path and not d.get("default_generation_settings"):
        return None
    gen = d.get("default_generation_settings") or {}
    ctx = _num(gen.get("n_ctx")) if isinstance(gen, dict) else None
    ngl = _num(gen.get("n_gpu_layers")) if isinstance(gen, dict) else None
    return {"path": path, "ctx": ctx, "ngl": ngl, "source": "props"}


def _probe_openai(port: int) -> dict | None:
    """OpenAI 兼容口：LM Studio / vLLM / SGLang / LocalAI / llama.cpp 都有。"""
    d = _get_json(port, "/v1/models")
    if not d:
        return None
    data = d.get("data")
    if not isinstance(data, list) or not data:
        return None
    ids = [str((x or {}).get("id") or "").strip() for x in data if isinstance(x, dict)]
    ids = [i for i in ids if i]
    if not ids:
        return None
    return {"path": ids[0], "models": ids, "source": "v1"}


def _probe_ollama(port: int = 11434) -> list[dict]:
    """Ollama 的 /api/ps：已加载的模型，连显存占用一起给。"""
    d = _get_json(port, "/api/ps")
    if not d:
        return []
    out = []
    for m in (d.get("models") or []):
        if not isinstance(m, dict):
            continue
        nm = str(m.get("name") or m.get("model") or "").strip()
        if not nm:
            continue
        out.append({
            "path": nm,
            "vram_gb": round(float(m.get("size_vram") or 0) / 1073741824, 1),
            "expires": str(m.get("expires_at") or ""),
            "source": "ps",
        })
    return out


def _listening(port: int) -> bool:
    """端口通不通，先摸一下再打 HTTP —— 免得每次等满 0.8 秒超时。"""
    s = socket.socket()
    s.settimeout(0.25)
    try:
        return s.connect_ex(("127.0.0.1", port)) == 0
    finally:
        s.close()


def _port_owner() -> dict[int, int]:
    """本机在听的端口 → 占着它的 pid。

    用来把「探到一个模型，但不知道是哪个服务」归位：8081 上答 /v1/models 的
    到底是谁 —— 是 LM Studio 还是别的。没有 psutil、或系统不给看时返回空表，
    调用方照常降级成「OpenAI 兼容」，不影响主结论。
    """
    try:
        import psutil
    except ImportError:
        return {}
    try:
        conns = psutil.net_connections(kind="inet")
    except Exception:                                      # noqa: BLE001
        return {}
    out: dict[int, int] = {}
    for c in conns:
        try:
            if c.status != psutil.CONN_LISTEN or not c.laddr or not c.pid:
                continue
            out.setdefault(int(c.laddr.port), int(c.pid))
        except (AttributeError, TypeError, ValueError):
            continue
    return out


# ------------------------------------------------------------------ 主入口
def _is_ollama_blob(argv: list[str], name: str) -> bool:
    """这是 Ollama 自己拉起来的那个 llama-server 子进程吗。

    Ollama 不自己推理，它把模型交给一个 `llama-server` 子进程，命令行里的模型
    路径是 `~/.ollama/models/blobs/sha256-…`。不认出来的话，界面上就会出现一条
    「模型名叫 sha256-a9d0a3e1…」的怪东西，而真正的名字（`qwen3.5-4b-gguf:q4km`）
    只有 `ollama serve` 的 `/api/ps` 知道。
    """
    hay = " ".join(argv).lower()
    return (".ollama/models/blobs" in hay
            or "lib/ollama/llama-server" in hay
            or "ollama_llama_server" in (name or "").lower())


# 这些是「宿主型」推理服务：自己不推理，拉子进程（llama-server）干活，
# 对外只开一个 API 端口。模型认它们的孩子，不认它们本身 ——
# 否则 LM Studio 那个空壳 GUI 也会被列成一条「模型」。
SERVICE_FAMS = {"LM Studio", "Ollama", "Jan", "GPT4All", "LocalAI"}

# 子进程用这些名字跑（llama.cpp 家族）
_WORKER_FAMS = {"llama.cpp", "llama-box"}


def _norm(s: str) -> str:
    return re.sub(r"[^a-z0-9]", "", (s or "").lower())


def _best_api_port(name: str, probes: dict, used: set[int]) -> int | None:
    """这个模型是通过哪个端口对外服务的。

    LM Studio 会给每个加载的模型起一个内部端口（实测 62253 这种），用户真正
    调的是它前面那个 API 口（1234）。判断办法：拿模型的规范名去对 `/v1/models`
    报出来的 id —— LM Studio 的 id 就是模型名的缩写版。
    """
    n = _norm(name)
    if not n:
        return None
    for p, got in probes.items():
        if p in used:
            continue
        for mid in got.get("models") or []:
            m = _norm(mid)
            if m and (m in n or n.startswith(m)):
                return p
    return None


def _build() -> dict:
    cands: list[dict] = []
    by_pid: dict[int, dict] = {}
    for pid, ppid, name, argv, rss, age in _iter_procs():
        fam = _family(name, argv)
        if not fam:
            continue
        blob = _is_ollama_blob(argv, name)
        c = {
            "pid": pid, "ppid": ppid, "argv": argv,
            "family": "Ollama" if blob else fam,
            "blob": blob,
            "rss_gb": round(rss / 1024.0, 1), "age": age,
            "model_hint": _take(argv, _MODEL_FLAGS),
            "alias": _take(argv, _ALIAS_FLAGS),
            "port": _num(_take(argv, _PORT_FLAGS)),
            "ctx": _num(_take(argv, _CTX_FLAGS)),
            "ngl": _num(_take(argv, _NGL_FLAGS)),
        }
        cands.append(c)
        by_pid[pid] = c

    # Ollama 的子进程：既按 blob 路径认，也按父进程认（老版本路径不在这里也认得出）
    ollama_main = {c["pid"] for c in cands if c["family"] == "Ollama" and not c["blob"]}
    for c in cands:
        if c["blob"] or c["ppid"] in ollama_main:
            c["family"], c["child"] = "Ollama", True

    # 宿主型服务的子进程：归到父进程那个族去（LM Studio 的 llama-server 也算 LM Studio）
    for c in cands:
        par = by_pid.get(c["ppid"])
        if par and par["family"] in SERVICE_FAMS and c["family"] in _WORKER_FAMS:
            c["parent"], c["family"] = par, par["family"]

    # 探哪些端口：命令行写了的先探，Ollama 固定 11434，其余试各家默认值。
    ports: list[int] = []
    for c in cands:
        if c["port"] and not c.get("parent"):
            ports.append(int(c["port"]))
    if any(c["family"] == "Ollama" for c in cands) and 11434 not in ports:
        ports.append(11434)
    for p in _DEFAULT_PORTS:
        if p not in ports:
            ports.append(p)

    probes: dict[int, dict] = {}
    for p in ports:
        if not _listening(p):
            continue
        got: dict = {}
        props = _probe_llama_cpp(p)
        if props:
            got.update({k: v for k, v in props.items() if v is not None})
        openai = _probe_openai(p)
        if openai:
            # 别让 /v1/models 的 id 盖掉 /props 给的真实路径：Ollama 那边 id
            # 就是一串 sha256，路径反而信息量更大。
            got.setdefault("path", openai["path"])
            got.setdefault("models", openai["models"])
            got.setdefault("source", openai["source"])
        if p == 11434:
            ps = _probe_ollama(p)
            if ps:
                got["ollama"] = ps
        if got:
            probes[p] = got

    models: list[dict] = []
    used_ports: set[int] = set()
    used_pids: set[int] = set()

    def add(path: str, *, family: str, pid, port, rss: float = 0.0,
            ctx=None, ngl=None, source: str = "argv", extra: dict | None = None):
        info = _pretty(path)
        row = {
            "name": info["name"] or (family or "未知模型"),
            "family": family or "未知",
            "quant": info["quant"], "size": info["size"],
            "pid": pid, "port": port, "ctx": ctx, "ngl": ngl,
            "rss_gb": round(float(rss or 0), 1), "source": source,
        }
        if extra:
            row.update(extra)
        models.append(row)

    # 1) 宿主型服务拉起来的子进程 —— 每个子进程就是一个真正加载着的模型。
    for c in cands:
        par = c.get("parent")
        if not par:
            continue
        used_pids.update({c["pid"], par["pid"]})
        probe = probes.get(c["port"]) if c["port"] else None
        path = (probe or {}).get("path") or c["model_hint"] or c["alias"]
        if not path:
            continue                    # 问不出名字的，交给下面「在跑但没模型」那一段
        api_port = _best_api_port(path, probes, used_ports)
        if api_port:
            used_ports.add(api_port)
        add(path, family=par["family"], pid=c["pid"], port=api_port,
            rss=c["rss_gb"],
            ctx=(probe or {}).get("ctx") or c["ctx"],
            ngl=(probe or {}).get("ngl") or c["ngl"],
            source=(probe or {}).get("source", "argv"))

    # 2) Ollama 单独补一遍名字：/api/ps 给的是 `qwen3.5-4b-gguf:q4km` 这种
    #    人能读的名字，比 blobs 里的 sha256 强。已经用 ps 名字的就跳过。
    ollama_ps = (probes.get(11434) or {}).get("ollama") or []
    if ollama_ps:
        used_ports.add(11434)
        group_rss = round(sum(c["rss_gb"] for c in cands
                              if c["family"] == "Ollama"), 1)
        main = next((c for c in cands if c["family"] == "Ollama"
                     and not c.get("child")), None)
        kid = next((c for c in cands if c["family"] == "Ollama" and c.get("child")), None)
        # 上一段已经按子进程列过的 Ollama 行（名字是 blob）先撤掉，换成人读的
        models[:] = [m for m in models if m["family"] != "Ollama"]
        for i, m in enumerate(ollama_ps):
            add(m["path"], family="Ollama", pid=(main or {}).get("pid"), port=11434,
                rss=group_rss if i == 0 else 0.0,
                ctx=(kid or {}).get("ctx") if i == 0 else None,
                source="ps",
                extra={"vram_gb": m.get("vram_gb"), "expires": m.get("expires")})

    # 3) 没有子进程的独立推理服务（llama.cpp 自己起、或容器里跑的）
    for c in cands:
        if c.get("parent") or c["family"] in SERVICE_FAMS:
            continue
        p = int(c["port"]) if c["port"] else None
        probe = probes.get(p) if p else None
        if probe is None:
            for cp, got in probes.items():
                if cp in used_ports or got.get("ollama"):
                    continue
                if got.get("path"):
                    probe, p = got, cp
                    break
        path = (probe or {}).get("path") or c["model_hint"] or c["alias"]
        if path:
            add(path, family=c["family"], pid=c["pid"], port=p, rss=c["rss_gb"],
                ctx=(probe or {}).get("ctx") or c["ctx"],
                ngl=(probe or {}).get("ngl") or c["ngl"],
                source=(probe or {}).get("source", "argv"))
        else:
            # 认出了进程却问不出模型（刚起、或没开 API 口）：照样列一条，
            # 至少能说清「有个 llama.cpp 在跑，模型没报出来」。
            add(c["family"], family=c["family"], pid=c["pid"], port=p,
                rss=c["rss_gb"], source="proc")
        if p:
            used_ports.add(p)

    # 4) 探得到、但没对上任何进程的（容器里跑、进程名被改过）
    #    Ollama 那个口不参与：它的 `/v1/models` 列的是**下载过的**模型，
    #    不是加载着的 —— 拿它当「在跑」会把整个模型库都列出来。
    if any(c["family"] == "Ollama" for c in cands):
        used_ports.add(11434)
    owner = _port_owner()
    covered = {_norm(m["name"]) for m in models}
    for p, got in probes.items():
        if p in used_ports or got.get("ollama"):
            continue
        fam = "OpenAI 兼容"
        opid = owner.get(p)
        ofam = (by_pid.get(opid) or {}).get("family") if opid else ""
        if ofam in SERVICE_FAMS:
            fam = ofam
        elif p == 11434:
            fam = "Ollama"
        for mid in got.get("models") or []:
            nm = _norm(mid)
            if not nm or any(nm in c or c in nm for c in covered if c):
                continue
            add(mid, family=fam, pid=None, port=p,
                ctx=got.get("ctx"), source="v1")
            covered.add(nm)

    # 5) 在跑但此刻没加载模型的服务（Ollama 空闲 5 分钟就会把模型卸掉）。
    #    这类不列进 models —— 没有模型就是没有模型，但要说清「服务是活的」。
    fam_with_models = {m["family"] for m in models}
    services, seen_fam = [], set()
    for c in cands:
        fam = c["family"]
        if fam not in SERVICE_FAMS or fam in fam_with_models or fam in seen_fam:
            continue
        seen_fam.add(fam)
        port = 11434 if fam == "Ollama" else None
        for p, pid in owner.items():
            if p in probes and (by_pid.get(pid) or {}).get("family") == fam:
                port = p
                break
        services.append({"family": fam, "pid": c["pid"], "port": port,
                         "rss_gb": c["rss_gb"]})

    # 去重：同一个模型可能被进程层和服务层各说一遍
    dedup: dict[tuple, dict] = {}
    for m in models:
        k = (_norm(m["name"]), m.get("port") or 0)
        old = dedup.get(k)
        if old is None:
            dedup[k] = m
            continue
        if (m.get("rss_gb") or 0) > (old.get("rss_gb") or 0):
            m["pid"] = m["pid"] or old.get("pid")
            dedup[k] = m
        else:
            old["pid"] = old.get("pid") or m.get("pid")
            old["ctx"] = old.get("ctx") or m.get("ctx")

    out = sorted(dedup.values(), key=lambda m: (-(m.get("rss_gb") or 0), m["name"]))
    return {"ok": True, "at": round(time.time(), 1), "models": out,
            "total_rss_gb": round(sum(m.get("rss_gb") or 0 for m in out), 1),
            "services": services, "procs": len(cands),
            "probed_ports": sorted(probes.keys())}


def snapshot(fresh: bool = False) -> dict:
    """探测结果（缓存 20 秒）。任何异常都收成 ok:False，不让 /api/overview 跟着挂。"""
    now = time.time()
    with _LOCK:
        if not fresh and _CACHE["val"] and now - _CACHE["at"] < CACHE_TTL:
            return _CACHE["val"]
    try:
        val = _build()
    except Exception as e:                                 # noqa: BLE001
        val = {"ok": False, "at": round(now, 1), "models": [], "total_rss_gb": 0.0,
               "error": f"{type(e).__name__}: {e}"}
    with _LOCK:
        _CACHE["at"] = now
        _CACHE["val"] = val
    return val


if __name__ == "__main__":                                 # 手动看一眼
    print(json.dumps(snapshot(fresh=True), ensure_ascii=False, indent=2))
