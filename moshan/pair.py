#!/usr/bin/env python3
"""手机接入：把「打开就能用」的那条链接发到手机。

为什么又做一页：口袋是常驻的、手机随时连得上（局域网 / Tailscale），而
智能设备服务（8778）是按需起的。**发链接这件事必须随时能用**，所以窗口放
口袋；真正的上报仍然只写这台设备的档案（13-设备/档案/<id>.json）—— 一处数据、三个
入口（8778 面板 / 口袋 /pair / 微信助手），谁也别说自己是唯一真相。

数据不在这里算：整包都来自 8778 的 /api/mobile/pair（本机口子，含上报令牌，
局域网打不开），这里只做「展示 + 发微信」。

发微信走 wechat_push（网关 WebSocket，不经过模型），跟口袋入库回执同一条路。
"""
from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

from fastapi import Header, HTTPException, Query, Request
from fastapi.responses import HTMLResponse

DEV_BASE = "http://127.0.0.1:8778"
PAGE_NAME = "pair.html"
_LOCAL = ("127.0.0.1", "::1", "localhost")


def page_path(cfg: dict) -> Path:
    roots = cfg.get("allowed_roots") or []
    base = Path(roots[0]) if roots else Path.home()
    return base / "_工作台" / PAGE_NAME


def fetch(did: str = "", q: str = "") -> dict:
    """向智能设备服务要一整包。它没起来就老实说，不要拿旧缓存糊人。

    did=设备编号（最准）；q=设备名（微信里说「手机接入 15T」走这条）。
    """
    url = DEV_BASE + "/api/mobile/pair"
    qs = []
    if did:
        qs.append("did=" + urllib.parse.quote(str(did), safe=""))
    if q:
        qs.append("q=" + urllib.parse.quote(str(q), safe=""))
    if qs:
        url += "?" + "&".join(qs)
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            return json.loads(r.read().decode("utf-8"))
    except (urllib.error.URLError, OSError, ValueError) as e:
        return {"ok": False,
                "error": "智能设备服务（8778）没起来：%s" % str(e)[:90]}


def compose(data: dict) -> str:
    """发到微信的那条消息。短、能直接点、不贴令牌以外的垃圾。"""
    if not data.get("ok"):
        return "手机接入：%s" % (data.get("error") or "取不到地址")
    dev = (data.get("device") or {}).get("name") or ""
    url = data.get("phone_url") or ""
    if not url:
        return "手机接入：没探到可用地址（局域网和 Tailscale 都不通），先在电脑上看一眼网络。"
    head = "【手机接入】" + (dev if dev else "选好这台手机")
    tail = ("打开后电量自动读本机、屏幕时间带上今天已报的值，点一下「上报」就行；"
            "想连按钮都不点，把地址加到主屏幕（结尾带 go=1 打开即报）。")
    if not data.get("enabled"):
        tail = "⚠️ 上报通道现在是关的（智能设备 → 接入设备 → 手机实时上报），开了这条链接才打得开。" + tail
    return "%s\n%s\n%s" % (head, url, tail)


def register(app, cfg: dict, token: str, view_token: str) -> None:
    def _ok(k, x_token) -> bool:
        got = str(x_token or k or "")
        return bool(got) and got in (token, view_token)

    @app.get("/pair", response_class=HTMLResponse)
    def pair_page(request: Request) -> HTMLResponse:
        """页面放数据根，改完刷新即生效 —— 跟 /dash、/fleet 一个套路。
        本机打开时把 view_token 注进去；局域网/Tailscale 进来要求 ?k=。"""
        p = page_path(cfg)
        if not p.exists():
            return HTMLResponse(
                "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>"
                "<h3>手机接入页还没生成</h3><p style='color:#666'>应当位于：%s</p></body>" % p,
                status_code=404)
        html = p.read_text(encoding="utf-8")
        host = (request.client.host if request.client else "") or ""
        if host in _LOCAL:
            tok = json.dumps(str(view_token or ""))
            html = html.replace("</head>", "<script>window.__PK__=%s</script></head>" % tok, 1)
        return HTMLResponse(html, headers={"Cache-Control": "no-store"})

    @app.get("/api/pair")
    def api_pair(k: str | None = Query(None), x_token: str | None = Header(None),
                 did: str = "", q: str = "") -> dict:
        if not _ok(k, x_token):
            raise HTTPException(401, "token 无效")
        return fetch(did, q)

    @app.post("/api/pair/send")
    def api_send(body: dict, k: str | None = Query(None),
                 x_token: str | None = Header(None)) -> dict:
        """把接入方案发到微信（=发到手机）。text 可覆盖，默认按设备现拼。"""
        if not _ok(k, x_token):
            raise HTTPException(401, "token 无效")
        text = str(body.get("text") or "").strip()
        if not text:
            text = compose(fetch(str(body.get("did") or ""), str(body.get("q") or "")))
        try:
            import wechat_push
            ok = wechat_push.send(text)
        except Exception as e:                          # noqa: BLE001
            return {"ok": False, "error": "微信没发出去：%s" % str(e)[:80], "text": text}
        return {"ok": bool(ok), "text": text,
                "error": "" if ok else "网关没接（微信助手没跑？）"}
