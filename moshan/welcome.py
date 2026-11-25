"""新手引导页（/welcome）。

为什么单独成模块而不是塞进 server.py：
  server.py 已经 3000 行，引导是一次性路径，混进去只会让主流程更难读。
  这里只依赖 bootstrap 与 paths，出问题也不影响中台本体。

四步（2026-11-25 改）：
  1 环境检查   —— 本机有没有 Ollama，程序/配置/资料库在哪
  2 选功能     —— 哪些模块要摆上中台；要不要本机大模型
  3 数据存哪   —— 默认 ~/Documents/Moshan，可以换盘
  4 说明存什么 —— 逐条列出「会存 / 不存 / 只读」的边界，然后进中台

第 3 步的选择会写进 config.json 的 library_dir，paths.library_dir() 认它。

页面免 token，但受 server.py 的 guard_network 保护 —— 只有本机与 Tailscale
来源能打开，公网与局域网默认被挡。
"""
from __future__ import annotations

import json
import os
import subprocess
import sys

from fastapi import APIRouter, HTTPException, Request
from fastapi.responses import HTMLResponse, JSONResponse

import bootstrap
import paths
import version

WELCOME_HTML = r"""<!doctype html>
<html lang="zh-CN"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>欢迎使用 Moshan 墨山中台</title>
<style>
  :root{
    --bg:#f4f1ea; --card:#fffdf8; --line:#e2ddd1; --tx:#23231f; --tx2:#6b6a62;
    --tx3:#96938a; --ok:#2f7d54; --warn:#a8641c; --bad:#a33a32; --accent:#2b4b6f;
    --radius:14px; --mono:ui-monospace,SFMono-Regular,Menlo,monospace;
  }
  @media (prefers-color-scheme:dark){
    :root{--bg:#15161a; --card:#1d1f24; --line:#2e3138; --tx:#e9e7e1; --tx2:#9a988f;
          --tx3:#75736b; --ok:#5dbf8a; --warn:#d99a4e; --bad:#e07b70; --accent:#8fb4d9;}
  }
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--tx);
       font:15px/1.65 -apple-system,"PingFang SC","Microsoft YaHei",system-ui,sans-serif;
       padding:40px 20px 72px}
  .wrap{max-width:740px;margin:0 auto}
  header{display:flex;align-items:baseline;gap:12px;margin-bottom:6px}
  h1{font-size:26px;margin:0;letter-spacing:.5px}
  .ver{font:12px/1 var(--mono);color:var(--tx2);border:1px solid var(--line);
       padding:4px 8px;border-radius:99px}
  .lead{color:var(--tx2);margin:0 0 26px;font-size:14px}
  .step{background:var(--card);border:1px solid var(--line);border-radius:var(--radius);
        padding:20px 22px;margin-bottom:14px}
  .step h2{font-size:16px;margin:0 0 4px;display:flex;align-items:center;gap:9px}
  .num{display:inline-flex;width:22px;height:22px;border-radius:50%;
       background:var(--accent);color:#fff;font:600 12px/22px var(--mono);
       text-align:center;justify-content:center;flex:0 0 auto}
  .hint{color:var(--tx2);font-size:13.5px;margin:2px 0 14px}
  .row{display:flex;gap:10px;align-items:center;flex-wrap:wrap}
  .tag{font:12px/1 var(--mono);padding:4px 9px;border-radius:6px;
       border:1px solid var(--line);color:var(--tx2)}
  .tag.ok{color:var(--ok);border-color:color-mix(in srgb,var(--ok) 40%,var(--line))}
  .tag.bad{color:var(--bad);border-color:color-mix(in srgb,var(--bad) 40%,var(--line))}
  .tag.warn{color:var(--warn);border-color:color-mix(in srgb,var(--warn) 40%,var(--line))}
  button{font:inherit;padding:9px 16px;border-radius:9px;border:1px solid var(--accent);
         background:var(--accent);color:#fff;cursor:pointer}
  button.ghost{background:transparent;color:var(--accent)}
  button.mini{padding:5px 11px;font-size:13px}
  button:disabled{opacity:.45;cursor:not-allowed}
  input[type=text]{font:inherit;padding:8px 11px;border-radius:9px;
       border:1px solid var(--line);background:var(--bg);color:var(--tx);width:100%}
  pre{font:12px/1.55 var(--mono);background:var(--bg);border:1px solid var(--line);
      border-radius:9px;padding:11px 13px;max-height:190px;overflow:auto;
      white-space:pre-wrap;word-break:break-all;margin:12px 0 0;color:var(--tx2)}
  .kv{display:grid;grid-template-columns:auto 1fr;gap:4px 14px;font-size:13px;
      color:var(--tx2);margin-top:10px}
  .kv b{color:var(--tx);font-weight:500}
  .foot{text-align:center;color:var(--tx2);font-size:12.5px;margin-top:26px}
  .path{font:11.5px/1.5 var(--mono);word-break:break-all;overflow-wrap:anywhere}

  /* 功能选择：两列小格子，点一下切换 */
  .feats{display:grid;grid-template-columns:1fr 1fr;gap:9px;margin:4px 0 14px}
  @media (max-width:560px){ .feats{grid-template-columns:1fr} }
  .feat{display:flex;gap:9px;align-items:flex-start;border:1px solid var(--line);
        border-radius:10px;padding:9px 11px;cursor:pointer;background:var(--bg);
        transition:border-color .12s,background .12s}
  .feat:hover{border-color:var(--accent)}
  .feat input{width:15px;height:15px;margin:3px 0 0;flex:0 0 auto;accent-color:var(--accent)}
  .feat .fn{font-size:13.5px;font-weight:600;line-height:1.35}
  .feat .fd{font-size:11.5px;color:var(--tx3);line-height:1.45;margin-top:1px}
  .feat.off{opacity:.55}

  .llm{margin-top:4px;border-top:1px dashed var(--line);padding-top:14px}
  .llm .row{margin-top:10px}
  .llmBody[hidden]{display:none}

  /* 存什么：三栏清单 */
  .lbl{display:inline-block;font:600 11.5px/1 var(--mono);padding:3px 7px;border-radius:5px;
       margin:14px 0 7px}
  .lbl.yes{background:color-mix(in srgb,var(--ok) 14%,transparent);color:var(--ok)}
  .lbl.no{background:color-mix(in srgb,var(--bad) 14%,transparent);color:var(--bad)}
  .lbl.ro{background:color-mix(in srgb,var(--accent) 14%,transparent);color:var(--accent)}
  ul.dt{margin:0;padding-left:19px;font-size:13.5px;color:var(--tx2);line-height:1.72}
  ul.dt li b{color:var(--tx);font-weight:600}
  ul.dt code{font:11.5px var(--mono);background:var(--bg);padding:1px 5px;border-radius:4px}
</style></head>
<body><div class="wrap">
  <header><h1>墨山中台</h1><span class="ver" id="ver">__VERSION__</span></header>
  <p class="lead">四步配完就能用。程序在这台电脑上跑，数据留在你自己的盘里。</p>

  <div class="step">
    <h2><span class="num">1</span>环境检查</h2>
    <p class="hint">看看本机有没有 Ollama，以及程序、配置、数据分别在哪儿。</p>
    <div class="row" id="envRow"><span class="tag">检测中…</span></div>
    <div class="kv" id="dirs"></div>
  </div>

  <div class="step">
    <h2><span class="num">2</span>要哪些功能</h2>
    <p class="hint">勾上的会出现在中台里，没勾的以后也能在「设置 → 功能」找回。</p>
    <div class="feats" id="feats"></div>
    <div class="llm">
      <label class="feat" id="llmSw" style="background:transparent;border:0;padding:0">
        <input type="checkbox" id="useLlm">
        <div><div class="fn">本机大模型（可选）</div>
          <div class="fd">给「对话」用，模型文件约 2.4 GB。不装也能用，只是对话会提示去配一个。</div></div>
      </label>
      <div class="llmBody" id="llmBody" hidden>
        <div class="row">
          <input type="text" id="model" value="qwen3.5-4b-gguf:q4km" style="flex:1;min-width:220px">
          <button id="pullBtn">开始下载</button>
        </div>
        <pre id="pullLog" style="display:none"></pre>
      </div>
    </div>
  </div>

  <div class="step">
    <h2><span class="num">3</span>数据存哪里</h2>
    <p class="hint">你的文件、账目、笔记都放这个目录下。换盘就点「换一个位置」，会建在新位置上、不会搬旧数据。</p>
    <div class="row">
      <input type="text" id="libDir" readonly style="flex:1;min-width:240px">
      <button class="ghost mini" id="pickBtn">换一个位置</button>
      <button class="ghost mini" id="resetBtn">用默认</button>
    </div>
    <div class="kv" id="libHint"></div>
  </div>

  <div class="step">
    <h2><span class="num">4</span>会存你哪些数据</h2>
    <p class="hint">写清楚，免得你事后才发现「怎么多了一份」。</p>

    <span class="lbl yes">会存</span>
    <ul class="dt">
      <li><b>你自己放进去的文件</b>：存图、收件箱、阅读原文 —— 原样保存</li>
      <li><b>账目</b>：记账流水、工作流水、Token 用量折算（含单价与实付）</li>
      <li><b>会话摘要</b>：一个 AI 会话一行 —— 时间、时长、轮次、工具调用次数、token。
          实测百来个会话也才 <code>60 KB</code></li>
      <li><b>你自己的东西</b>：笔记、日程、待办、收藏</li>
    </ul>

    <span class="lbl no">不存</span>
    <ul class="dt">
      <li>AI 对话的正文 —— 不复制，需要时回原客户端看</li>
      <li>工具调用的入参与返回（那才是几百 MB 的来源）</li>
      <li>模型思考过程、文件改动快照、附件 base64</li>
    </ul>

    <span class="lbl ro">只读、不复制</span>
    <ul class="dt">
      <li>WorkBuddy / 千问办公 / Cherry Studio / TRAE / Codex 等客户端的会话目录</li>
      <li>只读打开、只统计，绝不写入；关掉中台不影响它们</li>
    </ul>

    <div class="row" style="margin-top:18px">
      <button id="doneBtn">进入中台</button>
      <span class="tag" id="saveState" hidden></span>
    </div>
  </div>

  <p class="foot">Moshan · 本地优先的个人中台 · 程序目录与数据目录分离，更新不动你的文件</p>
</div>
<script>
const TOKEN = "__TOKEN__";
const $ = (s) => document.querySelector(s);
let ollamaOk = false;
let libDefault = "";

const FEATS = [
  {id:"pocket", name:"存图收藏",  desc:"公众号 / 小红书链接存成图集"},
  {id:"reader", name:"阅读",      desc:"订阅与稍后读"},
  {id:"ledger", name:"记账",      desc:"账户与流水"},
  {id:"host",   name:"Agent 监控", desc:"本机各 AI 客户端的活账"},
  {id:"flow",   name:"工作流水",  desc:"每天做了什么的账"},
  {id:"ops",    name:"系统巡检",  desc:"服务、磁盘、内存"},
  {id:"media",  name:"影音室",    desc:"番号解析、封面、演员名册"},
  {id:"plan",   name:"AI 计划",   desc:"项目 → 任务 → 收件箱"}
];

async function api(path, body){
  const opt = body ? {method:"POST", headers:{"Content-Type":"application/json"},
                      body:JSON.stringify(body)} : {};
  const r = await fetch(path, opt);
  if(!r.ok) throw new Error(path + " -> " + r.status);
  return r.json();
}

function tag(text, kind){
  const s = document.createElement("span");
  s.className = "tag" + (kind ? " " + kind : "");
  s.textContent = text;
  return s;
}

function paintFeats(){
  const box = $("#feats");
  box.innerHTML = "";
  FEATS.forEach(f => {
    const l = document.createElement("label");
    l.className = "feat";
    l.innerHTML = '<input type="checkbox" data-f="' + f.id + '" checked>'
      + '<div><div class="fn">' + f.name + '</div><div class="fd">' + f.desc + '</div></div>';
    const cb = l.querySelector("input");
    cb.addEventListener("change", () => l.classList.toggle("off", !cb.checked));
    box.appendChild(l);
  });
}

function picked(){
  return [].slice.call(document.querySelectorAll("#feats input:checked"))
           .map(el => el.getAttribute("data-f"));
}

async function loadEnv(){
  const row = $("#envRow"); row.innerHTML = "";
  try{
    const s = await api("/welcome/status");
    const o = s.ollama || {};
    row.appendChild(tag(o.installed ? "已装 Ollama" : "未装 Ollama", o.installed ? "ok" : "warn"));
    row.appendChild(tag(o.running ? "服务运行中" : "服务未启动", o.running ? "ok" : "bad"));
    if(o.version) row.appendChild(tag(o.version));
    ollamaOk = !!o.installed && !!o.running;

    const d = s.dirs || {};
    const kv = $("#dirs"); kv.innerHTML = "";
    const add = (k,v)=>{ kv.insertAdjacentHTML("beforeend",
      '<b>'+k+'</b><span class="path">'+(v||"-")+'</span>'); };
    add("程序", d.app_dir);
    add("配置", d.config);
    add("数据", d.library_dir);

    libDefault = d.library_dir || "";
    $("#libDir").value = libDefault;
    paintLibHint();
  }catch(e){
    row.appendChild(tag("检测失败：" + e.message, "bad"));
  }
}

function paintLibHint(){
  const h = $("#libHint");
  const changed = $("#libDir").value && libDefault && $("#libDir").value !== libDefault;
  h.innerHTML = '<b>默认</b><span class="path">' + (libDefault || "-") + '</span>'
    + '<b>占用</b><span>只增不减地放你自己的文件；会话摘要那份按 300 字节/会话算，一万个会话约 3 MB</span>'
    + (changed ? '<b>改动</b><span style="color:var(--warn)">保存后重启一次中台才完全生效（旧目录不删）</span>' : '');
}

async function startPull(){
  const model = $("#model").value.trim();
  if(!model) return;
  $("#pullBtn").disabled = true;
  $("#pullLog").style.display = "block";
  $("#pullLog").textContent = "";
  try{
    const r = await api("/welcome/pull", {model});
    if(!r.started){
      $("#pullLog").textContent = r.reason || "未能启动";
      $("#pullBtn").disabled = false; return;
    }
    pollPull();
  }catch(e){
    $("#pullLog").textContent = "启动失败：" + e.message;
    $("#pullBtn").disabled = false;
  }
}

async function pollPull(){
  try{
    const s = await api("/welcome/pull");
    $("#pullLog").textContent = (s.tail || []).join("\n");
    $("#pullLog").scrollTop = $("#pullLog").scrollHeight;
    if(s.running){ setTimeout(pollPull, 1200); return; }
    $("#pullBtn").disabled = false;
  }catch(e){
    $("#pullBtn").disabled = false;
  }
}

async function saveThenEnter(){
  const btn = $("#doneBtn");
  btn.disabled = true; btn.textContent = "保存中…";
  const body = {
    features: picked(),
    use_llm: $("#useLlm").checked,
    model: $("#model").value.trim(),
    library_dir: $("#libDir").value.trim()
  };
  try{
    const r = await api("/welcome/apply", body);
    const st = $("#saveState");
    st.hidden = false;
    st.className = "tag " + (r.ok ? "ok" : "bad");
    st.textContent = r.ok ? ("已保存 · " + (r.moved_hint ? "重启后生效" : "可以进中台了"))
                          : ("没保存上：" + (r.error || ""));
  }catch(e){
    const st = $("#saveState");
    st.hidden = false; st.className = "tag bad";
    st.textContent = "保存失败：" + e.message;
  }
  await api("/welcome/finish", {}).catch(()=>{});
  /* 进中台，不进口袋首页 —— 口袋只是中台里的一项能力 */
  location.href = "/dash?k=" + encodeURIComponent(TOKEN);
}

$("#pickBtn").addEventListener("click", async () => {
  try{
    const r = await api("/welcome/pickdir", {});
    if(r && r.ok){ $("#libDir").value = r.path; paintLibHint(); }
    else if(r && r.error && !r.cancelled) alert(r.error);
  }catch(e){ alert("选择器打不开：" + e.message); }
});
$("#resetBtn").addEventListener("click", () => {
  $("#libDir").value = libDefault; paintLibHint();
});
$("#useLlm").addEventListener("change", (e) => {
  $("#llmBody").hidden = !e.target.checked;
});
$("#pullBtn").addEventListener("click", startPull);
$("#doneBtn").addEventListener("click", saveThenEnter);

paintFeats();
loadEnv();
</script></body></html>
"""


