"""装插件 / 更新提示 —— 注入到工作台页面的一段 UI。

为什么走「服务端注入」，不改 `_工作台/dash.html`
    1. dash.html 是用户自己的文件，98 万字节、一万八千行，每加一个按钮都在里面
       挪动别人写的东西，改错一次整个工作台白屏；
    2. about.py 已经证明了这条路可行：`inject()` 在 </body> 前拼一段，页面本身
       一个字节不动，脚本坏了也只是「角落里那个按钮不见了」；
    3. 这段东西想改就改 py 文件，刷新页面即生效，不用等下次构建。

它做什么
    · 右下角一颗「插件」胶囊：点开是抽屉，里面三页（装插件 / 已装 / 更新）；
    · **拖到窗口任意位置**松手就装：.moshanplug、.zip、整个插件文件夹都认；
    · 「选文件」「选文件夹」「从链接装」三条路，对应包 / 目录 / 让中台自己去下；
    · 有任何更新（本体或插件）时胶囊变成陶土色并报数，不弹窗打断。

和 dash.html 的边界
    这里只碰两件事：读它的全局 `TOKEN`（拿不到就退到 ?k= / localStorage），
    以及装完调一次它的 `loadAll()`（拿不到就提示用户刷新）。别的全局一概不摸，
    所有命名都带 mpk 前缀，包在 IIFE 里。
"""
from __future__ import annotations

