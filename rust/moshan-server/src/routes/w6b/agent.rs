// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：Agent 主体 8 条路由 —— 能力清单 / 报用量 / 写事件 / 读 / 自述 / 探活 / 现状摘要。
// 不做：`/agent` 那一页说明书（不在本泳道清单里）；不写 hostwork 的会话缓存。
//
// 两条通路的区别必须留着：
//   POST /api/agent/event  → `_工作台/agents/events.jsonl`（总线，给别的 agent 读）
//   POST /api/agent/note   → `_本机工作/Agent自述.jsonl`（AI 工作卡读的那份）
// 混一次，卡片就读不到 agent 的活。

use super::bus;
use super::import as scan;
use super::util::{self, get as gv, *};
use crate::paths::Ctx;
use crate::routes::common::{self, peek, CtxState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use axum::routing::{get, post};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/agent/schema", get(schema))
        .route("/api/agent/usage", post(usage))
        .route("/api/agent/event", post(event))
        .route("/api/agent/read", get(read))
        .route("/api/agent/note", post(note))
        .route("/api/trae/note", post(trae_note))
        .route("/api/agent/probe", post(probe))
        .route("/api/agent/brief", get(brief))
}

// ---------------------------------------------------------------- 鉴权

/// `auth_agent`：本机回环**免令牌**。
///
/// 本进程只绑 127.0.0.1（见 main.rs 的 `SocketAddr::from(([127,0,0,1], port))`），
/// 所有连接都是回环，因此 Python 的 `_is_loopback(request)` 在这里恒真 ——
/// 与 `routes/w6a/perms.rs` 的处理同一口径。别的来源根本连不上来，
/// 所以不存在「靠 Host 头冒充本机」的口子。
fn agent_gate() {}

/// `dash._root(cfg)`：`allowed_roots[0]`，空则收件箱的上级（实现在 mod.rs）。
fn data_root(ctx: &Ctx) -> PathBuf {
    super::data_root(ctx)
}

/// FastAPI `int` 查询参数非法时的 422 形状（与 w4b 同一套文案）。
fn bad_query(field: &str, input: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "int_parsing",
            "loc": ["query", field],
            "msg": "Input should be a valid integer, unable to parse string as an integer",
            "input": input,
            "url": "https://errors.pydantic.dev/2.14/v/int_parsing",
        }]})),
    )
        .into_response()
}

/// `JSONResponse({...}, status_code=n)`：业务自回，形状 `{"ok": false, "error": ...}`。
fn json_status(code: u16, v: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(v),
    )
        .into_response()
}

/// `await request.json()`：坏 JSON 等价于 Python 的 ValueError。
fn parse_json(raw: &Bytes) -> Result<Value, ()> {
    if raw.is_empty() {
        return Err(());
    }
    serde_json::from_slice::<Value>(raw).map_err(|_| ())
}

// ---------------------------------------------------------------- 能力清单

const AGENT_FIELDS_USAGE: &[(&str, &str)] = &[
    ("client", "哪个客户端在报（trae / cursor / claude-code），必填"),
    ("in", "输入 token（int，>=0）"),
    ("out", "输出 token（int，>=0）"),
    ("cached", "缓存命中的输入 token（可省）"),
    ("calls", "请求次数，缺省 1"),
    ("model", "模型名，缺省「未知模型」"),
    ("ts", "时间，缺省现在；epoch 秒/毫秒 或 2026-11-21T21:50"),
    ("cost", "真金白银（元），订阅制不知道就别填"),
    ("currency", "缺省 CNY"),
    ("task", "这笔在干什么（120 字内）"),
    ("ref", "会话 id / 链接，方便回查"),
    ("id", "幂等键；不填按内容指纹去重"),
];

const AGENT_FIELDS_EVENT: &[(&str, &str)] = &[
    ("ns", "归档槽位：设备 / 伙食 / 账目 / 工作 / 阅读 / 系统 / 杂项"),
    ("type", "note / metric / task / link / file / span，缺省 note"),
    ("title", "一句话标题（120 字内）"),
    ("text", "正文（4000 字内，别贴大段材料）"),
    ("data", "结构化字段（对象或数组）"),
    ("tags", "字符串数组，最多 8 个"),
    ("ts", "同用量"),
    ("id", "幂等键"),
];

fn fields_map(pairs: &[(&str, &str)]) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), json!(v));
    }
    Value::Object(m)
}

