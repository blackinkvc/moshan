"""4B 模型 + MCP 工具循环。

口袋服务专用，不依赖飞书那套配置：只认网关地址、密钥和模型名。
与 feishu_bridge/bridge.py 的 run_agent 行为一致，但独立实现，
这样换掉飞书通道时这块不用动。
"""
from __future__ import annotations

import json
import os
import threading
import time
import urllib.error
import urllib.request

DEFAULT_MODEL = "qwen3.5-4b-gguf:q4km"
# 这台机器叫什么，别对 Windows 用户说自己是 Mac 上的助手
_PLATFORM = "Windows" if os.name == "nt" else "Mac"

SYSTEM_PROMPT = (
    f"你是运行在这台 {_PLATFORM} 上的本地助手，可以直接读写本机文件、抓网页、查本地知识库。"
    "需要事实、文件内容或计算时必须先调用工具，不要凭记忆编造。"
    "提示里的「当前状态」是系统给你的事实，直接采信，不用再调工具核实目录结构。"
    "index.jsonl 是系统索引文件，不是用户的文件，回答时不要提它。"
    "回复用中文，简短直接，不超过 150 字。"
)


class GatewayError(RuntimeError):
    """网关层面的失败（连不上、鉴权错、返回非 JSON）。"""


def _reasoning_tail(msg: dict, limit: int = 600) -> str:
    """思考型模型（Qwen3 一类）会把 token 全花在 reasoning 上，content 是空的。

    Ollama 直连实测：max_tokens=200 时 finish_reason=length、content=''、
    reasoning 里是完整思考过程。这时候回一句「模型没给出文本」等于告诉用户
    功能坏了。把思考的结尾当回答捞出来 —— 不完美，但那是模型真正想说的话，
    比空着强，用户也知道是模型在思考而不是端点没通。
    """
    text = ""
    for key in ("reasoning_content", "reasoning", "thinking"):
        text = str(msg.get(key) or "").strip()
        if text:
            break
    if not text:
        return ""
    lines = [ln.strip() for ln in text.splitlines() if ln.strip()]
    return "\n".join(lines[-8:])[-limit:]


