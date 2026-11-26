// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：页面类路由（/、/dash、/about、/agent、/hostwork、/pair、/docs/{name}）。
// 不做：不迁 /fleet 与 /gray —— server.py 里 fleet.register 已被注释、
//       gray 根本没人 import，Python 对这两条就是 404。Rust 若自己应答反而制造差异。

use crate::paths::Ctx;
use crate::routes::common::http_err;
use crate::routes::u::appearance;
use crate::routes::u::util::{html, sec_headers, tailscale_ip, text};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Uri};
use axum::response::Response;
use serde_json::Value;
use std::path::{Path as SysPath, PathBuf};
use std::sync::Arc;

type CtxState = State<Arc<Ctx>>;

const ABOUT_PAGE: &str = include_str!("assets/about_page.txt");
const ABOUT_DRAWER: &str = include_str!("assets/about_drawer.txt");
const PLUGINUI_BLOCK: &str = include_str!("assets/pluginui.txt");
const MODELUI_BLOCK: &str = include_str!("assets/modelui.txt");
const HOSTWORK_SHIM: &str = include_str!("assets/hostwork_shim.html");
const AGENT_README: &str = include_str!("assets/agent_readme.md");

fn repo_default() -> &'static str {
    crate::paths::DEFAULT_REPO
}

/// `dashmod._root(cfg)`：allowed_roots[0]，否则 inbox_dir 的父目录。
fn data_root(cfg: &Value) -> PathBuf {
    if let Some(first) = cfg
        .get("allowed_roots")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    {
        return PathBuf::from(first);
    }
    let inbox = cfg.get("inbox_dir").and_then(|v| v.as_str()).unwrap_or(".");
    SysPath::new(inbox)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn insert_before_last_body(html_text: &str, block: &str) -> String {
    match html_text.rfind("</body>") {
        Some(i) => format!("{}{}{}", &html_text[..i], block, &html_text[i..]),
        None => format!("{html_text}{block}"),
    }
}

// ---------------------------------------------------------------- 首页

pub async fn index(State(ctx): CtxState, _headers: HeaderMap, uri: Uri) -> Response {
    let ui = ctx.app_dir.join("ui.html");
    if !ui.exists() {
        return html(200, "<h1>口袋服务</h1><p>ui.html 缺失</p>", &[]);
    }
    let mut body = std::fs::read_to_string(&ui).unwrap_or_default();
    if let Some(ip) = tailscale_ip() {
        if !ip.is_empty() {
            body = body.replace("__TS_IP__", &ip);
        }
    }
    // about.inject(html)（drawer=False）自 2026-11-26 起是空操作；这里保留判断顺序。
    if !body.contains("mosh-upd-row") && !body.contains("mosh-about") {
        // 不挂胶囊
    }
    let extra = sec_headers();
    let mut res = html(200, body, &extra);
    // `?k=正确令牌` → 换 HttpOnly Cookie，之后地址栏里可以不带令牌。
    let k = crate::routes::common::q(&uri, "k");
    if !k.is_empty() && k == ctx.token {
        let secure = if uri.scheme_str() == Some("https") { "; Secure" } else { "" };
        let cookie = format!(
            "moshan_k={}; Path=/; SameSite=strict; Max-Age={}; HttpOnly{}",
            ctx.token,
            180 * 86400,
            secure
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            res.headers_mut().append(header::SET_COOKIE, v);
        }
    }
    res
}

// ---------------------------------------------------------------- 工作台

/// `server.py:_ensure_page`：把随包分发的 dash.html 铺到数据根。
/// 备份那一层（gzip 到 `_bak/YYYY-MM/`）没有复刻 —— 它只影响磁盘上的历史副本，不影响响应。
fn ensure_page(p: &SysPath, src: &SysPath) {
    if !src.exists() {
        return;
    }
    if !p.exists() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(src, p);
        return;
    }
    let same = match (std::fs::read(src), std::fs::read(p)) {
        (Ok(a), Ok(b)) => a == b,
        _ => true,
    };
    if !same {
        let _ = std::fs::copy(src, p);
    }
}