async fn schema(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    agent_gate();
    let root = data_root(&ctx);
    let mut files = Map::new();
    files.insert("usage".into(), json!(bus::usage_path(&root).display().to_string()));
    files.insert("events".into(), json!(bus::events_path(&root).display().to_string()));

    let mut ep_read_query = Map::new();
    ep_read_query.insert("ns".into(), json!("all|usage|events"));
    ep_read_query.insert("since".into(), json!("24h / 7d / today / 日期"));
    ep_read_query.insert("limit".into(), json!("1-1000，缺省 100"));
    ep_read_query.insert("client".into(), json!("按客户端过滤"));
    ep_read_query.insert("q".into(), json!("全文关键词"));

    let endpoints = json!([
        {"method": "POST", "path": "/api/agent/usage",
         "what": "报一笔 token 用量", "body": fields_map(AGENT_FIELDS_USAGE)},
        {"method": "POST", "path": "/api/agent/event",
         "what": "写一条事件", "body": fields_map(AGENT_FIELDS_EVENT)},
        {"method": "GET", "path": "/api/agent/read",
         "what": "读自报账", "query": Value::Object(ep_read_query)},
        {"method": "GET", "path": "/api/agent/brief",
         "what": "中台现状摘要（token / 花费 / 最近工作 / 设备数）"},
    ]);

    let mut o = Map::new();
    o.insert("ok".into(), json!(true));
    o.insert("v".into(), json!(bus::V));
    o.insert("service".into(), json!("个人中台"));
    o.insert("base".into(), json!("http://127.0.0.1:8323"));
    o.insert("doc".into(), json!("/agent"));
    o.insert("root".into(), json!(root.display().to_string()));
    o.insert("files".into(), Value::Object(files));
    o.insert("endpoints".into(), endpoints);
    o.insert(
        "notes".into(),
        json!([
            "本机 127.0.0.1 免令牌；别的来源带 x-token",
            "只追加不改写；重复 id 自动吞掉",
            format!(
                "单条正文上限 {} 字，一次最多 {} 条",
                bus::MAX_TEXT,
                bus::MAX_BATCH
            ),
        ]),
    );
    Json(Value::Object(o)).into_response()
}

// ---------------------------------------------------------------- 写：用量 / 事件

async fn usage(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let payload = match parse_json(&body) {
        Ok(v) => v,
        Err(_) => return json_status(400, json!({"ok": false, "error": "body 不是 JSON"})),
    };
    if !payload.is_object() && !payload.is_array() {
        return json_status(400, json!({"ok": false, "error": "body 要是对象或数组"}));
    }
    let root = data_root(&ctx);
    let res = bus::add_usage(&root, &payload, "agent");
    let ok = res.get("ok").map(truthy).unwrap_or(false);
    let bad = res.get("bad").and_then(|x| x.as_i64()).unwrap_or(0);
    if !ok && bad != 0 {
        let mut v = res.clone();
        if let Some(o) = v.as_object_mut() {
            o.insert(
                "error".into(),
                json!("没有可写的字段：client 必填，in/out/calls 至少一个 >0"),
            );
        }
        return json_status(400, v);
    }
    Json(res).into_response()
}

async fn event(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let payload = match parse_json(&body) {
        Ok(v) => v,
        Err(_) => return json_status(400, json!({"ok": false, "error": "body 不是 JSON"})),
    };
    if !payload.is_object() && !payload.is_array() {
        return json_status(400, json!({"ok": false, "error": "body 要是对象或数组"}));
    }
    let root = data_root(&ctx);
    let res = bus::add_event(&root, &payload, "agent");
    let ok = res.get("ok").map(truthy).unwrap_or(false);
    let bad = res.get("bad").and_then(|x| x.as_i64()).unwrap_or(0);
    if !ok && bad != 0 {
        let mut v = res.clone();
        if let Some(o) = v.as_object_mut() {
            o.insert("error".into(), json!("没有可写的字段：title / text / data 至少一个"));
        }
        return json_status(400, v);
    }
    Json(res).into_response()
}

// ---------------------------------------------------------------- 读

async fn read(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let ns = {
        let v = common::q(&uri, "ns");
        if v.is_empty() { "all".to_string() } else { v }
    };
    let since = {
        let v = common::q(&uri, "since");
        if v.is_empty() { "7d".to_string() } else { v }
    };
    let limit_raw = common::q(&uri, "limit");
    let limit = if limit_raw.trim().is_empty() {
        100i64
    } else {
        match limit_raw.trim().parse::<i64>() {
            Ok(v) => v,
            Err(_) => return bad_query("limit", &limit_raw),
        }
    };
    let q = common::q(&uri, "q");
    let client = common::q(&uri, "client");
    let root = data_root(&ctx);
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let _ = ctx2;
        bus::read(&root, &ns, &since, limit, &q, &client)
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 读取失败", "items": []}));
    Json(out).into_response()
}

