// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：一组杂项路由 —— 插件清单（add / remove）、整理收件箱（sort）、
//       在访达里打开（reveal）、触发重扫（rescan）、原生选目录（pick/dir）、
//       行程解析（parse-event）、对话（chat）、微信入站（intake）。
// 不做：不走别的泳道；不改共享模块。
//
// 已知边界（都写在对应函数头上，汇报里也照这个说）：
//   · `/api/rescan` 只按 Python 的形状回 `{"ok": true}`；真正扫会话的是采集层
//     `hostwork.scan_sessions`，Rust 侧没有第二套解析器（w4b 也只读它落的缓存）。
//   · `/api/parse-event` 会先问本机 4B（ollama），失败才退回正则。降级路径与
//     Python 逐字对齐；模型可用时的输出本身不做逐字节保证（模型不全确定）。
//   · `/api/chat` 的「收件箱有什么」快问快答逐字对齐；真问模型时只走纯对话一轮，
//     `/mcp` 的工具循环没有搬 —— 端点没工具时 Python 也只跑一轮，口径一致。
//   · `/api/intake` 搬的是入站前半程（抽链接 / 判存图还是文章 / 回执文案）。
//     抓取那半在 cuntu 子进程里，Rust 不接；因此带链接的响应当前只给 count + receipt。

use super::util::{self, collapse, cut, pystr, s_or, truthy};
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/chat", post(chat_route))
        .route("/api/intake", post(intake_route))
        .route("/api/parse-event", post(parse_event_route))
        .route("/api/pick/dir", post(pick_dir_route))
        .route("/api/plugins/add", post(plugins_add_route))
        .route("/api/plugins/remove", post(plugins_remove_route))
        .route("/api/rescan", post(rescan_route))
        .route("/api/reveal", post(reveal_route))
        .route("/api/sort", post(sort_route))
}

// ---------------------------------------------------------------- FastAPI 形状

/// `HTTPException(status, detail)` 的响应体。
fn detail(code: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(json!({"detail": msg})),
    )
        .into_response()
}

fn json_status(code: u16, v: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(v),
    )
        .into_response()
}

// ---------------------------------------------------------------- 收件箱小工具

const SORTED_DIRNAME: &str = "_已分类";

/// `_walk_inbox(limit)`：递归列收件箱里的文件，按 mtime 倒序。
fn walk_inbox(inbox: &Path, limit: usize) -> Vec<PathBuf> {
    if !inbox.exists() {
        return Vec::new();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    collect_files(inbox, &mut out, limit);
    out.sort_by(|a, b| {
        let ma = std::fs::metadata(a).and_then(|m| m.modified()).ok();
        let mb = std::fs::metadata(b).and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });
    out
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, limit: usize) {
    if out.len() >= limit {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if out.len() >= limit {
            return;
        }
        let p = e.path();
        let is_link = std::fs::symlink_metadata(&p)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if p.is_dir() {
            // 符号链接目录不跟（Path.rglob 默认口径）
            if !is_link {
                collect_files(&p, out, limit);
            }
            continue;
        }
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with('.') || name == "index.jsonl" {
            continue;
        }
        out.push(p);
    }
}

fn rel_of(p: &Path, base: &Path) -> String {
    p.strip_prefix(base)
        .map(|r| r.display().to_string())
        .unwrap_or_else(|_| p.display().to_string())
}

/// `_inbox_hint(limit)`：给模型/用户的收件箱现状（纯代码，秒回）。
fn inbox_hint(inbox: &Path, limit: usize) -> String {
    let files = walk_inbox(inbox, 400);
    let head = format!("【当前状态】收件箱：{}", inbox.display());
    if files.is_empty() {
        return format!("{}\n目前是空的。", head);
    }
    let mut lines = vec![format!(
        "{}\n共 {} 个文件，最近的在前：",
        head,
        files.len()
    )];
    for p in files.iter().take(limit) {
        let Ok(md) = std::fs::metadata(p) else { continue };
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        lines.push(format!(
            "- {}｜{}｜{}｜{}",
            rel_of(p, inbox),
            crate::storage::human(md.len() as f64),
            crate::storage::category(&name),
            util::strftime_secs(mtime, "%m-%d %H:%M")
        ));
    }
    if files.len() > limit {
        lines.push(format!(
            "（另有 {} 个未列出，需要时可调 list_directory）",
            files.len() - limit
        ));
    }
    lines.join("\n")
}

const LIST_WORDS: [&str; 9] = [
    "有什么",
    "有哪些",
    "列出",
    "列一下",
    "多少个",
    "几个文件",
    "文件列表",
    "看看文件",
    "都有什么",
];

fn quick_answer(text: &str, inbox: &Path) -> Option<String> {
    let t = text
        .trim()
        .trim_end_matches(|c: char| matches!(c, '？' | '?' | '。' | '.'));
    if matches!(t, "文件" | "列文件" | "文件列表" | "收件箱") {
        return Some(inbox_hint(inbox, 30).replace("【当前状态】", ""));
    }
    if t.contains("收件箱") && LIST_WORDS.iter().any(|w| t.contains(w)) {
        return Some(inbox_hint(inbox, 30).replace("【当前状态】", ""));
    }
    None
}

// ---------------------------------------------------------------- /api/sort

async fn sort_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || sort_inbox(&ctx2))
        .await
        .unwrap_or_else(|_| json!({"reply": "整理失败了。", "moved": {}, "failed": 0}));
    Json(out).into_response()
}

