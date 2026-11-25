"""设置抽屉里的「对话模型」入口 —— 注入，不改 dash.html。

为什么用注入（而不是像其他设置组那样写进 dash.html）：
  1. dash.html 是并行开发里最热的那个文件 —— 换入口、改卡片、调布局都动它，
     一次改动动辄上千行。设置这东西塞进去，每次和别人撞；
  2. 注入对任何版本的 dash.html 都生效，包括用户手改过、页面戳对不上的那份
     （main 现在会按页内戳自动换新页面，换新就会覆盖用户手改，注入不会被盖）；
  3. 和 pluginui / about 的做法一致，上线节奏不用等页面发布。

三件事一起注入：一段 style、一个 <template>、一段 IIFE。
  template 是为了不把一大坨 HTML 塞进 JS 字符串里；
  IIFE 负责把它插进抽屉（「AI 历史账」那组之前），并绑定交互。
"""
from __future__ import annotations

_CSS = """<style>
/* 抽屉里原本只有开关和按钮，没有能填东西的地方 */
#mosh-mdl-group .set-in{
  width:100%;margin:6px 0 0;padding:7px 10px;border-radius:8px;
  border:1px solid var(--line);background:var(--bg);color:var(--tx);
  font-family:inherit;font-size:12.5px;line-height:1.4;
}
#mosh-mdl-group .set-in:focus{outline:0;border-color:var(--pri-line);background:var(--card)}
/* 原生箭头在深色皮肤下发白、跟背景糊在一起，自己画一个跟着 currentColor 的 */
#mosh-mdl-group select.set-in{
  appearance:none;-webkit-appearance:none;padding-right:26px;
  background-image:linear-gradient(45deg,transparent 50%,currentColor 50%),
                   linear-gradient(135deg,currentColor 50%,transparent 50%);
  background-position:calc(100% - 15px) 52%,calc(100% - 10px) 52%;
  background-size:5px 5px,5px 5px;background-repeat:no-repeat;
}
#mosh-mdl-group .set-note{font-size:11px;color:var(--tx3);line-height:1.5;margin-top:6px;
  white-space:pre-wrap;word-break:break-all}
</style>"""

_TPL = """<template id="mosh-mdl-tpl">
  <div class="set-grp" id="mosh-mdl-group">
    <div class="set-t">对话模型</div>
    <div class="set-row">
      <div class="sl"><b id="moshMdlNow">—</b><span id="moshMdlSub">正在读模型清单…</span></div>
      <button class="btn-line" id="moshMdlReload">重新探测</button>
    </div>
    <div class="set-row set-col">
      <div class="sl"><b>这台机上已有的</b><span>选一条点「切换」，立刻生效，不用重启</span></div>
      <select class="set-in" id="moshMdlPick"></select>
    </div>
    <div class="set-row">
      <div class="sl"><b>用这个</b><span>写进配置的 model / gateway_base</span></div>
      <button class="btn-line" id="moshMdlUse">切换</button>
    </div>
    <div class="set-row set-col">
      <div class="sl"><b>接别的端点</b><span>任何 OpenAI 兼容地址；带不带 /v1 都行。本机 Ollama 填 127.0.0.1:11434 即可</span></div>
      <input type="text" class="set-in" id="moshMdlBase" placeholder="https://api.deepseek.com/v1">
      <input type="text" class="set-in" id="moshMdlKey" placeholder="API Key（本机服务留空；已存的不回显）">
      <input type="text" class="set-in" id="moshMdlName" placeholder="模型名，如 deepseek-chat / qwen3.5:9b">
      <div class="set-row" style="padding:8px 0 2px">
        <button class="btn-line" id="moshMdlTest">测试连接</button>
        <button class="btn-line" id="moshMdlSave">保存并使用</button>
      </div>
      <div class="set-note" id="moshMdlNote"></div>
    </div>
  </div>
</template>"""

