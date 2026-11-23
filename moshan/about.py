"""关于 / 更新页面（`/about`）。

为什么单独成模块，而不是在 ui.html 里加个按钮：

  ui.html 是与上游开发目录**逐字同步**的文件（见 docs/SYNC.md）。往里加按钮，
  下一次同步就会被覆盖掉，而且不报错 —— 属于「改完当天好使，过几天神秘消失」的那类坑。
  所以这里走「服务端注入 + 独立页面」：ui.html 一个字节都不动，由 server.py 在
  返回首页时把一个小入口拼进 HTML 末尾。

这一页负责三件事：
  1. 说清楚「我装的是什么版本、数据放在哪」（排障时最常问的两个问题）
  2. 检查更新：查 GitHub Release、显示更新说明、下载（带进度与 sha256 校验）
  3. 装更新：校验通过才允许点「安装」，交给系统安装程序覆盖程序目录

进度与结果都放进程内的 STATE，不落盘：下载是几百兆的短事务，进程重启就该重来，
留个半截状态反而更容易误导人。已下载好的包会留在状态目录的 updates/ 下，
所以「下载完关了浏览器、第二天再安装」也能接上（页面会读到那个文件）。
"""
from __future__ import annotations

import json
import threading
import time
from pathlib import Path
from typing import Optional

from fastapi import FastAPI, Request
from fastapi.responses import HTMLResponse, JSONResponse

import paths
import updater
import version

# 自动检查的节流：启动后先等一会（别和首启引导抢注意力），之后每 6 小时一次。
FIRST_DELAY = 90.0
INTERVAL = 6 * 3600.0

STATE: dict = {
    "checked_at": 0.0,      # 上次成功检查的时间戳
    "info": None,           # updater.check() 的原始返回
    "error": "",            # 上次检查的失败原因
    "phase": "idle",        # idle | downloading | done | error
    "done": 0,
    "total": 0,
    "path": "",             # 已下载并校验通过的安装包
    "msg": "",
}
_LOCK = threading.Lock()
_BUSY = {"download": False}
_TOKEN = ""
_CFG: dict = {}


# ---------------------------------------------------------------- 检查

def _do_check(timeout: float = 10.0) -> dict:
    """跑一次检查并写入 STATE。失败只记原因，不抛。"""
    try:
        info = updater.check(_CFG.get("update", {}).get("repo", ""), timeout=timeout)
    except Exception as exc:                            # noqa: BLE001  网络问题一律降级
        with _LOCK:
            STATE["error"] = f"{type(exc).__name__}: {exc}"[:200]
            STATE["checked_at"] = time.time()
        return {"ok": False, "error": STATE["error"]}
    with _LOCK:
        STATE["info"] = info
        STATE["error"] = ""
        STATE["checked_at"] = time.time()
    return {"ok": True, **info}


def _auto_enabled() -> bool:
    """后台自动检查的开关：config.json 里 update.auto，默认开。

    每次循环重读配置文件而不是用 import 时抓的那份 —— 用户改完不想重启服务。
    """
    try:
        import json
        d = json.loads(paths.config_file().read_text(encoding="utf-8"))
        return bool((d.get("update") or {}).get("auto", True))
    except Exception:                                   # noqa: BLE001  读不到就按默认（开）
        return True


def _push_mode() -> bool:
    """更新模式：默认查 GitHub Release；`update.mode = "push"` 表示只认主机推送。

    办公室电脑那类机器连不上 github.com（实测 8 秒超时），对它们来说「检查更新」
    是个必然失败的按钮。改成 push 后：不查、不下载、不跑安装包，等着主机把包推来。
    每次都重读配置文件 —— 改完开关不用重启服务。
    """
    try:
        import json
        d = json.loads(paths.config_file().read_text(encoding="utf-8"))
        return str((d.get("update") or {}).get("mode") or "").strip().lower() == "push"
    except Exception:                                   # noqa: BLE001
        return False