fn sort_inbox(ctx: &Ctx) -> Value {
    let inbox = ctx.inbox_dir();
    let files = walk_inbox(&inbox, 5000);
    if files.is_empty() {
        return json!({"reply": "收件箱是空的。", "moved": {}});
    }
    let mut moved: Map<String, Value> = Map::new();
    let mut failed = 0i64;
    for p in &files {
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let cat = crate::storage::category(&name);
        let dest_dir = inbox.join(SORTED_DIRNAME).join(&cat);
        let ok = std::fs::create_dir_all(&dest_dir).is_ok();
        if !ok {
            failed += 1;
            continue;
        }
        let dest = crate::storage::unique_path(&dest_dir, &name);
        if std::fs::rename(p, &dest).is_err() {
            failed += 1;
            continue;
        }
        let n = moved.get(&cat).and_then(|v| v.as_i64()).unwrap_or(0);
        moved.insert(cat, json!(n + 1));
    }
    let mut pairs: Vec<(String, i64)> = moved
        .iter()
        .map(|(k, v)| (k.clone(), v.as_i64().unwrap_or(0)))
        .collect();
    pairs.sort_by(|a, b| b.1.cmp(&a.1));
    let parts = pairs
        .iter()
        .map(|(c, n)| format!("{} {}", c, n))
        .collect::<Vec<_>>()
        .join("、");
    let total: i64 = moved.values().filter_map(|v| v.as_i64()).sum();
    let mut reply = format!("已整理 {} 个文件", total);
    if !parts.is_empty() {
        reply.push_str(&format!("：{}", parts));
    }
    if failed > 0 {
        reply.push_str(&format!("；{} 个没动成", failed));
    }
    crate::storage::log(
        ctx,
        json!({"event": "sort", "moved": Value::Object(moved.clone()), "failed": failed}),
    );
    json!({"reply": reply, "moved": Value::Object(moved), "failed": failed})
}

// ---------------------------------------------------------------- /api/reveal

async fn reveal_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = super::parse_json_opt(&body).unwrap_or_else(|| json!({}));
    let target = if b.is_object() {
        {
            let v = s_or(b.get("target").unwrap_or(&Value::Null));
            if v.trim().is_empty() {
                "root".to_string()
            } else {
                v.trim().to_string()
            }
        }
    } else {
        "root".to_string()
    };
    let p = match target.as_str() {
        "root" => super::data_root(&ctx),
        "state" => ctx.state_dir.clone(),
        _ => {
            return json_status(400, json!({"ok": false, "error": "只支持 root / state"}));
        }
    };
    if let Err(e) = std::fs::create_dir_all(&p) {
        return json_status(
            500,
            json!({"ok": false, "error": format!("目录建不出来：{}", util::py_os_err_path(&e, &p))}),
        );
    }
    let r = crate::sysopen::open_path(&p);
    if let Err(e) = r {
        return json_status(
            500,
            json!({"ok": false, "error": util::py_os_err(&e), "path": p.display().to_string()}),
        );
    }
    Json(json!({"ok": true, "path": p.display().to_string()})).into_response()
}

// ---------------------------------------------------------------- /api/rescan

async fn rescan_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let _b = super::parse_json_opt(&body).unwrap_or_else(|| json!({}));
    // Python 在这里起一条 daemon 线程跑 `hostwork.scan_sessions(force)`，立刻回 ok。
    // Rust 侧没有第二套会话解析器（w4b 也只读采集层落的缓存），所以这里只按同一
    // 形状应答；真正的重扫仍由采集层自己做（见文件头的「已知边界」）。
    Json(json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------- /api/pick/dir

async fn pick_dir_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    if !cfg!(target_os = "macos") {
        return Json(json!({"ok": false, "error": "只有 macOS 有原生选择器，直接填路径"}))
            .into_response();
    }
    let out = tokio::task::spawn_blocking(|| {
        std::process::Command::new("osascript")
            .arg("-e")
            .arg("POSIX path of (choose folder with prompt \"选一个客户端的数据目录\")")
            .output()
    })
    .await;
    let out = match out {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Json(json!({"ok": false, "error": util::py_os_err(&e)})).into_response()
        }
        Err(_) => {
            return Json(json!({"ok": false, "error": "RuntimeError: 对话框失败"})).into_response()
        }
    };
    let p = String::from_utf8_lossy(&out.stdout)
        .trim()
        .trim_end_matches('/')
        .to_string();
    if !out.status.success() || p.is_empty() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = cut(&msg, 200);
        return Json(json!({"ok": false, "cancelled": true,
                           "error": if msg.is_empty() { "已取消".to_string() } else { msg }}))
        .into_response();
    }
    Json(json!({"ok": true, "path": p})).into_response()
}

// ---------------------------------------------------------------- 插件清单

const PLUGINS_FILE: &str = "plugins.json";

const PLUGIN_TEMPLATE: &str = "加插件就往下写一条：{\"id\":\"backup\",\"name\":\"备份\",\"port\":8781,\"cmd\":\"14-备份/_备份.command\",\"note\":\"一句话说明\",\"group\":\"other\"}。group 决定弹窗里归哪一档（tool=小工具 / plugin=内置插件 / other=其他插件 —— 自己写的、自带数据与页面、既不是随手打开也不是中台运维；不写按有没有端口猜）；还没做的加 \"soon\":true 先占位。写完刷新页面即可，不用重启口袋；id 跟内置重名会覆盖内置那一行。";

const BUILTIN_IDS: [&str; 9] = [
    "pocket",
    "gallery",
    "reader",
    "media",
    "inspect",
    "hostwork",
    "llmcost",
    "follow",
    "review",
];

fn plugins_path(root: &Path) -> PathBuf {
    root.join("_工作台").join(PLUGINS_FILE)
}

fn slug(text: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^a-z0-9]+").unwrap());
    let s = re.replace_all(&text.to_lowercase(), "-").trim_matches('-').to_string();
    cut(&s, 24)
}

