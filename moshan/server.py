#!/usr/bin/env python3
"""口袋服务：手机通过浏览器控制这台 Mac。

按 NAS 的思路组织，一共五件事：
  1. 浏览 —— 目录层级、面包屑、搜索、空间用量
  2. 操作 —— 新建目录、重命名、移动、复制、删除进回收站、恢复
  3. 传输 —— 上传任意目录、单文件下载、多选打包下载
  4. 分享 —— 生成限时可撤销的只读链接，发给别人不用给主令牌
  5. 对话 —— 交给本机 4B + MCP 工具（读写文件、抓网页、查知识库）

传输安全：
  - 全程 HTTPS（certs/ 下的证书，优先用 Tailscale 签发的公信证书）
  - 默认只接受私有网段来源（回环 / 局域网 / Tailscale 100.64/10），公网 IP 直接 403
  - WebDAV 默认关闭：明文 Basic 认证在手机端体验也差，改用网页 + Tailscale 传输
"""
from __future__ import annotations

import base64
import hashlib
import hmac
import ipaddress
import json
import os
import re
import secrets
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timedelta
from pathlib import Path

BASE = Path(__file__).resolve().parent
sys.path.insert(0, str(BASE))  # 保证 launchd 用任意 cwd 启动时也能 import agent
import paths  # 目录约定：程序目录 / 状态目录 / 资料库分开，更新不碰用户数据
import netca  # 交付版：把 CA 证书指到 certifi，否则打包后所有 https 都失败

netca.install()

from fastapi import Depends, FastAPI, File, Header, HTTPException, Query, Request, UploadFile
from fastapi.responses import (FileResponse, HTMLResponse, JSONResponse,
                               PlainTextResponse, RedirectResponse, Response)
from starlette.background import BackgroundTask

import storage
import naming
import agentbus
import agent_link
import appearance as appmod
import dash as dashmod
import plan as planmod
import extplugins
import brief as briefmod
import ledger
import transfer
import metrics
import sysinfo
import version
# import fleet as fleetmod      # 多机监控已拆成独立服务（2026-11-21），见下方挂载处注释
import report
import report_runner
import dedupe_runner
import ai_trends_runner
import heartbeat
import ocr_runner
import wechat_guard
import tls as tlsmod
from agent import GatewayClient, GatewayError, SYSTEM_PROMPT
from linkbox import LinkBox, split_links
import pending
from articlebox import ArticleBox
from intake import Intake
from webdav import build_router

# —— 本机工作：从「另起一个 8775 服务」改成口袋内嵌 ——
#   以前工作台的资源视图要跨端口读 8775，https 页面会被混合内容拦掉，
#   只能靠服务端转发，还多一个常驻进程。现在直接 import 同一个模块，
#   采样线程挂在口袋进程里，页面同源取数，8775 彻底不需要了。
#   找不到 cuntu 目录也不报错：这台机器没装就不显示，别把口袋拖死。
#   分发版默认没有 cuntu（那是作者的扩展插件集），用 MOSHAN_CUNTU 指向自己的目录即可。
CUNTU_DIR = Path(os.environ.get("MOSHAN_CUNTU") or "~/Documents/cuntu").expanduser()
if CUNTU_DIR.is_dir():
    sys.path.insert(0, str(CUNTU_DIR))
try:
    import hostwork as hwmod
except Exception as _hw_err:                           # noqa: BLE001
    hwmod = None
    _HW_IMPORT_ERR = f"{type(_hw_err).__name__}: {_hw_err}"
else:
    _HW_IMPORT_ERR = ""

try:                                                   # TRAE 自报一笔 → TRAE 卡
    import trae_work as twmod
except Exception:                                      # noqa: BLE001
    twmod = None

CFG_PATH = paths.config_file()
UI_PATH = BASE / "ui.html"
LOG_PATH = BASE / "pocket.jsonl"


SORTED_DIRNAME = "_已分类"
_EXTS = {
    "文档": "pdf doc docx txt md markdown rtf pages odt epub mobi azw3",
    "表格": "xls xlsx csv tsv numbers",
    "演示": "ppt pptx key keynote",
    "图片": "jpg jpeg png gif webp heic heif bmp tif tiff svg ico",
    "音视频": "mp3 wav m4a aac flac ogg opus mp4 mov mkv avi webm flv m4v",
    "压缩包": "zip rar 7z tar gz tgz bz2 xz dmg iso",
    "代码": "py js ts jsx tsx json yaml yml toml ini html css scss sh sql",
}
CATEGORY_OF_EXT = {e: c for c, exts in _EXTS.items() for e in exts.split()}


def _bootstrap_config() -> dict:
    """首次运行时生成配置。

    所有默认路径都取自 paths：别人把 Moshan 装在自己机器上也能直接跑，
    不再出现「写死某个用户的家目录」的情况。
    """
    paths.ensure_layout()
    gw_base, gw_key, model = "http://127.0.0.1:8080", "", "qwen3.5-4b-gguf:q4km"
    # 兼容老用户：同目录若还留着飞书桥配置，就继承其中的网关地址与密钥
    bridge_cfg = BASE.parent / "feishu_bridge" / "config.json"
    if bridge_cfg.exists():
        try:
            c = json.loads(bridge_cfg.read_text(encoding="utf-8"))
            gw_base = c.get("gateway_base", gw_base)
            gw_key = c.get("gateway_api_key", "")
            model = c.get("model", model)
        except (OSError, ValueError):
            pass
    lib = paths.library_dir()
    cfg = {
        "token": secrets.token_urlsafe(24),
        "host": "0.0.0.0",
        "port": 8321,
        "gateway_base": gw_base,
        "gateway_api_key": gw_key,
        "model": model,
        "inbox_dir": str(lib / "收件箱"),
        "allowed_roots": [str(lib)],
        "trash_dir": str(lib / ".回收站"),
        "dav_root": str(lib),
        "trash_days": 30,
        "max_upload_mb": 500,
        "tls": True,
        "tls_dir": str(paths.certs_dir()),
        "allow_private_only": True,
        "dav_enabled": False,
    }
    CFG_PATH.write_text(json.dumps(cfg, ensure_ascii=False, indent=2),
                        encoding="utf-8")
    os.chmod(CFG_PATH, 0o600)
    return cfg


CFG = (json.loads(CFG_PATH.read_text(encoding="utf-8"))
       if CFG_PATH.exists() else _bootstrap_config())

# 老配置缺新键时补齐并写回，保证 show_token.py 等外部脚本看到的是同一份事实
_NEW_DEFAULTS = {
    "trash_dir": str(paths.library_dir() / ".回收站"),
    "dav_root": (CFG["allowed_roots"][0] if CFG.get("allowed_roots") else ""),
    "trash_days": 30,
    "tls": True,
    "tls_dir": str(paths.certs_dir()),
    "allow_private_only": True,
    "dav_enabled": False,
}
_missing = [k for k in _NEW_DEFAULTS if k not in CFG]
for _k, _v in _NEW_DEFAULTS.items():
    CFG.setdefault(_k, _v)
if _missing:
    try:
        CFG_PATH.write_text(json.dumps(CFG, ensure_ascii=False, indent=2),
                            encoding="utf-8")
        os.chmod(CFG_PATH, 0o600)
    except OSError:
        pass

TOKEN = CFG["token"]
INBOX_DIR = Path(CFG["inbox_dir"])
ALLOWED_ROOTS = [Path(r) for r in CFG.get("allowed_roots", [])]
MAX_UPLOAD = float(CFG.get("max_upload_mb", 500)) * 1048576
DAV_ROOT = Path(CFG["dav_root"]).expanduser()
TRASH_DIR = Path(CFG["trash_dir"]).expanduser()
SHARES_PATH = paths.state_dir() / "shares.json"

TLS_ENABLED = bool(CFG.get("tls", True))
TLS_DIR = Path(CFG.get("tls_dir", str(paths.certs_dir()))).expanduser()
DAV_ENABLED = bool(CFG.get("dav_enabled", False))
PRIVATE_ONLY = bool(CFG.get("allow_private_only", True))
# 局域网来源默认不放行：自签证书在局域网里可被同网段的人替换（手机点一下「继续」
# 就等于放弃校验），在这种页面输入的口令会直接落到对方手里。要临时用局域网，
# 把 config.json 的 allow_lan 改成 true。
ALLOW_LAN = bool(CFG.get("allow_lan", False))
# allow_lan 打开后，还能再收窄到「只有这些设备」：填了 IP 就只有它们能进局域网入口，
# 列表为空才表示整个局域网都放行。配合路由器里的 DHCP 固定租约用，手机换网络也不怕。
LAN_ALLOW_IPS = {str(x).strip() for x in (CFG.get("lan_allow_ips") or []) if str(x).strip()}

storage.configure([str(r) for r in ALLOWED_ROOTS], str(TRASH_DIR), str(SHARES_PATH))

GW = GatewayClient(CFG.get("gateway_base", "http://127.0.0.1:8080"),
                   CFG.get("gateway_api_key", ""),
                   CFG.get("model", "qwen3.5-4b-gguf:q4km"))

app = FastAPI(title="口袋服务", docs_url=None, redoc_url=None)
if DAV_ENABLED:
    app.include_router(build_router(TOKEN, DAV_ROOT))

# 新手引导（/welcome）：首次启动给一条最短可用路径。
# 引导页本身不带 token 校验，但仍受 guard_network 限制，只对本机与隧道来源开放。
try:
    import welcome as welcomemod
    welcomemod.register(app, CFG, TOKEN)
except Exception as _e:                                # noqa: BLE001
    print(f"[警告] 引导页注册失败：{_e}")

# 关于 / 更新（/about）：版本、数据目录、检查并安装更新。
# 单独成模块的理由见 about.py 顶部：ui.html 是逐字同步文件，往里加按钮会被同步覆盖，
# 所以这里注册独立页面，并在 index() 里注入一个小入口。
try:
    import about as aboutmod
    aboutmod.register(app, CFG, TOKEN)
except Exception as _e:                                # noqa: BLE001
    aboutmod = None                                    # type: ignore[assignment]
    print(f"[警告] 关于页注册失败：{_e}")

# 手机接入（/pair）：把「打开就能用」的链接摆出来、一键发到微信。
# 数据仍来自 8778 的 /api/mobile/pair，这里只做展示和投递 —— 口袋常驻，
# 手机随时连得上，发链接这件事不该依赖智能设备服务那个按需进程还醒着。
try:
    import pair as pairmod
    pairmod.register(app, CFG, TOKEN, str(CFG.get("view_token") or ""))
except Exception as _e:                                # noqa: BLE001
    print("手机接入页没挂上：%s" % _e)

# 多机监控（虫巢思维）已于 2026-11-21 拆成独立服务，不再挂在这里。
#   起因：监控系统不该和被监控的东西共命运 —— 改一次口袋配置 kickstart 一下，
#   虫巢的 HTTP 跟着断十几秒，正上报的被控机整条丢掉。
#   现在是 <数据根>/16-多机/nest_srv.py，自己的端口（8781 https / 8782 本机）、
#   自己的配置（nest.json）、自己的 launchd（com.local-ai.nest）。
#   这个文件留在原处当备份：哪天要回滚，把下面三行放开、同时停掉 nest 服务即可
#   （注意两边共用同一个 fleet.db，别同时挂载，否则自采会重复写同一条设备）。
#
# try:
#     fleetmod.register(app, CFG, CFG_PATH)
# except Exception as _e:      # noqa: BLE001
#     print(f"[fleet] 多机模块挂载失败，已跳过：{_e}")


# 明确的私有网段白名单。不能依赖 ipaddress.is_private：
# 它把 Tailscale 的 100.64/10 判为「非私有」，却把 203.0.113/24 这类文档网段判为「私有」。
_PRIVATE_NETS = (
    ipaddress.ip_network("127.0.0.0/8"),
    ipaddress.ip_network("10.0.0.0/8"),
    ipaddress.ip_network("172.16.0.0/12"),
    ipaddress.ip_network("192.168.0.0/16"),
    ipaddress.ip_network("169.254.0.0/16"),
    ipaddress.ip_network("100.64.0.0/10"),   # Tailscale / CGNAT
    ipaddress.ip_network("::1/128"),
    ipaddress.ip_network("fc00::/7"),        # 唯一本地地址
    ipaddress.ip_network("fe80::/10"),       # 链路本地
)


def _is_private_peer(host: str) -> bool:
    """来源地址是否属于「本机 / 局域网 / Tailscale」这三类可信网络。"""
    if not host:
        return False
    if host in ("testclient", "localhost"):
        return True
    try:
        ip = ipaddress.ip_address(host.split("%")[0])
    except ValueError:
        return False
    if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped:
        ip = ip.ipv4_mapped
    return any(ip in net for net in _PRIVATE_NETS if net.version == ip.version)


# 隧道来源：本机回环 + Tailscale 自己的网段（IPv4 CGNAT 段、IPv6 ULA 段）。
# 这一段是 WireGuard 端到端加密且双向认证的，链路上谁都插不进来。
_TUNNEL_NETS = (
    ipaddress.ip_network("127.0.0.0/8"),
    ipaddress.ip_network("::1/128"),
    ipaddress.ip_network("100.64.0.0/10"),
    ipaddress.ip_network("fd7a:115c:a1e0::/48"),
)


def _is_tunnel_peer(host: str) -> bool:
    if not host:
        return False
    if host in ("testclient", "localhost"):
        return True
    try:
        ip = ipaddress.ip_address(host.split("%")[0])
    except ValueError:
        return False
    if isinstance(ip, ipaddress.IPv6Address) and ip.ipv4_mapped:
        ip = ip.ipv4_mapped
    return any(ip in net for net in _TUNNEL_NETS if net.version == ip.version)


_TS_IP_CACHE: str | None = None


def _cached_ts_ip() -> str:
    """隧道地址只在第一次用到时查，避免每个被拒请求都去起一次 tailscale 命令。"""
    global _TS_IP_CACHE
    if _TS_IP_CACHE is None:
        try:
            _TS_IP_CACHE = tlsmod.tailscale_ip() or ""
        except Exception:
            _TS_IP_CACHE = ""
    return _TS_IP_CACHE


def _blocked_page(host: str) -> str:
    ip = _cached_ts_ip()
    port = int(CFG.get("plain_port", 8322))
    if LAN_ALLOW_IPS:
        why = ('局域网只放行指定的设备（' + '、'.join(sorted(LAN_ALLOW_IPS))
               + '），你这台的地址不在名单里。')
    elif ALLOW_LAN:
        why = '这个来源既不是本机，也不是局域网，更不在 Tailscale 隧道里。'
    else:
        why = ('局域网里的页面可能被同一网段的人替换，在上面输入的口令会被截走，'
               '所以这里默认不提供服务。')
    tip = ""
    if ip:
        url = f"http://{ip}:{port}/"
        tip = ('<p style="margin:10px 0 0;padding:12px 14px;background:#e6f2ec;border-radius:12px">'
               f'<span style=" font-size:12.5px;color:#5d6b64">改用这条隧道地址</span><br>'
               f'<a href="{url}" style="color:#1d6b52;word-break:break-all">{url}</a></p>')
    return (
        '<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">'
        '<meta name="viewport" content="width=device-width,initial-scale=1">'
        '<meta name="referrer" content="no-referrer"><title>换个地址</title></head>'
        '<body style="margin:0;min-height:100vh;display:flex;flex-direction:column;'
        'justify-content:center;gap:12px;padding:32px 24px;'
        "font:15px/1.75 -apple-system,BlinkMacSystemFont,'PingFang SC',sans-serif;"
        'background:#f6f6f4;color:#1a1a18;-webkit-text-size-adjust:100%">'
        '<h2 style="margin:0;font-size:19px">这个地址不能用</h2>'
        f'<p style="margin:0">你打开的是 <code>{host}</code>。</p>'
        f'<p style="margin:0;color:#75756e;font-size:13.5px">{why}</p>'
        f'{tip}</body></html>'
    )


@app.middleware("http")
async def guard_network(request: Request, call_next):
    """只接受本机 / Tailscale 隧道来源，以及 allow_lan 放开后的局域网来源。

    公网一律拒绝（路由器把端口转出去也用不了）；局域网默认同样拒绝，
    因为那里的页面可能被同网段的人替换掉。allow_lan 可临时放开，
    lan_allow_ips 非空时局域网只放行名单里的设备。
    """
    peer = request.client.host if request.client else ""
    lan_ok = ALLOW_LAN and _is_private_peer(peer) and (
        not LAN_ALLOW_IPS or peer in LAN_ALLOW_IPS)
    ok = _is_tunnel_peer(peer) or lan_ok
    if not ok:
        log({"event": "blocked_source", "ip": peer, "path": request.url.path,
             "lan": bool(ALLOW_LAN), "allow": sorted(LAN_ALLOW_IPS)})
        host = (request.headers.get("host") or peer or "这个地址").split(":")[0]
        return HTMLResponse(_blocked_page(host), status_code=403)
    return await call_next(request)


def log(entry: dict) -> None:
    entry = {"ts": time.strftime("%Y-%m-%dT%H:%M:%S%z"), **entry}
    try:
        with LOG_PATH.open("a", encoding="utf-8") as f:
            f.write(json.dumps(entry, ensure_ascii=False) + "\n")
    except OSError:
        pass


# 链接入库：手机粘条链接，后台跑 cuntu 抓取归档。
# 完成时回调写一条日志，避免前端每次轮询都重复记一行。
LINKS = LinkBox(on_finish=lambda j: log({
    "event": "link_done", "id": j.get("id"), "state": j.get("state"),
    "platform": j.get("platform"), "images": j.get("images", 0),
    "bytes": j.get("bytes", 0), "dir": j.get("dir_name", ""),
    "routed": j.get("routed", ""),
    # 失败原因必须落盘：只写 state=error 的话，工作台上永远只有一句
    # 「抓取出错」，看不出是短链失效还是笔记没图（2026-11-18 踩过）。
    "error": j.get("message", ""),
    # 原链接也落盘：存失败的那一条，用户想自己动手打开链接再试一次，
    # 工作台要能直接把链接还给他（2026-11-19 加）。
    "url": j.get("url", ""),
}))

# 文章入库：抓正文 + 4B 分主题，同样异步，避免手机浏览器久等
ARTICLES = ArticleBox(on_finish=lambda j: log({
    "event": "article_done", "id": j.get("id"), "state": j.get("state"),
    "kind": j.get("kind", "fetch"), "topic": j.get("topic", ""),
    "chars": j.get("chars", 0), "rel": j.get("rel", ""), "routed": j.get("routed", ""),
    "error": j.get("error", ""), "url": j.get("url", ""),
}), on_gallery=lambda url, hint="": LINKS.start(url, hint=hint or "存图"))