_JS = """<script>(function(){
  if (document.getElementById('mosh-mdl-group')) return;
  var TPL = document.getElementById('mosh-mdl-tpl');
  var drawer = document.getElementById('drawer');
  if (!TPL || !drawer || !TPL.content) return;
  var box = drawer.querySelector('.dr-b');
  if (!box) return;

  var node = TPL.content.firstElementChild.cloneNode(true);
  /* 落在「AI 历史账」那组之前；用户那份 dash.html 里没有这组就放最后一组前，
     都没有就 append —— 三个版本都插得进去，不会因为锚点不存在就静默失效 */
  var grps = [].slice.call(box.children).filter(function(el){
    return el.classList && el.classList.contains('set-grp');
  });
  var anchor = null;
  for (var i = 0; i < grps.length; i++) {
    var t = grps[i].querySelector('.set-t');
    if (t && t.textContent.indexOf('AI 历史账') >= 0) { anchor = grps[i]; break; }
  }
  if (!anchor && grps.length) anchor = grps[grps.length - 1];
  box.insertBefore(node, anchor);

  var $ = function(id){ return document.getElementById(id); };
  var TK = (typeof TOKEN !== 'undefined' && TOKEN) || '';
  if (!TK) {
    try { TK = new URLSearchParams(location.search).get('k') || ''; } catch (e) { }
  }

  function call(path, opt){
    opt = opt || {};
    var h = { 'x-token': TK };
    if (opt.body) h['Content-Type'] = 'application/json';
    return fetch(path, {method: opt.method || 'GET', headers: h,
                        body: opt.body, cache: 'no-store'})
      .then(function(r){
        if (!r.ok) throw new Error(path + ' -> ' + r.status);
        return r.json();
      });
  }

  function say(msg){ var el = $('moshMdlNote'); if (el) el.textContent = msg || ''; }

  function paint(cur){
    $('moshMdlNow').textContent = cur.model || '还没配';
    $('moshMdlSub').textContent = (cur.base || '—')
      + (cur.key_set ? '（密钥已存 ' + cur.key_mask + '）' : '');
  }

  function save(body){
    return call('/api/model/save', {method: 'POST', body: JSON.stringify(body)})
      .then(function(r){
        if (r && r.ok === false) throw new Error(r.error || '保存被拒');
        paint(r.current || {});
        return r.current || {};
      });
  }

  function load(){
    var sub = $('moshMdlSub');
    sub.textContent = '正在读模型清单…';
    return call('/api/model').then(function(s){
      var cur = s.current || {};
      var nb = function(x){ return String(x || '').replace(/\\/+$/, ''); };
      paint(cur);
      var pick = $('moshMdlPick');
      var keep = pick.value;
      pick.innerHTML = '';
      var n = 0;
      (s.sources || []).forEach(function(src){
        (src.models || []).forEach(function(m){
          var o = document.createElement('option');
          o.value = [src.kind, src.base, m].join('\\u0001');
          o.textContent = src.name + ' · ' + m
            + (src.kind === 'gateway' ? '（可调工具）' : '');
          if (cur.base && nb(src.base) === nb(cur.base) && m === cur.model) o.selected = true;
          pick.appendChild(o); n++;
        });
      });
      if (!n) {
        var o = document.createElement('option');
        o.value = ''; o.textContent = '没探到本机的模型 —— 在下面填地址接一个';
        pick.appendChild(o);
      }
      /* 重新探测时留住刚选的那条，别一手抖跳回第一个 */
      if (keep) {
        [].slice.call(pick.options).forEach(function(o){
          if (o.value === keep) pick.value = keep;
        });
      }
      $('moshMdlBase').value = cur.base || '';
      if (cur.model) $('moshMdlName').value = cur.model;
    }).catch(function(e){
      sub.textContent = '读失败：' + e.message;
    });
  }

  $('moshMdlReload').addEventListener('click', load);

  $('moshMdlUse').addEventListener('click', function(){
    var v = $('moshMdlPick').value;
    if (!v) { say('没有可选的模型'); return; }
    var p = v.split('\\u0001');
    save({kind: p[0], base: p[1], model: p[2]})
      .then(function(){ say('已切换'); })
      .catch(function(e){ say('失败：' + e.message); });
  });

  $('moshMdlTest').addEventListener('click', function(){
    say('探测中…');
    var body = JSON.stringify({base: $('moshMdlBase').value.trim(),
                               key: $('moshMdlKey').value.trim()});
    /* 探测要连外面，给足 15 秒；页面的默认超时会先掐断 */
    var ctl = null, to = 0;
    if (typeof AbortController === 'function') {
      ctl = new AbortController();
      to = setTimeout(function(){ ctl.abort(); }, 15000);
    }
    fetch('/api/model/test', {method: 'POST', body: body, cache: 'no-store',
                              signal: ctl ? ctl.signal : undefined,
                              headers: {'x-token': TK, 'Content-Type': 'application/json'}})
      .then(function(r){ return r.json(); })
      .then(function(r){
        if (to) clearTimeout(to);
        if (!r.ok) { say('不通：' + (r.reason || '未知原因')); return; }
        var ms = r.models || [];
        say('通了，' + ms.length + ' 个模型'
          + (ms.length ? '：' + ms.slice(0, 6).join('、') + (ms.length > 6 ? ' 等' : '') : ''));
        if (!ms.length) return;
        if (!$('moshMdlName').value.trim()) $('moshMdlName').value = ms[0];
      })
      .catch(function(e){ say('失败：' + e.message); });
  });

  $('moshMdlSave').addEventListener('click', function(){
    var base = $('moshMdlBase').value.trim();
    var model = $('moshMdlName').value.trim();
    if (!base || !model) { say('地址和模型名都要填'); return; }
    save({kind: 'openai', base: base, model: model, key: $('moshMdlKey').value.trim()})
      .then(function(){ say('已保存并生效'); })
      .catch(function(e){ say('失败：' + e.message); });
  });

  /* 抽屉第一次打开时才探 —— 三个端点最坏要 2.5 秒，不该拖慢页面首屏 */
  var loaded = false;
  function ensure(){ if (loaded) return; loaded = true; load(); }
  if (typeof MutationObserver === 'function') {
    new MutationObserver(function(){ if (!drawer.hidden) ensure(); })
      .observe(drawer, {attributes: true, attributeFilter: ['hidden']});
  }
  document.addEventListener('click', function(e){
    var t = e.target;
    if (t && t.closest && t.closest('#btnSettings')) ensure();
  }, true);
  if (!drawer.hidden) ensure();
})();</script>"""


def inject(html: str) -> str:
    """把模型设置拼进中台页面。已经拼过就不重复拼。

    必须拼在**最后一个** `</body>` 前：dash.html 里有一句
    `document.write('…<body …>正在启动…')`，第一个 </body> 落在 JS 字符串中间，
    按第一个拼会把整段塞进引号里，整页脚本 SyntaxError（about.py 里踩过）。
    口袋首页（/）没有设置抽屉，脚本自己会判 drawer 不存在然后什么都不做。
    """
    if "mosh-mdl-group" in html or "moshMdlPick" in html:
        return html
    blob = _CSS + _TPL + _JS
    i = html.rfind("</body>")
    if i >= 0:
        return html[:i] + blob + html[i:]
    return html + blob