fn plugin_id_ok(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 32 {
        return false;
    }
    if !(b[0].is_ascii_lowercase()) {
        return false;
    }
    b.iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn rand_hex6() -> String {
    // uuid4 的随机尾巴只为「名字兜不出 id」时用；这里取进程内唯一即可。
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    format!("{:06x}", n & 0xff_ffff)
}

/// `_norm_plugin(raw, idx)`：一行 json → 一份插件规格。缺名字或落点的整行丢掉。
fn norm_plugin(raw: &Value) -> Option<Value> {
    let r = raw.as_object()?;
    let g = |k: &str| r.get(k).cloned().unwrap_or(Value::Null);
    let name = cut(&s_or(&g("name")).trim().to_string(), 24);
    if name.is_empty() {
        return None;
    }
    let soon = truthy(&g("soon"));
    let ext = s_or(&g("ext")).trim().to_string();

    let port_raw = g("port");
    let port: Option<i64> = match &port_raw {
        Value::Null => None,
        Value::String(s) if s.is_empty() || s == "0" => None,
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f == 0.0 {
                None
            } else {
                Some(f as i64)
            }
        }
        Value::Bool(false) => None,
        Value::Bool(true) => Some(1),
        Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as i64).filter(|v| *v != 0),
        _ => None,
    };

    let mut url = s_or(&g("url")).trim().to_string();
    if url.is_empty() {
        if let Some(p) = port {
            if ext.is_empty() {
                url = format!("http://127.0.0.1:{}/", p);
            }
        }
    }
    if url.is_empty() && !soon && ext.is_empty() {
        return None;
    }

    let mut pid = s_or(&g("id")).trim().to_lowercase();
    if !plugin_id_ok(&pid) {
        let named = {
            let raw_id = g("id");
            let base = if truthy(&raw_id) {
                s_or(&raw_id)
            } else {
                name.clone()
            };
            slug(&base)
        };
        pid = if !named.is_empty() {
            named
        } else if let Some(p) = port {
            format!("p{}", p)
        } else {
            format!("p{}", rand_hex6())
        };
        if !plugin_id_ok(&pid) {
            pid = format!("p{}", rand_hex6());
        }
    }

    // cmd: ["dir","file"] 或 "dir/file"
    let cmd_v = g("cmd");
    let cmd_t: Option<(String, String)> = match &cmd_v {
        Value::Array(a) if a.len() == 2 => {
            let x = pystr(&a[0]).trim().to_string();
            let y = pystr(&a[1]).trim().to_string();
            if !x.is_empty() && !y.is_empty() {
                Some((x, y))
            } else {
                None
            }
        }
        Value::String(s) if s.contains('/') => {
            let t = s.trim();
            match t.rsplit_once('/') {
                Some((d, f)) if !d.trim().is_empty() && !f.trim().is_empty() => {
                    Some((d.trim().to_string(), f.trim().to_string()))
                }
                _ => None,
            }
        }
        _ => None,
    };

    let mut group = s_or(&g("group")).trim().to_lowercase();
    if !matches!(group.as_str(), "tool" | "plugin" | "other") {
        group = if port.is_some() || cmd_t.is_some() {
            "plugin".to_string()
        } else {
            "tool".to_string()
        };
    }
    let glyph = {
        let s = s_or(&g("glyph")).trim().to_string();
        if s.is_empty() {
            "plugin".to_string()
        } else {
            s
        }
    };
    let note = cut(&s_or(&g("note")).trim().to_string(), 60);
    let host = cut(&s_or(&g("host")).trim().to_string(), 64);
    let scope = if !ext.is_empty() {
        "ext"
    } else if soon {
        "soon"
    } else if port.is_some() || cmd_t.is_some() {
        "service"
    } else {
        "view"
    };
    let svc_name = {
        let s = s_or(&g("svc_name")).trim().to_string();
        if s.is_empty() {
            name.clone()
        } else {
            s
        }
    };

    let mut m = Map::new();
    m.insert("id".into(), json!(pid));
    m.insert("name".into(), json!(name));
    m.insert("glyph".into(), json!(glyph));
    m.insert("note".into(), json!(note));
    m.insert("url".into(), json!(url));
    m.insert("port".into(), match port {
        Some(p) => json!(p),
        None => Value::Null,
    });
    m.insert("host".into(), json!(host));
    m.insert("scope".into(), json!(scope));
    m.insert(
        "cmd".into(),
        match &cmd_t {
            Some((a, b)) => json!([a, b]),
            None => Value::Null,
        },
    );
    m.insert("svc_name".into(), json!(svc_name));
    m.insert("source".into(), json!("plugin"));
    m.insert("group".into(), json!(group));
    m.insert("soon".into(), json!(soon));
    m.insert("ext".into(), json!(ext));
    Some(Value::Object(m))
}