# 微信入站：代码抽链接 → 入队 → 模板回执；抓完由 push 直接发回微信。
# 4B 不再参与前半程（它漏调工具、把存图叫成存文章、一次两条就超时）。
INTAKE = Intake(LINKS, ARTICLES, log=log)

# 「两边都像」的挂起单：用户回一句「存图 / 存文章」就由这里把活交给对应的队列。
# 存图那条早就落盘了（就地转存文章，不重抓网页）；阅读那条是同一个任务再跑一遍。
pending.set_resolvers(
    link=lambda e, mode: LINKS.resolve(str(e.get("job") or ""), mode, e),
    article=lambda e, mode: ARTICLES.resolve(e, mode),
)


def auth(x_token: str | None = Header(None), k: str | None = Query(None)) -> None:
    supplied = x_token or k or ""
    if not supplied or not hmac.compare_digest(supplied, TOKEN):
        raise HTTPException(401, "token 无效")


def _safe_path(raw: str) -> Path:
    """只允许访问 allowed_roots 以内的路径，挡住 ../ 穿越。"""
    try:
        return storage.safe_path(raw)
    except storage.PathError as e:
        raise HTTPException(e.code, str(e))


def _unique_path(directory: Path, name: str) -> Path:
    return storage.unique_path(directory, name)


def _category(name: str) -> str:
    return storage.category(name)


def _human(n: int) -> str:
    return storage.human(n)


@app.get("/", response_class=HTMLResponse)
def index() -> HTMLResponse:
    # no-store：手机上改完 ui.html 刷新就能拿到新版本，不会被 Safari 缓存糊住
    if UI_PATH.exists():
        html = UI_PATH.read_text(encoding="utf-8")
        # 注入隧道地址：页面连接失败时能自己切到 100.x 明文口，不靠用户记地址
        ts_ip = tlsmod.tailscale_ip() or ""
        if ts_ip:
            html = html.replace("__TS_IP__", ts_ip)
        if aboutmod is not None:                       # 角落里的「关于 / 新版本」入口
            html = aboutmod.inject(html)
        return HTMLResponse(html, headers={
            "Cache-Control": "no-store, max-age=0",
            "Referrer-Policy": "no-referrer",       # 别让 URL 里的路径/令牌随 Referer 外流
            "X-Content-Type-Options": "nosniff",
            "X-Frame-Options": "DENY",              # 防止被别的页面套进 iframe 点按
        })
    return HTMLResponse("<h1>口袋服务</h1><p>ui.html 缺失</p>")


@app.get("/api/ping")
def ping() -> dict:
    return {"ok": True, "service": "pocket", "inbox": str(INBOX_DIR)}


@app.on_event("startup")
def _startup() -> None:
    TRASH_DIR.mkdir(parents=True, exist_ok=True)
    try:
        n = storage.auto_purge(int(CFG.get("trash_days", 30)))
        if n:
            log({"event": "trash_auto_purge", "n": n})
    except OSError:
        pass
    _start_dedupe_scheduler()
    _start_trends_scheduler()
    _start_heartbeat()
    _start_ocr()
    _start_reports()
    _start_wechat_guard()
    _start_llmcost_scheduler()
    _start_hostwork()
    _start_pending()
    _start_brief()
    _start_trae_bridge()


_BRIDGE_STARTED = {"done": False}


def _start_trae_bridge() -> None:
    """升级后自动把「TRAE 自述」说明书塞进 Trae 的技能目录。

    用户装完新版本不会去点「装说明」，但 Trae 不知道要报账，TRAE 卡就永远是空的。
    所以这一步默认执行，版本变了才重写；失败只打日志，绝不影响启动。
    """
    if _BRIDGE_STARTED["done"]:
        return
    _BRIDGE_STARTED["done"] = True

    def run() -> None:
        try:
            base = f"http://127.0.0.1:{int(CFG.get('local_port') or 8323)}"
            res = agent_link.auto_install(dashmod._root(CFG), base, version.VERSION)
        except Exception as exc:                       # noqa: BLE001
            print(f"[Moshan] TRAE 说明书自动投放失败（不影响使用）：{exc}", flush=True)
            return
        if res.get("ok") and not res.get("skipped"):
            print(f"[Moshan] 已更新 TRAE 自述说明书 {version.VERSION} → {res.get('path')}",
                  flush=True)
        elif res.get("skipped") in ("no-trae", "no-path"):
            print(f"[Moshan] 本机没装 Trae，跳过说明书投放（{res.get('skipped')}）",
                  flush=True)

    threading.Thread(target=run, daemon=True, name="trae-bridge").start()


_BRIEF_STARTED = {"done": False}


def _start_brief() -> None:
    """每日简报的定时推送（2026-11-21）。

    放在口袋进程里的原因见 brief.scheduler_loop 的注释：这套沙箱里 launchctl
    装不进去（bootstrap 报 EIO），而且 8 点关机时 launchd 也不会补。
    两个 uvicorn 实例共用一个 app，所以要闸门；就算闸门漏了，brief 里还有
    一块原子牌子（<日期>.push）兜着 —— 不会出现一口气十条。
    """
    if _BRIEF_STARTED["done"]:
        return
    _BRIEF_STARTED["done"] = True
    try:
        threading.Thread(target=briefmod.scheduler_loop, args=(CFG, log),
                         name="brief-scheduler", daemon=True).start()
        log({"event": "brief_scheduler_start"})
    except Exception as e:                             # noqa: BLE001
        log({"event": "brief_scheduler_error", "error": str(e)[:200]})


def _start_pending() -> None:
    """「存图还是文章」的挂起单自己会发问句：起个发送线程。

    为什么不在 intake 里顺手发：批次看门狗活在内存里，重启就没了；
    网页上贴链接进来的那条更连批次都没有。挂起单存在磁盘上，重启后
    这里一拉，还没发出去的问话就会补到聊天窗。
    """
    try:
        pending.start(log=log)
        log({"event": "pending_watcher_start",
             "open": len(pending.open_items())})
    except Exception as e:                             # noqa: BLE001
        log({"event": "pending_start_error", "error": str(e)[:200]})


_HW_STARTED = {"done": False}


def _start_hostwork() -> None:
    """本机工作的三个采集线程（进程 10 秒 / 会话增量 / 图标），起一次就够。

    两个 uvicorn 共用 app（8321 与 8323 是同一份代码），所以要有闸门；
    线程都是 daemon，口袋重启后由这里重新拉起来，8775 不用再单开。
    """
    if hwmod is None or _HW_STARTED["done"]:
        return
    _HW_STARTED["done"] = True
    try:
        hwmod.load_proc_history()
    except Exception as e:                             # noqa: BLE001
        log({"event": "hostwork_history_error", "error": str(e)[:200]})
    for fn, name in ((hwmod.sampler_loop, "hw-sampler"),
                     (hwmod.icon_worker, "hw-icon"),
                     (hwmod.scan_loop, "hw-scan")):
        try:
            threading.Thread(target=fn, name=name, daemon=True).start()
        except Exception as e:                         # noqa: BLE001
            log({"event": "hostwork_thread_error", "error": f"{name}: {e}"[:200]})
    log({"event": "hostwork_start"})



def _start_reports() -> None:
    """使用报告的采样与出报线程。两个 uvicorn 共用 app，start 内部自带闸门。"""
    try:
        report_runner.start(log=log)
    except Exception as e:              # 报告起不来不该拦住上传/浏览
        log({"event": "report_runner_error", "error": str(e)[:200]})


# ---- 微信链接漏存守卫：和去重/心跳同样的闸，双 uvicorn 只起一个线程 ----
_GUARD_ON = False


def _start_wechat_guard() -> None:
    """微信链接漏存兜底：4B 助手漏调工具时替它补一次。

    只在微信入站消息里找「带链接但口袋没开过任务」的，90 秒后才动手，
    所以正常情况它什么都不做（助手自己已经存好了）。
    """
    global _GUARD_ON
    if _GUARD_ON:
        return
    if (CFG.get("wechat_guard") or {}).get("enabled") is False:
        return
    _GUARD_ON = True
    threading.Thread(target=wechat_guard.scheduler_loop, args=(CFG, log),
                     daemon=True, name="wechat-guard").start()
    log({"event": "wechat_guard_scheduler",
         "interval_sec": int((CFG.get("wechat_guard") or {}).get("interval_sec", 60))})


# ---- 存图去重：每天 dedupe.hour 点自动扫，页面里也能手动触发 ----
_SCHED_ON = False


def _start_dedupe_scheduler() -> None:
    """两个 uvicorn（https 8321 / 明文 8322）共用同一个 app，startup 会跑两次，
    所以这里自己加一道闸，只允许起一个定时线程。"""
    global _SCHED_ON
    if _SCHED_ON:
        return
    if (CFG.get("dedupe") or {}).get("enabled") is False:
        return
    _SCHED_ON = True

    def _done(out: dict) -> None:
        log({"event": "dedupe_done", "ok": bool(out.get("ok")),
             "groups": len(out.get("groups", [])),
             "freed": out.get("freed", 0),
             "dirs": (out.get("scanned") or {}).get("dirs", 0),
             "error": out.get("error", "")})

    import threading
    threading.Thread(target=dedupe_runner.scheduler_loop, args=(CFG, _done),
                     daemon=True, name="dedupe-scheduler").start()
    log({"event": "dedupe_scheduler", "next": dedupe_runner.next_run_at(CFG)})


@app.get("/api/dedupe", dependencies=[Depends(auth)])
def dedupe_state() -> dict:
    cfg = CFG.get("dedupe") or {}
    return {
        "hour": int(cfg.get("hour", 3)),
        "apply": bool(cfg.get("apply", True)),
        "root": str(Path(cfg.get("root") or Path.home() / "Documents" / "AI项目" / "04-存图")),
        "next_at": dedupe_runner.next_run_at(CFG),
        "state": dedupe_runner.STATE,
        "latest": dedupe_runner.latest(CFG),
        "history": dedupe_runner.history(CFG, 10),
        "batches": dedupe_runner.batches(CFG),
    }


@app.post("/api/dedupe/run", dependencies=[Depends(auth)])
def dedupe_run(apply: bool = Query(True)) -> dict:
    if dedupe_runner.STATE.get("running"):
        return {"ok": False, "error": "正在扫，等这次结束"}
    import threading
    threading.Thread(target=dedupe_runner.run, args=(CFG,),
                     kwargs={"apply": apply}, daemon=True).start()
    log({"event": "dedupe_manual", "apply": apply})
    return {"ok": True, "started": True}


@app.post("/api/dedupe/restore", dependencies=[Depends(auth)])
def dedupe_restore(batch: str = Query(...)) -> dict:
    out = dedupe_runner.restore(CFG, batch)
    log({"event": "dedupe_restore", "batch": batch, "ok": out.get("ok")})
    return out


# ---- AI 技术动向：每天 ai_trends.hour 点自动跑，页面里也能手动触发 ----
_TREND_ON = False


def _start_trends_scheduler() -> None:
    """和去重调度同样的闸：双 uvicorn 共用 app，startup 会跑两次。"""
    global _TREND_ON
    if _TREND_ON:
        return
    if (CFG.get("ai_trends") or {}).get("enabled") is False:
        return
    _TREND_ON = True

    def _done(out: dict) -> None:
        log({"event": "trends_done", "ok": bool(out.get("ok")),
             "skipped": bool(out.get("skipped")),
             "status": out.get("status", ""),
             "items": (out.get("stats") or {}).get("day_total", 0),
             "llm": (out.get("llm") or {}).get("ok"),
             "error": out.get("error", "")})

    import threading
    threading.Thread(target=ai_trends_runner.scheduler_loop, args=(CFG, _done),
                     daemon=True, name="trends-scheduler").start()
    log({"event": "trends_scheduler", "next": ai_trends_runner.next_run_at(CFG)})


# ---- Token 价格追踪：每天 llmcost.hour 点重算一遍并落快照 ----
#   价格服务（8776）是双击才起的，但工作台那张「Token 价格追踪」卡片读的是
#   13-算价/out/accuracy.json。不每天跑一次，卡片就会一直停在旧数据，
#   价格版本轨迹（data/snapshots/api_prices_<日期>.csv）也长不出来。
#   所以借口袋这个常驻进程起个线程，定时跑一次子进程；算完就退出，不常驻内存。
_LLMCOST_ON = False


def _start_llmcost_scheduler() -> None:
    global _LLMCOST_ON
    if _LLMCOST_ON:
        return
    cfg = CFG.get("llmcost") or {}
    if cfg.get("enabled") is False:
        return
    _LLMCOST_ON = True
    hour = int(cfg.get("hour", 8))
    minute = int(cfg.get("minute", 20))

    def loop() -> None:
        py = cfg.get("python") or sys.executable
        script = cfg.get("script") or ""
        data = cfg.get("root") or str(Path.home() / "Documents/AI项目/13-算价")
        while True:
            now = datetime.now()
            target = now.replace(hour=hour, minute=minute, second=0, microsecond=0)
            if target <= now:
                target += timedelta(days=1)
            time.sleep(max(60.0, (target - now).total_seconds()))
            if not os.path.exists(script):
                log({"event": "llmcost_snapshot", "ok": False, "error": "脚本不在"})
                continue
            try:
                env = dict(os.environ, LLMCOST_DIR=data)
                r = subprocess.run([py, script, "--snapshot"], capture_output=True,
                                   text=True, timeout=600, env=env)
                tail = (r.stdout or r.stderr or "").strip().splitlines()[-1:]
                log({"event": "llmcost_snapshot", "ok": r.returncode == 0, "tail": tail})
            except (OSError, subprocess.SubprocessError) as e:
                log({"event": "llmcost_snapshot", "ok": False, "error": str(e)[:200]})

    import threading
    threading.Thread(target=loop, daemon=True, name="llmcost-scheduler").start()
    log({"event": "llmcost_scheduler", "at": "%02d:%02d" % (hour, minute)})


@app.get("/api/trends", dependencies=[Depends(auth)])
def trends_state() -> dict:
    cfg = CFG.get("ai_trends") or {}
    day = datetime.now().strftime("%Y-%m-%d")
    return {
        "hour": int(cfg.get("hour", 9)),
        "enabled": bool(cfg.get("enabled", True)),
        "model": "qwen3.5-4b-gguf:q4km",
        "next_at": ai_trends_runner.next_run_at(CFG),
        "today_done": ai_trends_runner.already_done(CFG, day),
        "state": ai_trends_runner.STATE,
        "latest": ai_trends_runner.latest(CFG),
        "history": ai_trends_runner.history(CFG, 14),
    }


@app.post("/api/trends/run", dependencies=[Depends(auth)])
def trends_run(force: bool = Query(False)) -> dict:
    if ai_trends_runner.STATE.get("running"):
        return {"ok": False, "error": "正在跑，等这次结束"}
    import threading
    threading.Thread(target=ai_trends_runner.run, args=(CFG,),
                     kwargs={"force": force}, daemon=True).start()
    log({"event": "trends_manual", "force": force})
    return {"ok": True, "started": True}


# ---- 系统心跳：纯代码服务自检 + 自愈（不调用任何模型）----
# 挂在这里而不是独立 LaunchAgent：本机 launchctl bootstrap 报 I/O error，
# 装不了新常驻服务；pocket 本身开机常驻，由它兜底巡检并拉起 gateway。
_HEART_ON = False


def _start_heartbeat() -> None:
    """和去重/简报同样的闸：双 uvicorn 共用 app，startup 会跑两次。"""
    global _HEART_ON
    if _HEART_ON:
        return
    if (CFG.get("heartbeat") or {}).get("enabled") is False:
        return
    _HEART_ON = True

    def _done(out: dict) -> None:
        rec = {"event": "heartbeat_done", "ok": bool(out.get("ok")),
               "online": next((c["ok"] for c in out.get("checks", [])
                               if c["id"] == "gateway_online"), None),
               "healed": out.get("healed", []),
               "needs_attention": bool(out.get("needs_attention")),
               "error": out.get("error", "")}
        log(rec)

    import threading
    threading.Thread(target=heartbeat.scheduler_loop, args=(CFG, _done),
                     daemon=True, name="heartbeat").start()
    log({"event": "heartbeat_scheduler",
         "interval_sec": int((CFG.get("heartbeat") or {}).get("interval_sec", 120))})


@app.get("/api/heartbeat", dependencies=[Depends(auth)])
def heartbeat_state() -> dict:
    cfg = CFG.get("heartbeat") or {}
    return {
        "enabled": bool(cfg.get("enabled", True)),
        "interval_sec": int(cfg.get("interval_sec", 120)),
        "heal": bool(cfg.get("heal", True)),
        "state": heartbeat.STATE,
    }


@app.post("/api/heartbeat/run", dependencies=[Depends(auth)])
def heartbeat_run(heal: bool = Query(True)) -> dict:
    if heartbeat.STATE.get("running"):
        return {"ok": False, "error": "正在自检，等这次结束"}
    import threading
    threading.Thread(target=heartbeat.run_once, args=(CFG,),
                     kwargs={"heal": heal}, daemon=True).start()
    log({"event": "heartbeat_manual", "heal": heal})
    return {"ok": True, "started": True}


_OCR_ON = False


def _start_ocr() -> None:
    """图片文章 OCR 的定时扫描。同样是双 uvicorn 共用一个闸。"""
    global _OCR_ON
    if _OCR_ON:
        return
    if (CFG.get("ocr") or {}).get("enabled") is False:
        return
    _OCR_ON = True

    def _done(out: dict) -> None:
        if out.get("skipped"):
            return
        log({"event": "ocr_sweep", "ok": bool(out.get("ok")),
             "scanned": out.get("scanned", 0), "done": out.get("done", 0),
             "failed": out.get("failed", 0), "error": out.get("error", "")})

    threading.Thread(target=ocr_runner.scheduler_loop, args=(CFG, _done),
                     daemon=True, name="ocr").start()
    log({"event": "ocr_scheduler",
         "interval_sec": int((CFG.get("ocr") or {}).get("interval_sec", 300))})


@app.get("/api/ocr", dependencies=[Depends(auth)])
def ocr_state() -> dict:
    cfg = CFG.get("ocr") or {}
    return {"enabled": bool(cfg.get("enabled", True)),
            "interval_sec": int(cfg.get("interval_sec", 300)),
            "post_process": bool(cfg.get("post_process", True)),
            "state": ocr_runner.STATE}


