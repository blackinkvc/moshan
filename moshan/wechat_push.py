#!/usr/bin/env python3
"""给微信发一条消息，**不经过模型**。

为什么需要它：链接入站改成代码接管以后，「已存入《…》，N 张图」这句话
不能再指望 4B 说出来（它连工具都不一定调得对）。既然结论是代码算出来的，
就由代码直接发。

怎么发：网关（127.0.0.1:18789）的 WebSocket RPC。
  1) connect（token 认证，role=operator）
  2) conversations.list 拿到这个渠道的 conversationRef（微信只有一个会话）
  3) conversations.send 投递纯文本。这条路径**不跑 agent**，所以不会多出
     一条「系统运行信息」之类的废话。

发文件（图片/视频/文件）走另一条：`send` 方法 + base64。为什么不走
`conversations.send`：它只吃 message 一个字段，媒体是 `send` 的事。
两个坑记在这儿（2026-11-20 实测）：
  · `to` 不能带 "user:" 前缀 —— 带前缀时渠道内部把整串塞进 to_user_id
    去调 getuploadurl，微信回 {"ret":-1}，报「returned no upload URL」。
  · mediaUrl 只认网关白名单目录（~/.openclaw/media、workspace 下），
    14-传输/ 不在里面，会 LocalMediaAccessError。所以用 buffer（base64）
    让网关自己落到临时目录，绕开白名单。

失败一律吞掉（只记日志）：发不出去不该让抓取任务跟着失败。
"""
from __future__ import annotations

import asyncio
import base64
import json
import mimetypes
import time
import uuid
from pathlib import Path

CONFIG = Path.home() / ".openclaw" / "openclaw.json"
URL = "ws://127.0.0.1:18789"
CHANNEL = "openclaw-weixin"
PROTO = 4

# 最近一次失败的原话。失败只返回 False 的话，上层记账/巡检就只能写
# 「推不出去」这种废话；把渠道的原话（例如 sendMessage ret=-2 prepare failed）
# 留在这里，账本和页面才有可查的东西。
LAST_ERROR = ""

_REF_CACHE: dict[str, tuple[str, float]] = {}     # channel -> (ref, ts)
_REF_TTL = 600


def _token() -> str:
    try:
        cfg = json.loads(CONFIG.read_text(encoding="utf-8"))
        return str(((cfg.get("gateway") or {}).get("auth") or {}).get("token") or "")
    except (OSError, ValueError):
        return ""


async def _connect(ws) -> dict:
    """走完握手，返回 hello-ok 的 payload。"""
    try:
        first = json.loads(await asyncio.wait_for(ws.recv(), timeout=5))
        if first.get("event") != "connect.challenge":
            pass                      # 老网关可能没有 challenge，照常 connect
    except asyncio.TimeoutError:
        pass
    await ws.send(json.dumps({
        "type": "req", "id": "c1", "method": "connect",
        "params": {
            "minProtocol": PROTO, "maxProtocol": PROTO,
            "client": {"id": "cli", "version": "1.0.0",
                       "platform": "macos", "mode": "cli"},
            "role": "operator",
            "scopes": ["operator.read", "operator.write", "operator.admin"],
            "caps": [], "commands": [], "permissions": {},
            "auth": {"token": _token()},
            "locale": "zh-CN", "userAgent": "pocket-intake/1.0",
        },
    }))
    while True:
        msg = json.loads(await asyncio.wait_for(ws.recv(), timeout=8))
        if msg.get("id") != "c1":
            continue
        if not msg.get("ok"):
            raise RuntimeError(str((msg.get("error") or {}).get("message") or "握手失败"))
        return msg.get("payload") or {}


async def _call(ws, mid: str, method: str, params: dict, timeout: float = 25) -> dict:
    await ws.send(json.dumps({"type": "req", "id": mid, "method": method,
                              "params": params}))
    while True:
        msg = json.loads(await asyncio.wait_for(ws.recv(), timeout=timeout))
        if msg.get("id") != mid:
            continue                         # 心跳/事件，跳过
        if not msg.get("ok"):
            raise RuntimeError(str((msg.get("error") or {}).get("message") or "调用失败"))
        return msg.get("payload") or {}


async def _conversation_ref(ws, agent: str, channel: str) -> str:
    hit = _REF_CACHE.get(channel)
    if hit and time.time() - hit[1] < _REF_TTL:
        return hit[0]
    payload = await _call(ws, "c2", "conversations.list",
                          {"agentId": agent, "limit": 20})
    for c in payload.get("conversations") or []:
        if c.get("channel") != channel:
            continue
        ref = str(c.get("conversationRef") or "")
        if ref:
            _REF_CACHE[channel] = (ref, time.time())
            return ref
    raise RuntimeError(f"找不到 {channel} 的会话")