fn read_plugins(root: &Path) -> Vec<Value> {
    let d = util::load_json(&plugins_path(root)).unwrap_or_else(|| json!({}));
    let rows = match &d {
        Value::Object(m) => m.get("plugins").cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    let Some(arr) = rows.as_array() else { return Vec::new() };
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for raw in arr {
        if let Some(p) = norm_plugin(raw) {
            let id = s_or(p.get("id").unwrap_or(&Value::Null));
            if !seen.contains(&id) {
                seen.push(id);
                out.push(p);
            }
        }
    }
    out
}

fn write_plugins(root: &Path, rows: &[Value]) {
    let p = plugins_path(root);
    let mut items: Vec<Value> = Vec::new();
    for r in rows {
        let mut it = Map::new();
        it.insert("id".into(), r.get("id").cloned().unwrap_or(Value::Null));
        it.insert("name".into(), r.get("name").cloned().unwrap_or(Value::Null));
        it.insert(
            "url".into(),
            json!(s_or(r.get("url").unwrap_or(&Value::Null))),
        );
        if let Some(port) = r.get("port").and_then(|v| v.as_i64()) {
            it.insert("port".into(), json!(port));
        }
        let host = s_or(r.get("host").unwrap_or(&Value::Null));
        if !host.is_empty() {
            it.insert("host".into(), json!(host));
        }
        if let Some(cmd) = r.get("cmd").and_then(|v| v.as_array()) {
            if cmd.len() == 2 {
                it.insert(
                    "cmd".into(),
                    json!(format!(
                        "{}/{}",
                        pystr(&cmd[0]),
                        pystr(&cmd[1])
                    )),
                );
            }
        }
        let glyph = s_or(r.get("glyph").unwrap_or(&Value::Null));
        if !glyph.is_empty() && glyph != "plugin" {
            it.insert("glyph".into(), json!(glyph));
        }
        let note = s_or(r.get("note").unwrap_or(&Value::Null));
        if !note.is_empty() {
            it.insert("note".into(), json!(note));
        }
        let svc = s_or(r.get("svc_name").unwrap_or(&Value::Null));
        let name = s_or(r.get("name").unwrap_or(&Value::Null));
        if !svc.is_empty() && svc != name {
            it.insert("svc_name".into(), json!(svc));
        }
        let group = s_or(r.get("group").unwrap_or(&Value::Null));
        if matches!(group.as_str(), "tool" | "plugin" | "other") {
            it.insert("group".into(), json!(group));
        }
        if truthy(r.get("soon").unwrap_or(&Value::Null)) {
            it.insert("soon".into(), json!(true));
        }
        let ext = s_or(r.get("ext").unwrap_or(&Value::Null));
        if !ext.is_empty() {
            it.insert("ext".into(), json!(ext));
        }
        items.push(Value::Object(it));
    }
    let payload = json!({
        "_说明": PLUGIN_TEMPLATE,
        "plugins": items,
        "updated_at": util::strftime_now("%Y-%m-%dT%H:%M:%S"),
    });
    let _ = util::ensure_parent(&p);
    let tmp = p.with_file_name(format!("{}.tmp", PLUGINS_FILE));
    if super::write_json_indent(&tmp, &payload, 1).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn add_plugin(root: &Path, body: &Value) -> Value {
    let b = body.as_object().cloned().unwrap_or_default();
    let mut raw = Map::new();
    for k in [
        "id", "name", "glyph", "port", "url", "cmd", "note", "svc_name", "group", "ext",
    ] {
        if let Some(v) = b.get(k) {
            raw.insert(k.to_string(), v.clone());
        }
    }
    let Some(p) = norm_plugin(&Value::Object(raw)) else {
        return json!({"ok": false, "error": "名字必填，地址和端口至少给一个"});
    };
    let pid = s_or(p.get("id").unwrap_or(&Value::Null));
    let mut rows = read_plugins(root);
    if let Some(dup) = rows
        .iter()
        .find(|x| s_or(x.get("id").unwrap_or(&Value::Null)) == pid)
    {
        return json!({
            "ok": false,
            "error": format!(
                "清单里已经有 id={} 的插件了（「{}」）",
                pid,
                s_or(dup.get("name").unwrap_or(&Value::Null))
            ),
        });
    }
    let mut p = p;
    if BUILTIN_IDS.contains(&pid.as_str()) {
        if let Some(m) = p.as_object_mut() {
            m.insert("overrode".into(), json!(true));
        }
    }
    rows.push(p.clone());
    write_plugins(root, &rows);
    json!({"ok": true, "plugin": p})
}

fn remove_plugin(root: &Path, pid: &str) -> Value {
    let rows = read_plugins(root);
    let keep: Vec<Value> = rows
        .iter()
        .filter(|x| s_or(x.get("id").unwrap_or(&Value::Null)) != pid)
        .cloned()
        .collect();
    if keep.len() == rows.len() {
        return json!({"ok": false, "error": "清单里没有这个插件（内置插件改不了，只能覆盖）"});
    }
    write_plugins(root, &keep);
    json!({"ok": true})
}

async fn plugins_add_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match super::parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let root = super::data_root(&ctx);
    Json(add_plugin(&root, &b)).into_response()
}

async fn plugins_remove_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match super::parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let root = super::data_root(&ctx);
    let pid = s_or(b.get("id").unwrap_or(&Value::Null));
    Json(remove_plugin(&root, &pid)).into_response()
}

// ---------------------------------------------------------------- /api/parse-event

const EV_OLLAMA: &str = "http://127.0.0.1:11434/api/generate";

const EV_PROMPT: &str = "从下面这段中文里抽出行程信息，只输出一个 JSON 对象，不要解释、不要代码块。

字段（没有的就填空字符串 \"\" 或空数组 []）：
- title: 一句话概括，不超过 12 字
- date: 出发日期，YYYY-MM-DD
- time: 出发时刻，HH:MM
- endDate: 到达日期，YYYY-MM-DD（当天到达就留空）
- endTime: 到达时刻，HH:MM
- from: 出发地
- stops: 途经地，数组
- to: 到达地
- flight: 航班号或车次
- fee: 路费，只要数字
- delay: 延误分钟数，只要数字

今天是 {today}（{wd}）。文中「明天」「后天」「下周三」都按今天推算。

文字：
{text}

JSON:";

fn local_model_cfg(ctx: &Ctx) -> String {
    let s = s_or(util::get(&ctx.cfg, "model")).trim().to_string();
    if s.is_empty() {
        "qwen3.5-4b-gguf:q4km".to_string()
    } else {
        s
    }
}

/// `_llm_json(prompt, model)`：本机 ollama 的 /api/generate。
/// 错误按 Python 的异常类名回报（note 里要写 `模型不可用（X）`）。
async fn llm_json(prompt: &str, model: &str) -> Result<String, String> {
    let body = json!({
        "model": model, "prompt": prompt, "stream": false, "think": false,
        "format": "json",
        "options": {"temperature": 0.1, "num_predict": 420, "top_p": 0.9},
    });
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(75))
        .no_proxy()
        .build()
    {
        Ok(c) => c,
        Err(_) => return Err("URLError".to_string()),
    };
    let resp = match client.post(EV_OLLAMA).json(&body).send().await {
        Ok(r) => r,
        Err(_) => return Err("URLError".to_string()),
    };
    if !resp.status().is_success() {
        return Err("HTTPError".to_string());
    }
    let txt = match resp.text().await {
        Ok(t) => t,
        Err(_) => return Err("URLError".to_string()),
    };
    let d: Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return Err("JSONDecodeError".to_string()),
    };
    Ok(s_or(util::get(&d, "response")))
}

/// `_grab_json(raw)`：模型爱在 JSON 外面包 ```json 或者写两句废话。
fn grab_json(raw: &str) -> Value {
    static FENCE_HEAD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static FENCE_TAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let head = FENCE_HEAD.get_or_init(|| regex::Regex::new(r"^```[a-zA-Z]*\s*").unwrap());
    let tail = FENCE_TAIL.get_or_init(|| regex::Regex::new(r"\s*```$").unwrap());
    let mut s = raw.trim().to_string();
    s = head.replace(&s, "").to_string();
    s = tail.replace(&s, "").trim().to_string();
    if let (Some(i), Some(j)) = (s.find('{'), s.rfind('}')) {
        if j > i {
            s = s[i..j + 1].to_string();
        }
    }
    match serde_json::from_str::<Value>(&s) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

/// `_num(v)`：从任意值里抠第一个数字。
fn num_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        _ => {
            static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let re = RE.get_or_init(|| regex::Regex::new(r"-?\d+(?:\.\d+)?").unwrap());
            let s = pystr(v).replace(',', "");
            if s.is_empty() {
                return None;
            }
            re.find(&s).and_then(|m| m.as_str().parse::<f64>().ok())
        }
    }
}