@app.post("/api/ocr/run", dependencies=[Depends(auth)])
def ocr_run() -> dict:
    if ocr_runner.STATE.get("running"):
        return {"ok": False, "error": "正在扫描，等这次结束"}
    threading.Thread(target=ocr_runner.run_once, args=(CFG,), daemon=True).start()
    log({"event": "ocr_manual"})
    return {"ok": True, "started": True}


@app.get("/api/sys", dependencies=[Depends(auth)])
def sys_now() -> dict:
    """这台机器现在的内存 / CPU / 负载 / 开机多久（2026-11-22）。

    和 /api/overview 分开是刻意的：那一条要算 token、读进程库、翻日聚合，
    一百多 KB 起步；虫巢那边的「内存」卡片只要五个数，30 秒问一次，
    不该为了它把整包账走一遍。实现全在 sysinfo.py，Windows 也认。
    """
    try:
        return sysinfo.sample()
    except Exception as e:                     # noqa: BLE001  量不到就说量不到
        return {"ok": False, "error": f"{type(e).__name__}: {e}"[:160]}


@app.get("/api/status", dependencies=[Depends(auth)])
def status() -> dict:
    try:
        tools, _ = GW.tools()
        tool_names = [t["function"]["name"] for t in tools]
        gw_ok = True
    except (GatewayError, OSError, ValueError) as e:
        tool_names, gw_ok = [], False
        log({"event": "status_gateway_error", "error": str(e)[:200]})
    files = _walk_inbox()
    return {
        "gateway_ok": gw_ok,
        "model": GW.model,
        "tools": len(tool_names),
        "tool_names": tool_names,
        "inbox": str(INBOX_DIR),
        "inbox_count": len(files),
        "home": str(ALLOWED_ROOTS[0]) if ALLOWED_ROOTS else "",
        "dav": "/dav" if DAV_ENABLED else None,
        "dav_root": str(DAV_ROOT),
        "tls": TLS_ENABLED,
        "cert": tlsmod.describe(TLS_DIR),
        "private_only": PRIVATE_ONLY,
        "trash_count": len(storage.list_trash()),
        "usage": storage.usage(INBOX_DIR),
        "max_upload_mb": CFG.get("max_upload_mb", 500),
    }


@app.get("/dash", response_class=HTMLResponse)
def workbench() -> HTMLResponse:
    """工作台页面。文件放在数据根的 _工作台/ 下，改完刷新即生效，不用重启服务。

    外观（皮肤 + 强调色）在 </head> 前现插：一份 <link> 一份内联脚本。
    为什么不写进 dash.html：皮肤是「这台机器上所有页面共享的一份设置」，
    写死在页面里就等于把设置焊死在模板上，插件页也拿不到。
    """
    p = dashmod.page_path(CFG)
    if not p.exists():
        return HTMLResponse(
            "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>"
            f"<h3>工作台页面还没生成</h3><p style='color:#666'>应当位于：{p}</p></body>",
            status_code=404)
    html = p.read_text(encoding="utf-8")
    # sync 顺手把 appearance.css 刷一遍：json 被手改过也能跟上，不必等下次保存
    appmod.sync(CFG)
    boot = appmod.bootstrap(CFG, href="/__appearance.css")
    if "</head>" in html:
        html = html.replace("</head>", boot + "</head>", 1)
    else:
        html = boot + html
    return HTMLResponse(html, headers={
        "Cache-Control": "no-store, max-age=0",
        "Referrer-Policy": "no-referrer",
        "X-Content-Type-Options": "nosniff",
    })


# —— 外观（2026-11-22）——
# 皮肤 + 强调色存 <数据根>/_工作台/appearance.json，生成物是同目录的 appearance.css。
# 读不要令牌（就一个皮肤名和一个色值，页面本身也是公开 HTML，多一道令牌
# 反而会让插件页和「另开一个浏览器」对不上）；写要令牌。
#
# css 公开的另一个原因：插件页要 <link> 它。CSS 不进 CORS，跨端口引是合法的，
# 五个插件各引一次比自己生成一遍稳（生成逻辑只能有一份，见 appearance.py 文件头）。
@app.get("/__appearance.css")
def appearance_css() -> Response:
    text = appmod.sync(CFG)
    return Response(text, media_type="text/css",
                    headers={"Cache-Control": "no-store, max-age=0"})


@app.get("/__appearance-bg.jpg")
def appearance_bg() -> FileResponse:
    """老路由，兼容还在跑旧 CSS 的页面：返回**当前生效**的那张壁纸。"""
    key = appmod.effective_bg(CFG)
    p = appmod.wall_path(CFG, key) if key else appmod.bg_path(CFG)
    if not p.is_file():
        raise HTTPException(404, "没有壁纸")
    return FileResponse(p, media_type="image/jpeg",
                        headers={"Cache-Control": "public, max-age=86400"})


@app.get("/__wall/{name}.jpg")
def appearance_wall(name: str) -> FileResponse:
    """按 key 取壁纸。key 在白名单里才认（appmod.wall_path 里挡了 ../）。

    单独走图片路由不塞 base64：一张 90KB，内联进 CSS 等于每个页面重下一遍；
    走路由能被浏览器缓存，插件页和主站还能共用同一份缓存条目。"""
    p = appmod.wall_path(CFG, name)
    if not p.is_file():
        raise HTTPException(404, "没有这张壁纸")
    return FileResponse(p, media_type="image/jpeg",
                        headers={"Cache-Control": "public, max-age=86400"})


@app.get("/api/appearance")
def appearance_get() -> dict:
    cur = appmod.load(CFG)
    cur["skins"] = [
        {"id": "classic", "name": "经典", "note": "工作台原本的靛紫 + 中性 slate"},
        {"id": "g1", "name": "拟物", "note": "Aqua / iOS 6：亚麻底、斜面高光、小圆角"},
        {"id": "g2", "name": "扁平毛玻璃", "note": "iOS 7–18：色块 + vibrancy，密度最高"},
        {"id": "g3", "name": "液态玻璃", "note": "iOS 26：镜面白边、大圆角、字重更粗"},
    ]
    cur["presets"] = [{"name": n, "hex": h} for n, h in appmod.PRESETS]
    # 背景：auto = 跟随皮肤默认（每皮肤一张），其余 key = 四套皮肤共用这一张
    cur["bgs"] = [{"key": k, "name": n, "note": d,
                   "has": appmod.has_wall(CFG, k),
                   "ver": appmod.wall_ver(CFG, k)} for k, n, d in appmod.PRESET_BGS]
    cur["bgs"].append({"key": "custom", "name": "自定义",
                       "note": "自己传图，只留这一张",
                       "has": appmod.has_wall(CFG, "custom"),
                       "ver": appmod.wall_ver(CFG, "custom")})
    cur["bg_eff"] = appmod.effective_bg(CFG)
    cur["skin_bg"] = dict(appmod.SKIN_DEFAULT_BG)
    return cur


@app.post("/api/appearance", dependencies=[Depends(auth)])
def appearance_set(body: dict) -> dict:
    """改皮肤 / 强调色 / 背景。返回落盘后的完整设置（含调过明度的强调色）。

    强调色只对 g1/g2/g3 有效；皮肤选 classic 时保留用户上次挑的色，
    但不生效 —— 切回去还在，不用重挑。
    bg 传 "auto" 或某个壁纸 key（见 appearance.PRESET_BGS）；传不认识的 key
    会被忽略而不是报错 —— 设置面板宁可保持原值，也别因为一张图没下好就整块存不上。
    """
    out = appmod.save(CFG, body or {})
    out["ok"] = True
    out["bg_eff"] = appmod.effective_bg(CFG)
    return out


@app.post("/api/appearance/wall", dependencies=[Depends(auth)])
def appearance_wall_upload(body: dict) -> dict:
    """自定义壁纸：前端把图缩到 1800 宽、转成 dataURL 再 POST 上来。

    为什么在前端缩：手机相机的原图动不动 8MB、5000px，传到服务端再缩要多装一个
    Pillow（这台机器的系统 python 没有），而浏览器本来就有一整套 canvas 缩放。
    服务端只做三件事：验前缀、限大小、落盘 —— 收下的字节直接当壁纸用。

    存成固定名 custom.jpg（不是每传一张加一个文件）：壁纸是「一张底」，
    不是相册；要换就是替换。前端靠 ?v= 时间戳破缓存。"""
    raw = str((body or {}).get("data") or "")
    if not raw.startswith("data:image/"):
        raise HTTPException(400, "不是图片数据")
    b64 = raw.split(",", 1)[1] if "," in raw else ""
    try:
        import base64
        blob = base64.b64decode(b64, validate=False)
    except Exception:                                    # noqa: BLE001
        raise HTTPException(400, "图片解码失败")
    if len(blob) < 1024:
        raise HTTPException(400, "图片太小了")
    if len(blob) > 12 * 1024 * 1024:
        raise HTTPException(413, "图片太大（上限 12MB）")
    p = appmod.wall_path(CFG, "custom")
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(blob)
    out = appmod.save(CFG, {"bg": "custom"})   # 传完直接切过去，省一次点击
    out["ok"] = True
    out["bg_eff"] = appmod.effective_bg(CFG)
    return out



@app.get("/api/dash", dependencies=[Depends(auth)])
def dash_data() -> dict:
    """工作台一次性拉全量：探测/巡检/心跳/新闻/存图/阅读/收件箱/去重/OCR。"""
    data = dashmod.payload(CFG)
    data["ledger"] = ledger.payload(CFG)
    return data


@app.get("/api/notes", dependencies=[Depends(auth)])
def notes_read() -> dict:
    return dashmod.read_notes(CFG)


# —— AI 工作计划（2026-11-22）——
#   数据在 19-AI工作计划/计划.json，读写、解析、与 token 的联通全部在 plan.py。
#   这里只当门口，三个约定：
#     1. 写接口一律回整份视图（view），前端不用写完再问一次 —— 两次往返之间
#        另一个标签页或 agent 可能又改了一笔，直接给最新的是唯一一致的答案。
#     2. 「AI 判断归到哪个子任务」只在 triage 这一个口子，且是**建议不落库**；
#        落库要显式 assign。模型说错话最多多一条待确认，不会把计划写坏。
#     3. token / 项目归属不经过任何模型 —— 那是 plan.usage_of() 的确定性打分。
@app.get("/api/plan", dependencies=[Depends(auth)])
def plan_read() -> dict:
    try:
        return planmod.payload(planmod.root_of(CFG))
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "projects": [], "inbox": []}


def _plan_after(root) -> dict:
    try:
        return planmod.payload(root)
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}"}


@app.post("/api/plan/act", dependencies=[Depends(auth)])
def plan_act(body: dict) -> dict:
    """增删改、打钩、导入、收件箱归位，全部 action 走这一个口。"""
    root = planmod.root_of(CFG)
    try:
        r = planmod.act(root, body or {})
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}"}
    if r.get("ok") and not r.get("dry"):
        r.pop("doc", None)                             # 原始 doc 对前端没用，视图更好用
        r["view"] = _plan_after(root)
    return r


@app.post("/api/plan/triage", dependencies=[Depends(auth)])
def plan_triage(body: dict) -> dict:
    """收件箱归位建议。代码先跑，拿不准的才问本机模型；两条都只回建议。"""
    b = body if isinstance(body, dict) else {}
    ids = [str(x) for x in (b.get("ids") or []) if str(x).strip()]
    root = planmod.root_of(CFG)
    try:
        items = planmod.candidates(root, ids)
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "items": []}
    out = {"ok": True, "items": items, "ai": False, "note": ""}
    if not b.get("ai"):
        return out
    weak = [x for x in items if float((x.get("hint") or {}).get("score") or 0) < 0.45]
    if not weak:
        out["note"] = "代码已经能归位，不用问模型"
        return out
    try:
        doc = planmod.load(root)
        model = dashmod._local_model_cfg(CFG)
        prompt = planmod.triage_prompt(weak, doc["projects"])
        raw = dashmod._llm_json(prompt, model, num_predict=600, timeout=90)
        got = dashmod._grab_json(raw)
        arr = got.get("items") if isinstance(got, dict) else None
        known_p = {p["id"] for p in doc["projects"]}
        known_t = {n["id"] for p in doc["projects"] for n, _s, _pd in planmod.walk(p["tasks"])}
        by_id = {x["id"]: x for x in weak}
        hit = 0
        for it in (arr or []):
            if not isinstance(it, dict):
                continue
            src = by_id.get(str(it.get("id")))
            if src is None:
                continue
            pid = str(it.get("proj") or "")
            par = str(it.get("parent") or "")
            # 模型给的 id 必须是真实存在的；编出来的当没看见，保留代码建议
            if pid and pid not in known_p:
                pid = ""
            if par and par not in known_t:
                par = ""
            src["hint"].update({
                "proj": pid, "task_id": par,
                "task": str(it.get("title") or src["t"])[:300],
                "new_task": not bool(par),
                "score": 0.0,
                "why": "模型判断" + (f"：挂到「{par}」" if par else "：新建一条子任务"),
                "ai": True,
                "new_proj": str(it.get("new_proj") or "")[:80],
            })
            hit += 1
        out["ai"] = True
        out["model"] = model
        out["note"] = f"模型给了 {hit} 条判断" if hit else "模型没给出可用判断，先按代码建议"
    except Exception as e:                             # noqa: BLE001
        out["note"] = f"模型没答上来（{type(e).__name__}），先按代码建议"
    return out


@app.post("/api/plan/assign", dependencies=[Depends(auth)])
def plan_assign(body: dict) -> dict:
    """把（人确认过的）归位建议落库。每项独立成败。"""
    b = body if isinstance(body, dict) else {}
    items = [x for x in (b.get("items") or []) if isinstance(x, dict)]
    if not items:
        return {"ok": False, "error": "没有要落库的条目"}
    root = planmod.root_of(CFG)
    r = planmod.apply_ai_assign(root, items)
    r["view"] = _plan_after(root)
    return r


# —— 每日简报（2026-11-21）——
#   五段账：中台 / token / 工作与计划 / 新闻 / 关注。生成和推送都收在 brief.py，
#   这里只当门口。三个路由的分工：
#     GET  /api/brief      读（有当天落盘的就直接用，不吃 payload 重算）
#     POST /api/brief/run  重建（force），可选同时推微信
#     POST /api/brief/push 只推送，不重算
#   都用普通 def：现算一次要探活十几个端口 + 扫会话流水，几秒钟，
#   别拿 async def 把事件循环堵住。
@app.get("/api/brief", dependencies=[Depends(auth)])
def brief_get(day: str = Query(""), force: int = Query(0)) -> dict:
    data = briefmod.build(day, CFG, force=bool(force))
    data["days"] = briefmod.days_for(CFG)
    return data


@app.post("/api/brief/run", dependencies=[Depends(auth)])
def brief_run(body: dict) -> dict:
    day = str((body or {}).get("day") or "")
    data = briefmod.build(day, CFG, force=True)
    data["days"] = briefmod.days_for(CFG)
    if (body or {}).get("push"):
        # 推送**逐条发**，一条一段 —— 微信那头是 4B，一次灌五段会爆上下文
        data["push"] = briefmod.push(data, CFG)
    return data


@app.post("/api/brief/push", dependencies=[Depends(auth)])
def brief_push(body: dict) -> dict:
    day = str((body or {}).get("day") or "")
    data = briefmod.build(day, CFG, force=False)
    return briefmod.push(data, CFG)

@app.post("/api/notes", dependencies=[Depends(auth)])
def notes_write(body: dict) -> dict:
    return dashmod.write_notes(CFG, body)


# —— 行程：一段话交给本机 4B 读成字段（2026-11-18）——
#   同步阻塞 3~8 秒（冷启动更久），所以路由用普通 def 让 FastAPI 丢线程池，
#   别用 async def 把事件循环卡住。解析失败也要给结果：dash.py 里会退回正则。
@app.post("/api/parse-event", dependencies=[Depends(auth)])
def parse_event(body: dict) -> dict:
    return dashmod.parse_event_text(CFG, str(body.get("text") or ""))


# —— 工作记录 ——
#   自动那半在 /api/dash 的 worklog 里（现算，不落盘）；这里只管「补记一条」，
#   AI 改完代码/写完文档可以自己 POST 过来，落在 _工作台/worklog.json。

@app.get("/api/worklog", dependencies=[Depends(auth)])
def worklog_read() -> dict:
    return dashmod._worklog(CFG)


@app.post("/api/worklog", dependencies=[Depends(auth)])
def worklog_add(body: dict) -> dict:
    return dashmod.append_worklog(CFG, body)


# —— Agent 总线：别的 AI（Trae / Cursor / Claude Code）读写中台 ——
#   口径全在 agentbus.py，这里只做 HTTP 壳。为什么要单独一套而不是复用上面
#   那些接口：上面每个接口都是「某个页面要什么就给什么」，字段随页面改；
#   agent 需要的是**稳定的**契约 —— 字段加只加不删、写坏了只丢最后一行。
#
#   鉴权：本机回环免令牌。理由 —— 本机任何进程本来就能读 ~/Documents 下的数据，
#   再要它去 config.json 里翻令牌只是给人添麻烦；而且 Trae 的规则里贴不了密钥。
#   别的来源（局域网 / Tailscale）仍走 auth，令牌一个都不少。

def _is_loopback(request: Request) -> bool:
    host = (request.client.host if request.client else "") or ""
    return host in ("127.0.0.1", "::1", "localhost")


def auth_agent(request: Request, x_token: str | None = Header(None),
               k: str | None = Query(None)) -> None:
    if _is_loopback(request):
        return
    auth(x_token, k)


AGENT_FIELDS_USAGE = {
    "client": "哪个客户端在报（trae / cursor / claude-code），必填",
    "in": "输入 token（int，>=0）",
    "out": "输出 token（int，>=0）",
    "cached": "缓存命中的输入 token（可省）",
    "calls": "请求次数，缺省 1",
    "model": "模型名，缺省「未知模型」",
    "ts": "时间，缺省现在；epoch 秒/毫秒 或 2026-11-21T21:50",
    "cost": "真金白银（元），订阅制不知道就别填",
    "currency": "缺省 CNY",
    "task": "这笔在干什么（120 字内）",
    "ref": "会话 id / 链接，方便回查",
    "id": "幂等键；不填按内容指纹去重",
}
AGENT_FIELDS_EVENT = {
    "ns": "归档槽位：设备 / 伙食 / 账目 / 工作 / 阅读 / 系统 / 杂项",
    "type": "note / metric / task / link / file / span，缺省 note",
    "title": "一句话标题（120 字内）",
    "text": "正文（4000 字内，别贴大段材料）",
    "data": "结构化字段（对象或数组）",
    "tags": "字符串数组，最多 8 个",
    "ts": "同用量",
    "id": "幂等键",
}