def register(app, cfg: dict, token: str) -> None:
    """把引导相关路由挂到主应用上。"""
    router = APIRouter()

    @router.get("/welcome", response_class=HTMLResponse)
    def welcome_page() -> HTMLResponse:
        html = (WELCOME_HTML
                .replace("__VERSION__", f"v{version.VERSION}")
                .replace("__TOKEN__", token))
        return HTMLResponse(html, headers={"Cache-Control": "no-store"})

    @router.get("/welcome/status")
    def welcome_status() -> JSONResponse:
        return JSONResponse(bootstrap.summary())

    @router.post("/welcome/pickdir")
    def welcome_pickdir() -> JSONResponse:
        """弹原生的「选择文件夹」。网页拿不到绝对路径，只能借 osascript。"""
        if sys.platform != "darwin":
            return JSONResponse({"ok": False,
                                 "error": "这个系统没有原生选择器，直接填路径"})
        try:
            out = subprocess.run(
                ["osascript", "-e",
                 'POSIX path of (choose folder with prompt "选一个目录放墨山的数据")'],
                capture_output=True, text=True, timeout=180)
        except Exception as exc:                      # noqa: BLE001
            return JSONResponse({"ok": False, "error": f"{type(exc).__name__}: {exc}"})
        p = (out.stdout or "").strip().rstrip("/")
        if out.returncode != 0 or not p:
            return JSONResponse({"ok": False, "cancelled": True, "error": "已取消"})
        return JSONResponse({"ok": True, "path": p})

    @router.post("/welcome/apply")
    async def welcome_apply(request: Request) -> JSONResponse:
        """把四个选择写进 config.json。

        library_dir 改了要连带改 allowed_roots / trash_dir / dav_root ——
        否则新目录里的文件会被「不在允许范围」挡在外面，用户以为存丢了。
        """
        try:
            body = await request.json()
        except Exception:                             # noqa: BLE001
            body = {}
        body = body or {}

        p = paths.config_file()
        try:
            cur = json.loads(p.read_text(encoding="utf-8")) if p.exists() else dict(cfg)
        except (OSError, ValueError):
            cur = dict(cfg)

        feats = body.get("features")
        if isinstance(feats, list):
            cur["features"] = [str(x) for x in feats if str(x).strip()]
        cur["use_llm"] = bool(body.get("use_llm"))
        if body.get("model"):
            cur["model"] = str(body["model"]).strip()

        moved = False
        want = str(body.get("library_dir") or "").strip()
        old = str(cur.get("library_dir") or "").strip()
        if want and want != old:
            try:
                target = paths.ensure(want)
            except OSError as exc:
                return JSONResponse({"ok": False,
                                     "error": f"这个目录建不出来：{exc}"})
            cur["library_dir"] = str(target)
            cur["allowed_roots"] = [str(target)]
            cur["trash_dir"] = str(target / ".回收站")
            cur["dav_root"] = str(target)
            cur["inbox_dir"] = str(target / "07-收件箱")
            moved = True

        try:
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(json.dumps(cur, ensure_ascii=False, indent=2), encoding="utf-8")
            os.chmod(p, 0o600)
        except OSError as exc:
            return JSONResponse({"ok": False, "error": f"写配置失败：{exc}"})
        return JSONResponse({"ok": True, "moved_hint": moved,
                             "library_dir": cur.get("library_dir") or str(paths.library_dir())})

    @router.post("/welcome/pull")
    async def welcome_pull(request: Request) -> JSONResponse:
        try:
            payload = await request.json()
        except Exception:  # noqa: BLE001 —— 空 body 也允许
            payload = {}
        model = str((payload or {}).get("model") or cfg.get("model") or "").strip()
        if not model:
            raise HTTPException(400, "缺少模型名")
        base = str(cfg.get("ollama_base") or "http://127.0.0.1:11434")
        try:
            return JSONResponse(bootstrap.pull_async(model, base))
        except bootstrap.BootstrapError as exc:
            raise HTTPException(400, str(exc)) from exc

    @router.get("/welcome/pull")
    def welcome_pull_status() -> JSONResponse:
        return JSONResponse(bootstrap.pull_status())

    @router.post("/welcome/finish")
    def welcome_finish() -> JSONResponse:
        bootstrap.mark_done()
        return JSONResponse({"ok": True, "dirs": paths.describe()})

    @router.post("/welcome/gateway/test")
    async def welcome_gateway_test(request: Request) -> JSONResponse:
        try:
            payload = await request.json()
        except Exception:  # noqa: BLE001
            payload = {}
        base = str((payload or {}).get("base") or cfg.get("gateway_base") or "")
        key = str((payload or {}).get("key") or cfg.get("gateway_api_key") or "")
        return JSONResponse(bootstrap.probe_gateway(base, key))

    app.include_router(router)