def _auto_check_loop() -> None:
    time.sleep(FIRST_DELAY)
    while True:
        if _push_mode():
            time.sleep(1800)                            # 主机推送模式：不查 Release
        elif _auto_enabled():
            try:
                _do_check()
            except Exception:                           # noqa: BLE001  线程不许死
                pass
            time.sleep(INTERVAL)
        else:
            time.sleep(600)                             # 关掉了就每 10 分钟看一次开关


def _existing_package() -> Optional[Path]:
    """状态目录里已有的安装包（上次下载完没装的那份）。"""
    d = paths.state_dir() / "updates"
    if not d.is_dir():
        return None
    files = [f for f in d.iterdir()
             if f.suffix.lower() in {".dmg", ".exe", ".zip", ".gz"}]
    if not files:
        return None
    return max(files, key=lambda f: f.stat().st_mtime)


# ---------------------------------------------------------------- 鉴权
#
# 这里不 import server 的 auth（会循环依赖），自己比一次令牌。
# 页面由本机浏览器打开，令牌从 ?k= 或 localStorage 带过来。

def _token_ok(request: Request) -> bool:
    import hmac
    got = (request.headers.get("x-token")
           or request.query_params.get("k") or "").strip()
    return bool(_TOKEN) and hmac.compare_digest(got, _TOKEN)


def _deny() -> JSONResponse:
    return JSONResponse({"ok": False, "error": "需要访问令牌"}, status_code=401)


# ---------------------------------------------------------------- 页面