CSS = r"""
<style id="mpk-css">
/* 全部走工作台自己的变量：换皮肤时这里不用跟着改 */
/* 右下角这颗撤了（2026-11-25）：入口并到顶栏「＋插件」。节点留着 —— 下面的脚本还在
   往 #mpk-launch-t 写更新数、切 .hot，删节点会让那几行抛错。 */
#mpk-launch{display:none!important}
#mpk-launch{position:fixed;right:16px;bottom:18px;z-index:9996;display:flex;align-items:center;gap:7px;
  height:34px;padding:0 13px;border-radius:99px;border:1px solid var(--line,#e5e8ef);
  background:var(--card,#fff);color:var(--tx2,#565f6e);cursor:pointer;
  font:12.5px/1 -apple-system,'PingFang SC',system-ui,sans-serif;
  box-shadow:var(--sh2,0 2px 5px rgba(17,20,32,.06));transition:.16s}
#mpk-launch:hover{color:var(--tx,#111420);border-color:var(--pri-line,#dcdcfb);
  box-shadow:0 4px 14px -6px rgba(17,20,32,.22)}
#mpk-launch svg{width:15px;height:15px;flex:none}
#mpk-launch .n{font-variant-numeric:tabular-nums}
#mpk-launch.hot{background:#a8641c;border-color:#a8641c;color:#fff}
#mpk-launch.hot:hover{background:#93581a;color:#fff}

#mpk-veil{position:fixed;inset:0;z-index:9995;display:none;place-items:center;
  background:rgba(20,20,18,.42);backdrop-filter:blur(2px);
  font:14px/1.6 -apple-system,'PingFang SC',system-ui,sans-serif}
#mpk-veil.on{display:grid}
#mpk-veil .box{padding:26px 34px;border-radius:16px;border:2px dashed rgba(255,255,255,.55);
  color:#fff;text-align:center;background:rgba(255,255,255,.07)}
#mpk-veil b{display:block;font-size:17px;font-weight:600;margin-bottom:4px}
#mpk-veil span{opacity:.82;font-size:12.5px}

#mpk-scrim{position:fixed;inset:0;z-index:9997;background:rgba(17,20,32,.34);
  opacity:0;pointer-events:none;transition:.2s}
#mpk-scrim.on{opacity:1;pointer-events:auto}
#mpk-panel{position:fixed;z-index:9998;right:16px;top:56px;width:min(432px,calc(100vw - 24px));
  max-height:min(640px,calc(100vh - 120px));display:flex;flex-direction:column;
  border-radius:16px;border:1px solid var(--line,#e5e8ef);background:var(--card,#fff);
  box-shadow:0 18px 48px -18px rgba(17,20,32,.34);overflow:hidden;
  opacity:0;transform:translateY(10px) scale(.985);pointer-events:none;transition:.2s;
  font:13px/1.5 -apple-system,'PingFang SC',system-ui,sans-serif;color:var(--tx,#111420)}
#mpk-panel.on{opacity:1;transform:none;pointer-events:auto}
#mpk-panel .hd{display:flex;align-items:center;gap:8px;padding:13px 14px 0}
#mpk-panel .hd h3{margin:0;font-size:14px;font-weight:650;letter-spacing:.01em}
#mpk-panel .hd .sp{flex:1}
#mpk-panel .x{width:26px;height:26px;border:0;border-radius:8px;background:transparent;
  color:var(--tx3,#7c8698);cursor:pointer;display:grid;place-items:center}
#mpk-panel .x:hover{background:var(--chip,#f1f3f8);color:var(--tx,#111420)}
#mpk-panel .x svg{width:14px;height:14px}
#mpk-tabs{display:flex;gap:2px;padding:10px 12px 0}
#mpk-tabs button{flex:1;border:0;background:transparent;padding:7px 6px;border-radius:8px 8px 0 0;
  color:var(--tx3,#7c8698);font:12.5px/1 inherit;cursor:pointer;border-bottom:2px solid transparent}
#mpk-tabs button.on{color:var(--pri,#4f46e5);border-bottom-color:var(--pri,#4f46e5);font-weight:600}
#mpk-tabs button b{font-variant-numeric:tabular-nums}
#mpk-body{flex:1;overflow:auto;padding:12px 14px 14px;border-top:1px solid var(--line2,#eef0f6)}
#mpk-body::-webkit-scrollbar{width:8px}
#mpk-body::-webkit-scrollbar-thumb{background:var(--line,#e5e8ef);border-radius:8px}

.mpk-drop{border:1.5px dashed var(--line,#e5e8ef);border-radius:12px;background:var(--card2,#f8f9fc);
  padding:20px 14px;text-align:center;cursor:pointer;transition:.15s}
.mpk-drop:hover,.mpk-drop.over{border-color:var(--pri,#4f46e5);background:var(--pri-soft,#eef0fe)}
.mpk-drop b{display:block;font-size:13.5px;font-weight:600;margin-bottom:3px}
.mpk-drop span{font-size:11.5px;color:var(--tx3,#7c8698);line-height:1.7}
.mpk-row{display:flex;gap:8px;margin-top:10px}
.mpk-b{flex:1;height:32px;border-radius:9px;border:1px solid var(--line,#e5e8ef);background:var(--card,#fff);
  color:var(--tx2,#565f6e);font:12.5px/1 inherit;cursor:pointer;display:flex;align-items:center;
  justify-content:center;gap:5px;transition:.14s}
.mpk-b:hover{border-color:var(--pri-line,#dcdcfb);color:var(--tx,#111420);background:var(--card2,#f8f9fc)}
.mpk-b.pri{background:var(--pri,#4f46e5);border-color:var(--pri,#4f46e5);color:#fff;font-weight:600}
.mpk-b.pri:hover{filter:brightness(1.06);color:#fff}
.mpk-b[disabled]{opacity:.5;cursor:not-allowed}
.mpk-b svg{width:14px;height:14px}
.mpk-url{display:flex;gap:6px;margin-top:10px}
.mpk-in{flex:1;height:32px;padding:0 10px;border-radius:9px;border:1px solid var(--line,#e5e8ef);
  background:var(--card,#fff);color:var(--tx,#111420);font:12.5px/1 inherit;outline:none;min-width:0}
.mpk-in:focus{border-color:var(--pri,#4f46e5);box-shadow:0 0 0 3px var(--pri-soft,#eef0fe)}
.mpk-hint{margin-top:9px;font-size:11px;color:var(--tx3,#7c8698);line-height:1.75}
.mpk-hint code{font-size:10.5px;padding:1px 4px;border-radius:4px;background:var(--chip,#f1f3f8)}
.mpk-log{margin-top:10px;border-radius:10px;padding:9px 11px;font-size:11.5px;line-height:1.7;
  background:var(--card2,#f8f9fc);border:1px solid var(--line2,#eef0f6);display:none;white-space:pre-wrap;
  word-break:break-word}
.mpk-log.on{display:block}
.mpk-log.ok{background:var(--ok-soft,#e7f7f0);border-color:#bfe9d7;color:#0a6f4d}
.mpk-log.bad{background:var(--bad-soft,#fdecea);border-color:#f6c9c4;color:#a51d12}
.mpk-log .w{display:block;margin-top:4px;color:var(--warn,#c07908)}
.mpk-bar{height:3px;border-radius:3px;background:var(--line2,#eef0f6);overflow:hidden;margin-top:9px;display:none}
.mpk-bar.on{display:block}
.mpk-bar i{display:block;height:100%;width:0;background:var(--pri,#4f46e5);transition:.15s}

.mpk-list{margin-top:4px}
.mpk-li{display:flex;align-items:center;gap:9px;padding:9px 2px;border-bottom:1px solid var(--line2,#eef0f6)}
.mpk-li:last-child{border-bottom:0}
.mpk-li .ic{width:26px;height:26px;border-radius:8px;background:var(--chip,#f1f3f8);flex:none;
  display:grid;place-items:center;font-size:11px;color:var(--tx2,#565f6e);font-weight:600}
.mpk-li .mn{flex:1;min-width:0}
.mpk-li .nm{display:block;font-size:12.5px;font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.mpk-li .sub{display:block;font-size:10.5px;color:var(--tx3,#7c8698);white-space:nowrap;
  overflow:hidden;text-overflow:ellipsis;font-variant-numeric:tabular-nums}
.mpk-li .tag{font-size:10px;padding:2px 6px;border-radius:6px;background:var(--chip,#f1f3f8);
  color:var(--tx3,#7c8698);flex:none;font-variant-numeric:tabular-nums}
.mpk-li .tag.new{background:#fbeee0;color:#a8641c;font-weight:600}
.mpk-li .b{height:26px;padding:0 9px;border-radius:7px;border:1px solid var(--line,#e5e8ef);
  background:var(--card,#fff);color:var(--tx2,#565f6e);font:11.5px/1 inherit;cursor:pointer;flex:none}
.mpk-li .b:hover{border-color:var(--pri-line,#dcdcfb);color:var(--tx,#111420)}
.mpk-li .b.danger:hover{border-color:#f6c9c4;color:#a51d12;background:var(--bad-soft,#fdecea)}
.mpk-empty{padding:22px 8px;text-align:center;color:var(--tx3,#7c8698);font-size:12px;line-height:1.8}
.mpk-sec{margin:6px 0 2px;font-size:10.5px;letter-spacing:.06em;color:var(--tx3,#7c8698);text-transform:uppercase}
.mpk-card{border:1px solid var(--line,#e5e8ef);border-radius:11px;padding:10px 11px;margin-top:8px;
  background:var(--card2,#f8f9fc)}
.mpk-card .t{display:flex;align-items:center;gap:6px;font-size:12.5px;font-weight:600}
.mpk-card .t .sp{flex:1}
.mpk-card .d{font-size:11px;color:var(--tx3,#7c8698);margin-top:3px;line-height:1.65}
.mpk-card .a{display:flex;gap:6px;margin-top:8px}
@media (max-width:560px){
  #mpk-panel{right:8px;left:8px;width:auto;bottom:56px;max-height:calc(100vh - 80px)}
  #mpk-launch{right:10px;bottom:12px}
}
</style>
"""