fn ok_date(v: &Value) -> String {
    let s = cut(&pystr(v), 10);
    match chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        Ok(d) => d.format("%Y-%m-%d").to_string(),
        Err(_) => String::new(),
    }
}

fn ok_time(v: &Value) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^\s*(\d{1,2})[:：.点](\d{1,2})?\s*$").unwrap()
    });
    let s = pystr(v);
    let Some(c) = re.captures(&s) else { return String::new() };
    let h: u32 = c.get(1).map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
    let mi: u32 = c
        .get(2)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0);
    if h > 23 || mi > 59 {
        return String::new();
    }
    format!("{:02}:{:02}", h, mi)
}

fn clean_place(v: &Value) -> String {
    static R1: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static R2: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let r1 = R1.get_or_init(|| {
        regex::Regex::new(
            r"^(大后天|后天|明天|今天|今晚|明早|下周[一二三四五六日天]?|(?:周|星期)[一二三四五六日天]|凌晨|上午|中午|下午|晚上|早上|半夜|从|到|去|往|在)",
        )
        .unwrap()
    });
    let r2 = R2.get_or_init(|| {
        regex::Regex::new(
            r"^(大后天|后天|明天|今天|今晚|明早|凌晨|上午|中午|下午|晚上|早上|半夜|从|到|去|往|在|的)",
        )
        .unwrap()
    });
    let s = pystr(v).trim_matches(|c: char| matches!(c, ' ' | '的' | '，' | ',' | '。')).to_string();
    let s = r1.replace(&s, "").trim().to_string();
    let s = r2.replace(&s, "").trim().to_string();
    s
}

/// `_cn_date(text, today)` → (日期, 匹配原文)
fn cn_date(text: &str, today: chrono::NaiveDate) -> (String, String) {
    use chrono::Datelike;
    static R1: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static R2: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static R3: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static R4: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let r1 = R1.get_or_init(|| {
        regex::Regex::new(r"(20\d{2})\s*[-/.年]\s*(\d{1,2})\s*[-/.月]\s*(\d{1,2})").unwrap()
    });
    let r2 = R2.get_or_init(|| regex::Regex::new(r"(\d{1,2})\s*月\s*(\d{1,2})\s*[日号]").unwrap());
    let r3 = R3.get_or_init(|| regex::Regex::new(r"(?<!\d)(\d{1,2})[/／](\d{1,2})(?!\d)").unwrap());
    let r4 = R4.get_or_init(|| {
        regex::Regex::new(r"(下周|礼拜|周|星期)([一二三四五六日天])").unwrap()
    });
    if let Some(c) = r1.captures(text) {
        let y: i32 = c[1].parse().unwrap_or(0);
        let m: u32 = c[2].parse().unwrap_or(0);
        let d: u32 = c[3].parse().unwrap_or(0);
        let s = format!("{:04}-{:02}-{:02}", y, m, d);
        return (ok_date(&json!(s)), c.get(0).map(|m| m.as_str()).unwrap_or("").to_string());
    }
    if let Some(c) = r2.captures(text) {
        let m: u32 = c[1].parse().unwrap_or(0);
        let d: u32 = c[2].parse().unwrap_or(0);
        let s = format!("{:04}-{:02}-{:02}", today.year(), m, d);
        return (ok_date(&json!(s)), c.get(0).map(|m| m.as_str()).unwrap_or("").to_string());
    }
    if let Some(c) = r3.captures(text) {
        let m: u32 = c[1].parse().unwrap_or(0);
        let d: u32 = c[2].parse().unwrap_or(0);
        let s = format!("{:04}-{:02}-{:02}", today.year(), m, d);
        return (ok_date(&json!(s)), c.get(0).map(|m| m.as_str()).unwrap_or("").to_string());
    }
    for (word, off) in [
        ("大后天", 3i64),
        ("后天", 2),
        ("明天", 1),
        ("今天", 0),
        ("今晚", 0),
        ("明早", 1),
    ] {
        if text.contains(word) {
            let d = today + chrono::Duration::days(off);
            return (d.format("%Y-%m-%d").to_string(), word.to_string());
        }
    }
    if let Some(c) = r4.captures(text) {
        let word = c[2].replace('天', "日");
        let want = "一二三四五六日".find(&word);
        if let Some(want) = want {
            let base = if &c[1] == "下周" {
                // 下周三 = 下周一 + 2，不是「今天 +7 +2」
                today + chrono::Duration::days(7 - today.weekday().num_days_from_monday() as i64)
            } else {
                today
            };
            let d = if &c[1] == "下周" {
                base + chrono::Duration::days(want as i64)
            } else {
                today + chrono::Duration::days(
                    (want as i64 - today.weekday().num_days_from_monday() as i64).rem_euclid(7),
                )
            };
            return (d.format("%Y-%m-%d").to_string(), c.get(0).map(|m| m.as_str()).unwrap_or("").to_string());
        }
    }
    (String::new(), String::new())
}