async def _send(text: str, agent: str, channel: str) -> dict:
    import websockets
    async with websockets.connect(URL, max_size=None) as ws:
        await _connect(ws)
        ref = await _conversation_ref(ws, agent, channel)
        return await _call(ws, "s1", "conversations.send", {
            "agentId": agent,
            "conversationRef": ref,
            "message": text,
            "operationId": "convop_" + uuid.uuid4().hex[:24],
        })


def send(text: str, agent: str = "main", channel: str = CHANNEL,
         log=None) -> bool:
    """发一条微信。成功 True，失败 False（永不抛）。"""
    global LAST_ERROR
    body = (text or "").strip()
    if not body:
        LAST_ERROR = "空文本"
        return False
    try:
        r = asyncio.run(_send(body[:1500], agent, channel))
        ok = str(r.get("status") or "") in ("sent", "queued", "suppressed")
        if ok:
            LAST_ERROR = ""
        else:
            LAST_ERROR = "状态 %s" % r.get("status")
            if log:
                log({"event": "push_fail", "reason": LAST_ERROR})
        return ok
    except Exception as e:              # noqa: BLE001  发不出去不算任务失败
        LAST_ERROR = str(e)[:200] or e.__class__.__name__
        if log:
            log({"event": "push_fail", "reason": LAST_ERROR})
        return False


# —— 发文件：图片 / 视频 / 其它文件 ——
#   渠道按 MIME 自己分流（image→图片、video→视频、其它→文件），我们只管把
#   字节送过去。上限 16MB：base64 会胀到 4/3，网关那端一帧最多 25MB，
#   再大就别赌了 —— 超了退回一句话，至少手机上知道有这件东西。
MEDIA_MAX = 16 * 1024 * 1024


async def _send_media(path: Path, text: str, agent: str, channel: str) -> dict:
    import websockets
    raw = path.read_bytes()
    mime = mimetypes.guess_type(path.name)[0] or "application/octet-stream"
    async with websockets.connect(URL, max_size=None) as ws:
        await _connect(ws)
        payload = await _call(ws, "c2", "conversations.list",
                              {"agentId": agent, "limit": 20})
        conv = next((c for c in payload.get("conversations") or []
                     if c.get("channel") == channel), None)
        if not conv:
            raise RuntimeError(f"找不到 {channel} 的会话")
        to = str(conv.get("target") or "")
        if to.startswith("user:"):       # 上传接口不认前缀，见文件头注释
            to = to[len("user:"):]
        params = {
            "to": to,
            "channel": channel,
            "message": (text or "")[:500],
            "buffer": base64.b64encode(raw).decode(),
            "filename": path.name,
            "contentType": mime,
            "idempotencyKey": "tfmedia_" + uuid.uuid4().hex[:20],
        }
        if conv.get("accountId"):
            params["accountId"] = conv["accountId"]
        return await _call(ws, "m1", "send", params, timeout=120)


def send_media(path: str | Path, text: str = "", agent: str = "main",
               channel: str = CHANNEL, log=None) -> bool:
    """发一个本地文件到微信。成功 True，失败 False（永不抛）。"""
    global LAST_ERROR
    p = Path(path)
    if not p.is_file():
        LAST_ERROR = "文件不在了"
        if log:
            log({"event": "media_fail", "reason": LAST_ERROR, "name": p.name})
        return False
    try:
        size = p.stat().st_size
    except OSError:
        size = 0
    if size <= 0 or size > MEDIA_MAX:
        LAST_ERROR = "大小不合适（%d 字节，上限 %d）" % (size, MEDIA_MAX)
        if log:
            log({"event": "media_skip", "reason": LAST_ERROR, "name": p.name})
        return False
    try:
        r = asyncio.run(_send_media(p, text, agent, channel))
        mid = str(r.get("messageId") or "")
        if mid:
            LAST_ERROR = ""
        else:
            LAST_ERROR = "没有 messageId"
            if log:
                log({"event": "media_fail", "reason": LAST_ERROR, "name": p.name})
        return bool(mid)
    except Exception as e:              # noqa: BLE001
        LAST_ERROR = str(e)[:200] or e.__class__.__name__
        if log:
            log({"event": "media_fail", "reason": LAST_ERROR, "name": p.name})
        return False


def _main() -> int:
    import sys
    text = " ".join(sys.argv[1:]).strip() or "［自测］代码直发通道"
    print("ok" if send(text, log=lambda r: print(json.dumps(r, ensure_ascii=False)))
          else "fail")
    return 0


if __name__ == "__main__":
    raise SystemExit(_main())