HTML = r"""
<div id="mpk-scrim"></div>
<div id="mpk-panel" role="dialog" aria-label="插件">
  <div class="hd">
    <h3>插件</h3>
    <span class="sp"></span>
    <button class="x" id="mpk-x" title="关闭" aria-label="关闭">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round">
        <path d="M6 6l12 12M18 6L6 18"/></svg>
    </button>
  </div>
  <div id="mpk-tabs">
    <button type="button" data-t="install" class="on">装插件</button>
    <button type="button" data-t="have">已装 <b id="mpk-n-have">0</b></button>
    <button type="button" data-t="upd">更新 <b id="mpk-n-upd">0</b></button>
  </div>
  <div id="mpk-body">
    <!-- ① 装插件：三条路 -->
    <div data-p="install">
      <div class="mpk-drop" id="mpk-drop">
        <b>把插件包拖到这里</b>
        <span>.moshanplug / .zip 都行；整个插件文件夹也能拖<br>也可以直接拖到窗口任意位置</span>
      </div>
      <div class="mpk-row">
        <button class="mpk-b" id="mpk-pick-zip">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 16V4m0 0L7 9m5-5l5 5"/><path d="M4 17v2a1 1 0 001 1h14a1 1 0 001-1v-2"/></svg>
          选文件</button>
        <button class="mpk-b" id="mpk-pick-dir">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
            <path d="M3 7a1 1 0 011-1h5l2 2h8a1 1 0 011 1v9a1 1 0 01-1 1H4a1 1 0 01-1-1z"/></svg>
          选文件夹</button>
      </div>
      <div class="mpk-url">
        <input class="mpk-in" id="mpk-url" type="text" autocomplete="off" spellcheck="false"
               placeholder="https://…/llmcost.moshanplug">
        <button class="mpk-b pri" id="mpk-url-go" style="flex:none;padding:0 12px">去下载</button>
      </div>
      <div class="mpk-row" style="margin-top:8px">
        <button class="mpk-b" id="mpk-manual" title="服务已经在本机跑着了，只想在顶栏登记一行">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round">
            <path d="M12 5v14M5 12h14"/></svg>
          手动填写名字 / 端口</button>
      </div>
      <div class="mpk-hint">
        包里要有 <code>plugin.json</code>（id / name / version）。装完进
        <code>_插件/&lt;id&gt;/</code>，并登记到 <code>_工作台/plugins.json</code>；
        数据目录、别的插件的行都不动。
      </div>
      <div class="mpk-bar" id="mpk-bar"><i></i></div>
      <div class="mpk-log" id="mpk-log"></div>
      <input type="file" id="mpk-f-zip" accept=".moshanplug,.zip" multiple hidden>
      <input type="file" id="mpk-f-dir" webkitdirectory directory multiple hidden>
    </div>

    <!-- ② 已装 -->
    <div data-p="have" hidden>
      <div class="mpk-list" id="mpk-have"></div>
    </div>

    <!-- ③ 更新 -->
    <div data-p="upd" hidden>
      <div id="mpk-self"></div>
      <div class="mpk-sec" style="margin-top:14px">插件</div>
      <div class="mpk-list" id="mpk-upd"></div>
      <div class="mpk-row" style="margin-top:12px">
        <button class="mpk-b" id="mpk-recheck">重新检查</button>
      </div>
      <div class="mpk-hint" id="mpk-upd-hint">插件更新看包里的 version；本体更新看 GitHub Release。</div>
    </div>
  </div>
</div>

<button id="mpk-launch" title="装插件 / 看更新">
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round">
    <path d="M10 3h4v3.2a2 2 0 002.8 1.9l2.4-1.2 2 3.4-2.4 1.4a2 2 0 000 3.5l2.4 1.4-2 3.4-2.4-1.2a2 2 0 00-2.8 1.9V21h-4v-3.2a2 2 0 00-2.8-1.9l-2.4 1.2-2-3.4 2.4-1.4a2 2 0 000-3.5L3.8 7.4l2-3.4 2.4 1.2A2 2 0 0010 3.2z"/></svg>
  <span class="n" id="mpk-launch-t">插件</span>
</button>
<div id="mpk-veil"><div class="box"><b>松手就装</b><span>插件包 / 插件文件夹</span></div></div>
"""