_PAGE = """<!DOCTYPE html>
<html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>关于 · 墨山中台</title>
<style>
  :root{
    --bg:#f4f1ea; --card:#fffdf8; --line:#e2ddd1; --tx:#23231f; --tx2:#6b6a62;
    --ok:#2f7d54; --warn:#a8641c; --bad:#a33a32; --accent:#2b4b6f;
    --radius:14px; --mono:ui-monospace,SFMono-Regular,Menlo,monospace;
  }
  @media (prefers-color-scheme:dark){
    :root{--bg:#15161a; --card:#1d1f24; --line:#2e3138; --tx:#e9e7e1; --tx2:#9a988f;
          --ok:#5dbf8a; --warn:#d99a4e; --bad:#e07b70; --accent:#8fb4d9;}
  }
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--tx);
       font:15px/1.65 -apple-system,"PingFang SC","Microsoft YaHei",system-ui,sans-serif;
       padding:40px 20px 72px}
  .wrap{max-width:720px;margin:0 auto}
  header{display:flex;align-items:baseline;gap:12px;margin-bottom:6px;flex-wrap:wrap}
  h1{font-size:24px;margin:0;letter-spacing:.5px}
  .ver{font:12px/1 var(--mono);color:var(--tx2);border:1px solid var(--line);
       padding:4px 8px;border-radius:99px}
  .lead{color:var(--tx2);margin:0 0 24px;font-size:14px}
  .card{background:var(--card);border:1px solid var(--line);border-radius:var(--radius);
        padding:20px 22px;margin-bottom:14px}
  .card h2{font-size:15px;margin:0 0 10px}
  .kv{display:grid;grid-template-columns:auto 1fr;gap:5px 14px;font-size:13px;color:var(--tx2)}
  .kv b{color:var(--tx);font-weight:500}
  .path{font:11.5px/1.5 var(--mono);word-break:break-all}
  button{font:inherit;padding:9px 16px;border-radius:9px;border:1px solid var(--accent);
         background:var(--accent);color:#fff;cursor:pointer}
  button.ghost{background:transparent;color:var(--accent)}
  button:disabled{opacity:.45;cursor:not-allowed}
  .row{display:flex;gap:10px;align-items:center;flex-wrap:wrap;margin-top:12px}
  .tag{font:12px/1 var(--mono);padding:4px 9px;border-radius:6px;
       border:1px solid var(--line);color:var(--tx2)}
  .tag.ok{color:var(--ok)} .tag.warn{color:var(--warn)} .tag.bad{color:var(--bad)}
  #state:empty{display:none}
  .bar{height:7px;border-radius:99px;background:var(--line);overflow:hidden;margin-top:12px;display:none}
  .bar i{display:block;height:100%;width:0;background:var(--accent);transition:width .2s}
  pre{font:12px/1.6 var(--mono);background:var(--bg);border:1px solid var(--line);
      border-radius:9px;padding:12px 14px;white-space:pre-wrap;word-break:break-word;
      margin:12px 0 0;max-height:260px;overflow:auto;color:var(--tx2)}
  .foot{text-align:center;color:var(--tx2);font-size:12.5px;margin-top:24px}
  a{color:var(--accent)}
</style></head>
<body><div class="wrap">

<header>
  <h1>墨山中台</h1>
  <span class="ver" id="ver">Moshan</span>
  <span class="tag" id="chan"></span>
</header>
<p class="lead">本地优先的个人中台 · MIT 许可 · 数据全部留在本机</p>

<div class="card">
  <h2>更新</h2>
  <div class="kv">
    <span>当前版本</span><b id="cur">—</b>
    <span>最新版本</span><b id="latest">未检查</b>
    <span>更新源</span><b class="path" id="repo">—</b>
  </div>
  <div class="bar" id="bar"><i id="fill"></i></div>
  <div class="row">
    <button id="btn-check" disabled>检查更新</button>
    <button id="btn-dl" class="ghost" style="display:none">下载安装包</button>
    <button id="btn-apply" style="display:none">打开安装包</button>
    <span class="tag" id="state"></span>
  </div>
  <pre id="notes" style="display:none"></pre>
</div>

<div class="card">
  <h2>数据放在哪</h2>
  <div class="kv">
    <span>程序</span><b class="path" id="p-app">—</b>
    <span>状态</span><b class="path" id="p-state">—</b>
    <span>资料库</span><b class="path" id="p-lib">—</b>
    <span>配置</span><b class="path" id="p-cfg">—</b>
    <span>运行方式</span><b id="p-frozen">—</b>
  </div>
  <p class="lead" style="margin:12px 0 0;font-size:13px">
    安装包只覆盖「程序」这一层。更新、覆盖安装、重装都不会动上面两个目录。
  </p>
</div>

<div class="card">
  <h2>入口</h2>
  <div class="row" style="margin-top:0">
    <a href="/?k=__K__">返回中台</a>
    <span class="tag">/</span>
    <a href="/welcome?k=__K__">重新看新手引导</a>
    <span class="tag">/welcome</span>
    <a href="https://github.com/__REPO__" target="_blank" rel="noreferrer">GitHub</a>
  </div>
</div>

<p class="foot">Moshan · 墨山中台 · MIT</p>
</div>

<script>
const K = new URLSearchParams(location.search).get('k')
       || localStorage.getItem('pocket_token') || '';
const H = {'X-Token': K};
const $ = id => document.getElementById(id);

function tag(el, text, cls){ el.textContent = text; el.className = 'tag ' + (cls||''); }

async function j(url, opt){
  const r = await fetch(url, Object.assign({headers: H}, opt || {}));
  const d = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(d.error || ('HTTP ' + r.status));
  return d;
}

async function loadAbout(){
  try{
    const a = await j('/api/about');
    $('ver').textContent = 'Moshan ' + a.version + (a.build ? '+' + a.build : '');
    $('cur').textContent = a.version;
    $('chan').textContent = a.channel;
    $('repo').textContent = a.repo;
    $('p-app').textContent = a.app_dir;
    $('p-state').textContent = a.state_dir;
    $('p-lib').textContent = a.library_dir;
    $('p-cfg').textContent = a.config;
    $('p-frozen').textContent = a.frozen ? '打包运行（安装版）' : '源码运行';
    $('btn-check').disabled = false;
  }catch(e){
    // 直接访问 /about（没带令牌）是常见情况，不该显示成故障：
    // 说清楚"怎么进来"，并把按钮禁掉，免得点了继续报错。
    $('ver').textContent = 'Moshan';
    ['cur','repo','p-app','p-state','p-lib','p-cfg','p-frozen'].forEach(function(id){
      $(id).textContent = '需要令牌';
    });
    $('latest').textContent = '需要令牌';
    $('btn-check').disabled = true;
    tag($('state'), '这一页需要访问令牌：请从中台页面右下角「关于」进入', 'warn');
  }
}

function render(st){
  if (st.info){
    const i = st.info;
    $('latest').textContent = i.latest || '—';
    if (i.has_update){
      tag($('state'), '有新版本 ' + i.latest, 'warn');
      $('notes').style.display = 'block';
      $('notes').textContent = (i.notes || '').trim() || '（这个版本没有写更新说明）';
      $('btn-dl').style.display = st.path ? 'none' : '';
      $('btn-apply').style.display = st.path ? '' : 'none';
    }else{
      tag($('state'), '已是最新', 'ok');
      $('notes').style.display = 'none';
      $('btn-dl').style.display = 'none';
      $('btn-apply').style.display = st.path ? '' : 'none';
    }
  }else if (st.error){
    tag($('state'), '检查失败：' + st.error, 'bad');
  }

  if (st.phase === 'downloading'){
    $('bar').style.display = 'block';
    const pct = st.total > 0 ? Math.floor(st.done * 100 / st.total) : 0;
    $('fill').style.width = pct + '%';
    tag($('state'), '下载中 ' + pct + '%  ' + mb(st.done) + '/' + mb(st.total));
  }else if (st.phase === 'done'){
    $('bar').style.display = 'block';
    $('fill').style.width = '100%';
    tag($('state'), st.msg || '已下载并校验通过', st.msg ? 'warn' : 'ok');
    $('btn-apply').style.display = '';
  }else if (st.phase === 'error'){
    $('bar').style.display = 'none';
    tag($('state'), '下载失败：' + st.msg, 'bad');
  }
}

const mb = n => (n / 1048576).toFixed(1) + ' MB';

async function poll(){
  try{
    const st = await j('/api/update/state');
    render(st);
    if (st.phase === 'downloading'){ setTimeout(poll, 800); }
  }catch(e){}
}

$('btn-check').onclick = async () => {
  tag($('state'), '检查中…');
  $('btn-check').disabled = true;
  try{
    const d = await j('/api/update/check', {method: 'POST'});
    render({info: d.has_update === undefined ? null : d, phase: 'idle'});
    if (d.error) tag($('state'), '检查失败：' + d.error, 'bad');
  }catch(e){ tag($('state'), '检查失败：' + e.message, 'bad'); }
  $('btn-check').disabled = false;
};

$('btn-dl').onclick = async () => {
  $('btn-dl').disabled = true;
  try{
    await j('/api/update/download', {method: 'POST'});
    $('bar').style.display = 'block';
    poll();
  }catch(e){ tag($('state'), '下载失败：' + e.message, 'bad'); }
  $('btn-dl').disabled = false;
};

$('btn-apply').onclick = async () => {
  try{
    const d = await j('/api/update/apply', {method: 'POST'});
    tag($('state'), d.hint || '已打开安装包', 'ok');
  }catch(e){ tag($('state'), '打开失败：' + e.message, 'bad'); }
};

loadAbout();
poll();
</script>
</body></html>
"""