fn ev_norm(d: &Value, today: chrono::NaiveDate) -> Value {
    let mut out = Map::new();
    out.insert("title".into(), json!(""));
    out.insert("date".into(), json!(""));
    out.insert("time".into(), json!(""));
    out.insert("endDate".into(), json!(""));
    out.insert("endTime".into(), json!(""));
    out.insert("from".into(), json!(""));
    out.insert("stops".into(), json!([]));
    out.insert("to".into(), json!(""));
    out.insert("flight".into(), json!(""));
    out.insert("fee".into(), Value::Null);
    out.insert("delay".into(), Value::Null);
    let Some(o) = d.as_object() else {
        return Value::Object(out);
    };
    let g = |k: &str| o.get(k).cloned().unwrap_or(Value::Null);
    out.insert(
        "title".into(),
        json!(cut(&collapse(&s_or(&g("title"))), 24)),
    );
    out.insert("date".into(), json!(ok_date(&g("date"))));
    out.insert("time".into(), json!(ok_time(&g("time"))));
    out.insert("endDate".into(), json!(ok_date(&g("endDate"))));
    out.insert("endTime".into(), json!(ok_time(&g("endTime"))));
    out.insert("from".into(), json!(cut(&clean_place(&g("from")), 24)));
    out.insert("to".into(), json!(cut(&clean_place(&g("to")), 24)));

    let stops_v = g("stops");
    let stops: Vec<Value> = match &stops_v {
        Value::String(s) => s
            .split(|c| matches!(c, ',' | '，' | '、' | '/'))
            .map(|x| json!(x))
            .collect(),
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    };
    let mut st: Vec<Value> = Vec::new();
    for x in &stops {
        let s = cut(&collapse(&pystr(x)), 24);
        if !pystr(x).trim().is_empty() {
            st.push(json!(s));
        }
        if st.len() >= 6 {
            break;
        }
    }
    out.insert("stops".into(), Value::Array(st));

    let fl = {
        static WS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = WS.get_or_init(|| regex::Regex::new(r"\s+").unwrap());
        re.replace_all(&s_or(&g("flight")), "").to_uppercase()
    };
    out.insert("flight".into(), json!(cut(&fl, 12)));

    let fee = num_of(&g("fee"));
    out.insert(
        "fee".into(),
        match fee {
            Some(f) if (0.0..1e6).contains(&f) => json!(util::py_round(f, 2)),
            _ => Value::Null,
        },
    );
    let dl = num_of(&g("delay"));
    out.insert(
        "delay".into(),
        match dl {
            Some(f) if f > 0.0 && f < 10000.0 => json!(f as i64),
            _ => Value::Null,
        },
    );

    // 跨天但只给了到达时刻：按「比出发晚」补日期
    let date = s_or(out.get("date").unwrap_or(&Value::Null));
    let etime = s_or(out.get("endTime").unwrap_or(&Value::Null));
    let etdate = s_or(out.get("endDate").unwrap_or(&Value::Null));
    let time = s_or(out.get("time").unwrap_or(&Value::Null));
    if !date.is_empty() && !etime.is_empty() && etdate.is_empty() {
        if !time.is_empty() && etime.as_str() < time.as_str() {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d") {
                out.insert(
                    "endDate".into(),
                    json!((d + chrono::Duration::days(1)).format("%Y-%m-%d").to_string()),
                );
            }
        }
    }
    // 只有日期没有到达时刻的「跨天」是模型瞎猜的，清掉
    let etdate = s_or(out.get("endDate").unwrap_or(&Value::Null));
    let etime = s_or(out.get("endTime").unwrap_or(&Value::Null));
    let date = s_or(out.get("date").unwrap_or(&Value::Null));
    if !etdate.is_empty() && (etime.is_empty() || etdate.as_str() <= date.as_str()) {
        out.insert("endDate".into(), json!(""));
    }
    let _ = today;
    Value::Object(out)
}

/// `_ev_rule(text, today)`：正则兜底，也是「硬字段」的裁判。
fn ev_rule(text: &str, today: chrono::NaiveDate) -> Value {
    let mut out = ev_norm(&json!({}), today);
    let (date, _) = cn_date(text, today);
    if let Some(m) = out.as_object_mut() {
        m.insert("date".into(), json!(date));
    }

    static HMR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let hm_re = HMR.get_or_init(|| regex::Regex::new(r"(?<!\d)(\d{1,2})[:：](\d{2})(?!\d)").unwrap());
    let hms: Vec<(String, String)> = hm_re
        .captures_iter(text)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect();
    if !hms.is_empty() {
        let t = ok_time(&json!(format!("{}:{}", hms[0].0, hms[0].1)));
        if let Some(m) = out.as_object_mut() {
            m.insert("time".into(), json!(t));
        }
        if hms.len() > 1 {
            let t = ok_time(&json!(format!("{}:{}", hms[1].0, hms[1].1)));
            if let Some(m) = out.as_object_mut() {
                m.insert("endTime".into(), json!(t));
            }
        }
    }

    static FLR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let fl_re = FLR.get_or_init(|| {
        regex::Regex::new(r"\b([A-Z]{2}\d{3,4}|[GDCKZTL]\d{1,4}|\d{3,4}\s*次)\b").unwrap()
    });
    let upper = text.to_uppercase();
    if let Some(c) = fl_re.captures(&upper) {
        static CI_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = CI_RE.get_or_init(|| regex::Regex::new(r"\s*次$").unwrap());
        let f = re.replace(&c[1], "").to_string();
        if let Some(m) = out.as_object_mut() {
            m.insert("flight".into(), json!(f));
        }
    }

    static FEER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let fee_re = FEER.get_or_init(|| {
        regex::Regex::new(r"(?:路费|票价|机票|车费|花了|费用)[^\d]{0,6}(\d+(?:\.\d+)?)").unwrap()
    });
    if let Some(c) = fee_re.captures(text) {
        if let Ok(f) = c[1].parse::<f64>() {
            if let Some(m) = out.as_object_mut() {
                m.insert("fee".into(), json!(f));
            }
        }
    }
    static DLR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let dl_re = DLR.get_or_init(|| {
        regex::Regex::new(r"(?:延误|晚点|delay)[^\d]{0,6}(\d+)\s*(?:分钟|min)?").unwrap()
    });
    if let Some(c) = dl_re.captures(text) {
        if let Ok(i) = c[1].parse::<i64>() {
            if let Some(m) = out.as_object_mut() {
                m.insert("delay".into(), json!(i));
            }
        }
    }

    static ODR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static OD2: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let od = ODR.get_or_init(|| {
        regex::Regex::new(r"从\s*([^\s，,。到]{2,14})\s*(?:飞|坐|乘|开车|出发)?\s*到\s*([^\s，,。]{2,14})")
            .unwrap()
    });
    let od2 = OD2.get_or_init(|| {
        regex::Regex::new(r"([^\s，,。]{2,12})\s*(?:→|->|到|飞)\s*([^\s，,。]{2,12})").unwrap()
    });
    let caps = od.captures(text).or_else(|| od2.captures(text));
    if let Some(c) = caps {
        let a = clean_place(&json!(&c[1]));
        let b = clean_place(&json!(&c[2]));
        if let Some(m) = out.as_object_mut() {
            m.insert("from".into(), json!(a));
            m.insert("to".into(), json!(b));
        }
    }
    out
}