JS = r"""
<script id="mpk-js">
(function () {
  'use strict';
  if (window.__mpkLoaded) return;
  window.__mpkLoaded = true;

  var $ = function (id) { return document.getElementById(id); };
  var PANEL = $('mpk-panel'), SCRIM = $('mpk-scrim'), VEIL = $('mpk-veil');

  /* ---------- 令牌：只读不写 ----------
     dash.html 里 TOKEN 是顶层 const，同页后续脚本能看见；看不见就退回 ?k= /
     localStorage。三种都拿不到就什么都不做（本地一般不会发生）。 */
  function token() {
    try { if (typeof TOKEN === 'string' && TOKEN) return TOKEN; } catch (e) { }
    try {
      var q = new URLSearchParams(location.search).get('k');
      if (q) return q;
    } catch (e) { }
    try { return localStorage.getItem('pocket_token') || ''; } catch (e) { return ''; }
  }

  function api(path, opt) {
    opt = opt || {};
    var h = Object.assign({ 'x-token': token() }, opt.headers || {});
    if (opt.body && !(opt.body instanceof FormData)) h['Content-Type'] = 'application/json';
    return fetch(path, Object.assign({}, opt, { headers: h, cache: 'no-store' }))
      .then(function (r) {
        return r.json().catch(function () { return { ok: false, error: 'HTTP ' + r.status }; })
          .then(function (d) { if (!r.ok && d.ok !== false) d.ok = false; return d; });
      });
  }

  function say(msg, kind) {
    try { if (typeof toast === 'function') { toast(msg); return; } } catch (e) { }
    console.log('[插件]', msg, kind || '');
  }

  /* ---------- 抽屉 ---------- */
  var TAB = 'install';
  function open(tab) {
    if (tab) setTab(tab);
    SCRIM.classList.add('on'); PANEL.classList.add('on');
    refresh();
  }
  function close() { PANEL.classList.remove('on'); SCRIM.classList.remove('on'); }
  function setTab(t) {
    TAB = t;
    [].forEach.call(document.querySelectorAll('#mpk-tabs button'), function (b) {
      b.classList.toggle('on', b.dataset.t === t);
    });
    [].forEach.call(document.querySelectorAll('#mpk-body > div[data-p]'), function (p) {
      p.hidden = p.dataset.p !== t;
    });
  }
  document.querySelectorAll('#mpk-tabs button').forEach(function (b) {
    b.addEventListener('click', function () { setTab(b.dataset.t); });
  });
  $('mpk-launch').addEventListener('click', function () { open(TAB); });
  $('mpk-x').addEventListener('click', close);
  SCRIM.addEventListener('click', close);
  document.addEventListener('keydown', function (e) {
    if (e.key === 'Escape' && PANEL.classList.contains('on')) close();
  });

  /* ---------- 顶栏「＋插件」接管（2026-11-25） ----------
     以前两处入口：顶栏「＋插件」开 dash 自己那个三 tab 弹窗（填名字/端口），
     右下角胶囊开这个面板（装包/已装/更新）。同一件事摆两个门，用户找不到到底
     点哪个。现在只留顶栏一处：点「＋插件」直接开这个面板，面板「装插件」页里
     多一个「手动填写」按钮跳回原来那个表单（给已经在跑、只想登记一行的服务）。
     用 capture 拦：dash 那边 #appBar 的冒泡监听会先开自己的弹窗，拦不住。 */
  document.addEventListener('click', function (e) {
    var t = e.target && e.target.closest ? e.target.closest('#btnAddApp') : null;
    if (!t) return;
    e.preventDefault();
    e.stopImmediatePropagation();
    open('install');
  }, true);

  /* 面板里的「手动填写」→ 原来的表单（openAppModal 在 dash.html 里是全局函数）。 */
  document.addEventListener('click', function (e) {
    if (!e.target || !e.target.closest || !e.target.closest('#mpk-manual')) return;
    close();
    if (typeof window.openAppModal === 'function') window.openAppModal('plugin');
    else say('这个版本没有手动填写表单，用拖入或选文件装包');
  });

  /* ---------- 结果区 ---------- */
  function log(text, kind, warns) {
    var el = $('mpk-log');
    el.className = 'mpk-log on' + (kind ? ' ' + kind : '');
    el.textContent = text;
    (warns || []).forEach(function (w) {
      var s = document.createElement('span');
      s.className = 'w'; s.textContent = '· ' + w; el.appendChild(s);
    });
  }
  function busy(on) {
    $('mpk-pick-zip').disabled = on; $('mpk-pick-dir').disabled = on;
    $('mpk-url-go').disabled = on;
    $('mpk-bar').classList.toggle('on', on);
    if (!on) $('mpk-bar').firstElementChild.style.width = '0';
  }
  function progress(p) { $('mpk-bar').firstElementChild.style.width = Math.round(p * 100) + '%'; }

  /* ---------- 上传（XHR：要进度条）---------- */
  function upload(url, fd) {
    return new Promise(function (res, rej) {
      var x = new XMLHttpRequest();
      x.open('POST', url);
      x.setRequestHeader('x-token', token());
      x.upload.onprogress = function (e) { if (e.lengthComputable) progress(e.loaded / e.total); };
      x.onload = function () {
        var d = {};
        try { d = JSON.parse(x.responseText); } catch (e) { d = { ok: false, error: '返回不是 JSON' }; }
        if (!x.status || x.status >= 400) d.ok = false;
        res(d);
      };
      x.onerror = function () { rej(new Error('网络断了')); };
      x.send(fd);
    });
  }

  function afterInstall(r) {
    busy(false);
    if (!r.ok) { log(r.error || '装不上', 'bad'); return; }
    var p = r.plugin || {};
    var head = (r.upgraded ? '已更新 ' : '已装 ') + (p.name || p.id) + ' v' + (p.version || '');
    if (r.data_dir) head += '\n数据目录：' + r.data_dir;
    head += '\n程序目录：' + (r.dir || '');
    log(head, 'ok', r.warnings);
    say(head.split('\n')[0]);
    loadHave();
    // 让工作台顶栏立刻长出新药丸；拿不到 loadAll 就让用户刷新
    try {
      if (typeof loadAll === 'function') { loadAll(); }
      else { $('mpk-log').appendChild(note('顶栏要刷新一下才会出现。')); }
    } catch (e) { }
  }

  function note(t) {
    var s = document.createElement('span'); s.className = 'w'; s.textContent = t; return s;
  }

  /* ---------- 装：单包 ---------- */
  function installZip(file) {
    if (!file) return;
    busy(true);
    log('正在装 ' + file.name + '（' + (file.size / 1048576).toFixed(1) + ' MB）…');
    var fd = new FormData();
    fd.append('file', file, file.name);
    upload('/api/pack/install', fd).then(afterInstall).catch(function (e) {
      busy(false); log('装不上：' + e.message, 'bad');
    });
  }

  /* ---------- 装：文件夹 / 一堆文件 ---------- */
  function installFiles(items) {
    // items: [{path, file}]
    if (!items || !items.length) return;
    var total = items.reduce(function (a, x) { return a + x.file.size; }, 0);
    if (total > 200 * 1024 * 1024) {
      log('这些文件一共 ' + (total / 1048576).toFixed(0) + ' MB，太大了（上限 200 MB）。先打成 .moshanplug 再装。', 'bad');
      return;
    }
    busy(true);
    log('正在装 ' + items.length + ' 个文件（' + (total / 1048576).toFixed(1) + ' MB）…');
    var fd = new FormData();
    items.forEach(function (x) { fd.append('files', x.file, x.file.name); });
    fd.append('paths', JSON.stringify(items.map(function (x) { return x.path; })));
    upload('/api/pack/install-files', fd).then(afterInstall).catch(function (e) {
      busy(false); log('装不上：' + e.message, 'bad');
    });
  }

  /* ---------- 拖拽：整个窗口都能接 ---------- */
  var dragDepth = 0;
  function hasFiles(e) {
    var t = e.dataTransfer && e.dataTransfer.types;
    return !!t && [].indexOf.call(t, 'Files') >= 0;
  }
  window.addEventListener('dragenter', function (e) {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragDepth++;
    VEIL.classList.add('on');
  });
  window.addEventListener('dragover', function (e) {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  });
  window.addEventListener('dragleave', function (e) {
    if (!hasFiles(e)) return;
    dragDepth = Math.max(0, dragDepth - 1);
    if (!dragDepth) VEIL.classList.remove('on');
  });
  window.addEventListener('drop', function (e) {
    if (!hasFiles(e)) return;
    e.preventDefault();
    dragDepth = 0; VEIL.classList.remove('on');
    open('install');
    var dt = e.dataTransfer;

    // 文件夹：用 entry API 递归（浏览器不给「文件夹」这个东西，只能一层层读）
    var entries = [];
    if (dt.items && dt.items.length && dt.items[0].webkitGetAsEntry) {
      for (var i = 0; i < dt.items.length; i++) {
        var en = dt.items[i].webkitGetAsEntry && dt.items[i].webkitGetAsEntry();
        if (en) entries.push(en);
      }
    }
    if (entries.length) {
      var dirs = entries.filter(function (x) { return x.isDirectory; });
      if (dirs.length) {
        log('正在读文件夹…');
        walk(dirs[0]).then(function (items) {
          if (!items.length) { log('这个文件夹里没有文件', 'bad'); return; }
          installFiles(items);
        }).catch(function (err) { log('读文件夹失败：' + err.message, 'bad'); });
        return;
      }
    }
    var files = [].slice.call(dt.files || []);
    if (!files.length) { log('没拿到文件', 'bad'); return; }
    var zips = files.filter(function (f) { return /\.(moshanplug|zip)$/i.test(f.name); });
    if (zips.length) { zips.forEach(installZip); return; }
    // 不是 zip：当成「一堆散文件」按相对路径装（用户拖的是解开的插件目录里的东西）
    installFiles(files.map(function (f) {
      return { path: (f.webkitRelativePath || f.name), file: f };
    }));
  });

  function walk(entry, prefix) {
    prefix = prefix || '';
    return new Promise(function (res, rej) {
      if (entry.isFile) {
        entry.file(function (f) { res([{ path: prefix + entry.name, file: f }]); }, rej);
        return;
      }
      if (!entry.isDirectory) { res([]); return; }
      var reader = entry.createReader(), all = [];
      (function step() {
        reader.readEntries(function (batch) {
          if (!batch.length) {
            Promise.all(all.map(function (en) { return walk(en, prefix + entry.name + '/'); }))
              .then(function (groups) { res([].concat.apply([], groups)); }, rej);
            return;
          }
          all = all.concat([].slice.call(batch));
          step();     // readEntries 一次最多给 100 个，必须读到空
        }, rej);
      })();
    });
  }

  /* ---------- 三条入口 ---------- */
  $('mpk-drop').addEventListener('click', function () { $('mpk-f-zip').click(); });
  $('mpk-pick-zip').addEventListener('click', function () { $('mpk-f-zip').click(); });
  $('mpk-pick-dir').addEventListener('click', function () { $('mpk-f-dir').click(); });
  $('mpk-f-zip').addEventListener('change', function (e) {
    var fs = [].slice.call(e.target.files || []);
    fs.forEach(installZip);
    e.target.value = '';
  });
  $('mpk-f-dir').addEventListener('change', function (e) {
    var fs = [].slice.call(e.target.files || []);
    installFiles(fs.map(function (f) { return { path: f.webkitRelativePath || f.name, file: f }; }));
    e.target.value = '';
  });
  $('mpk-url-go').addEventListener('click', function () {
    var u = $('mpk-url').value.trim();
    if (!u) { $('mpk-url').focus(); return; }
    busy(true);
    log('正在下载 ' + u + ' …');
    api('/api/pack/install-url', { method: 'POST', body: JSON.stringify({ url: u }) })
      .then(afterInstall).catch(function (e) { busy(false); log('装不上：' + e.message, 'bad'); });
  });
  $('mpk-url').addEventListener('keydown', function (e) {
    if (e.key === 'Enter') $('mpk-url-go').click();
  });

  /* ---------- 已装 ---------- */
  function esc(s) {
    return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
      return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
    });
  }
  var HAVE = [];
  function loadHave() {
    api('/api/pack/list').then(function (d) {
      HAVE = d.plugins || [];
      $('mpk-n-have').textContent = HAVE.length;
      var box = $('mpk-have');
      if (!HAVE.length) {
        box.innerHTML = '<div class="mpk-empty">还没有用包装过插件。<br>拖一个 .moshanplug 进来试试。</div>';
        return;
      }
      box.innerHTML = HAVE.map(function (p) {
        var sub = 'v' + esc(p.version || '?') + (p.port ? ' · ' + p.port : '')
          + (p.data_default ? ' · ' + esc(p.data_default) : '');
        return '<div class="mpk-li" data-id="' + esc(p.id) + '">'
          + '<span class="ic">' + esc((p.name || p.id).slice(0, 1)) + '</span>'
          + '<span class="mn"><span class="nm">' + esc(p.name) + '</span>'
          + '<span class="sub" title="' + esc(p.dir) + '">' + sub + '</span></span>'
          + '<button class="b" data-act="uninst">卸载</button></div>';
      }).join('');
    }).catch(function () { });
  }

  $('mpk-have').addEventListener('click', function (e) {
    var b = e.target.closest('[data-act]');
    if (!b) return;
    var li = b.closest('.mpk-li');
    var id = li && li.dataset.id;
    if (!id) return;
    if (b.dataset.armed !== '1') {
      b.dataset.armed = '1'; b.textContent = '真删'; b.classList.add('danger');
      setTimeout(function () { if (b.dataset.armed === '1') { b.dataset.armed = ''; b.textContent = '卸载'; b.classList.remove('danger'); } }, 4000);
      return;
    }
    b.disabled = true;
    api('/api/pack/uninstall', { method: 'POST', body: JSON.stringify({ id: id }) })
      .then(function (r) {
        if (!r.ok) { say(r.error || '卸载失败'); b.disabled = false; return; }
        say('已卸载（数据和历史版本都留着）');
        loadHave(); refresh();
      })
      .catch(function (err) { say('卸载失败：' + err.message); b.disabled = false; });
  });

  /* ---------- 更新 ---------- */
  var UPD = { self: null, plugins: [], updates: 0 };
  function renderUpd() {
    var s = UPD.self || {};
    var box = $('mpk-self');
    var curRaw = s.current || '', cur = curRaw || '—', lat = s.latest || '';
    if (s.has_update) {
      box.innerHTML = '<div class="mpk-card"><div class="t">墨山中台 <span class="tag new">有新版本</span>'
        + '<span class="sp"></span><button class="b" id="mpk-go-about">去更新</button></div>'
        + '<div class="d">当前 ' + esc(cur) + ' → <b>' + esc(lat) + '</b>'
        + (s.platform ? ' · ' + esc(s.platform) : '') + '</div></div>';
    } else {
      // 开发版没有 about.py（更新页那个模块），所以 latest 一定是空的 ——
      // 这时候说「已是最新」是撒谎：没查过不等于查过没新版。
      var txt = !curRaw ? '这个版本没有版本号，比不了新版。'
        : (s.error ? '检查失败：' + esc(s.error)
          : (s.mode === 'push' ? '这台机器由主机推送更新'
            : (!lat ? '这个版本没有更新检查（开发版：直接 git pull）' : '已是最新')));
      box.innerHTML = '<div class="mpk-card"><div class="t">墨山中台'
        + '<span class="sp"></span><span class="tag">' + esc(curRaw || '开发版') + '</span></div>'
        + '<div class="d">' + txt + '</div></div>';
    }

    var list = (UPD.plugins || []).filter(function (p) { return true; });
    var up = list.filter(function (p) { return p.has_update; });
    var box2 = $('mpk-upd');
    if (!up.length) {
      box2.innerHTML = '<div class="mpk-empty">插件都是最新。</div>';
    } else {
      box2.innerHTML = up.map(function (p) {
        return '<div class="mpk-li" data-id="' + esc(p.id) + '">'
          + '<span class="ic">' + esc(p.name.slice(0, 1)) + '</span>'
          + '<span class="mn"><span class="nm">' + esc(p.name) + '</span>'
          + '<span class="sub">' + esc(p.version) + ' → ' + esc(p.latest)
          + (p.error ? ' · ' + esc(p.error) : '') + '</span></span>'
          + '<button class="b" data-act="up">更新</button></div>';
      }).join('');
    }
    var chk = list.filter(function (p) { return p.error; });
    if (chk.length) {
      $('mpk-upd-hint').textContent = chk.length + ' 个插件查不到新版：'
        + chk.map(function (p) { return p.name; }).join('、') + '（多半是没配 repo 或网络不通）';
    } else if (list.length) {
      var t = list[0].checked_at ? new Date(list[0].checked_at * 1000) : null;
      $('mpk-upd-hint').textContent = '插件更新看包里的 version；本体更新看 GitHub Release。'
        + (t ? '上次检查 ' + t.toLocaleString('zh-CN', { hour12: false }) : '');
    }
  }

  $('mpk-upd').addEventListener('click', function (e) {
    var b = e.target.closest('[data-act="up"]');
    if (!b) return;
    var li = b.closest('.mpk-li');
    var id = li && li.dataset.id;
    if (!id) return;
    b.disabled = true; b.textContent = '更新中';
    api('/api/pack/upgrade', { method: 'POST', body: JSON.stringify({ id: id }) })
      .then(function (r) {
        if (!r.ok) {
          b.disabled = false; b.textContent = '更新';
          // 失败原因写在这行下面，别只弹个 toast —— 关掉就忘，用户还想照着改
          $('mpk-upd-hint').textContent = '更新失败：' + (r.error || '不知道什么原因');
          say(r.error || '更新失败');
          return;
        }
        say('已更新 ' + (r.plugin ? r.plugin.name : id) + ' → v' + (r.plugin ? r.plugin.version : ''));
        // 文件换了，但正在跑的进程还是旧代码 —— 说清楚，别让人以为没生效
        $('mpk-upd-hint').textContent = '已换到 v' + (r.plugin ? r.plugin.version : '?')
          + '。它现在开着的话，重启一次才用上新代码。'
          + (r.warnings && r.warnings.length ? ' ' + r.warnings.join('；') : '');
        refresh(true);
      })
      .catch(function (err) {
        b.disabled = false; b.textContent = '更新';
        $('mpk-upd-hint').textContent = '更新失败：' + err.message;
        say('更新失败：' + err.message);
      });
  });

  document.addEventListener('click', function (e) {
    if (e.target && e.target.id === 'mpk-go-about') {
      location.href = '/about' + (token() ? '?k=' + encodeURIComponent(token()) : '');
    }
  });

  $('mpk-recheck').addEventListener('click', function () { refresh(true); });

  var announced = false;
  function paintBadge() {
    var n = (UPD.updates || 0) + ((UPD.self && UPD.self.has_update) ? 1 : 0);
    var l = $('mpk-launch'), t = $('mpk-launch-t');
    $('mpk-n-upd').textContent = (UPD.plugins || []).filter(function (p) { return p.has_update; }).length;
    if (n > 0) { l.classList.add('hot'); t.textContent = n + ' 项更新'; }
    else { l.classList.remove('hot'); t.textContent = '插件'; }
    // 第一次发现更新时说一句，之后不再吵（用户已经知道角落里那颗变颜色了）
    if (n > 0 && !announced && !PANEL.classList.contains('on')) {
      announced = true;
      say(n + ' 项更新 · 右下角「插件」里看');
    }
  }

  function refresh(force) {
    var url = '/api/updates' + (force ? '?force=1' : '');
    api(url).then(function (d) {
      if (!d || d.ok === false) return;
      UPD = d; renderUpd(); paintBadge();
    }).catch(function () { });
    if (!HAVE.length) loadHave();
  }

  /* 首次：等页面自己画完再问，别和首屏抢；之后每 30 分钟一次。
     有更新只变颜色不弹窗 —— 工作台是干活的地方，不是推送中心。 */
  setTimeout(function () { refresh(false); }, 3500);
  setInterval(function () { refresh(false); }, 30 * 60 * 1000);
})();
</script>
"""


def inject(html: str) -> str:
    """拼到 </body> 前。拼过就不再拼（同一进程多次渲染也安全）。

    **必须用最后一个 `</body>`，不能用 replace(…, 1)。** dash.html 里前两个
    `</body>` 出现在内联 JS 的字符串里（`document.write('<title>…</title><body…>
    </body>')` 那两处启动占位页），替换第一个等于把整段 UI 塞进 JS 字符串中间 ——
    浏览器报 `Invalid or unexpected token`，页面直接少一截，而且看着像「什么都没做」。
    """
    if "mpk-launch" in html:
        return html
    block = CSS + HTML + JS
    i = html.rfind("</body>")
    if i >= 0:
        return html[:i] + block + html[i:]
    return html + block
