"""新手引导页（/welcome）。

为什么单独成模块而不是塞进 server.py：
  server.py 已经 3000 行，引导是一次性路径，混进去只会让主流程更难读。
  这里只依赖 bootstrap 与 paths，出问题也不影响中台本体。

页面免 token，但受 server.py 的 guard_network 保护 —— 只有本机与 Tailscale
来源能打开，公网与局域网默认被挡。
"""
from __future__ import annotations

import json

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
  .path{font:11.5px/1.5 var(--mono);word-break:break-all}
</style></head>
<body><div class="wrap">
  <header><h1>墨山中台</h1><span class="ver" id="ver">__VERSION__</span></header>
  <p class="lead">三步就能用。文件、传输、分享、对话都在本机跑，数据不离开这台电脑。</p>

  <div class="step">
    <h2><span class="num">1</span>环境检查</h2>
    <p class="hint">检测本机是否已有 Ollama，以及程序与数据目录的位置。</p>
    <div class="row" id="envRow"><span class="tag">检测中…</span></div>
    <div class="kv" id="dirs"></div>
  </div>

  <div class="step">
    <h2><span class="num">2</span>准备模型</h2>
    <p class="hint" id="modelHint">模型是可选能力。没有它，文件、传输、分享照常可用，只有「对话」会降级。</p>
    <div class="row">
      <input type="text" id="model" value="qwen3.5-4b-gguf:q4km" style="flex:1;min-width:220px">
      <button id="pullBtn">开始下载</button>
      <button class="ghost" id="skipBtn">跳过</button>
    </div>
    <pre id="pullLog" style="display:none"></pre>
  </div>

  <div class="step">
    <h2><span class="num">3</span>完成</h2>
    <p class="hint">进入中台后，左下角「？」随时能找回这份引导。</p>
    <div class="row"><button id="doneBtn" disabled>进入中台</button></div>
  </div>

  <p class="foot">Moshan · 本地优先的个人中台 · 数据目录与程序目录分离，更新不会动你的文件</p>
</div>
<script>
const TOKEN = "__TOKEN__";
const $ = (s) => document.querySelector(s);
let ollamaOk = false;

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
      '<b>'+k+'</b><span class="path">'+v+'</span>'); };
    add("程序", d.app_dir || "-");
    add("配置", d.config || "-");
    add("资料库", d.library_dir || "-");

    if(ollamaOk){
      const has = (o.models||[]).some(m => m === $("#model").value || m === $("#model").value+":latest");
      $("#modelHint").textContent = has ? "本机已有该模型，可直接进入中台。"
        : "Ollama 已就绪，点「开始下载」拉取模型（约 2.4 GB，可断点续传）。";
      $("#doneBtn").disabled = false;
    }else{
      $("#modelHint").textContent = "未检测到运行中的 Ollama。可先装好 Ollama 再回来下载；" +
        "不使用对话功能的话，直接点「跳过」继续。";
      $("#doneBtn").disabled = false;
    }
  }catch(e){
    row.appendChild(tag("检测失败：" + e.message, "bad"));
    $("#doneBtn").disabled = false;
  }
}

async function startPull(){
  const model = $("#model").value.trim();
  if(!model) return;
  $("#pullBtn").disabled = true;
  $("#pullLog").style.display = "block";
  $("#pullLog").textContent = "";
  try{
    const r = await api("/welcome/pull", {model});
    if(!r.started){ $("#pullLog").textContent = r.reason || "未能启动"; $("#pullBtn").disabled=false; return; }
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
    if(s.ok === true){ $("#modelHint").textContent = "模型已就绪。"; }
    else if(s.ok === false){ $("#modelHint").textContent = "下载未完成：" + (s.error || "未知原因"); }
  }catch(e){
    $("#pullBtn").disabled = false;
  }
}

async function finish(){
  try{ await api("/welcome/finish", {}); }catch(e){}
  location.href = "/?k=" + encodeURIComponent(TOKEN);
}

$("#pullBtn").addEventListener("click", startPull);
$("#skipBtn").addEventListener("click", () => {
  $("#modelHint").textContent = "已跳过。之后可在中台左侧「设置 → 模型」里再配置。";
  $("#doneBtn").disabled = false;
});
$("#doneBtn").addEventListener("click", finish);
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