def _page_html() -> str:
    return (_PAGE
            .replace("__REPO__", version.DEFAULT_REPO)
            .replace("__K__", _TOKEN))


# ---------------------------------------------------------------- 注册

def register(app: FastAPI, cfg: dict, token: str) -> None:
    global _TOKEN, _CFG
    _TOKEN, _CFG = token, cfg

    @app.get("/about", response_class=HTMLResponse)
    def about_page() -> HTMLResponse:
        """关于页。和引导页一样免令牌，但受 server.py 的 guard_network 限制。"""
        return HTMLResponse(_page_html(), headers={"Cache-Control": "no-store"})

    @app.get("/api/about")
    def about_data(request: Request):
        if not _token_ok(request):
            return _deny()
        d = paths.describe()
        app_dir = d["app_dir"]
        if d["frozen"]:
            # PyInstaller 的 .app 里，真正的可执行文件在 Contents/Frameworks/ 下，
            # 直接显示它对人没意义 —— 用户要的是「装在哪」，那就给 .app 的路径。
            p = Path(app_dir)
            if p.name == "Frameworks" and p.parent.name == "Contents":
                app_dir = str(p.parent.parent)
        return {
            "ok": True,
            "version": version.VERSION,
            "build": version.BUILD,
            "channel": version.CHANNEL,
            "repo": _CFG.get("update", {}).get("repo") or version.DEFAULT_REPO,
            "platform": updater.platform_key(),
            "frozen": d["frozen"],
            "app_dir": app_dir,
            "state_dir": d["state_dir"],
            "library_dir": d["library_dir"],
            "config": d["config"],
        }

    @app.post("/api/update/check")
    def update_check(request: Request):
        if not _token_ok(request):
            return _deny()
        if _push_mode():
            # 这台机器的更新由主机推过来（nucbox 那种连不上 github.com 的），
            # 去 Release 查只会白等 8 秒超时，还把按钮做成「永远失败」。
            return {"ok": True, "has_update": False, "mode": "push",
                    "note": "本机更新由主机推送（update.mode=push），不走 GitHub"}
        return _do_check()

    @app.post("/api/update/mode")
    async def update_mode(request: Request):
        """把「本机怎么更新」钉死 —— 主机用这个替远端机器做决定。

        为什么需要它：nucbox 那台没有入站端口、也连不上 github.com，它的更新只能
        由主机推。但「它该走推」这个判断在主机这边，不在它自己身上。让人跑到那台
        机器上手改 config.json 是最容易漏的一步，漏了它就会一直去 GitHub 白等 8 秒
        超时，界面上的「检查更新」永远是一个必然失败的按钮。

        所以留一个开关接口：主机一次调用就能把它钉成 push，之后 /api/update/check
        直接返回「由主机推送」，不再碰网络。改完立即生效（_push_mode 每次重读文件）。
        """
        if not _token_ok(request):
            return _deny()
        try:
            body = await request.json()
        except Exception:                               # noqa: BLE001
            body = {}
        mode = str((body or {}).get("mode") or "").strip().lower()
        if mode not in ("push", "github"):
            return JSONResponse(
                {"ok": False, "error": "mode 只能是 push（只认主机推送）或 github（自己查 Release）"},
                status_code=400)
        p = paths.config_file()
        try:
            cur = json.loads(p.read_text(encoding="utf-8"))
            if not isinstance(cur, dict):
                cur = {}
        except (OSError, ValueError):
            cur = {}
        upd = cur.get("update")
        if not isinstance(upd, dict):
            upd = {}
        before = str(upd.get("mode") or "github")
        upd["mode"] = mode
        cur["update"] = upd
        tmp = p.with_suffix(p.suffix + ".tmp")
        try:
            p.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_text(json.dumps(cur, ensure_ascii=False, indent=1), encoding="utf-8")
            tmp.replace(p)
        except OSError as exc:
            return JSONResponse({"ok": False, "error": f"写不了配置文件：{exc}"[:200]},
                                status_code=500)
        _CFG["update"] = upd                            # 内存里那份也跟上，/api/about 立刻准
        return {"ok": True, "mode": mode, "before": before, "config": str(p),
                "note": ("之后只认主机推送，不再查 GitHub" if mode == "push"
                         else "恢复自己查 GitHub Release")}

    @app.get("/api/update/state")
    def update_state(request: Request):
        if not _token_ok(request):
            return _deny()
        with _LOCK:
            st = dict(STATE)
        # 上次下载好但还没装的那份，直接摆出来让用户能点「打开安装包」
        if not st["path"]:
            pkg = _existing_package()
            if pkg:
                st["path"] = str(pkg)
        return st

    @app.post("/api/update/download")
    def update_download(request: Request):
        if not _token_ok(request):
            return _deny()
        with _LOCK:
            info = STATE["info"]
        if not info or not info.get("has_update"):
            return JSONResponse({"ok": False, "error": "没有可下载的新版本，先检查一次"},
                                status_code=400)
        asset = info.get("asset") or {}
        if not asset:
            return JSONResponse(
                {"ok": False, "error": "这个版本没有当前平台的安装包"}, status_code=400)
        if _BUSY["download"]:
            return {"ok": True, "started": False, "note": "已经在下载了"}

        def work() -> None:
            def progress(done: int, total: int) -> None:
                with _LOCK:
                    STATE["done"], STATE["total"] = done, total
            try:
                with _LOCK:
                    STATE.update(phase="downloading", done=0, total=int(asset.get("size") or 0),
                                 msg="", path="")
                p = updater.download(asset, on_progress=progress)
                # GitHub 有时不回 digest 字段（老资产或 API 变更），此时下载仍然完成
                # 但没做完整性校验 —— 别在界面上写「已校验通过」，那是假话。
                note = ""
                try:
                    meta = json.loads(p.with_suffix(p.suffix + ".meta").read_text(encoding="utf-8"))
                    if not meta.get("verified"):
                        note = "已下载，但 GitHub 未提供校验值，未做完整性校验"
                except (OSError, ValueError):
                    pass
                with _LOCK:
                    STATE.update(phase="done", path=str(p), msg=note,
                                 done=STATE["total"] or STATE["done"])
                try:
                    updater.cleanup(keep=2)
                except Exception:                       # noqa: BLE001  清旧包失败不影响本次
                    pass
            except Exception as exc:                    # noqa: BLE001
                with _LOCK:
                    STATE.update(phase="error", msg=f"{type(exc).__name__}: {exc}"[:200])
            finally:
                _BUSY["download"] = False

        _BUSY["download"] = True
        threading.Thread(target=work, daemon=True).start()
        return {"ok": True, "started": True}

    def _source_zip(tag: str) -> bytes:
        r"""从 codeload 取某个 tag 的源码包（不带 .git，约 1~3 MB）。

        为什么源码部署走这条路而不是安装包：EVO 那种部署是「run-evo.cmd →
        C:\moshan-024\moshan\server.py」，开机自启指向的就是那个目录。给它
        装一个 exe，要么起两个服务，要么自启还指着旧的源码目录 —— 都不对。
        """
        import urllib.request
        r_ = (_CFG.get("update", {}) or {}).get("repo") or version.DEFAULT_REPO
        url = f"https://codeload.github.com/{r_}/zip/refs/tags/v{tag.lstrip('vV')}"
        req = urllib.request.Request(url, headers={"User-Agent": "curl/8"})
        with urllib.request.urlopen(req, timeout=90) as r:      # noqa: S310
            return r.read()

    def _strip_top(raw: bytes) -> bytes:
        """codeload 的 zip 顶层多一层 `moshan-0.3.1/` 壳。

        `pushupd._unpack` 是平铺写到程序目录的（它收到的主机推送包本来就没壳），
        不削掉这层，server.py 会落在 `moshan-0.3.1/` 子目录里，随后被判「包里没有
        server.py，拒收」。
        """
        import io
        import zipfile
        src = zipfile.ZipFile(io.BytesIO(raw))
        names = src.namelist()
        tops = {n.split("/")[0] for n in names if "/" in n}
        if len(tops) != 1:
            return raw
        top = tops.pop() + "/"
        out = io.BytesIO()
        with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED) as dst:
            for n in names:
                if n.startswith(top) and not n.endswith("/"):
                    dst.writestr(n[len(top):], src.read(n))
        return out.getvalue()

    def _apply_source_from_github(tag: str) -> dict:
        """源码部署自己从 GitHub 更新：取源码包 → 削壳 → 交给 pushupd 原地替换。"""
        import base64
        import hashlib
        import pushupd

        with _LOCK:
            STATE.update(phase="downloading", done=0, total=0, msg="", path="")
        try:
            raw = _strip_top(_source_zip(tag))
        except Exception as exc:                                # noqa: BLE001
            with _LOCK:
                STATE.update(phase="error", msg=f"取源码包失败：{exc}"[:200])
            return {"ok": False, "error": f"取源码包失败：{type(exc).__name__}: {exc}"}
        digest = hashlib.sha256(raw).hexdigest()
        # 一片 1 MB：base64 会涨 4/3，单片 1.4 MB 的请求体对 8 MB 的包最省事
        chunk = 1 << 20
        parts = [raw[i:i + chunk] for i in range(0, len(raw), chunk)] or [b""]
        body = {"version": tag, "sha256": digest, "name": f"github-source-{tag}.zip",
                "total": len(parts)}
        out: dict = {"ok": False, "error": "没有分片"}
        for i, p in enumerate(parts):
            body.update(index=i, b64=base64.b64encode(p).decode("ascii"))
            # 只在最后一片要求重启：中途重启会把剩下的分片丢掉
            body["restart"] = (i == len(parts) - 1)
            out = pushupd.push(dict(body))
            if not out.get("ok"):
                return out
        with _LOCK:
            STATE.update(phase="done", done=len(raw), total=len(raw),
                         path=f"github:v{tag}", msg=f"源码包 {len(raw) // 1024} KB")
        return out

    @app.post("/api/update/apply")
    def update_apply(request: Request):
        if not _token_ok(request):
            return _deny()
        if _push_mode():
            return JSONResponse(
                {"ok": False, "mode": "push",
                 "error": "本机更新由主机推送（update.mode=push）：主机推完会自动重启，"
                          "不需要在本机跑安装包"},
                status_code=400)
        with _LOCK:
            p = STATE["path"]
        path = Path(p) if p else _existing_package()
        # 源码部署：安装包对它没用（装完要么起两个服务，要么自启还指着旧目录），
        # 改从 GitHub 取源码包原地替换 —— 和主机推送走的是同一套 pushupd。
        import sys as _sys
        if not getattr(_sys, "frozen", False):
            with _LOCK:
                info = STATE["info"] or {}
            tag = str(info.get("latest") or "").strip()
            if not tag:
                return JSONResponse(
                    {"ok": False, "error": "还没查过有没有新版本，先点一次「检查更新」"},
                    status_code=400)
            return _apply_source_from_github(tag)
        if not path or not Path(path).exists():
            return JSONResponse({"ok": False, "error": "还没有下载好的安装包"},
                                status_code=400)
        try:
            hint = updater.launch_installer(Path(path))
        except Exception as exc:                        # noqa: BLE001
            return JSONResponse({"ok": False, "error": f"{type(exc).__name__}: {exc}"},
                                status_code=500)
        return {"ok": True, "hint": hint, "path": str(path)}

    threading.Thread(target=_auto_check_loop, daemon=True).start()