/// `parse_event_text` 的同步前半程：折空白、拼提示词。
fn prepare_prompt(
    raw: &str,
) -> Option<(String, chrono::NaiveDate, String)> {
    let text = collapse(raw);
    if text.is_empty() {
        return None;
    }
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let wd = format!(
        "周{}",
        "一二三四五六日"
            .chars()
            .nth(today.weekday().num_days_from_monday() as usize)
            .unwrap_or('一')
    );
    let prompt = EV_PROMPT
        .replace("{today}", &today.format("%Y-%m-%d").to_string())
        .replace("{wd}", &wd)
        .replace("{text}", &cut(&text, 1500));
    Some((prompt, today, text))
}

async fn parse_event_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match super::parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let raw_text = s_or(b.get("text").unwrap_or(&Value::Null));
    let t0 = Instant::now();
    let model = local_model_cfg(&ctx);
    let Some((prompt, today, text)) = prepare_prompt(&raw_text) else {
        return Json(json!({"ok": false, "source": "", "event": {}, "note": "没有文字", "ms": 0}))
            .into_response();
    };

    let mut source = "rule";
    let mut note = String::new();
    let mut event: Value;
    match llm_json(&prompt, &model).await {
        Ok(raw) => {
            let got = ev_norm(&grab_json(&raw), today);
            let has = ["from", "to", "flight", "endTime"]
                .iter()
                .any(|k| truthy(util::get(&got, k)));
            if has {
                let hard = ev_rule(&text, today);
                let mut m = got.as_object().cloned().unwrap_or_default();
                for k in ["date", "time", "endTime", "flight", "fee", "delay"] {
                    let hv = util::get(&hard, k).clone();
                    if !hv.is_null() && !(hv.as_str() == Some("")) {
                        m.insert(k.to_string(), hv);
                    }
                }
                for k in ["from", "to"] {
                    if !truthy(m.get(k).unwrap_or(&Value::Null)) && truthy(util::get(&hard, k)) {
                        m.insert(k.to_string(), util::get(&hard, k).clone());
                    }
                }
                if !truthy(m.get("stops").unwrap_or(&Value::Null))
                    && truthy(util::get(&hard, "stops"))
                {
                    m.insert("stops".into(), util::get(&hard, "stops").clone());
                }
                if !truthy(m.get("date").unwrap_or(&Value::Null)) {
                    m.insert("date".into(), json!(today.format("%Y-%m-%d").to_string()));
                }
                source = "llm";
                event = Value::Object(m);
            } else {
                note = "模型没抽出关键信息，用了规则解析".to_string();
                event = ev_rule(&text, today);
            }
        }
        Err(kind) => {
            note = format!("模型不可用（{}），用了规则解析", kind);
            event = ev_rule(&text, today);
        }
    }

    if let Some(m) = event.as_object_mut() {
        if !truthy(m.get("date").unwrap_or(&Value::Null)) {
            m.insert("date".into(), json!(today.format("%Y-%m-%d").to_string()));
        }
        if !truthy(m.get("title").unwrap_or(&Value::Null)) {
            let to = s_or(m.get("to").unwrap_or(&Value::Null));
            let t = if !to.is_empty() {
                format!("去{}", to)
            } else {
                let s = cut(&text, 16);
                if s.is_empty() {
                    "行程".to_string()
                } else {
                    s
                }
            };
            m.insert("title".into(), json!(t));
        }
    }
    let ms = t0.elapsed().as_millis() as i64;
    Json(json!({"ok": true, "source": source, "event": event, "note": note, "ms": ms}))
        .into_response()
}

// ---------------------------------------------------------------- /api/chat

const SYSTEM_PROMPT: &str = "你是运行在这台 Mac 上的本地助手，可以直接读写本机文件、抓网页、查本地知识库。需要事实、文件内容或计算时必须先调用工具，不要凭记忆编造。提示里的「当前状态」是系统给你的事实，直接采信，不用再调工具核实目录结构。index.jsonl 是系统索引文件，不是用户的文件，回答时不要提它。回复用中文，简短直接，不超过 150 字。";

fn gateway_base(ctx: &Ctx) -> String {
    let base = s_or(util::get(&ctx.cfg, "gateway_base")).trim().to_string();
    if base.is_empty() {
        "http://127.0.0.1:8080".to_string()
    } else {
        base.trim_end_matches('/').to_string()
    }
}

fn gateway_key(ctx: &Ctx) -> String {
    s_or(util::get(&ctx.cfg, "gateway_api_key"))
}