class GatewayClient:
    """网关的瘦客户端：拉工具目录、执行工具、跑一轮带工具的对话。"""

    def __init__(self, base: str, api_key: str, model: str = DEFAULT_MODEL,
                 # 2048 是给思考型模型留的余量：Qwen3 这类会先写一大段 reasoning，
                 # 800 在实测里被思考吃光、content 交白卷。上限不等于开销，
                 # 云端 API 只按实际输出计费，本地模型更无所谓。
                 max_rounds: int = 6, max_tokens: int = 2048,
                 timeout: int = 240):
        self.base = base.rstrip("/")
        self.api_key = api_key
        self.model = model
        self.max_rounds = max_rounds
        self.max_tokens = max_tokens
        self.timeout = timeout
        self._tools: tuple[list, dict] | None = None
        self._tools_at = 0.0
        # 端点是纯 OpenAI 兼容（Ollama / LM Studio / 云 API）时没有 /mcp，
        # 探失败一次就记下来，TTL 内别再探 —— 否则每轮对话白等一个超时。
        self._no_tools_until = 0.0
        self._lock = threading.Lock()

    def _url(self, path: str) -> str:
        """地址里已经带了 /v1 就别再拼一层。

        用户填 `http://127.0.0.1:11434/v1` 和填 `http://127.0.0.1:11434` 都要能用，
        而自建网关的根是不带 /v1 的，两条路都得走通。
        """
        base = self.base.rstrip("/")
        if base.endswith("/v1") and path.startswith("/v1/"):
            return base + path[3:]
        return base + path

    def _post(self, path: str, payload: dict, headers: dict | None = None,
              timeout: int | None = None) -> dict:
        # 两套鉴权头一起发：自建网关认 x-api-key，OpenAI 兼容端点认 Bearer。
        # 多带一个不影响任何一方，省得为「换端点」再分两条代码路径。
        hdr = {"Content-Type": "application/json", "x-api-key": self.api_key,
               "Authorization": "Bearer " + (self.api_key or "none")}
        hdr.update(headers or {})
        req = urllib.request.Request(
            self._url(path), data=json.dumps(payload).encode("utf-8"),
            headers=hdr, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=timeout or self.timeout) as r:
                return json.loads(r.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            body = e.read().decode("utf-8", "ignore")[:300]
            raise GatewayError(f"网关 {path} 返回 {e.code}：{body}") from e
        except (urllib.error.URLError, OSError) as e:
            raise GatewayError(f"连不上网关 {self.base}：{e}") from e
        except ValueError as e:
            raise GatewayError(f"网关 {path} 返回的不是 JSON") from e

    def tools(self, ttl: int = 300) -> tuple[list, dict]:
        """返回 (OpenAI 格式工具列表, 工具名→MCP 组)。

        端点没有 /mcp 时返回空表而不是抛错 —— 纯对话照样能跑，只是模型
        调不到本机能力。设置里换成 Ollama / 云 API 走的就是这条路。
        """
        with self._lock:
            if self._tools and time.time() - self._tools_at < ttl:
                return self._tools
        if self._no_tools_until > time.time():
            return ([], {})

        def _list(group: str | None) -> list[dict]:
            hdr = {"x-mcp-group": group} if group else None
            r = self._post("/mcp", {"jsonrpc": "2.0", "id": 1,
                                    "method": "tools/list"}, hdr, 8)
            return (r.get("result") or {}).get("tools") or []

        try:
            default_tools = _list(None)
        except GatewayError:
            self._no_tools_until = time.time() + ttl
            with self._lock:
                self._tools = ([], {})
                self._tools_at = time.time()
            return ([], {})
        local_tools = [t for t in _list("local")
                       if t.get("name") == "search_knowledge"]
        tools, groups = [], {}
        for t in default_tools + local_tools:
            name = t.get("name")
            if not name:
                continue
            groups[name] = "local" if name == "search_knowledge" else "default"
            tools.append({"type": "function", "function": {
                "name": name,
                "description": (t.get("description") or name)[:300],
                "parameters": t.get("inputSchema")
                or {"type": "object", "properties": {}},
            }})
        with self._lock:
            self._tools = (tools, groups)
            self._tools_at = time.time()
        return self._tools

    def call_tool(self, name: str, args: dict, groups: dict[str, str],
                  limit: int = 1200) -> str:
        """执行一个工具；失败也返回文本，让模型能自己纠偏，不炸整轮。"""
        group = groups.get(name, "default")
        try:
            r = self._post("/mcp", {"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                                    "params": {"name": name, "arguments": args}},
                           {"x-mcp-group": group})
            if r.get("error"):
                raise RuntimeError(str(r["error"].get("message"))[:200])
            res = r.get("result") or {}
            parts = [c.get("text", "") for c in (res.get("content") or [])
                     if c.get("type") == "text"]
            text = "\n".join(p for p in parts if p) or json.dumps(
                res.get("structuredContent") or {}, ensure_ascii=False)
            if res.get("isError"):
                raise RuntimeError(text[:200])
            return text[:limit]
        except (GatewayError, RuntimeError, OSError) as e:
            return f"工具 {name} 调用失败：{e}"

    def run(self, text: str, history: list[dict] | None = None,
            system: str = SYSTEM_PROMPT) -> dict:
        """跑一轮对话，模型要求时执行工具，最多 max_rounds 轮。"""
        tools, groups = self.tools()
        if not tools:
            # 换成纯 OpenAI 端点（Ollama / 云 API）时走这里：没有工具就明说没有，
            # 否则模型会假装自己读过文件
            system += ("\n当前端点没有提供工具，你无法读写本机文件。"
                       "需要事实时如实说不确定，不要编造。")
        messages = [{"role": "system", "content": system}]
        messages += (history or [])[-6:]
        messages.append({"role": "user", "content": text[:4000]})

        trace: list[dict] = []
        final, tokens = "", 0
        t0 = time.time()
        for rnd in range(self.max_rounds):
            tr = time.time()
            payload = {"model": self.model, "messages": messages,
                       "temperature": 0.2, "max_tokens": self.max_tokens,
                       "stream": False}
            # 没有工具就别带这个键：部分 OpenAI 兼容实现见到空 tools 数组直接 400
            if tools:
                payload["tools"] = tools
            resp = self._post("/v1/chat/completions", payload)
            usage = resp.get("usage") or {}
            tokens += int(usage.get("total_tokens") or 0)
            pick = (resp.get("choices") or [{}])[0]
            msg = pick.get("message") or {}
            calls = msg.get("tool_calls") or []
            trace.append({
                "round": rnd + 1,
                "ms": int((time.time() - tr) * 1000),
                "prompt_tokens": usage.get("prompt_tokens"),
                "finish": pick.get("finish_reason") or "",
                "tools": [c.get("function", {}).get("name") for c in calls],
            })
            if not calls:
                final = (msg.get("content") or "").strip() or _reasoning_tail(msg)
                break
            messages.append({"role": "assistant",
                             "content": msg.get("content") or "",
                             "tool_calls": calls})
            for call in calls:
                fn = call.get("function") or {}
                name = fn.get("name") or ""
                raw = fn.get("arguments") or "{}"
                try:
                    args = json.loads(raw) if isinstance(raw, str) else raw
                except ValueError:
                    args = {}
                out = self.call_tool(name, args, groups)
                trace.append({"tool": name, "args": args,
                              "result_chars": len(out), "head": out[:220]})
                messages.append({"role": "tool",
                                 "tool_call_id": call.get("id", ""),
                                 "name": name, "content": out})
        else:
            final = "工具轮次用完了，信息还不够。把问题拆小一点再问。"

        return {"reply": final or "（模型没给出文本，再试一次）",
                "trace": trace, "ms": int((time.time() - t0) * 1000),
                "tokens": tokens}