@app.get("/agent", response_class=PlainTextResponse)
def agent_doc() -> PlainTextResponse:
    """给 agent 看的一页说明（Markdown）。curl 下来直接读，不用翻代码。

    顺带落一份 README.md 到 agents/ 目录：人在访达里点开也能看懂，
    agent 直接 tail 文件时也有据可查。
    """
    root = dashmod._root(CFG)
    try:
        txt = agentbus.readme(root)
    except Exception as e:                             # noqa: BLE001
        return PlainTextResponse(f"说明书生成失败：{e}", status_code=500)
    try:
        (agentbus.agents_dir(root) / "README.md").write_text(txt, encoding="utf-8")
    except OSError:
        pass
    return PlainTextResponse(txt, media_type="text/markdown; charset=utf-8")


@app.get("/api/agent/schema", dependencies=[Depends(auth_agent)])
def agent_schema() -> dict:
    """机器可读的能力清单：agent 先读这个，再决定调哪个。"""
    root = dashmod._root(CFG)
    return {
        "ok": True, "v": agentbus.V, "service": "个人中台",
        "base": "http://127.0.0.1:8323",
        "doc": "/agent",
        "root": str(root),
        "files": {"usage": str(agentbus.usage_path(root)),
                  "events": str(agentbus.events_path(root))},
        "endpoints": [
            {"method": "POST", "path": "/api/agent/usage",
             "what": "报一笔 token 用量", "body": AGENT_FIELDS_USAGE},
            {"method": "POST", "path": "/api/agent/event",
             "what": "写一条事件", "body": AGENT_FIELDS_EVENT},
            {"method": "GET", "path": "/api/agent/read",
             "what": "读自报账",
             "query": {"ns": "all|usage|events", "since": "24h / 7d / today / 日期",
                       "limit": "1-1000，缺省 100", "client": "按客户端过滤",
                       "q": "全文关键词"}},
            {"method": "GET", "path": "/api/agent/brief",
             "what": "中台现状摘要（token / 花费 / 最近工作 / 设备数）"},
        ],
        "notes": [
            "本机 127.0.0.1 免令牌；别的来源带 x-token",
            "只追加不改写；重复 id 自动吞掉",
            f"单条正文上限 {agentbus.MAX_TEXT} 字，一次最多 {agentbus.MAX_BATCH} 条",
        ],
    }


@app.post("/api/agent/usage", dependencies=[Depends(auth_agent)])
async def agent_usage(request: Request) -> dict:
    """写用量。body 可以是单条对象，也可以是一批（数组）。"""
    try:
        payload = await request.json()
    except ValueError:
        return JSONResponse({"ok": False, "error": "body 不是 JSON"}, status_code=400)
    if not isinstance(payload, (dict, list)):
        return JSONResponse({"ok": False, "error": "body 要是对象或数组"}, status_code=400)
    res = agentbus.add_usage(dashmod._root(CFG), payload)
    if not res.get("ok") and res.get("bad"):
        return JSONResponse(
            {**res, "error": "没有可写的字段：client 必填，in/out/calls 至少一个 >0"},
            status_code=400)
    return res


@app.post("/api/agent/event", dependencies=[Depends(auth_agent)])
async def agent_event(request: Request) -> dict:
    try:
        payload = await request.json()
    except ValueError:
        return JSONResponse({"ok": False, "error": "body 不是 JSON"}, status_code=400)
    if not isinstance(payload, (dict, list)):
        return JSONResponse({"ok": False, "error": "body 要是对象或数组"}, status_code=400)
    res = agentbus.add_event(dashmod._root(CFG), payload)
    if not res.get("ok") and res.get("bad"):
        return JSONResponse(
            {**res, "error": "没有可写的字段：title / text / data 至少一个"},
            status_code=400)
    return res


@app.get("/api/agent/read", dependencies=[Depends(auth_agent)])
def agent_read(ns: str = "all", since: str = "7d", limit: int = 100,
               q: str = "", client: str = "") -> dict:
    return agentbus.read(dashmod._root(CFG), ns=ns, since=since,
                         limit=limit, q=q, client=client)


@app.post("/api/trae/note", dependencies=[Depends(auth_agent)])
async def trae_note(request: Request) -> dict:
    """TRAE 收尾时自报一笔「这一轮干了什么、花了多少」，进 **TRAE 卡**。

    跟 /api/agent/event 的分工：那条进 agent 总线（给别的 agent 读），
    这条落 `_本机工作/TRAE自述.jsonl`，由 dash._trae_self_items() 读成逐轮记录。
    同一 task 5 分钟内只记一次，防止模型每轮重复刷同一句。

    用量字段（model / in / out / cached / calls）可省：估不出来就别填，
    卡片按 0 显示。**这个接口在没装 cuntu 的干净机器上也得能用** —— 以前
    trae_work 导入失败就直接 503，等于「新装的中台收不到任何自述」。
    """
    try:
        body = await request.json()
    except ValueError:
        return JSONResponse({"ok": False, "error": "body 不是 JSON"}, status_code=400)
    if not isinstance(body, dict):
        return JSONResponse({"ok": False, "error": "body 要是对象"}, status_code=400)
    task = " ".join(str(body.get("task") or "").split())[:80]
    if not task:
        return JSONResponse({"ok": False, "error": "task 必填"}, status_code=400)
    try:
        at = float(body.get("at") or time.time())
    except (TypeError, ValueError):
        at = time.time()

    def _num(key: str) -> int:
        try:
            return max(0, int(float(body.get(key) or 0)))
        except (TypeError, ValueError):
            return 0

    rec = {
        "at": round(at, 1),
        "task": task,
        "summary": " ".join(str(body.get("summary") or "").split())[:400],
        "project": str(body.get("project") or "").strip()[:120],
        "model": str(body.get("model") or "").strip()[:60],
        "tin": _num("in"),
        "tout": _num("out"),
        "cached": _num("cached"),
        "calls": _num("calls"),
        "source": "TRAE 自述",
    }
    # 模块在就用它的路径（开发机上和采集层同源），不在就自己拼一份同样的位置 ——
    # 两边都指向 `<数据根>/_本机工作/TRAE自述.jsonl`，谁写谁读都对得上。
    p = Path(getattr(twmod, "NOTE_PATH", "") or
             (paths.data_root() / "_本机工作" / "TRAE自述.jsonl"))
    recent: set[str] = set()
    if p.exists():
        try:
            lines = p.read_text(encoding="utf-8", errors="replace").splitlines()[-80:]
        except OSError:
            lines = []
        for ln in lines:
            try:
                d = json.loads(ln)
            except ValueError:
                continue
            try:
                if abs(float(d.get("at") or 0) - at) < 300:
                    recent.add(str(d.get("task") or ""))
            except (TypeError, ValueError):
                continue
    if task in recent:
        return {"ok": True, "dup": True, "note": rec}
    try:
        p.parent.mkdir(parents=True, exist_ok=True)
        with p.open("a", encoding="utf-8") as f:
            f.write(json.dumps(rec, ensure_ascii=False) + "\n")
    except OSError as e:
        return JSONResponse({"ok": False, "error": f"写不进去：{e}"}, status_code=500)
    if hwmod is not None:                       # 让下一次 pulse 立刻重算，别等 20 秒
        cache = getattr(hwmod, "_PULSE_CACHE", None)
        if isinstance(cache, dict):
            cache["at"] = 0.0
    return {"ok": True, "note": rec}


@app.get("/api/agent/brief", dependencies=[Depends(auth_agent)])
def agent_brief(days: int = 7) -> dict:
    """中台现状：一份给 agent 的「现在什么情况」。

    刻意不返回整份 /api/dash（那是给页面画图的，几百 KB）：agent 要的是
    「今天花了多少、最近在干什么、手里有几台设备」，几 KB 就够。
    """
    root = dashmod._root(CFG)
    days = max(1, min(int(days or 7), 90))
    out: dict = {
        "ok": True, "at": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "root": str(root),
        "doc": "/agent",
        "self_report": {"days": days, "totals": agentbus.totals(root, days)},
    }
    # 自报账最近几条：agent 接手前先看「上一个 agent 干了什么」
    try:
        out["self_report"]["recent"] = agentbus.read(
            root, ns="all", since=f"{days}d", limit=8)["items"]
    except Exception:                                  # noqa: BLE001
        pass
    # 本机真实 token（WorkBuddy 的账，算今天的）
    try:
        if hwmod is not None:
            today = datetime.now().strftime("%Y-%m-%d")
            hrs = hwmod.hourly_tokens(today)
            out["today_tokens"] = {
                "in": sum(h["in"] for h in hrs),
                "out": sum(h["out"] for h in hrs),
                "calls": sum(h["calls"] for h in hrs),
                "apps": sorted({a for h in hrs for a in (h.get("apps") or {})}),
            }
            r = hwmod.rounds_items("today")
            rows = (r.get("items") or [])[:8]
            out["recent_work"] = [
                {"at": (x.get("start") or "")[11:16], "client": x.get("client"),
                 "task": (x.get("title") or x.get("task") or "")[:60],
                 "status": x.get("status")}
                for x in rows]
    except Exception as e:                             # noqa: BLE001
        out["hostwork_error"] = f"{type(e).__name__}: {e}"
    # 设备台账：只数个数，明细走设备服务（8778）
    try:
        arc = Path(root) / "13-设备" / "档案"
        out["devices"] = {"n": len([p for p in arc.glob("*.json")]),
                          "detail": "http://127.0.0.1:8778/"}
    except OSError:
        pass
    return out



# —— 接入别的 Agent（2026-11-22）——
#   中台页面「本机工作」卡上那颗「连接 Agent」按钮打的就是这一组。
#   四步一条链：认人（scan）→ 装说明（install，写进那个客户端的技能/规则目录，
#   它下次开会话就知道中台是什么）→ 收回报（report，agent 核对数据目录后回执）→
#   按回执导入（import）。细节在 agent_link.py 文件头。

def _link_base(request: Request) -> str:
    """说明书里写给 agent 的地址。agent 和它在同一台机器上，所以永远指回环。"""
    try:
        port = int((CFG or {}).get("local_port") or (CFG or {}).get("port") or 8323)
    except (TypeError, ValueError):
        port = 8323
    return f"http://127.0.0.1:{port}"


@app.get("/api/agent/link/scan", dependencies=[Depends(auth_agent)])
def agent_link_scan() -> dict:
    """装了哪些 agent、数据在哪、说明书装没装、上次回报/导入。"""
    try:
        return agent_link.scan(dashmod._root(CFG))
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "agents": []}


@app.get("/api/agent/link/prompt", dependencies=[Depends(auth_agent)])
def agent_link_prompt(request: Request, agent: str = "trae") -> PlainTextResponse:
    txt = agent_link.prompt(agent, _link_base(request))
    if not txt:
        return PlainTextResponse(f"不认识这个 agent：{agent}", status_code=404)
    return PlainTextResponse(txt, media_type="text/markdown; charset=utf-8")


@app.post("/api/agent/link/install", dependencies=[Depends(auth_agent)])
def agent_link_install(body: dict, request: Request) -> dict:
    """把说明书写进那个客户端的技能目录。Trae 是 ~/.trae-cn/skills/pocket-bridge/SKILL.md。"""
    return agent_link.install(dashmod._root(CFG), str(body.get("agent") or "trae"),
                              _link_base(request))


@app.post("/api/agent/link/report", dependencies=[Depends(auth_agent)])
async def agent_link_report(request: Request) -> dict:
    """agent 核对完数据目录后的回执。路径要真存在，导入按它说的走。"""
    try:
        payload = await request.json()
    except ValueError:
        return JSONResponse({"ok": False, "error": "body 不是 JSON"}, status_code=400)
    if not isinstance(payload, dict):
        return JSONResponse({"ok": False, "error": "body 要是对象"}, status_code=400)
    return agent_link.report(dashmod._root(CFG), payload)


@app.post("/api/agent/link/import", dependencies=[Depends(auth_agent)])
def agent_link_import(body: dict | None = None) -> dict:
    """按回执（没有就按注册表）把那个 agent 的明文数据导进中台自报账。"""
    b = body or {}
    return agent_link.do_import(dashmod._root(CFG), str(b.get("agent") or "trae"),
                                int(b.get("days") or 30))


# —— 工作流水 ——
#   半年的手写流水落在 09-个人工作台/流水/流水.json，近千条不进 notes.json。
#   首屏只要摘要（/api/dash 的 flow 字段），点开某个月才拉全月；勾一条只改一条。

@app.get("/api/flow", dependencies=[Depends(auth)])
def flow_month(month: str = "") -> dict:
    return dashmod.flow_month(CFG, month.strip())


@app.post("/api/flow", dependencies=[Depends(auth)])
def flow_toggle(body: dict) -> dict:
    return dashmod.toggle_flow(CFG, str(body.get("id") or ""), bool(body.get("done")))


@app.post("/api/flow/add", dependencies=[Depends(auth)])
def flow_add(body: dict) -> dict:
    """首屏速记：补一条进今天的流水（json + 底稿 md 都写）。"""
    return dashmod.flow_add(CFG, str(body.get("text") or ""), str(body.get("day") or ""))


# —— 记账 ——
#   丢一张订单截图进来：存原图 → Vision 认字 → 裁出商品主图 → 一件一条落
#   <数据根>/12-记账/账本.json。OCR/裁图是子进程，路由用普通 def 丢线程池。

@app.get("/api/ledger", dependencies=[Depends(auth)])
def ledger_all() -> dict:
    return ledger.payload(CFG)


@app.get("/api/ledger/item", dependencies=[Depends(auth)])
def ledger_one(id: str) -> dict:
    return ledger.full_item(CFG, id)


@app.get("/api/ledger/img", dependencies=[Depends(auth)])
def ledger_img(id: str, w: int = 400, raw: int = 0) -> FileResponse:
    """卡片上那张产品图 / 原截图。账本里存的是相对路径，这里按 id 解析，
    缩略图沿用 sips 的缓存（一张卡片图 30KB 和 400KB 是两种体验）。"""
    p = ledger.image_file(CFG, id, bool(raw))
    if not p:
        raise HTTPException(404, "没有这张图")
    if raw:
        return FileResponse(p, media_type=storage.mime_of(p),
                            headers={"Cache-Control": "private, max-age=60"})
    w = max(80, min(int(w), 1200))
    try:
        t = _make_thumb(p, w)
    except OSError:
        t = None
    if t is None:
        return FileResponse(p, media_type=storage.mime_of(p),
                            headers={"Cache-Control": "private, max-age=60"})
    return FileResponse(t, media_type="image/jpeg",
                        headers={"Cache-Control": "private, max-age=600"})


@app.post("/api/ledger/upload", dependencies=[Depends(auth)])
def ledger_upload(files: list[UploadFile] = File(...)) -> dict:
    ok, bad = [], []
    for f in files:
        try:
            raw = f.file.read()
        except OSError as e:
            bad.append({"name": f.filename or "", "error": str(e)[:120]})
            continue
        r = ledger.ingest(CFG, raw, f.filename or "")
        (ok if r.get("ok") else bad).append(r if r.get("ok") else
                                            {"name": f.filename or "", "error": r.get("error")})
    return {"ok": bool(ok), "added": ok, "failed": bad}


@app.post("/api/ledger/update", dependencies=[Depends(auth)])
def ledger_update(body: dict) -> dict:
    return ledger.update(CFG, str(body.get("id") or ""), body.get("patch") or {})


@app.post("/api/ledger/remove", dependencies=[Depends(auth)])
def ledger_remove(body: dict) -> dict:
    return ledger.remove(CFG, str(body.get("id") or ""))


@app.post("/api/ledger/image", dependencies=[Depends(auth)])
def ledger_image(file: UploadFile = File(...), id: str = "") -> dict:
    """换产品图：这张图直接当产品图用（不再自动裁）。"""
    try:
        raw = file.file.read()
    except OSError as e:
        raise HTTPException(400, f"读不出这张图：{e}")
    return ledger.replace_image(CFG, id, raw, file.filename or "")