pub async fn dash(State(ctx): CtxState) -> Response {
    let p = data_root(&ctx.cfg).join("_工作台").join("dash.html");
    ensure_page(&p, &ctx.app_dir.join("dash.html"));
    if !p.exists() {
        let body = format!(
            "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>\
<h3>工作台页面还没生成</h3><p style='color:#666'>应当位于：{}</p></body>",
            p.display()
        );
        return html(404, body, &[]);
    }
    let mut body = std::fs::read_to_string(&p).unwrap_or_default();
    // sync 顺手把 appearance.css 刷一遍（失败不影响页面）。
    appearance::sync(&ctx.cfg);
    let boot = appearance::bootstrap(&ctx.cfg, "/__appearance.css");

    if !body.contains("mpk-launch") {
        body = insert_before_last_body(&body, PLUGINUI_BLOCK);
    }
    if !body.contains("mosh-upd-row") && !body.contains("mosh-about") {
        let drawer = ABOUT_DRAWER.replace("__VER__", crate::paths::VERSION);
        body = insert_before_last_body(&body, &drawer);
    }
    if !body.contains("mosh-mdl-group") && !body.contains("moshMdlPick") {
        body = insert_before_last_body(&body, MODELUI_BLOCK);
    }
    if body.contains("</head>") {
        body = body.replacen("</head>", &format!("{boot}</head>"), 1);
    } else {
        body = format!("{boot}{body}");
    }
    let extra = sec_headers();
    html(200, body, &extra)
}

// ---------------------------------------------------------------- 关于

/// `/about` 需要把令牌拼进页面里的返回链接，所以单独走这个入口。
pub async fn about(State(ctx): CtxState) -> Response {
    let body = ABOUT_PAGE
        .replace("__REPO__", repo_default())
        .replace("__K__", &ctx.token);
    html(200, body, &[("cache-control", "no-store")])
}

// ---------------------------------------------------------------- Agent 说明

pub async fn agent(State(ctx): CtxState) -> Response {
    let root = data_root(&ctx.cfg);
    let root_s = root.to_string_lossy().to_string();
    let body = AGENT_README.replace("@@ROOT@@", &root_s);
    let readme = root.join("_工作台").join("agents").join("README.md");
    if let Some(parent) = readme.parent() {
        if parent.exists() {
            let _ = std::fs::write(&readme, body.as_bytes());
        }
    }
    text(200, body, "text/markdown; charset=utf-8")
}

// ---------------------------------------------------------------- 本机工作

pub async fn hostwork(State(ctx): CtxState) -> Response {
    let cuntu = std::env::var_os("MOSHAN_CUNTU")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join("Documents").join("cuntu")
        });
    let mut p: Option<PathBuf> = None;
    for cand in [cuntu.join("hostwork_ui.html"), ctx.app_dir.join("hostwork_ui.html")] {
        if cand.exists() {
            p = Some(cand);
            break;
        }
    }
    let Some(p) = p else {
        let body = "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>\
<h3>Agent 监控页面不在这台机器上</h3>\
<p style='color:#666'>找不到 hostwork_ui.html（cuntu 与包内均无）</p></body>";
        return html(404, body, &[]);
    };
    let mut body = std::fs::read_to_string(&p).unwrap_or_default();
    if body.contains("</head>") {
        body = body.replace("</head>", &format!("{HOSTWORK_SHIM}</head>"));
    } else {
        body = format!("{HOSTWORK_SHIM}{body}");
    }
    let extra = sec_headers();
    html(200, body, &extra)
}

// ---------------------------------------------------------------- 手机接入

pub async fn pair(State(ctx): CtxState) -> Response {
    let p = data_root(&ctx.cfg).join("_工作台").join("pair.html");
    if !p.exists() {
        let body = format!(
            "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>\
<h3>手机接入页还没生成</h3><p style='color:#666'>应当位于：{}</p></body>",
            p.display()
        );
        return html(404, body, &[]);
    }
    let mut body = std::fs::read_to_string(&p).unwrap_or_default();
    // Python 只在本机来源注入；Rust 的服务只绑 127.0.0.1，所有连接都是本机，
    // 因此这里恒注入（语义等价）。
    let tok = ctx
        .cfg
        .get("view_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let js = serde_json::to_string(tok).unwrap_or_else(|_| "\"\"".to_string());
    if body.contains("</head>") {
        body = body.replacen(
            "</head>",
            &format!("<script>window.__PK__={js}</script></head>"),
            1,
        );
    }
    html(200, body, &[("cache-control", "no-store")])
}

// ---------------------------------------------------------------- 功能文档

pub async fn docs(State(ctx): CtxState, Path(name): Path<String>) -> Response {
    let fn_name = match name.as_str() {
        "panorama" => "panorama.html",
        "impl" => "impl.html",
        "gloss" => "gloss.html",
        _ => return http_err(404, "没有这份文档"),
    };
    let p = data_root(&ctx.cfg).join("_工作台").join("docs").join(fn_name);
    if !p.exists() {
        let body = format!(
            "<meta charset='utf-8'><body style='font:15px -apple-system;padding:40px'>\
<h3>这份文档还没放到这台机器上</h3><p style='color:#666'>应当位于：{}</p></body>",
            p.display()
        );
        return html(404, body, &[]);
    }
    let body = std::fs::read_to_string(&p).unwrap_or_default();
    let extra = sec_headers();
    html(200, body, &extra)
}