# ---------------------------------------------------------------- 首页注入
#
# 不碰 ui.html：在返回首页时把一个小入口拼到 </body> 前面。
# 有新版本时它自己变成「新版本 x.y.z」，否则只是角落里一个「关于」。

_BADGE = """
<style>
  /* 底部有标签栏，入口必须抬到它上面，否则会压住「归整」那一格（宽屏同样会压）。 */
  #mosh-about{position:fixed;right:14px;bottom:72px;z-index:9998;
    font:12.5px/1 -apple-system,'PingFang SC',system-ui,sans-serif}
  #mosh-about a{display:inline-flex;align-items:center;gap:6px;
    padding:8px 12px;border-radius:99px;text-decoration:none;color:#f4f1ea;
    background:rgba(35,35,31,.72);backdrop-filter:blur(6px);
    box-shadow:0 2px 10px rgba(0,0,0,.18)}
  @media (max-width:600px){ #mosh-about{bottom:76px;right:10px} }
</style>
<div id="mosh-about">
  <a id="mosh-about-a" href="/about">关于</a>
</div>
<script>
(function(){
  var K = new URLSearchParams(location.search).get('k')
        || localStorage.getItem('pocket_token') || '';
  var a = document.getElementById('mosh-about-a');
  a.href = '/about' + (K ? '?k=' + encodeURIComponent(K) : '');
  fetch('/api/update/state', {headers: {'X-Token': K}})
    .then(function(r){ return r.ok ? r.json() : null; })
    .then(function(st){
      if (!st || !st.info || !st.info.has_update) return;
      a.textContent = '新版本 ' + st.info.latest;
      a.style.background = '#a8641c';
    })
    .catch(function(){});
})();
</script>
"""


def inject(html: str) -> str:
    """把关于入口拼进首页。已经拼过就不重复拼（同一进程内多次渲染也安全）。"""
    if "mosh-about" in html:
        return html
    if "</body>" in html:
        return html.replace("</body>", _BADGE + "</body>", 1)
    return html + _BADGE