@app.post("/api/ledger/open", dependencies=[Depends(auth)])
def ledger_open() -> dict:
    d = ledger.ledger_dir(CFG)
    d.mkdir(parents=True, exist_ok=True)
    try:
        subprocess.Popen(["open", str(d)], stdin=subprocess.DEVNULL,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    except OSError as e:
        return {"ok": False, "error": str(e)}
    return {"ok": True, "dir": str(d)}


# —— 文件传输助手：自己给自己发的东西都落这儿 ——
#   微信插件把每条消息都 POST 进来（有链接的另外还走 /api/intake 去抓），
#   网页这边也能发。同一条时间线，谁发的都看得见。
@app.get("/api/transfer", dependencies=[Depends(auth)])
def transfer_list(limit: int = 40, q: str = "", kind: str = "") -> dict:
    return transfer.payload(CFG, max(1, min(limit, 300)), q.strip(), kind.strip())


_TF_KIND_LABEL = {"image": "图片", "video": "视频", "voice": "语音", "file": "文件"}
# 一次推送最多试三回，间隔越试越久：微信媒体通道会整段坏几分钟，
# 试一次就放弃等于把东西丢了；试太密又只是白撞。
_TF_PUSH_BACKOFF = (0, 20, 90)


def _tf_push_try(item: dict, fallback: bool = True) -> tuple:
    """推一次，返回 (本体到了吗, 渠道原话)。

    **「本体到了吗」不等于「这次调用成功」**：媒体推不出去时会退回一句话
    （［图片］xxx.png），手机上算收到了一条，但那不是人想要的东西 ——
    账本按「本体」算，红色提示才说得清「图没到」。
    """
    try:
        import wechat_push
    except Exception:                   # noqa: BLE001  推不出去不影响记账
        return False, "没有 wechat_push"
    kind = str(item.get("kind") or "text")
    if kind in ("text", "link"):
        text = str(item.get("text") or "").strip()
        if not text:
            return False, "空文本"
        ok = wechat_push.send(text, log=log)
        return bool(ok), "" if ok else (wechat_push.LAST_ERROR or "文本没发出去")

    label = _TF_KIND_LABEL.get(kind, "文件")
    name = str(item.get("name") or "未命名")
    caption = f"［{label}］{name}"
    try:
        p = transfer.file_of(CFG, str(item.get("id") or ""))
    except Exception:                   # noqa: BLE001
        p = None
    ok = False
    if p is not None:
        try:
            # 媒体本体不带标题行（2026-11-21 用户要求）：传 caption 会让手机上先
            # 多出一条「［图片］xxx.png」，图反而要靠下翻。caption 只留给失败兜底。
            ok = wechat_push.send_media(p, "", log=log)
        except Exception:               # noqa: BLE001
            ok = False
    if ok:
        return True, ""
    err = wechat_push.LAST_ERROR or "本体没发出去"
    if fallback:
        # 本体发不出去，退回一句话：至少让手机知道有这件东西。
        # 只试第一回 —— 重试时再补一句会刷屏，而且那句话本来也不解决问题。
        try:
            note = "；只补了一句话" if wechat_push.send(caption, log=log) else ""
        except Exception:               # noqa: BLE001
            note = ""
        err = err + note
    return False, err


def _tf_push_wechat(item: dict) -> None:
    """网页发的东西回推一份到微信。**带重试，并且每次都记账。**

    以前这里只发一次、失败只写一行日志 —— 2026-11-21 的坑就是：微信侧的媒体
    通道整段坏掉（sendMessage ret=-2 prepare failed），图片/文件全推不出去，
    但卡片上照常显示「发送成功」，人要等到「手机怎么没收到」才发现。

    现在：三回（0 / 20 / 90 秒），每回一行账（14-传输/推送失败.jsonl），
    页面读账本画红条，巡检读账本判 transfer_push —— 口径只有账本一处。
    """
    err = "推不出去"
    for i, delay in enumerate(_TF_PUSH_BACKOFF):
        if delay:
            time.sleep(delay)
        ok, err = _tf_push_try(item, fallback=(i == 0))
        try:
            transfer.log_push(CFG, item, ok, err, attempts=i + 1)
        except Exception:               # noqa: BLE001
            pass
        log({"event": "tf_push", "ok": bool(ok), "kind": item.get("kind"),
             "name": str(item.get("name") or "")[:60], "attempt": i + 1,
             "error": ("" if ok else err)[:160]})
        if ok:
            return


@app.post("/api/transfer/add", dependencies=[Depends(auth)])
def transfer_add(body: dict) -> dict:
    """记一条。path 是这台机器上的文件（微信插件下载下来的图/文件走这个）。

    网页来源（src=web，默认）会在记账之后异步回推微信，微信来的不回推，
    否则自己发的东西自己再收一遍。push=false 可关掉这一条的推送。
    """
    src = str(body.get("src") or "web")
    r = transfer.add(CFG,
                     text=str(body.get("text") or ""),
                     path=str(body.get("path") or ""),
                     name=str(body.get("name") or ""),
                     mime=str(body.get("mime") or ""),
                     src=src,
                     conv=str(body.get("conv") or ""),
                     at=str(body.get("at") or ""),
                     move=bool(body.get("move")))
    if r.get("ok") and src == "web" and body.get("push", True):
        # 放后台：网关推送要握手 200~800ms，不该拖慢网页这条请求
        threading.Thread(target=_tf_push_wechat, args=(r.get("item") or {},),
                         daemon=True).start()
    return r


@app.post("/api/transfer/upload", dependencies=[Depends(auth)])
def transfer_upload(files: list[UploadFile] = File(...)) -> dict:
    ok, bad = [], []
    for f in files:
        try:
            raw = f.file.read()
        except OSError as e:
            bad.append({"name": f.filename or "", "error": str(e)[:120]})
            continue
        tmp = Path(tempfile.mkdtemp(prefix="tf_")) / (f.filename or "未命名")
        try:
            tmp.write_bytes(raw)
            r = transfer.add(CFG, path=tmp, name=f.filename or tmp.name,
                             mime=f.content_type or "", src="web", move=True)
        except OSError as e:
            r = {"ok": False, "error": f"写不进去：{e}"}
        finally:
            shutil.rmtree(tmp.parent, ignore_errors=True)
        (ok if r.get("ok") else bad).append(r.get("item") if r.get("ok")
                                            else {"name": f.filename or "", "error": r.get("error")})
    if ok:
        # 网页传上来的文件逐个回推微信（本体发过去，不是只发一句话）。
        # 一次选了太多就只推前 6 个，剩下的补一句汇总，别把手机刷爆。
        for it in ok[:6]:
            threading.Thread(target=_tf_push_wechat, args=(it,), daemon=True).start()
        if len(ok) > 6:
            rest = {"kind": "text",
                    "text": f"另外 {len(ok) - 6} 个文件已存进传输助手，去网页看"}
            threading.Thread(target=_tf_push_wechat, args=(rest,), daemon=True).start()
    return {"ok": bool(ok), "items": ok, "failed": bad}


@app.post("/api/transfer/repush", dependencies=[Depends(auth)])
def transfer_repush(body: dict) -> dict:
    """把「没推到手机」的那几条再推一次。

    为什么需要这个按钮：媒体通道是微信侧的状态（prepare failed），好的时候
    自己就恢复了。没有这个按钮，人只能一条条下载再手动发；有了它，通道一好
    点一下就把积压的补发完。不传 ids 就补最近 6 小时里**最后仍失败**的那些。
    """
    want = [str(x) for x in ((body or {}).get("ids") or []) if x]
    if not want:
        want = transfer.push_summary(CFG).get("ids") or []
    rows = {str(r.get("id")): r for r in transfer.items(CFG, limit=400).get("items") or []}
    todo = [rows[i] for i in want if i in rows]
    for it in todo:
        threading.Thread(target=_tf_push_wechat, args=(it,), daemon=True).start()
    return {"ok": True, "queued": len(todo),
            "missing": [i for i in want if i not in rows][:10]}


@app.post("/api/transfer/del", dependencies=[Depends(auth)])
def transfer_del(body: dict) -> dict:
    return transfer.remove(CFG, str(body.get("id") or ""))


@app.get("/api/transfer/file", dependencies=[Depends(auth)])
def transfer_file(id: str, w: int = 0, dl: int = 0) -> FileResponse:
    """条目对应的文件。dl=1 走下载（带上原文件名），w 给了就出缩略图。"""
    p = transfer.file_of(CFG, id)
    if not p:
        raise HTTPException(404, "这个条目没有文件（或已经被删掉）")
    mime = storage.mime_of(p)
    # 落盘名是「条目 id_原文件名」，下载时还原原文件名（原名里本来就有下划线也不怕）
    iid = str(id or "")
    name = p.name[len(iid) + 1:] if iid and p.name.startswith(iid + "_") else p.name
    if dl:
        return FileResponse(p, media_type=mime, filename=name,
                            headers={"Cache-Control": "private, max-age=60"})
    if w and transfer.is_image(p):
        w = max(60, min(int(w), 1600))
        try:
            t = _make_thumb(p, w)
        except OSError:
            t = None
        if t is not None:
            return FileResponse(t, media_type="image/jpeg",
                                headers={"Cache-Control": "private, max-age=600"})
    return FileResponse(p, media_type=mime,
                        headers={"Cache-Control": "private, max-age=120"})


# —— 本机工作：数据源自检 + 任务清单 ——
#   2026-11-20 之前这里是把 8775 的接口转发一遍：页面走 8321 https 时直连 http 会被
#   浏览器按混合内容拦掉，只能服务端绕一圈，还多养一个常驻进程。现在 8775 的
#   采集线程直接挂在口袋进程里（见 _start_hostwork），同一个进程内调用就行。
@app.get("/api/tasks", dependencies=[Depends(auth)])
def tasks_proxy(range: str = "7d") -> dict:
    key = range if range in ("today", "7d", "30d", "all") else "7d"
    if hwmod is None:
        return {"ok": False, "error": f"本机工作模块没装上（{_HW_IMPORT_ERR}）", "groups": []}
    try:
        return hwmod.tasks_view(key)
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "groups": []}


# —— 插件卡片：把插件页里的某张卡「拎」到主页 ——
#   清单在 cuntu/pgkit.py 的 CARDS 里（跟页面上的 data-card 写在一起），
#   这里只负责跟「装了哪些插件、有没有在线」合起来给前端。
_PCARD_CACHE: dict = {"at": 0.0, "data": None}


@app.get("/api/plugincards", dependencies=[Depends(auth)])
def plugincards_api(fresh: int = 0) -> dict:
    now = time.time()
    if not fresh and _PCARD_CACHE.get("data") and now - float(_PCARD_CACHE.get("at") or 0) < 30:
        return _PCARD_CACHE["data"]
    try:
        import pgkit
    except Exception as e:                                 # noqa: BLE001
        return {"ok": False, "error": f"pgkit 不可用（{e}）", "items": []}
    try:
        apps = dashmod.apps_payload(CFG)
    except Exception as e:                                 # noqa: BLE001
        return {"ok": False, "error": f"读不到插件清单（{e}）", "items": []}
    rows = []
    for a in (apps.get("builtin") or []) + (apps.get("plugins") or []):
        pid = str(a.get("id") or "")
        cards = pgkit.CARDS.get(pid) or []
        if not cards:
            continue
        rows.append({"id": pid, "name": a.get("name") or pid,
                     "port": a.get("port"), "up": bool(a.get("up")),
                     "url": a.get("url") or "",
                     "cards": [{"id": c[0], "name": c[1], "w": c[2], "h": c[3]} for c in cards]})
    data = {"ok": True, "items": rows, "at": datetime.now().strftime("%H:%M:%S")}
    _PCARD_CACHE.update(at=now, data=data)
    return data


def _plugin_port(pid: str) -> int:
    try:
        apps = dashmod.apps_payload(CFG)
    except Exception:                                      # noqa: BLE001
        return 0
    for a in (apps.get("builtin") or []) + (apps.get("plugins") or []):
        if str(a.get("id") or "") == pid and a.get("port"):
            try:
                return int(a["port"])
            except (TypeError, ValueError):
                return 0
    return 0


# 反代一层 /px/<插件>/<路径>：主页走 8321(https) 时直接 iframe 一个 http 插件页
# 会被浏览器按混合内容拦掉，而且跨源也拿不到 cookie/同源脚本。转发成同源就没这些事，
# 插件页里的相对路径 /api/xxx 也会自动落到 /px/<插件>/api/xxx 上。
@app.api_route("/px/{pid}/{tail:path}", methods=["GET", "POST"])
async def px_proxy(pid: str, tail: str, request: Request, k: str | None = Query(None)):
    direct = request.headers.get("x-token") or k or ""
    supplied = direct or _cookie_tok(request)
    if not supplied or not hmac.compare_digest(supplied, TOKEN):
        raise HTTPException(401, "token 无效")
    port = _plugin_port(pid)
    if not port:
        raise HTTPException(404, f"没有这个插件：{pid}")
    url = f"http://127.0.0.1:{port}/{tail}"
    qs = request.url.query
    if qs:
        url += "?" + qs
    body = await request.body() if request.method == "POST" else None
    req = urllib.request.Request(url, data=body, method=request.method)
    ctype = request.headers.get("content-type")
    if ctype:
        req.add_header("Content-Type", ctype)
    try:
        with urllib.request.urlopen(req, timeout=40) as r:
            data = r.read()
            ctype = r.headers.get("Content-Type") or "application/octet-stream"
            code = r.status
    except urllib.error.HTTPError as e:                    # 插件自己报的错，原样回
        data, code = e.read(), e.code
        ctype = e.headers.get("Content-Type") or "text/plain; charset=utf-8"
    except Exception as e:                                 # noqa: BLE001
        msg = f"插件没响应（{pid}）：{type(e).__name__}: {e}"
        return PlainTextResponse(msg, status_code=502)
    return _raw(data, ctype, code, cookie=bool(direct))


def _cookie_tok(request) -> str:
    """iframe 里的 __shell.css / __ping 这些子请求带不了 ?k=，靠这颗 cookie 认人。"""
    for part in (request.headers.get("cookie") or "").split(";"):
        name, _, val = part.strip().partition("=")
        if name == "pxk":
            return val
    return ""


def _raw(data: bytes, ctype: str, code: int, cookie: bool = False):
    from fastapi import Response
    hdr = {"Cache-Control": "no-store"}
    if cookie:                    # 只在用 ?k= 进来的那次落 cookie，后面子请求自己带上
        hdr["Set-Cookie"] = f"pxk={TOKEN}; Path=/px; SameSite=Lax; HttpOnly"
    return Response(content=data, media_type=ctype, status_code=code, headers=hdr)


@app.get("/api/sources", dependencies=[Depends(auth)])
def sources_proxy(force: int = 0) -> dict:
    if hwmod is None:
        return {"ok": False, "error": f"本机工作模块没装上（{_HW_IMPORT_ERR}）", "items": []}
    try:
        return hwmod.sources_report(force=bool(force))
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "items": []}


# —— 本机工作的机器指标（原 8775 的 /api/overview）——
#   资源卡、谁在干活、Token 折算都吃这一份；一起给，省得页面来回问三趟。

# —— 没有 hostwork 时的降级账 ————————————————————————————————
# hostwork 是本机 cuntu 的深度采集层，交付包里不带（见上面 CUNTU_DIR 那段：
# 分发版默认没有 cuntu）。可这几个接口以前硬依赖它 —— 别人装完 Moshan，
# 资源卡永远只有一句「本机工作模块没装上」，谁在干活、节奏账同样空着。
# 这里把能算的算出来：磁盘/内存/CPU/开机时长来自 metrics + sysinfo，
# 客户端与 token 给结构完整的空表。前端读的是 `m.disk || {}`、
# `pick(se.load)` 这类写法，空表不会崩，只是那几格显示「—」。
# 把 cuntu 挂回来（MOSHAN_CUNTU）之后 hostwork 能导，自动走原路，这份只当兜底。
_RANGE_LABEL = {"today": "今天", "7d": "7 天", "30d": "30 天", "all": "全部"}
_RANGE_DAYS = {"today": 1, "7d": 7, "30d": 30, "all": 365}


def _ov_fallback(key: str, reason: str) -> dict:
    machine: dict = {
        "disk": {"disk_pct": 0.0, "free_gb": 0.0, "total_gb": 0.0, "used_gb": 0.0},
        "extra_disks": [],
        "mem": {"mem_pct": None, "mem_gb": None, "mem_total_gb": None},
        "load": [], "cpus": os.cpu_count() or 1,
        "uptime_h": None, "cpu_pct": None, "cpu_ready": True,
        "cpu_proc_pct": None, "rss_total_gb": None,
        "net": {"in_bps": 0.0, "out_bps": 0.0, "in_total": 0.0, "out_total": 0.0},
        "gpu": {},
    }
    try:
        machine["disk"] = metrics._disk()
    except Exception:                                  # noqa: BLE001
        pass
    try:
        s = sysinfo.sample()
        mem = s.get("mem") or {}
        cpu = s.get("cpu") or {}
        machine["mem"] = {"mem_pct": mem.get("pct"), "mem_gb": mem.get("used_gb"),
                          "mem_total_gb": mem.get("total_gb")}
        machine["cpus"] = cpu.get("cores") or machine["cpus"]
        machine["cpu_pct"] = cpu.get("pct")
        machine["uptime_h"] = s.get("uptime_h")
        machine["rss_total_gb"] = s.get("rss_gb")
    except Exception:                                  # noqa: BLE001
        pass
    series = {"disk": [], "mem": [], "load": [], "cpus": machine["cpus"]}
    try:                                  # 有历史快照就画折线，没有就留空（前端不画）
        snaps = metrics.load_snapshot()[-60:]
        series["disk"] = [[x.get("at"), x.get("disk_pct")] for x in snaps]
        series["mem"] = [[x.get("at"), x.get("mem_pct")] for x in snaps]
        series["load"] = [[x.get("at"), x.get("load1")] for x in snaps]
    except Exception:                                  # noqa: BLE001
        pass
    return {
        "ok": True, "degraded": reason,
        "now": datetime.now().strftime("%Y-%m-%d %H:%M:%S"),
        "range": {"key": key, "label": _RANGE_LABEL.get(key, key),
                  "days": _RANGE_DAYS.get(key, 7)},
        "machine": machine, "series": series, "app_series": {"at": [], "step": 30,
                                                             "keys": [], "cpu": [], "apps": []},
        "spec": {}, "installed": {"apps": [], "formula": [], "leaves": [],
                                  "cask": [], "brew": ""},
        "units": {"user": [], "global": [], "daemon": [], "brew_services": []},
        "apps": [], "netproc": {"ok": False, "every": 60, "err": "降级模式不采网络明细"},
        "clients": [],
        "tokens": {"by_day": [], "by_model": [],
                   "totals": {"in": 0, "out": 0, "cached": 0, "calls": 0, "total": 0},
                   "hourly": [], "hour_days": [], "hour_day": "", "cache_hit": 0},
        "quota": {"days": 90, "events": [], "credit_n": 0, "quota_n": 0, "free_n": 0,
                  "other_n": 0, "retries": 0, "last": 0, "last_s": "", "last_kind": "",
                  "last_ok": 0, "last_ok_s": "", "blocked_until": 0, "blocked_until_s": "",
                  "switches": [], "switch_n": 0, "kinds": {}},
        "cherry": {"found": False, "calls": 0, "days": _RANGE_DAYS.get(key, 7)},
        "billing": {"ok": False, "accounts": [], "n": 0, "total_balance": 0,
                    "total_month": 0, "at": ""},
        "money": {"currency": "¥", "rows": [], "points": 0, "cost": 0,
                  "any_points": False, "any_cost": False, "priced_ratio": 0,
                  "unpriced": [], "formula": "", "points_formula": "", "config": ""},
        "pocket": {"scripts": [], "recent": [], "tokens": 0},
        "daily": [], "scan": {"done": 0, "total": 0, "running": False,
                              "started": "", "err": "", "bytes": 0, "cached_bytes": 0},
        "procs_path": "",
    }


@app.get("/api/overview", dependencies=[Depends(auth)])
def overview_api(range: str = "7d", fresh: int = 0) -> dict:
    key = range if range in ("today", "7d", "30d", "all") else "7d"
    if hwmod is not None:
        try:
            d = hwmod.overview(key, fresh=bool(fresh))
            if d.get("ok", True):
                return d
            # 采集层自己报了错（EVO 上就是 os.statvfs 在 Windows 不存在）：
            # 把它的错因带上，但照样给一份能看的机器指标，别整块空掉。
            return _ov_fallback(key, str(d.get("error") or "采集层返回失败"))
        except Exception as e:                         # noqa: BLE001
            return _ov_fallback(key, f"{type(e).__name__}: {e}")
    return _ov_fallback(key, f"本机工作模块没装上（{_HW_IMPORT_ERR}）")


