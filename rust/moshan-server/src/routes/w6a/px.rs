// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`/px/<插件>/<路径>` —— 把插件那个 http 端口反代成同源。
// 不做：不拉起插件、不改插件返回的内容。
//
// 为什么必须反代而不是直接 iframe 那个端口：
//   1. 主页从 8321（https）进来时，iframe 一个 http:// 地址会被浏览器按混合内容拦掉；
//   2. 跨源拿不到 cookie，插件页里的相对路径 `/api/xxx` 也不会落到插件自己身上。
// 转发成同源之后，插件页里的 `/api/x` 自动变成 `/px/<插件>/api/x`，全对上。
//
// 这一条在 Python 版是 `server.py:2550` 的 `px_proxy`；Rust 重写时整条漏了，
// 后果是所有插件卡片（iframe）一律 404 —— 界面上就是「插件拉不起来」（2026-11-27）。

use crate::paths::Ctx;
use crate::routes::common::peek;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use std::sync::Arc;
use std::sync::OnceLock;

/// 反代用的 HTTP 客户端。
///
/// 刻意**不启用 gzip 自动解压**：reqwest 不解压就不会带 `Accept-Encoding`，
/// 插件也就不会压 —— 压缩流原样转给浏览器反而会把 iframe 弄成乱码。
/// 超时给 40 秒，跟 Python 版一致：插件里有些接口（扫盘、拉 GitHub）确实慢。
fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(40))
            .no_proxy()
            .build()
            .unwrap_or_default()
    })
}

pub fn router() -> Router<Arc<Ctx>> {
    // 三条：`/px/<id>`、`/px/<id>/`、`/px/<id>/<tail>`。
    // axum 的通配段不匹配空串，所以带不带尾斜杠得分开注册。
    Router::new()
        .route("/px/{pid}", any(px_root))
        .route("/px/{pid}/", any(px_root))
        .route("/px/{pid}/{*tail}", any(px_tail))
}

async fn px_root(
    state: State<Arc<Ctx>>,
    path: Path<String>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    forward(state.0, path.0, String::new(), method, headers, uri, body).await
}

async fn px_tail(
    state: State<Arc<Ctx>>,
    path: Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let (pid, tail) = path.0;
    forward(state.0, pid, tail, method, headers, uri, body).await
}

async fn forward(
    ctx: Arc<Ctx>,
    pid: String,
    tail: String,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let port = super::apps::plugin_port(&ctx, &pid);
    if port == 0 {
        return (StatusCode::NOT_FOUND, format!("没有这个插件：{pid}")).into_response();
    }
    let host = super::apps::plugin_host(&ctx, &pid);

    let mut url = format!("http://{host}:{port}/{tail}");
    // 查询串原样带过去：但要把我们自己的 `k` 去掉，插件不认识它。
    if let Some(qs) = uri.query() {
        let kept: Vec<&str> = qs
            .split('&')
            .filter(|p| !p.is_empty() && !p.starts_with("k="))
            .collect();
        if !kept.is_empty() {
            url.push('?');
            url.push_str(&kept.join("&"));
        }
    }

    let mut req = client().request(
        reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET),
        &url,
    );
    // 只透传会影响响应内容的那几个头。把 Cookie 也带上：插件页自己有会话时不能丢。
    for name in [
        header::CONTENT_TYPE,
        header::ACCEPT,
        header::COOKIE,
        header::REFERER,
        header::USER_AGENT,
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
        header::RANGE,
    ] {
        if let Some(v) = headers.get(&name) {
            if let Ok(s) = v.to_str() {
                req = req.header(name.as_str(), s);
            }
        }
    }
    if !body.is_empty() || matches!(method, Method::POST | Method::PUT | Method::PATCH) {
        req = req.body(body.to_vec());
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("插件没响应（{pid}）：{e}"),
            )
                .into_response()
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::OK);
    // 先把头取出来，再 `bytes()` 消耗 body —— 顺序反了拿不到头。
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    // 插件自己发 3xx（虫巢的 `/` 就是 307 → `/fleet`）时，`Location` 必须补上
    // `/px/<插件>` 前缀。不补的话 iframe 会跳到中台根下的 `/fleet` —— 404，
    // 用户看到的就是「插件打开了但白屏」。
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(|loc| {
            if loc.starts_with('/') && !loc.starts_with("//") {
                format!("/px/{pid}{loc}")
            } else {
                loc.to_string()
            }
        });
    let payload = resp.bytes().await.unwrap_or_default();

    // HTML 里的**绝对路径**要补前缀：插件页写的是 `<img src="/api/qr/image?…">`、
    // `<a href="/fleet">`，进了 iframe 就变成请求中台根下的 `/api/...` —— 404，
    // 页面上就是「图不出来、链接点了没反应」。相对路径不用管（同源后自动对）。
    let payload = if ctype.starts_with("text/html") {
        match std::str::from_utf8(&payload) {
            // 顺序不能反：`rewrite_refs` 认 `="/`，而注入脚本里有 `==="/"`（含 `="/`），
            // 先注入会被它改成 `="/px/…`，把 JS 弄坏。所以先改属性、最后注入。
            Ok(s) => Bytes::from(inject_boot(&rewrite_refs(s, &pid), &pid).into_bytes()),
            Err(_) => payload,
        }
    } else {
        payload
    };

    let mut out = Response::new(Body::from(payload));
    *out.status_mut() = status;
    let h = out.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&ctype) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Some(loc) = location {
        if let Ok(v) = HeaderValue::from_str(&loc) {
            h.insert(header::LOCATION, v);
        }
    }
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    // 剥掉会挡住 iframe 的两个头：插件页是想被嵌的，只是它自己不知道。
    for name in ["x-frame-options", "content-security-policy"] {
        h.remove(name);
    }
    // 子请求（`__shell.css` / 静态图 / 插件自己的 `/api/x`）带不了 `?k=`，
    // 靠这颗 cookie 认人。只在这一次落，后面同源请求会自动带上。
    // 两个 cookie 必须**分成两个 Set-Cookie 头**：HTTP 不允许用逗号拼，
    // 拼了浏览器会把第二个当成第一个的属性，等于没设（2026-11-27 踩过）。
    if !headers.contains_key("x-token") {
        for ck in [
            format!("moshan_k={}; Path=/; SameSite=Lax; HttpOnly", ctx.token),
            format!("pxk={}; Path=/px; SameSite=Lax; HttpOnly", ctx.token),
        ] {
            if let Ok(v) = HeaderValue::from_str(&ck) {
                h.append(header::SET_COOKIE, v);
            }
        }
    }
    out
}