// ---------------------------------------------------------------- 自述（AI 工作卡）

fn self_note_path(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_本机工作").join("Agent自述.jsonl")
}

/// `_self_note(request, forced_agent)` —— 落 `_本机工作/Agent自述.jsonl`。
fn self_note(ctx: &Ctx, body: &Value, forced: &str, x_agent: &str) -> Response {
    let task_full = collapse(&s_or(gv(body, "task")));
    let task = cut(&task_full, 80);
    if task.is_empty() {
        return json_status(400, json!({"ok": false, "error": "task 必填"}));
    }
    let agent_raw = {
        let mut s = forced.to_string();
        if s.trim().is_empty() {
            s = s_or(gv(body, "agent"));
        }
        if s.trim().is_empty() {
            s = x_agent.to_string();
        }
        cut(s.trim(), 40)
    };
    let agent = if agent_raw.is_empty() { "未知".to_string() } else { agent_raw };

    let at = {
        let v = gv(body, "at");
        if truthy(v) {
            match fnum(v) {
                Some(f) => f,
                None => (util::now_ms() as f64) / 1000.0,
            }
        } else {
            (util::now_ms() as f64) / 1000.0
        }
    };

    let num = |key: &str| -> i64 {
        let v = gv(body, key);
        if truthy(v) {
            match fnum(v) {
                Some(f) => {
                    let n = f.trunc() as i64;
                    if n > 0 { n } else { 0 }
                }
                None => 0,
            }
        } else {
            0
        }
    };

    let mut rec = Map::new();
    rec.insert("at".into(), json!(util::py_round(at, 1)));
    rec.insert("agent".into(), json!(agent));
    rec.insert("task".into(), json!(task));
    rec.insert("summary".into(), json!(cut(&collapse(&s_or(gv(body, "summary"))), 400)));
    rec.insert("project".into(), json!(cut(s_or(gv(body, "project")).trim(), 120)));
    rec.insert("model".into(), json!(cut(s_or(gv(body, "model")).trim(), 60)));
    rec.insert("tin".into(), json!(num("in")));
    rec.insert("tout".into(), json!(num("out")));
    rec.insert("cached".into(), json!(num("cached")));
    rec.insert("calls".into(), json!(num("calls")));
    rec.insert("source".into(), json!(format!("{} 自述", agent)));

    let p = self_note_path(ctx);
    let mut recent: Vec<String> = Vec::new();
    if p.exists() {
        let lines = util::read_lines(&p);
        let start = lines.len().saturating_sub(80);
        for ln in &lines[start..] {
            let Ok(d) = serde_json::from_str::<Value>(ln.trim()) else { continue };
            if !d.is_object() {
                continue;
            }
            let other_at = match d.get("at") {
                Some(v) if truthy(v) => match fnum(v) {
                    Some(f) => f,
                    None => continue,
                },
                _ => 0.0,
            };
            if (other_at - at).abs() < 300.0 {
                let a = {
                    let v = d.get("agent").cloned().unwrap_or(Value::Null);
                    if truthy(&v) { pystr(&v) } else { "TRAE".into() }
                };
                let t = {
                    let v = d.get("task").cloned().unwrap_or(Value::Null);
                    if truthy(&v) { pystr(&v) } else { String::new() }
                };
                recent.push(format!("{}|{}", a, t));
            }
        }
    }
    let me = format!("{}|{}", agent, rec.get("task").map(|v| pystr(v)).unwrap_or_default());
    if recent.iter().any(|x| *x == me) {
        return Json(json!({"ok": true, "dup": true, "note": Value::Object(rec)})).into_response();
    }

    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .and_then(|mut f| {
            f.write_all(format!("{}\n", util::dumps_line(&Value::Object(rec.clone()))).as_bytes())?;
            f.flush()
        });
    match written {
        Ok(()) => Json(json!({"ok": true, "note": Value::Object(rec)})).into_response(),
        Err(e) => json_status(
            500,
            json!({"ok": false, "error": format!("写不进去：{}", util::py_os_err(&e))}),
        ),
    }
}