@app.get("/api/hourly", dependencies=[Depends(auth)])
def hourly_api(d: str = "", fresh: int = 0) -> dict:
    """某一天的每小时 token（第三视图翻日期用）。

    只算这一份轻量账，不碰进程 / 磁盘 —— 切日期时不该把整页数据重拉一遍。
    """
    if hwmod is None:
        return {"ok": False, "error": f"本机工作模块没装上（{_HW_IMPORT_ERR}）", "hours": []}
    try:
        return hwmod.hourly_bundle(d, fresh=bool(fresh))
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "hours": []}


@app.get("/api/rounds", dependencies=[Depends(auth)])
def rounds_api(range: str = "7d") -> dict:
    key = range if range in ("today", "7d", "30d", "all") else "7d"
    if hwmod is not None:
        try:
            d = hwmod.rounds_items(key)
            if d.get("ok"):
                return d
            reason = str(d.get("error") or "采集层返回失败")
        except Exception as e:                         # noqa: BLE001
            reason = f"{type(e).__name__}: {e}"
    else:
        reason = f"本机工作模块没装上（{_HW_IMPORT_ERR}）"
    # 降级：直接用工作台自己的读取器（TRAE / WorkBuddy / Cherry / Claude 都在
    # _CLIENTS 里）。它的条目结构与 hostwork 那份同源，前端不用改一行。
    try:
        items = dashmod._agent_items(_RANGE_DAYS.get(key, 7))
        # 排序与截断跟工作台那张卡同一套：进行中的顶头，其余按结束/开始倒序。
        # 不截断的话 7 天能出来四百多条，每行还带 steps，payload 直接翻几倍。
        items.sort(key=lambda x: (x.get("status") == "run",
                                  x.get("end") or x.get("start") or ""), reverse=True)
        limit = int(getattr(dashmod, "_WL_HOST_LIMIT", 80) or 80)
        return {"ok": True, "degraded": reason, "items": items[:limit],
                "at": datetime.now().strftime("%H:%M:%S")}
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{reason} / 降级也失败：{e}", "items": []}


@app.get("/api/pulse", dependencies=[Depends(auth)])
def pulse_api(days: int = 75, scope: str = "") -> dict:
    """节奏账：月相 / 时间投入 / 活跃时段 / 知识地层 / 知识指数五张卡共用。

    只回聚合值（每天一行 + 7×24 热力 + 每周分层），不回逐条会话 ——
    180 天的逐条明细有两三万行，前端画图用不上。

    scope 是「看哪一摊」：空 = 全部；`space:<空间名>` / `proj:<项目目录>`
    各收一摊。选项清单每次都在 opts 里回全量，前端切范畴不必再要菜单。
    """
    if hwmod is None:
        return {"ok": False, "error": f"本机工作模块没装上（{_HW_IMPORT_ERR}）",
                "days": [], "hours": [], "weeks": []}
    try:
        return hwmod.pulse(days, scope)
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}",
                "days": [], "hours": [], "weeks": []}


# —— 计费账号：把百炼 / 硅基流动这些的账号录进来，直接读官方余额与月度账单 ——
#   价表乘 token 只是估算，供应商的折扣 / 代金券 / 赠送额度它一概不知道。
#   这里读写都在 cuntu/billing.py（密钥只落本机文件，接口回来的一律打码）。
@app.get("/api/billing", dependencies=[Depends(auth)])
def billing_api() -> dict:
    if hwmod is None:
        return {"ok": False, "error": f"本机工作模块没装上（{_HW_IMPORT_ERR}）", "accounts": []}
    try:
        return hwmod.billing_payload()
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "accounts": []}


@app.get("/api/billing/providers", dependencies=[Depends(auth)])
def billing_providers_api() -> dict:
    try:
        import billing
        return billing.providers_payload()
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "items": []}


@app.post("/api/billing/save", dependencies=[Depends(auth)])
def billing_save_api(body: dict | None = None) -> dict:
    try:
        import billing
        return billing.save_account(body or {})
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}"}


@app.post("/api/billing/del", dependencies=[Depends(auth)])
def billing_del_api(body: dict | None = None) -> dict:
    try:
        import billing
        return billing.del_account(str((body or {}).get("id") or ""))
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}"}


@app.post("/api/billing/pull", dependencies=[Depends(auth)])
def billing_pull_api(body: dict | None = None) -> dict:
    """真去供应商那边要一次账。慢（各家加起来十几秒），所以是显式动作 + 后台定时。"""
    try:
        import billing
        return billing.pull(str((body or {}).get("id") or ""),
                            (body or {}).get("ids") or None)
    except Exception as e:                             # noqa: BLE001
        return {"ok": False, "error": f"{type(e).__name__}: {e}", "items": []}


@app.get("/api/icon")
def icon_api(f: str = "") -> FileResponse:
    """进程图标：本机工作自己按 bundle 生成的小 png。只认它自己起的名字。"""
    if hwmod is None:
        raise HTTPException(404, "本机工作模块没装上")
    if not re.fullmatch(r"[0-9a-f]{6,32}\.png", f or ""):
        raise HTTPException(400, "文件名不对")
    fp = hwmod.ICON_DIR / f
    if not fp.exists():
        raise HTTPException(404, "没有这个图标")
    return FileResponse(fp, media_type="image/png",
                        headers={"Cache-Control": "public, max-age=604800"})


@app.post("/api/rescan", dependencies=[Depends(auth)])
def rescan_api(body: dict | None = None) -> dict:
    if hwmod is None:
        return {"ok": False, "error": "本机工作模块没装上"}
    force = bool((body or {}).get("force"))
    threading.Thread(target=hwmod.scan_sessions, kwargs={"force": force},
                     daemon=True).start()
    return {"ok": True}


# —— 功能文档（2026-11-21）——
#   三份静态说明页：功能全景图 / 实现原理 / 功能与名称释义。
#   文件在 <数据根>/_工作台/docs/*.html，改完刷新即生效，不用重启服务。
#   路由名是 ascii、不走中文文件名，白名单只放这三个 —— 不做通用静态目录，
#   避免把 _工作台 下的备份文件（同名 .bak-*）也暴露出去。
DOCS = {
    "panorama": "panorama.html",
    "impl": "impl.html",
    "gloss": "gloss.html",
}


@app.get("/docs/{name}", response_class=HTMLResponse)
def docs_page(name: str) -> HTMLResponse:
    fn = DOCS.get(name)
    if not fn:
        raise HTTPException(status_code=404, detail="没有这份文档")
    p = dashmod.page_path(CFG).parent / "docs" / fn
    if not p.exists():
        return HTMLResponse(
            "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>"
            f"<h3>这份文档还没放到这台机器上</h3><p style='color:#666'>应当位于：{p}</p></body>",
            status_code=404)
    return HTMLResponse(p.read_text(encoding="utf-8"), headers={
        "Cache-Control": "no-store, max-age=0",
        "X-Content-Type-Options": "nosniff",
    })


@app.get("/hostwork", response_class=HTMLResponse)
def hostwork_page() -> HTMLResponse:
    """本机工作的「详情页」。以前是独立服务（8775），现在跟着口袋一起走：
    数据源还是同一份，页面只是把卡片摊开、加筛选和图表的完整版。"""
    p = (CUNTU_DIR / "hostwork_ui.html") if CUNTU_DIR.is_dir() else None
    if hwmod is None or p is None or not p.exists():
        return HTMLResponse(
            "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>"
            "<h3>本机工作页面不在这台机器上</h3>"
            f"<p style='color:#666'>应当位于：{p}</p></body>", status_code=404)
    html = p.read_text(encoding="utf-8")
    html = html.replace("</head>", _HOSTWORK_SHIM + "</head>") if "</head>" in html \
        else _HOSTWORK_SHIM + html
    return HTMLResponse(html, headers={
        "Cache-Control": "no-store, max-age=0",
        "X-Content-Type-Options": "nosniff",
    })


# 那一页的脚本是用相对路径 /api/xxx 取数的（它以前自己就是一个服务，没有令牌）。
# 挂到口袋下面之后每个请求都要认令牌，所以在这一页里塞一层 fetch 包装，
# 把地址栏里的 k 自动补到同源接口上 —— 页面本身一个字都不用改。
_HOSTWORK_SHIM = """<script>
(function () {
  var K = new URLSearchParams(location.search).get('k') || '';
  if (!K) return;
  var f = window.fetch;
  window.fetch = function (u, o) {
    if (typeof u === 'string' && u.indexOf('/api/') === 0) {
      u += (u.indexOf('?') >= 0 ? '&' : '?') + 'k=' + encodeURIComponent(K);
    }
    return f.call(this, u, o);
  };
})();
</script>"""



# —— 工作台顶栏的插件 ——
#   顶栏那一排统一叫「插件」。三种来源：plugins.json（用户自己加的）/ 内置 /
#   卡片上 data-tool 自报的。这里只管新增与删除，读走 /api/apps。
#   写插件是改 json，不需要重启进程 —— 加完刷新页面就出现。

@app.post("/api/plugins/add", dependencies=[Depends(auth)])
def plugins_add(body: dict) -> dict:
    return dashmod.add_plugin(CFG, body)


@app.post("/api/plugins/remove", dependencies=[Depends(auth)])
def plugins_remove(body: dict) -> dict:
    return dashmod.remove_plugin(CFG, str(body.get("id") or ""))


# —— 第三方插件：别人写的开源程序（LocalSend / IINA / Syncthing…）——
#   配方在 extplugins.py。这里只做五件事：看目录、装、开、退、加进中台。
#   加进中台之后它就跟内置插件一样是一颗药丸，走 /api/apps/launch 那条老路。

@app.get("/api/ext/list", dependencies=[Depends(auth)])
def ext_list() -> dict:
    return {"ok": True, "catalog": extplugins.catalog(CFG)}


@app.post("/api/ext/preview", dependencies=[Depends(auth)])
def ext_preview(body: dict) -> dict:
    """粘一个 .app 路径 / 应用名，认出一份配方草稿（不落盘）。"""
    return extplugins.draft_from_text(str(body.get("text") or ""))


@app.post("/api/ext/add", dependencies=[Depends(auth)])
def ext_add(body: dict) -> dict:
    """加进中台。带 recipe 草稿的（手动粘 .app）先落配方，再加。"""
    rid = str(body.get("id") or "")
    draft = body.get("recipe")
    if isinstance(draft, dict):
        clean = extplugins.sanitize_recipe(draft)
        if not clean:
            return {"ok": False, "error": "这份配方不完整：得有名字和启动方式"}
        extplugins.save_recipe(CFG, clean)
        rid = clean["id"]
    return dashmod.add_ext_plugin(CFG, rid)


@app.post("/api/ext/install", dependencies=[Depends(auth)])
def ext_install(body: dict) -> dict:
    return extplugins.install(CFG, str(body.get("id") or ""))


@app.post("/api/ext/start", dependencies=[Depends(auth)])
def ext_start(body: dict) -> dict:
    return extplugins.start(CFG, str(body.get("id") or ""))


@app.post("/api/ext/stop", dependencies=[Depends(auth)])
def ext_stop(body: dict) -> dict:
    return extplugins.stop(CFG, str(body.get("id") or ""))


# —— 配方接口：目录里没有的程序，用你自己的模型写一条 ——
#   spec / prompt 是「说明书 + 提示词」，parse 把模型回答变成配方，compose 是
#   在 _工作台/第三方插件-模型.json 填了 token 之后的一键版本。

@app.get("/api/ext/spec", dependencies=[Depends(auth)])
def ext_spec(goal: str = "") -> dict:
    return extplugins.spec(CFG, goal)


@app.get("/api/ext/prompt", response_class=PlainTextResponse, dependencies=[Depends(auth)])
def ext_prompt(goal: str = "") -> str:
    return extplugins.prompt_text(CFG, goal)


@app.post("/api/ext/parse", dependencies=[Depends(auth)])
def ext_parse(body: dict) -> dict:
    """把模型返回的整段文字抠成配方草稿（不落盘，前端确认后再 /api/ext/add）。"""
    return extplugins.parse_recipe_text(
        CFG, str(body.get("text") or ""), verify=bool(body.get("verify", True)))


@app.post("/api/ext/compose", dependencies=[Depends(auth)])
def ext_compose(body: dict) -> dict:
    """配了自己的模型就直接生成；没配就返回提示词，让用户自己粘给模型。"""
    return extplugins.compose(CFG, str(body.get("goal") or ""))


@app.get("/api/apps", dependencies=[Depends(auth)])
def apps_list() -> dict:
    return dashmod.apps_payload(CFG)


@app.post("/api/apps/launch", dependencies=[Depends(auth)])
def apps_launch(body: dict) -> dict:
    return dashmod.launch_app(CFG, str(body.get("id") or ""), bool(body.get("builtin")))


@app.post("/api/apps/add", dependencies=[Depends(auth)])
def apps_add(body: dict) -> dict:
    return dashmod.add_launcher(CFG, body)


@app.post("/api/apps/preview", dependencies=[Depends(auth)])
def apps_preview(body: dict) -> dict:
    return dashmod.preview_launcher(body)


@app.post("/api/apps/update", dependencies=[Depends(auth)])
def apps_update(body: dict) -> dict:
    return dashmod.update_launcher(CFG, body)


@app.post("/api/apps/remove", dependencies=[Depends(auth)])
def apps_remove(body: dict) -> dict:
    return dashmod.remove_launcher(CFG, str(body.get("id") or ""))


@app.get("/api/apps/icon", dependencies=[Depends(auth)])
def apps_icon(id: str = "") -> FileResponse:
    p = dashmod.launcher_icon(CFG, id)
    if not p:
        raise HTTPException(404, "没有图标")
    return FileResponse(p, media_type="image/png", headers={"Cache-Control": "max-age=86400"})


# ---------------------------------------------------------------- 使用报告
# 数据链：metrics.py 每 120 秒采一次 → 日聚合（永久）→ report.py 渲染各周期。
# 报告文件平铺在 10-系统记录/使用报告/，不建子文件夹。