/// 把 HTML 里的 `="/绝对路径"` 与 `url(/绝对路径)` 补上 `/px/<插件>` 前缀。
///
/// 只认这两个位置，不动 JS 里的字符串：`fetch('/api/x')` 这种改了容易把语法弄坏，
/// 而插件页里绝大多数请求都是相对路径，落到反代上本来就是对的。
/// `//host/x`（协议相对）和已经带前缀的跳过，免得改两次。
fn rewrite_refs(text: &str, pid: &str) -> String {
    let prefix = format!("/px/{pid}");
    let mut out = String::with_capacity(text.len() + 256);
    let mut i = 0usize;
    while i < text.len() {
        let rest = &text[i..];
        let at = if rest.starts_with("=\"/") {
            Some(i + 2)
        } else if rest.starts_with("url(/") {
            Some(i + 4)
        } else {
            None
        };
        match at {
            Some(pos)
                if !text[pos..].starts_with("//")
                    && !text[pos..].starts_with(&prefix) =>
            {
                out.push_str(&text[i..pos]);
                out.push_str(&prefix);
                i = pos;
            }
            _ => {
                // 按字符推进，保证 i 永远落在 UTF-8 边界上（否则切片会 panic）。
                let ch = rest.chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    out
}

/// 注入到插件页最前面的启动补丁：把 `fetch` 与 `XMLHttpRequest` 包一层，
/// 凡是**以单个 `/` 开头**的请求都补上 `/px/<插件>` 前缀。
///
/// 为什么必须在运行时补，而不像 `rewrite_refs` 那样改文本：`fetch('/api/x')` 是
/// JS 字符串，正则改容易把语法弄坏（模板串、`'\n'` 都可能被误伤）；而绝对路径的
/// 语义是确定的 —— 同一个页面在插件自己的端口上 `/` 就是它的根，进了 iframe 却成了
/// 中台的根，必须补前缀才对。
///
/// 真实后果（2026-11-28 用户报「虫巢思维一片 undefined」）：那页有 19 处
/// `fetch('/api/fleet/…')`，嵌进来后全打到中台。中台没有 `/api/fleet/overview`，
/// 回 404 `{"detail":"Not Found"}`；页面只判 401 不判 404，于是把这个对象当数据
/// 渲染 —— 界面上就是「在线 undefined / 共 undefined 台」+「还没有任何设备上报」。
fn inject_boot(text: &str, pid: &str) -> String {
    let boot = BOOT_JS.replace("__PX__", &format!("/px/{pid}"));
    // 插在 `<head>` 之后，保证早于页面自己的脚本；没有 head 就退到 `<html…>`、
    // 再不行就 `<body…>`；连这些都没有（片段）就放最前面 —— 总之别让页面里
    // 第一段脚本先跑起来。
    for anchor in ["<head>", "<head ", "<html>", "<html ", "<body>", "<body "] {
        if let Some(i) = text.find(anchor) {
            if let Some(off) = text[i..].find('>') {
                // 锚点标签本身留着，插在它的 `>` 之后
                let at = i + off + 1;
                let mut out = String::with_capacity(text.len() + boot.len());
                out.push_str(&text[..at]);
                out.push_str(&boot);
                out.push_str(&text[at..]);
                return out;
            }
        }
    }
    let mut out = String::with_capacity(text.len() + boot.len());
    out.push_str(&boot);
    out.push_str(text);
    out
}

/// 注入脚本本体。用 `__PX__` 占位而不是 `format!`，省掉 JS 里成片的 `{{`。
const BOOT_JS: &str = r##"<script>(function(){
var P="__PX__";
function fx(u){
  if(typeof u!=="string")return u;
  if(u.charAt(0)!=="/"||u.charAt(1)==="/")return u;
  if(u===P||u.indexOf(P+"/")===0)return u;
  return P+u;
}
var F=window.fetch;
if(F)window.fetch=function(u,o){return F.call(this,fx(u),o)};
var X=window.XMLHttpRequest&&window.XMLHttpRequest.prototype;
if(X&&X.open){var o=X.open;X.open=function(m,u){var a=[].slice.call(arguments);a[1]=fx(u);return o.apply(this,a)}}
})();</script>"##;