async fn note(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_json(&body) {
        Ok(v) => v,
        Err(_) => return json_status(400, json!({"ok": false, "error": "body 不是 JSON"})),
    };
    if !b.is_object() {
        return json_status(400, json!({"ok": false, "error": "body 要是对象"}));
    }
    let x_agent = headers
        .get("x-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    self_note(&ctx, &b, "", &x_agent)
}

async fn trae_note(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_json(&body) {
        Ok(v) => v,
        Err(_) => return json_status(400, json!({"ok": false, "error": "body 不是 JSON"})),
    };
    if !b.is_object() {
        return json_status(400, json!({"ok": false, "error": "body 要是对象"}));
    }
    let x_agent = headers
        .get("x-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    self_note(&ctx, &b, "TRAE", &x_agent)
}

// ---------------------------------------------------------------- 手选目录探活

async fn probe(State(ctx): CtxState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = match parse_json(&body) {
        Ok(v) => v,
        Err(_) => json!({}),
    };
    let path = s_or(gv(&b, "path")).trim().to_string();
    if path.is_empty() {
        return json_status(400, json!({"ok": false, "error": "没给路径"}));
    }
    let out = tokio::task::spawn_blocking(move || scan::probe_manual(&path))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 探活失败"}));
    Json(out).into_response()
}

// ---------------------------------------------------------------- 现状摘要

/// `hostwork.hourly_tokens(day)` 的 Rust 版（今天那 24 根柱子）。
///
/// Python 侧只扫 `~/.workbuddy/projects/*/*.jsonl` 里带 `"usage"` 的行，按
/// 「本机时区的 YYYY-MM-DDTH」分桶；再把 agent 自报账（usage.jsonl）折成同样
/// 的桶并进来。文件按 mtime **倒序**扫（刚说过话的会话先扫），这个顺序决定了
/// 桶内 `models` / `apps` 的键序，不能改。
fn hourly_tokens(day: &str, root: &Path) -> Vec<Value> {
    let wb = util::home().join(".workbuddy").join("projects");
    let now = util::now_secs() as f64;
    let mut cands: Vec<(f64, PathBuf)> = Vec::new();
    for sub in read_dir_unsorted(&wb) {
        if !sub.is_dir() {
            continue;
        }
        for f in read_dir_unsorted(&sub) {
            if !f.is_file() {
                continue;
            }
            if f.extension().map(|e| e == "jsonl").unwrap_or(false) {
                let mt = f
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let _ = now;
                cands.push((mt, f));
            }
        }
    }
    // sorted(key=-mtime)：mtime 相同保持 glob 顺序（稳定）
    cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut agg: Map<String, Value> = Map::new();
    for (_, f) in &cands {
        let hours = scan_usage_file(f);
        merge_hours(&mut agg, &hours);
    }
    // 自报账：key 用 '@' 开头，只为一眼看出不是真文件
    let ah = bus::hour_buckets(root, 30);
    if !ah.is_empty() {
        merge_hours(&mut agg, &ah);
    }

    let mut out: Vec<Value> = Vec::new();
    for h in 0..24 {
        let k = format!("{}T{:02}", day, h);
        let v = agg.get(&k).cloned().unwrap_or_else(|| {
            let mut b = Map::new();
            b.insert("in".into(), json!(0));
            b.insert("out".into(), json!(0));
            b.insert("cached".into(), json!(0));
            b.insert("calls".into(), json!(0));
            b.insert("models".into(), Value::Object(Map::new()));
            b.insert("apps".into(), Value::Object(Map::new()));
            Value::Object(b)
        });
        let mut o = Map::new();
        o.insert("key".into(), json!(k));
        o.insert("h".into(), json!(format!("{:02}", h)));
        for f in ["in", "out", "cached", "calls"] {
            o.insert(f.into(), v.get(f).cloned().unwrap_or(json!(0)));
        }
        o.insert("models".into(), v.get("models").cloned().unwrap_or(json!({})));
        o.insert("apps".into(), v.get("apps").cloned().unwrap_or(json!({})));
        out.push(Value::Object(o));
    }
    out
}

/// 目录项按 `read_dir` 原生顺序（不排序，与 `os.scandir`/`glob` 同源）。
fn read_dir_unsorted(p: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(p) else { return Vec::new() };
    rd.filter_map(|e| e.ok().map(|x| x.path())).collect()
}

/// `_hour_scan_file`：一个会话文件 → 小时桶（只看带 `"usage"` 的行）。
fn scan_usage_file(path: &Path) -> Map<String, Value> {
    let Ok(bytes) = std::fs::read(path) else { return Map::new() };
    let text = String::from_utf8_lossy(&bytes).to_string();
    let mut hours: Map<String, Value> = Map::new();
    for raw in text.split('\n') {
        if !raw.contains("\"usage\"") {
            continue;
        }
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(s) else { continue };
        let pd = d.get("providerData").cloned().unwrap_or(json!({}));
        let u = pd.get("usage").cloned().unwrap_or(Value::Null);
        if !u.is_object() {
            continue;
        }
        if !truthy(u.get("totalTokens").unwrap_or(&Value::Null)) {
            continue;
        }
        let ts = util::int0(d.get("timestamp").unwrap_or(&Value::Null));
        if ts == 0 {
            continue;
        }
        let k: String = util::strftime_ms(ts, "%Y-%m-%dT%H");
        let tin = util::int0(u.get("inputTokens").or_else(|| u.get("input_tokens")).unwrap_or(&Value::Null));
        let tout = util::int0(u.get("outputTokens").or_else(|| u.get("output_tokens")).unwrap_or(&Value::Null));
        let mut cache = 0i64;
        if let Some(det) = u.get("inputTokensDetails") {
            if let Some(a) = det.as_array() {
                if let Some(first) = a.first() {
                    if first.is_object() {
                        cache = util::int0(first.get("cached_tokens").unwrap_or(&Value::Null));
                    }
                }
            }
        }
        let model = {
            let m = pd.get("model").cloned().unwrap_or(Value::Null);
            let m = if truthy(&m) {
                m
            } else {
                let n = pd.get("requestModelName").cloned().unwrap_or(Value::Null);
                if truthy(&n) { n } else { json!("未知模型") }
            };
            pystr(&m)
        };
        let cur = hours.entry(k).or_insert_with(|| {
            let mut b = Map::new();
            b.insert("in".into(), json!(0));
            b.insert("out".into(), json!(0));
            b.insert("cached".into(), json!(0));
            b.insert("calls".into(), json!(0));
            b.insert("models".into(), Value::Object(Map::new()));
            b.insert("apps".into(), Value::Object(Map::new()));
            Value::Object(b)
        });
        let b = cur.as_object_mut().unwrap();
        bump_num(b, "in", tin);
        bump_num(b, "out", tout);
        bump_num(b, "cached", cache);
        bump_num(b, "calls", 1);
        let models = b.get_mut("models").unwrap().as_object_mut().unwrap();
        let mm = models.entry(model).or_insert_with(|| {
            let mut x = Map::new();
            x.insert("in".into(), json!(0));
            x.insert("out".into(), json!(0));
            x.insert("cached".into(), json!(0));
            x.insert("calls".into(), json!(0));
            Value::Object(x)
        });
        let mm = mm.as_object_mut().unwrap();
        bump_num(mm, "in", tin);
        bump_num(mm, "out", tout);
        bump_num(mm, "cached", cache);
        bump_num(mm, "calls", 1);
        let apps = b.get_mut("apps").unwrap().as_object_mut().unwrap();
        let ap = apps.entry("workbuddy".to_string()).or_insert_with(|| {
            let mut x = Map::new();
            x.insert("in".into(), json!(0));
            x.insert("out".into(), json!(0));
            x.insert("cached".into(), json!(0));
            x.insert("calls".into(), json!(0));
            Value::Object(x)
        });
        let ap = ap.as_object_mut().unwrap();
        bump_num(ap, "in", tin);
        bump_num(ap, "out", tout);
        bump_num(ap, "cached", cache);
        bump_num(ap, "calls", 1);
    }
    // 老桶扔掉（30 天）
    let keep: Vec<String> = (0..30)
        .map(|i| {
            let d = chrono::Local::now().date_naive() - chrono::Duration::days(i);
            d.format("%Y-%m-%d").to_string()
        })
        .collect();
    hours.retain(|k, _| {
        let day: String = k.chars().take(10).collect();
        keep.iter().any(|x| *x == day)
    });
    hours
}

fn bump_num(m: &mut Map<String, Value>, k: &str, n: i64) {
    let cur = m.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
    m.insert(k.to_string(), json!(cur + n));
}

/// 把一份小时桶并进总账，保持「先到先得」的键序（与 Python 的 dict.setdefault 同）。
fn merge_hours(agg: &mut Map<String, Value>, hours: &Map<String, Value>) {
    for (k, v) in hours {
        let cur = agg.entry(k.clone()).or_insert_with(|| {
            let mut b = Map::new();
            b.insert("in".into(), json!(0));
            b.insert("out".into(), json!(0));
            b.insert("cached".into(), json!(0));
            b.insert("calls".into(), json!(0));
            b.insert("models".into(), Value::Object(Map::new()));
            b.insert("apps".into(), Value::Object(Map::new()));
            Value::Object(b)
        });
        let c = cur.as_object_mut().unwrap();
        for f in ["in", "out", "cached", "calls"] {
            bump_num(c, f, v.get(f).and_then(|x| x.as_i64()).unwrap_or(0));
        }
        for dim in ["models", "apps"] {
            let src = v.get(dim).and_then(|x| x.as_object());
            let Some(src) = src else { continue };
            let dst = c.get_mut(dim).unwrap().as_object_mut().unwrap();
            for (name, mv) in src {
                let e = dst.entry(name.clone()).or_insert_with(|| {
                    let mut x = Map::new();
                    x.insert("in".into(), json!(0));
                    x.insert("out".into(), json!(0));
                    x.insert("cached".into(), json!(0));
                    x.insert("calls".into(), json!(0));
                    Value::Object(x)
                });
                let e = e.as_object_mut().unwrap();
                for f in ["in", "out", "cached", "calls"] {
                    bump_num(e, f, mv.get(f).and_then(|x| x.as_i64()).unwrap_or(0));
                }
            }
        }
    }
}

async fn brief(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let days_raw = common::q(&uri, "days");
    let days = if days_raw.trim().is_empty() {
        7i64
    } else {
        match days_raw.trim().parse::<i64>() {
            Ok(v) => v,
            Err(_) => return bad_query("days", &days_raw),
        }
    };
    let root = data_root(&ctx);
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let days = days.clamp(1, 90);
        let mut o = Map::new();
        o.insert("ok".into(), json!(true));
        o.insert("at".into(), json!(util::strftime_now("%Y-%m-%d %H:%M:%S")));
        o.insert("root".into(), json!(root.display().to_string()));
        o.insert("doc".into(), json!("/agent"));
        let mut sr = Map::new();
        sr.insert("days".into(), json!(days));
        sr.insert("totals".into(), bus::totals(&root, days));
        let recent = bus::read(&root, "all", &format!("{}d", days), 8, "", "");
        sr.insert(
            "recent".into(),
            recent.get("items").cloned().unwrap_or(json!([])),
        );
        o.insert("self_report".into(), Value::Object(sr));

        // 本机真实 token（WorkBuddy 的账，算今天的）
        let today = util::strftime_now("%Y-%m-%d");
        let hrs = hourly_tokens(&today, &root);
        let mut t_in = 0i64;
        let mut t_out = 0i64;
        let mut t_calls = 0i64;
        let mut apps: Vec<String> = Vec::new();
        for h in &hrs {
            t_in += h.get("in").and_then(|x| x.as_i64()).unwrap_or(0);
            t_out += h.get("out").and_then(|x| x.as_i64()).unwrap_or(0);
            t_calls += h.get("calls").and_then(|x| x.as_i64()).unwrap_or(0);
            if let Some(m) = h.get("apps").and_then(|x| x.as_object()) {
                for k in m.keys() {
                    if !apps.iter().any(|a| a == k) {
                        apps.push(k.clone());
                    }
                }
            }
        }
        apps.sort();
        let mut tt = Map::new();
        tt.insert("in".into(), json!(t_in));
        tt.insert("out".into(), json!(t_out));
        tt.insert("calls".into(), json!(t_calls));
        tt.insert("apps".into(), json!(apps));
        o.insert("today_tokens".into(), Value::Object(tt));

        // `recent_work` 要的是 hostwork.rounds_items("today")（逐轮记录），
        // 那一层是 w5a 泳道的会话解析器，本泳道没迁，所以这里**没有这个键**。
        // 其余字段（self_report / today_tokens / devices）与 Python 一致。

        // 设备台账：只数个数，明细走设备服务（8778）
        let arc = root.join("13-设备").join("档案");
        let n = read_dir_unsorted(&arc)
            .iter()
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .count();
        o.insert(
            "devices".into(),
            json!({"n": n, "detail": "http://127.0.0.1:8778/"}),
        );
        let _ = ctx2;
        Value::Object(o)
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "RuntimeError: 摘要失败"}));
    Json(out).into_response()
}