@app.get("/api/report/list", dependencies=[Depends(auth)])
def api_report_list() -> dict:
    """报告清单。每次现扫目录重建索引——手动删过文件也能自动纠回来。"""
    idx = report.write_index()
    try:
        data = json.loads(idx.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        data = {"items": [], "count": 0}
    data["state"] = report_runner.state()
    return data


@app.get("/api/report", response_class=PlainTextResponse, dependencies=[Depends(auth)])
def api_report(kind: str = "day", key: str = "") -> PlainTextResponse:
    """取一份报告原文。不传 key 就是最近一期。"""
    p = report.file_of(kind, key) if key else report.latest(kind)
    if not p or not p.exists():
        raise HTTPException(404, "还没有生成这个周期的报告")
    return PlainTextResponse(p.read_text(encoding="utf-8"),
                             media_type="text/markdown; charset=utf-8")


@app.post("/api/report/run", dependencies=[Depends(auth)])
def api_report_run(force: bool = Query(False)) -> dict:
    """立刻补一轮：重算日聚合、生成缺的周期、做归档。"""
    res = report.run(force=force)
    log({"event": "report_manual",
         "made": [f"{m['kind']}:{m['key']}" for m in res.get("made") or []],
         "removed": res.get("removed") or []})
    return {"ok": True, **res}


@app.post("/api/report/open", dependencies=[Depends(auth)])
def api_report_open(payload: dict) -> dict:
    """在 Mac 上用默认程序打开——报告是写给人读的，不该只在浏览器里当文本看。"""
    kind = str((payload or {}).get("kind") or "day")
    key = str((payload or {}).get("key") or "")
    p = report.file_of(kind, key) if key else report.latest(kind)
    if not p or not p.exists():
        raise HTTPException(404, "没有这份报告")
    subprocess.Popen(["open", str(p)])
    return {"ok": True, "file": str(p)}


# —— 巡检（2026-11-21：工作台 ?v=ops 是它唯一的脸）——
#   8774 的页面入口已经重定向回工作台，但真正的执行、报告归档还在 syscheck.py。
#   这里只做两件事：把报告原文按**文件名**读出来（绝不接受路径）、把「跑一轮」
#   转发给 8774。走 pocket 中转的理由和插件卡一样 —— 手机访问时页面里的
#   127.0.0.1 是手机自己，只有服务端 know 本机端口。
INSPECT_BASE = "http://127.0.0.1:8774"


@app.get("/api/inspect/report", dependencies=[Depends(auth)])
def api_inspect_report(name: str = "") -> dict:
    """读一份巡检报告。name 只允许「报告/」下的 xxx.md —— 挡路径穿越。"""
    nm = (name or "").strip()
    if not nm.endswith(".md") or "/" in nm or "\\" in nm or nm.startswith("."):
        raise HTTPException(400, "文件名不对")
    root = (dashmod._root(CFG) / "10-系统记录" / "巡检" / "报告").resolve()
    p = (root / nm).resolve()
    try:
        p.relative_to(root)
    except ValueError:
        raise HTTPException(400, "文件名不对")
    if not p.is_file():
        raise HTTPException(404, "没有这份报告")
    return {"ok": True, "name": nm,
            "text": p.read_text(encoding="utf-8", errors="replace")}


@app.post("/api/inspect/run", dependencies=[Depends(auth)])
def api_inspect_run(payload: dict) -> dict:
    """触发一轮巡检（heal=true 时顺带把能修的服务拉起来）。

    **这一轮是后台跑的**（8774 的 repair_round 起了个 daemon 线程，结果写 REPAIR），
    所以返回的只有 started；进度要靠 /api/inspect/state 轮询 —— 前端按钮上那个
    「跑着 12s」就是轮它。
    """
    heal = bool((payload or {}).get("heal", False))
    body = json.dumps({"heal": heal}).encode("utf-8")
    req = urllib.request.Request(INSPECT_BASE + "/api/repair", data=body,
                                 headers={"Content-Type": "application/json"},
                                 method="POST")
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.loads(r.read().decode("utf-8") or "{}")
    except Exception as e:  # 端口没起 / 超时 / 返回不是 JSON
        raise HTTPException(502, "巡检服务没响应：%s" % e)


@app.get("/api/inspect/state", dependencies=[Depends(auth)])
def api_inspect_state() -> dict:
    """这一轮的实时状态：还在跑？跑完了修了几项？

    只从 8774 的 /api/state 里拎 repair 段 —— 别把服务矩阵、机器状态整包转发，
    那个页面已经在 /api/dash 里有一份了，两处口径不同迟早对不上。
    """
    try:
        with urllib.request.urlopen(INSPECT_BASE + "/api/state", timeout=10) as r:
            st = json.loads(r.read().decode("utf-8") or "{}")
    except Exception as e:
        raise HTTPException(502, "巡检服务没响应：%s" % e)
    return {"ok": True, "repair": st.get("repair") or {}}


def _walk_inbox(limit: int = 800) -> list[Path]:
    if not INBOX_DIR.exists():
        return []
    out: list[Path] = []
    for p in INBOX_DIR.rglob("*"):
        if len(out) >= limit:
            break
        if p.is_file() and not p.name.startswith(".") and p.name != "index.jsonl":
            out.append(p)
    out.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    return out


@app.post("/api/upload", dependencies=[Depends(auth)])
async def upload(files: list[UploadFile] = File(...), dir: str = "") -> dict:
    """不指定 dir 时按日期落进收件箱（手机随手发的默认行为）；
    指定了就直达那个目录，跟 NAS 里"传到当前文件夹"一致。"""
    t0 = time.time()
    if dir.strip():
        day = _safe_path(dir)
        if not day.is_dir():
            raise HTTPException(400, "目标不是目录")
    else:
        day = INBOX_DIR / time.strftime("%Y-%m-%d")
    day.mkdir(parents=True, exist_ok=True)
    saved, failed = [], []
    for f in files:
        name = f.filename or f"上传_{int(time.time())}"
        dest = _unique_path(day, name)
        written = 0
        try:
            with dest.open("wb") as out:
                while True:
                    chunk = await f.read(1 << 20)
                    if not chunk:
                        break
                    written += len(chunk)
                    if written > MAX_UPLOAD:
                        raise ValueError(f"超过 {CFG.get('max_upload_mb', 500)}MB 上限")
                    out.write(chunk)
            saved.append({"name": dest.name, "path": str(dest), "bytes": written,
                          "category": _category(dest.name),
                          "size": _human(written)})
        except (OSError, ValueError) as e:
            dest.unlink(missing_ok=True)
            failed.append({"name": name, "error": str(e)[:200]})
    rec_ts = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    # 归档清单写在「日期目录」而不是「组目录」里：组目录应该只有图，
    # 列表页才不会第一行顶着个 index.jsonl。
    rec_dir = day.parent if day.parent.parent == INBOX_DIR else day
    if saved:
        try:
            with (rec_dir / "index.jsonl").open("a", encoding="utf-8") as f:
                for s in saved:
                    f.write(json.dumps({"ts": rec_ts, "name": s["name"],
                                        "path": s["path"], "bytes": s["bytes"],
                                        "category": s["category"],
                                        "source": "pocket_upload"},
                                       ensure_ascii=False) + "\n")
        except OSError:
            pass
    log({"event": "upload", "saved": len(saved), "failed": len(failed),
         "ms": int((time.time() - t0) * 1000),
         "names": [s["name"] for s in saved]})
    return {"saved": saved, "failed": failed, "dir": str(day)}


@app.post("/api/inbox/group", dependencies=[Depends(auth)])
def inbox_group(payload: dict) -> dict:
    """开一组：在今天的日期目录下建 组-HHMMSS/，这次传的都进它。

    手机上没法预先给一批图命名，所以先按时间开个组，传完再在「文件」里改名。
    名字留空就用 组-HHMMSS；重名自动加 -2，不会覆盖已有的组。
    """
    day = INBOX_DIR / time.strftime("%Y-%m-%d")
    try:
        day.mkdir(parents=True, exist_ok=True)
    except OSError as e:
        raise HTTPException(500, f"建不了日期目录：{e}"[:120])
    want = str((payload or {}).get("name") or "").strip()
    base = want or ("组-" + time.strftime("%H%M%S"))
    target = None
    for _ in range(4):
        cand = _unique_path(day, base)
        try:
            cand.mkdir()
            target = cand
            break
        except FileExistsError:
            continue          # 刚好撞上别人也在这个名字上，再取一个
    if target is None:
        raise HTTPException(500, "建不了组目录：一直撞名")
    log({"event": "inbox_group", "path": str(target)})
    return {"name": target.name, "path": str(target), "day": day.name}


_NAME_LOCK = threading.Lock()


@app.post("/api/inbox/name", dependencies=[Depends(auth)])
def api_inbox_name(payload: dict) -> dict:
    """给一组图起名：读图上的字 → 4B 概括 → 目录改成那个名字。

    只碰名字还是 组-HHMMSS 的目录（或显式 force），人工起过名的不覆盖。
    图里没字就不改，原样留着 —— 照片、视频截图没有可读文字，
    硬凑一个名字比留着时间戳更难找。整件事几秒到十几秒，同步等即可。
    """
    p = _safe_path(str((payload or {}).get("path") or ""))
    if not p.is_dir():
        raise HTTPException(400, "不是目录")
    try:
        inside = INBOX_DIR.resolve() in p.resolve().parents
    except OSError:
        inside = False
    if not inside:
        raise HTTPException(403, "只能给收件箱里的目录起名")
    if not _NAME_LOCK.acquire(blocking=False):
        raise HTTPException(429, "上一组还在读图起名，等它跑完")
    try:
        res = naming.name_folder(p, model=CFG.get("model"),
                                 force=bool((payload or {}).get("force")))
    except Exception as e:                      # OCR/模型出问题不该是 500 页面
        raise HTTPException(500, f"起名失败：{str(e)[:120]}")
    finally:
        _NAME_LOCK.release()
    log({"event": "inbox_name", "from": p.name, "to": res.get("name"),
         "ok": res.get("ok"), "renamed": res.get("renamed"),
         "images": res.get("images"), "chars": res.get("chars"),
         "ms": res.get("ms")})
    return res


@app.post("/api/link", dependencies=[Depends(auth)])
def api_link(payload: dict) -> dict:
    """手机粘链接的入口：判别平台 → 抓取 → 存成文件夹，全程异步。

    立刻返回任务号，前端轮询 /api/link/status；一篇 20 张图的公众号
    要十几秒，同步等会把手机浏览器的连接拖死。
    """
    text = str((payload or {}).get("text") or "").strip()
    if not text:
        raise HTTPException(400, "缺少链接")
    # hint 是用户原话：content 判定只认它做覆盖，其余看抓到的正文和图
    hint = str((payload or {}).get("hint") or "")[:300]
    # 一次贴了好几条链接 → 一条一个任务，各自抓各自的。
    # （以前整段丢给 cuntu，它只认第一条，剩下几条静默消失。）
    urls = split_links(text)
    if len(urls) > 1:
        jobs = []
        for u in urls:
            j = LINKS.start(u, hint=hint or text)
            if not j.get("error") and not j.get("dup"):
                log({"event": "link_start", "id": j["id"], "head": u[:80], "url": u})
            jobs.append(j)
        return {"multi": len(jobs), "jobs": jobs}
    job = LINKS.start(text, hint=hint)
    if job.get("error"):
        raise HTTPException(400, job["error"])
    # 同一个链接的重复请求：复用上一次的任务/结果，不新记一条开始。
    # 否则工作台上会出现「存图 · 小红书 ×9」，看着像九篇笔记全挂了。
    if job.get("dup"):
        return job
    log({"event": "link_start", "id": job["id"], "head": text[:80],
         "url": job.get("url", "")})
    return job


@app.get("/api/link/status", dependencies=[Depends(auth)])
def api_link_status(id: str) -> dict:
    job = LINKS.get(id)
    if not job:
        raise HTTPException(404, "任务不存在或已过期")
    return job


@app.get("/api/pending", dependencies=[Depends(auth)])
def api_pending() -> dict:
    """还没定「存图还是文章」的那几条，新的在前。

    用户原话（2026-11-20）：「有疑惑的，直接发对话问我好了，我直接回复文字」。
    这里就是「问了还没答」的那张单子，聊天里看一眼就知道在等哪条。
    """
    items = pending.open_items()
    return {"count": len(items), "items": items}


@app.post("/api/pending/answer", dependencies=[Depends(auth)])
def api_pending_answer(payload: dict) -> dict:
    """用户回了一句「1 / 存图 / 文章 / 都存图」：认出来就办，认不出就老实说。

    matched=False 时调用方（微信插件）应当把这句当普通消息继续往下走，
    别吞掉 —— 它可能根本不是在回答我们。
    """
    text = str((payload or {}).get("text") or "").strip()
    if not text:
        raise HTTPException(400, "缺少内容")
    out = pending.answer(text, by=str((payload or {}).get("by") or "回复"))
    log({"event": "pending_answer", "text": text[:60],
         "matched": bool(out.get("matched")), "mode": out.get("mode")})
    return out


@app.post("/api/pending/resolve", dependencies=[Depends(auth)])
def api_pending_resolve(payload: dict) -> dict:
    """点名处理某一条（手机页上那颗按钮走这里，比回文字更不容易错）。"""
    p = payload or {}
    pid = str(p.get("id") or "")
    mode = str(p.get("mode") or "")
    if not pid:
        raise HTTPException(400, "缺少 id")
    out = pending.resolve(pid, mode, by=str(p.get("by") or "选"))
    if not out.get("ok"):
        raise HTTPException(404, out.get("error") or "没处理成")
    return out


@app.post("/api/intake", dependencies=[Depends(auth)])
def api_intake(payload: dict) -> dict:
    """微信入站唯一入口：抽全部链接 → 一条一个任务 → 立刻回一句回执。

    刻意不在这里等抓取：以前 MCP 工具同步阻塞，一条消息两条链接就超时，
    超时以后 4B 还会照着上一轮的结论编一句话。现在「收下」和「存完」
    是两个时刻，存完由 wechat_push 直接发回微信。
    """
    p = payload or {}
    text = str(p.get("text") or "").strip()
    if not text:
        raise HTTPException(400, "缺少内容")
    out = INTAKE.accept(
        text,
        hint=str(p.get("hint") or text)[:300],
        agent=str(p.get("agent") or "main"),
        channel=str(p.get("channel") or "openclaw-weixin"),
        conv=str(p.get("conv") or ""),
    )
    if not out.get("ok"):
        raise HTTPException(400, out.get("reason") or "没有链接")
    log({"event": "intake", "n": out.get("count", 0),
         "batch": out.get("batch", "")})
    return out


@app.post("/api/article/fetch", dependencies=[Depends(auth)])
def api_article_fetch(payload: dict) -> dict:
    """存文章：粘链接 → 抓正文落到 08-阅读/未分类/，异步。"""
    text = str((payload or {}).get("text") or "").strip()
    if not text:
        raise HTTPException(400, "缺少链接")
    hint = str((payload or {}).get("hint") or "")[:300]
    urls = split_links(text)
    if len(urls) > 1:
        jobs = []
        for u in urls:
            j = ARTICLES.start(u, hint=hint or text)
            if not j.get("error") and not j.get("dup"):
                log({"event": "article_start", "id": j["id"], "head": u[:80], "url": u})
            jobs.append(j)
        return {"multi": len(jobs), "jobs": jobs}
    job = ARTICLES.start(text, hint=hint)
    if job.get("error"):
        raise HTTPException(400, job["error"])
    if job.get("dup"):
        return job
    log({"event": "article_start", "id": job["id"], "head": text[:80],
         "url": job.get("url", "")})
    return job


@app.post("/api/article/classify", dependencies=[Depends(auth)])
def api_article_classify(payload: dict) -> dict:
    """4B 分主题：传 rel 只分一篇，不传就把未分类的全部过一遍。"""
    rel = str((payload or {}).get("rel") or "")
    # 防越界：只接受 08-阅读 下的相对路径
    if rel and (".." in rel or rel.startswith("/")):
        raise HTTPException(400, "路径不合法")
    job = ARTICLES.start_classify(rel)
    if job.get("error"):
        raise HTTPException(400, job["error"])
    log({"event": "article_classify_start", "id": job["id"], "rel": rel})
    return job


@app.get("/api/article/status", dependencies=[Depends(auth)])
def api_article_status(id: str) -> dict:
    job = ARTICLES.get(id)
    if not job:
        raise HTTPException(404, "任务不存在或已过期")
    return job


@app.get("/api/article/topics", dependencies=[Depends(auth)])
def api_article_topics() -> dict:
    """已知主题 + 每个主题篇数，给手机端筛选用。"""
    import article as art
    root = art.READ_ROOT
    topics = art.load_topics()
    counts = {}
    for t in [art.UNCATEGORIZED] + topics:
        d = root / t
        counts[t] = len([p for p in d.iterdir() if p.is_dir()]) if d.exists() else 0
    return {"topics": topics, "counts": counts}


@app.get("/api/article/list", dependencies=[Depends(auth)])
def api_article_list() -> dict:
    """按主题分组列出所有文章，供手机端「阅读」页渲染。"""
    import article as art
    root = art.READ_ROOT
    groups: dict[str, list[dict]] = {}
    if not root.exists():
        return {"groups": {}}
    for topic_dir in sorted(root.iterdir()):
        if not topic_dir.is_dir() or topic_dir.name.startswith("_"):
            continue
        topic = topic_dir.name
        arts = []
        for d in sorted(topic_dir.iterdir(), key=lambda p: -p.stat().st_mtime if p.exists() else 0):
            if not d.is_dir():
                continue
            meta = art.article_meta(d)
            body = d / "正文.md"
            arts.append({
                "rel": f"{topic}/{d.name}", "title": meta.get("title", d.name),
                "author": meta.get("author", ""), "platform": meta.get("platform", ""),
                "chars": meta.get("chars", 0), "url": meta.get("url", ""),
                "saved_at": meta.get("saved_at", ""), "has_body": body.exists(),
            })
        if arts:
            groups[topic] = arts
    return {"groups": groups}


@app.get("/api/files", dependencies=[Depends(auth)])
def list_files(scope: str = "inbox", q: str = "", limit: int = 300) -> dict:
    root = INBOX_DIR if scope != "archive" else ALLOWED_ROOTS[0] / "04-归档"
    if not root.exists():
        return {"items": [], "root": str(root)}
    items = []
    for p in root.rglob("*"):
        if len(items) >= limit:
            break
        if not p.is_file() or p.name.startswith(".") or p.name == "index.jsonl":
            continue
        if q and q.lower() not in p.name.lower() and q not in str(p):
            continue
        try:
            st = p.stat()
        except OSError:
            continue
        items.append({"name": p.name, "path": str(p),
                      "rel": str(p.relative_to(root)),
                      "bytes": st.st_size, "size": _human(st.st_size),
                      "mtime": int(st.st_mtime),
                      "category": _category(p.name)})
    items.sort(key=lambda x: x["mtime"], reverse=True)
    return {"items": items, "root": str(root), "count": len(items)}


@app.get("/api/file", dependencies=[Depends(auth)])
def get_file(path: str) -> FileResponse:
    p = _safe_path(path)
    if not p.is_file():
        raise HTTPException(404, "文件不存在")
    return FileResponse(p, filename=p.name)


# ---------------------------------------------------------------- 浏览

def _root_of(p: Path) -> Path:
    """当前路径属于哪个 allowed_root，界面用它当"回到根目录"。"""
    rp = p.resolve()
    for r in ALLOWED_ROOTS:
        rr = r.resolve()
        if rp == rr or rr in rp.parents:
            return rr
    return ALLOWED_ROOTS[0].resolve()


@app.get("/api/browse", dependencies=[Depends(auth)])
def browse(path: str = "", q: str = "", sort: str = "name", inline: int = 1) -> dict:
    p = _safe_path(path) if path.strip() else INBOX_DIR
    if not p.exists():
        raise HTTPException(404, "目录不存在")
    # 指向文件时自动跳到它所在目录，方便从预览里"在文件夹中显示"
    if p.is_file():
        return RedirectResponse(f"/api/browse?path={p.parent}", status_code=307)
    try:
        out = storage.list_dir(p, q=q, sort=sort)
    except storage.PathError as e:
        raise HTTPException(e.code, str(e))
    # 走隧道时每张缩略图都要一次往返（中继下 0.7 秒/次），图多的目录干脆
    # 直接把缩略图内联进这次响应，20 多次往返压成 1 次。
    if inline and not q.strip():
        imgs = [f for f in (out.get("files") or []) if f.get("kind") == "image"]
        if 0 < len(imgs) <= INLINE_THUMB_MAX:
            for f in imgs:
                try:
                    t = _make_thumb(Path(f["path"]), 200)
                    if t is not None:
                        f["thumb"] = ("data:image/jpeg;base64,"
                                      + base64.b64encode(t.read_bytes()).decode("ascii"))
                except OSError:
                    pass
    return out


@app.get("/api/usage", dependencies=[Depends(auth)])
def api_usage() -> dict:
    return storage.usage(INBOX_DIR)


@app.post("/api/mkdir", dependencies=[Depends(auth)])
def api_mkdir(payload: dict) -> dict:
    parent = _safe_path(payload.get("path") or str(INBOX_DIR))
    try:
        return storage.mkdir(parent, payload.get("name", ""))
    except storage.PathError as e:
        raise HTTPException(e.code, str(e))


@app.post("/api/rename", dependencies=[Depends(auth)])
def api_rename(payload: dict) -> dict:
    p = _safe_path(payload.get("path", ""))
    try:
        item = storage.rename(p, payload.get("name", ""))
    except storage.PathError as e:
        raise HTTPException(e.code, str(e))
    except OSError as e:
        raise HTTPException(500, str(e)[:120])
    log({"event": "rename", "from": str(p), "to": item["path"]})
    return item


@app.post("/api/move", dependencies=[Depends(auth)])
def api_move(payload: dict) -> dict:
    paths = [_safe_path(x) for x in (payload.get("paths") or [])]
    if not paths:
        raise HTTPException(400, "没有选中文件")
    dest = _safe_path(payload.get("dest", ""))
    try:
        results = storage.move(paths, dest, copy=bool(payload.get("copy")))
    except storage.PathError as e:
        raise HTTPException(e.code, str(e))
    ok = sum(1 for r in results if r.get("ok"))
    log({"event": "copy" if payload.get("copy") else "move",
         "n": len(paths), "ok": ok, "dest": str(dest)})
    return {"results": results, "ok": ok, "failed": len(results) - ok, "dest": str(dest)}


@app.post("/api/delete", dependencies=[Depends(auth)])
def api_delete(payload: dict) -> dict:
    """删除 = 移进回收站，30 天后自动清理，和 NAS 默认行为一致。"""
    paths = [_safe_path(x) for x in (payload.get("paths") or [])]
    if not paths:
        raise HTTPException(400, "没有选中文件")
    try:
        results = storage.delete_to_trash(paths)
    except storage.PathError as e:
        raise HTTPException(e.code, str(e))
    ok = sum(1 for r in results if r.get("ok"))
    log({"event": "trash", "n": len(paths), "ok": ok})
    return {"results": results, "ok": ok, "failed": len(results) - ok,
            "reply": f"已移入回收站 {ok} 项" + (f"，{len(results) - ok} 项失败" if len(results) > ok else "")}


@app.get("/api/trash", dependencies=[Depends(auth)])
def api_trash() -> dict:
    rows = storage.list_trash()
    total = sum(int(r.get("bytes") or 0) for r in rows)
    return {"items": rows, "count": len(rows), "bytes": total,
            "size": storage.human(total), "days": CFG.get("trash_days", 30)}


@app.post("/api/trash/restore", dependencies=[Depends(auth)])
def api_restore(payload: dict) -> dict:
    out = storage.restore(payload.get("names") or [])
    ok = sum(1 for r in out if r.get("ok"))
    log({"event": "restore", "n": len(out), "ok": ok})
    return {"results": out, "ok": ok, "reply": f"已恢复 {ok} 项"}


@app.post("/api/trash/purge", dependencies=[Depends(auth)])
def api_purge(payload: dict | None = None) -> dict:
    names = (payload or {}).get("names")
    n = storage.purge(names)
    log({"event": "purge", "n": n, "all": names is None})
    return {"ok": True, "purged": n, "reply": f"已永久删除 {n} 项"}


# ---------------------------------------------------------------- 预览与打包

@app.get("/api/raw", dependencies=[Depends(auth)])
def api_raw(path: str) -> FileResponse:
    """内联返回，供 <img>/<video>/<iframe> 直接用，不触发下载。"""
    p = _safe_path(path)
    if not p.is_file():
        raise HTTPException(404, "文件不存在")
    return FileResponse(p, media_type=storage.mime_of(p),
                        headers={"Cache-Control": "private, max-age=60"})


THUMB_DIR = Path.home() / ".cache" / "pocket-thumbs"
INLINE_THUMB_MAX = 40                   # 图比这还多就退回逐张请求，别把响应撑大
_THUMB_SLOTS = threading.Semaphore(4)   # sips 是子进程，限制并发别把机器压死


def _thumb_target(p: Path, w: int) -> Path:
    st = p.stat()
    key = f"{p}|{st.st_mtime_ns}|{st.st_size}|{w}"
    return THUMB_DIR / (hashlib.sha1(key.encode("utf-8")).hexdigest() + ".jpg")


def _make_thumb(p: Path, w: int) -> Path | None:
    """用 macOS 自带的 sips 缩图，落盘缓存。不引入 Pillow，不联网。

    走隧道时原图一张 300 KB–1 MB，列表里二十几张就是好几 MB；
    缩到 360px、JPEG 60 后通常 20–40 KB，这是慢的主因。

    sips 是 macOS 专属：Windows 上直接返回 None，前端退回显示原图。
    这里提前判断，省得每次请求都白起一个注定失败的子进程。
    """
    if sys.platform != "darwin":
        return None
    out = _thumb_target(p, w)
    if out.exists() and out.stat().st_size > 0:
        return out
    THUMB_DIR.mkdir(parents=True, exist_ok=True)
    tmp = out.with_name(out.stem + f".{os.getpid()}.{threading.get_ident()}.tmp")
    variants = [
        ["sips", "-Z", str(w), "-s", "format", "jpeg",
         "-s", "formatOptions", "60", str(p), "--out", str(tmp)],
        ["sips", "-Z", str(w), str(p), "--out", str(tmp)],   # 有些格式不吃 format 参数
    ]
    with _THUMB_SLOTS:
        for argv in variants:
            try:
                r = subprocess.run(argv, capture_output=True, timeout=25)
            except (OSError, subprocess.SubprocessError):
                continue
            if r.returncode == 0 and tmp.exists() and tmp.stat().st_size > 0:
                try:
                    os.replace(tmp, out)
                    return out
                except OSError:
                    pass
            try:
                tmp.unlink()
            except OSError:
                pass
    return None


@app.get("/api/thumb", dependencies=[Depends(auth)])
def api_thumb(path: str, w: int = 360) -> FileResponse:
    """列表缩略图。缩不了就退回原图，功能不能断。"""
    w = max(80, min(int(w), 1200))
    p = _safe_path(path)
    if not p.is_file():
        raise HTTPException(404, "文件不存在")
    try:
        t = _make_thumb(p, w)
    except OSError:
        t = None
    if t is None:
        return FileResponse(p, media_type=storage.mime_of(p),
                            headers={"Cache-Control": "private, max-age=60"})
    return FileResponse(t, media_type="image/jpeg",
                        headers={"Cache-Control": "private, max-age=2592000"})


@app.get("/api/preview", dependencies=[Depends(auth)])
def api_preview(path: str) -> dict:
    p = _safe_path(path)
    if not p.exists():
        raise HTTPException(404, "文件不存在")
    if p.is_dir():
        return {"kind": "folder", "name": p.name, "path": str(p)}
    kind = storage.preview_kind(p)
    st = p.stat()
    out = {"kind": kind, "name": p.name, "path": str(p),
           "size": storage.human(st.st_size), "bytes": st.st_size,
           "mtime": int(st.st_mtime), "category": storage.category(p.name)}
    if kind == "text":
        try:
            out["text"] = p.read_text(encoding="utf-8", errors="replace")[:20000]
        except OSError as e:
            out["kind"] = "none"
            out["error"] = str(e)[:120]
    return out


@app.get("/api/zip", dependencies=[Depends(auth)])
def api_zip(path: str = "", paths: str = "") -> FileResponse:
    """多选或单目录打包下载。临时 zip 发完就删。"""
    targets = [_safe_path(x) for x in paths.split("|") if x.strip()] if paths else []
    if not targets and path.strip():
        targets = [_safe_path(path)]
    if not targets:
        raise HTTPException(400, "没有指定要打包的内容")
    tmp = Path(tempfile.mkdtemp(prefix="pocket-zip-"))
    name = targets[0].name if len(targets) == 1 else "口袋打包"
    try:
        z = storage.make_zip(targets, tmp, name)
    except OSError as e:
        raise HTTPException(500, f"打包失败：{e}"[:160])
    log({"event": "zip", "n": len(targets), "bytes": z.stat().st_size})

    def _cleanup() -> None:
        shutil.rmtree(tmp, ignore_errors=True)

    return FileResponse(z, media_type="application/zip",
                        filename=z.name, background=BackgroundTask(_cleanup))


# ---------------------------------------------------------------- 分享

@app.post("/api/share", dependencies=[Depends(auth)])
def api_share(payload: dict) -> dict:
    p = _safe_path(payload.get("path", ""))
    try:
        rec = storage.share_create(p, float(payload.get("hours") or 24))
    except storage.PathError as e:
        raise HTTPException(e.code, str(e))
    log({"event": "share", "path": str(p), "sid": rec["sid"]})
    rec["url"] = f"/s/{rec['sid']}/{rec['name']}"
    return rec


@app.get("/api/shares", dependencies=[Depends(auth)])
def api_shares() -> dict:
    rows = storage.share_list()
    for r in rows:
        r["url"] = f"/s/{r['sid']}/{r['name']}"
    return {"items": rows, "count": len(rows)}


@app.delete("/api/shares/{sid}", dependencies=[Depends(auth)])
def api_share_revoke(sid: str) -> dict:
    return {"ok": storage.share_revoke(sid)}


@app.get("/s/{sid}/{name:path}")
def public_share(sid: str, name: str = "", dl: int = 0):
    """公开下载：只认分享 ID，不带主令牌，过期自动失效。"""
    rec = storage.share_get(sid)
    if not rec:
        return HTMLResponse("<h2>链接已失效或已过期</h2>", status_code=410)
    p = Path(rec["path"])
    if not p.exists():
        return HTMLResponse("<h2>文件已被移走</h2>", status_code=404)
    log({"event": "share_hit", "sid": sid, "hits": rec.get("hits")})
    if p.is_dir():
        tmp = Path(tempfile.mkdtemp(prefix="pocket-share-"))
        try:
            z = storage.make_zip([p], tmp, p.name)
        except OSError:
            return HTMLResponse("<h2>打包失败</h2>", status_code=500)
        return FileResponse(z, media_type="application/zip", filename=z.name,
                            background=BackgroundTask(
                                lambda: shutil.rmtree(tmp, ignore_errors=True)))
    return FileResponse(p, media_type=storage.mime_of(p),
                        filename=p.name if dl else None)


def _inbox_hint(limit: int = 15) -> str:
    """把收件箱当前状态压成一段事实，省掉模型摸索目录的轮次。

    4B 不擅长「先猜路径再纠正」，与其让它调三次 list_directory，
    不如直接把清单给它 —— 实测能省下 3 轮、20 多秒。
    """
    files = _walk_inbox(limit=400)
    head = f"【当前状态】收件箱：{INBOX_DIR}"
    if not files:
        return head + "\n目前是空的。"
    lines = [head + f"\n共 {len(files)} 个文件，最近的在前："]
    for p in files[:limit]:
        try:
            st = p.stat()
            rel = p.relative_to(INBOX_DIR)
        except (OSError, ValueError):
            continue
        lines.append(f"- {rel}｜{_human(st.st_size)}｜{_category(p.name)}｜"
                     f"{time.strftime('%m-%d %H:%M', time.localtime(st.st_mtime))}")
    if len(files) > limit:
        lines.append(f"（另有 {len(files) - limit} 个未列出，需要时可调 list_directory）")
    return "\n".join(lines)


_LIST_WORDS = ("有什么", "有哪些", "列出", "列一下", "多少个", "几个文件",
               "文件列表", "看看文件", "都有什么")


def _quick_answer(text: str) -> str | None:
    """「收件箱里有什么」这类问题由代码直接答，秒回且不会漏项。

    4B 答这类问题要 20 秒、还可能把系统索引文件当条目念出来。
    问题形式足够固定，交给代码比交给模型可靠。
    """
    t = text.strip().rstrip("？?。.")
    if t in ("文件", "列文件", "文件列表", "收件箱"):
        return _inbox_hint(limit=30).replace("【当前状态】", "")
    if "收件箱" in t and any(w in t for w in _LIST_WORDS):
        return _inbox_hint(limit=30).replace("【当前状态】", "")
    return None


@app.post("/api/chat", dependencies=[Depends(auth)])
def chat(payload: dict) -> JSONResponse:
    text = (payload.get("text") or "").strip()
    if not text:
        raise HTTPException(400, "text 不能为空")
    history = payload.get("history") or []
    t0 = time.time()

    quick = _quick_answer(text)
    if quick:
        log({"event": "chat_quick", "text": text[:80],
             "ms": int((time.time() - t0) * 1000)})
        return JSONResponse({"reply": quick, "trace": [], "quick": True,
                             "ms": int((time.time() - t0) * 1000),
                             "tokens": 0})

    try:
        system = SYSTEM_PROMPT + "\n\n" + _inbox_hint()
        result = GW.run(text, history, system=system)
    except GatewayError as e:
        log({"event": "chat_gateway_error", "text": text[:120], "error": str(e)[:300]})
        return JSONResponse({"reply": f"网关出问题了：{e}", "trace": [],
                             "ms": int((time.time() - t0) * 1000), "error": True},
                            status_code=502)
    log({"event": "chat", "text": text[:200], "reply": result["reply"][:200],
         "ms": result["ms"], "tokens": result["tokens"],
         "tools": [t.get("tool") for t in result["trace"] if t.get("tool")]})
    return JSONResponse(result)


@app.post("/api/sort", dependencies=[Depends(auth)])
def sort_inbox() -> dict:
    files = _walk_inbox(limit=5000)
    if not files:
        return {"reply": "收件箱是空的。", "moved": {}}
    moved: dict[str, int] = {}
    failed = 0
    for p in files:
        cat = _category(p.name)
        try:
            dest_dir = INBOX_DIR / SORTED_DIRNAME / cat
            dest_dir.mkdir(parents=True, exist_ok=True)
            p.rename(_unique_path(dest_dir, p.name))
            moved[cat] = moved.get(cat, 0) + 1
        except OSError:
            failed += 1
    parts = "、".join(f"{c} {n}" for c, n in sorted(moved.items(), key=lambda x: -x[1]))
    reply = f"已整理 {sum(moved.values())} 个文件" + (f"：{parts}" if parts else "")
    if failed:
        reply += f"；{failed} 个没动成"
    log({"event": "sort", "moved": moved, "failed": failed})
    return {"reply": reply, "moved": moved, "failed": failed}


@app.get("/api/inbox/open", dependencies=[Depends(auth)])
def open_inbox() -> dict:
    """在 Mac 上直接打开收件箱（Finder），方便大文件本地处理。"""
    os.system(f'open "{INBOX_DIR}"')
    return {"ok": True, "path": str(INBOX_DIR)}


def main() -> int:
    """服务主入口。

    改成函数是为了让 launcher 能在同一进程里调用（PyInstaller 打包后没有独立
    python 解释器，subprocess 再起一个 server.py 是不可行的）。
    """
    import asyncio
    import re
    import socket

    import uvicorn

    # 访问日志自己记一份：token 在 query 里，TLS 保护不了落盘后的副本，
    # 所以写日志前先抹成 k=***，避免长期躺在磁盘上。
    @app.middleware("http")
    async def _access_log(request: Request, call_next):
        response = await call_next(request)
        try:
            qs = request.url.query
            if qs:
                qs = re.sub(r"(^|&)k=[^&]*", r"\1k=***", qs)
            target = request.url.path + (f"?{qs}" if qs else "")
            peer = request.client.host if request.client else "-"
            print(f'INFO:     {peer} - "{request.method} {target}" {response.status_code}',
                  flush=True)
        except Exception:
            pass
        return response

    scheme = "https" if TLS_ENABLED else "http"
    port = int(CFG.get("port", 8321))
    ssl_kw: dict = {}
    if TLS_ENABLED:
        pair = tlsmod.ensure(TLS_DIR)
        if pair:
            ssl_kw = {"ssl_certfile": str(pair[0]), "ssl_keyfile": str(pair[1])}
            info = tlsmod.describe(TLS_DIR)
            print(f"证书 {pair[0]}" +
                  ("（Tailscale 公信证书）" if info["trusted_ca"] else "（自签，手机首次访问需点“继续”）"))
        else:
            scheme = "http"
            print("[警告] 没找到 openssl，无法生成证书，退化为明文 HTTP。")

    ts_ip = tlsmod.tailscale_ip()
    ts_name = tlsmod.tailscale_name()

    def _can_bind(h: str, p: int) -> bool:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            s.bind((h, p))
            return True
        except OSError:
            return False
        finally:
            s.close()

    # 明文端口有两个，各管一边：
    #   local_port → 127.0.0.1，给本机浏览器（Safari/Chrome 对 127.0.0.1 的自签证书
    #                直接拦成「此连接非私人连接」，而且不给「继续」的常规入口，
    #                本机必须走明文才不打断使用。只绑 loopback，出不了这台机器。）
    #   plain_port → Tailscale 100.x，给手机（安卓不信任自签证书，会连带拒绝
    #                fetch/上传）。只绑隧道地址，根本不监听局域网。
    plain_binds: list[tuple[str, int]] = []

    # 本机明文端口必须无条件绑上：launcher 打开的就是 127.0.0.1:{local_port}，
    # 而 127.0.0.1 在 https 模式下本来也得靠这条明文口（自签证书会被 Safari/Chrome
    # 直接拦死，不给「继续」入口）。
    # 这段以前写在 `if scheme == "https"` 里面 —— 于是 TLS 建不起来的机器（Windows
    # 没有 openssl 就是这样）退化成 http 之后，local_port 一个都没绑：浏览器被领到
    # 没人监听的端口，用户只看到「无法连接」。0.2.1 的 Windows 冒烟就是这么挂的。
    local_port = int(CFG.get("local_port", 8323))
    if local_port and local_port != port:
        if _can_bind("127.0.0.1", local_port):
            plain_binds.append(("127.0.0.1", local_port))
        else:
            print(f"[警告] 本机明文端口 127.0.0.1:{local_port} 绑定失败（可能被占用）")

    if scheme == "https":
        plain_port = int(CFG.get("plain_port", 8322))
        if ts_ip and plain_port:
            if _can_bind(ts_ip, plain_port):
                plain_binds.append((ts_ip, plain_port))
            else:
                print(f"[警告] 明文端口 {ts_ip}:{plain_port} 绑定失败（可能被占用）")

    for _h, _p in plain_binds:
        _tip = "本机用这个，无证书警告" if _h.startswith("127.") else "手机用这个，无证书警告"
        print(f"口袋服务 http://{_h}:{_p}/?k={TOKEN}   （{_tip}）")
    print(f"口袋服务 {scheme}://127.0.0.1:{port}")
    if ts_name:
        print(f"         {scheme}://{ts_name}:{port}/?k={TOKEN}")
    for ip in (ts_ip, tlsmod.lan_ip()):
        if ip:
            print(f"         {scheme}://{ip}:{port}/?k={TOKEN}")
    print(f"访问令牌 {TOKEN}")
    print(f"收件箱 {INBOX_DIR}")
    if not DAV_ENABLED:
        print("WebDAV 已关闭（config.json 里 dav_enabled 改成 true 可恢复）")

    _host = CFG.get("host", "0.0.0.0")
    if plain_binds:
        servers = [uvicorn.Server(uvicorn.Config(app, host=_host, port=port,
                                                log_level="warning", **ssl_kw))]
        for _h, _p in plain_binds:
            _s = uvicorn.Server(uvicorn.Config(app, host=_h, port=_p,
                                               log_level="warning"))
            _s.install_signal_handlers = lambda: None  # 信号只由第一个 server 处理
            servers.append(_s)

        async def _serve_all() -> None:
            await asyncio.gather(*[s.serve() for s in servers])

        asyncio.run(_serve_all())
    else:
        uvicorn.run(app, host=_host, port=port, log_level="warning", **ssl_kw)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