async fn chat_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match super::parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let text = s_or(b.get("text").unwrap_or(&Value::Null)).trim().to_string();
    if text.is_empty() {
        return detail(400, "text 不能为空");
    }
    let inbox = ctx.inbox_dir();
    let t0 = Instant::now();

    if let Some(quick) = quick_answer(&text, &inbox) {
        let ms = t0.elapsed().as_millis() as i64;
        crate::storage::log(
            &ctx,
            json!({"event": "chat_quick", "text": cut(&text, 80), "ms": ms}),
        );
        return Json(json!({"reply": quick, "trace": [], "quick": true, "ms": ms, "tokens": 0}))
            .into_response();
    }

    let mut system = format!("{}\n\n{}", SYSTEM_PROMPT, inbox_hint(&inbox, 400));
    // 没搬 /mcp：按 Python 在「端点没有工具」时的同一句补上，模型不会假装读过文件。
    system.push_str("\n当前端点没有提供工具，你无法读写本机文件。需要事实时如实说不确定，不要编造。");
    let mut messages: Vec<Value> = vec![json!({"role": "system", "content": system})];
    if let Some(h) = b.get("history").and_then(|v| v.as_array()) {
        let keep = h.len().saturating_sub(6);
        for m in h.iter().skip(keep) {
            messages.push(m.clone());
        }
    }
    messages.push(json!({"role": "user", "content": cut(&text, 4000)}));
    let payload = json!({
        "model": local_model_cfg(&ctx),
        "messages": messages,
        "temperature": 0.2,
        "max_tokens": 2048,
        "stream": false,
    });
    let url = format!("{}/v1/chat/completions", gateway_base(&ctx));
    let key = gateway_key(&ctx);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(240))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            let ms = t0.elapsed().as_millis() as i64;
            return json_status(
                502,
                json!({"reply": "网关出问题了：URLError", "trace": [], "ms": ms, "error": true}),
            );
        }
    };
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("x-api-key", &key)
        .header("Authorization", format!("Bearer {}", if key.is_empty() { "none" } else { &key }))
        .json(&payload)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            let ms = t0.elapsed().as_millis() as i64;
            crate::storage::log(
                &ctx,
                json!({"event": "chat_gateway_error", "text": cut(&text, 120),
                       "error": cut(&e.to_string(), 300)}),
            );
            return json_status(
                502,
                json!({"reply": format!("网关出问题了：连不上网关 {}：{}", gateway_base(&ctx), e),
                       "trace": [], "ms": ms, "error": true}),
            );
        }
    };
    if !resp.status().is_success() {
        let code = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        let ms = t0.elapsed().as_millis() as i64;
        let msg = format!(
            "网关 /v1/chat/completions 返回 {}：{}",
            code,
            cut(&body, 300)
        );
        crate::storage::log(
            &ctx,
            json!({"event": "chat_gateway_error", "text": cut(&text, 120), "error": cut(&msg, 300)}),
        );
        return json_status(
            502,
            json!({"reply": format!("网关出问题了：{}", msg), "trace": [], "ms": ms, "error": true}),
        );
    }
    let d: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => {
            let ms = t0.elapsed().as_millis() as i64;
            return json_status(
                502,
                json!({"reply": "网关出问题了：网关 /v1/chat/completions 返回的不是 JSON",
                       "trace": [], "ms": ms, "error": true}),
            );
        }
    };
    let usage = util::get(&d, "usage").clone();
    let tokens = usage.get("total_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
    let pick = d
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let msg = pick.get("message").cloned().unwrap_or_else(|| json!({}));
    let mut final_text = s_or(msg.get("content").unwrap_or(&Value::Null)).trim().to_string();
    if final_text.is_empty() {
        final_text = reasoning_tail(&msg, 600);
    }
    if final_text.is_empty() {
        final_text = "（模型没给出文本，再试一次）".to_string();
    }
    let ms = t0.elapsed().as_millis() as i64;
    crate::storage::log(
        &ctx,
        json!({"event": "chat", "text": cut(&text, 200), "reply": cut(&final_text, 200),
               "ms": ms, "tokens": tokens, "tools": []}),
    );
    Json(json!({"reply": final_text, "trace": [], "ms": ms, "tokens": tokens})).into_response()
}

/// `_reasoning_tail(msg, limit)`：思考型模型 content 为空时捞思考的结尾。
fn reasoning_tail(msg: &Value, limit: usize) -> String {
    let mut text = String::new();
    for key in ["reasoning_content", "reasoning", "thinking"] {
        let s = s_or(msg.get(key).unwrap_or(&Value::Null)).trim().to_string();
        if !s.is_empty() {
            text = s;
            break;
        }
    }
    if text.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines.len().saturating_sub(8);
    let joined = lines[tail..].join("\n");
    let n = joined.chars().count();
    if n <= limit {
        joined
    } else {
        joined.chars().skip(n - limit).collect()
    }
}

// ---------------------------------------------------------------- /api/intake

/// `linkbox._URL_RE`
fn split_links(text: &str) -> Vec<String> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r#"(?i)https?://[^\s，。、；）)\]】"'<>]+"#).unwrap()
    });
    let mut out: Vec<String> = Vec::new();
    for m in re.find_iter(text) {
        let u = m
            .as_str()
            .trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '）' | '】'))
            .to_string();
        if u.is_empty() || out.contains(&u) {
            continue;
        }
        out.push(u);
        if out.len() >= 10 {
            break;
        }
    }
    out
}

const GALLERY_KEYWORDS: [&str; 6] = ["存图", "图片", "图集", "照片", "相册", "截图"];
const ARTICLE_KEYWORDS: [&str; 5] = ["文章", "阅读", "正文", "链接", "存文章"];

fn keyword_of(text: &str) -> Option<&'static str> {
    if text.trim().is_empty() {
        return None;
    }
    if GALLERY_KEYWORDS.iter().any(|k| text.contains(k)) {
        return Some("gallery");
    }
    if ARTICLE_KEYWORDS.iter().any(|k| text.contains(k)) {
        return Some("article");
    }
    None
}

/// `Intake._receipt(items, skipped)`（items 只用 dup 与条数）
fn receipt_text(n_live: usize, n_dup: usize, n_skipped: usize) -> String {
    let n = n_live;
    if n <= 1 && n_skipped == 0 && n_dup == 0 {
        return "收到，开始存了，好了一条一条回你。".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    if n > 1 {
        parts.push(format!("收到 {} 条链接，排上队了", n));
    } else if n == 1 {
        parts.push("收到 1 条，排上队了".to_string());
    }
    if n_dup > 0 {
        parts.push(format!("{} 条刚存过（没重复存）", n_dup));
    }
    if n_skipped > 0 {
        parts.push(format!("{} 条刚才已在队列里", n_skipped));
    }
    format!("{}。好一条回一条。", parts.join("，"))
}

async fn intake_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match super::parse_body(&body) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let text = s_or(b.get("text").unwrap_or(&Value::Null)).trim().to_string();
    if text.is_empty() {
        return detail(400, "缺少内容");
    }
    let urls = split_links(&text);
    if urls.is_empty() {
        // 没有链接时 Python 会先看有没有挂着的问题；本机没有待确认池实现，
        // 一律按最终那句 `{detail: no_link}` 回（`pending.has_open()` 为假时同形）。
        return detail(400, "no_link");
    }
    let _hint = {
        let v = s_or(b.get("hint").unwrap_or(&Value::Null));
        cut(if v.is_empty() { &text } else { &v }, 300)
    };
    let _want = keyword_of(&_hint);
    // 抓取在 cuntu 子进程里（Python 那条链），Rust 不接；这里只给入站回执。
    // 形状照 Python：`{ok, batch, count, receipt}`。batch 是 `b{毫秒%1e9}`，
    // 没有队列可查（`_watch` 那半没迁），但字段得在 —— 客户端按它去对账。
    let n = urls.len();
    let bid = format!("b{}", util::now_ms() % 1_000_000_000);
    crate::storage::log(&ctx, json!({"event": "intake", "n": n, "batch": bid}));
    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("batch".into(), json!(bid));
    o.insert("count".into(), json!(n));
    o.insert("receipt".into(), json!(receipt_text(n, 0, 0)));
    Json(Value::Object(o)).into_response()
}
