// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：工作记录（`moshan/dash.py` 的 read_worklog / append_worklog / _worklog）
//       GET  /api/worklog  两份账：左卡 items（中台在做的事）+ 右卡 host（本机别的活）
//       POST /api/worklog  补记一条（落 `_工作台/worklog.json`）
// 不做：**自动采集层没迁**。左卡的 `_auto_worklog`（pocket.jsonl）、`_plugin_items`
//       （设备/游戏/影音/应用的账）和右卡的 `_agent_items`（hostwork 解析各 AI 客户端
//       会话流水）都不在本泳道里 —— 它们属于 dash / hostwork 两条模块，动辄上千行。
//       所以 `items` 恒为空数组、`host` 只含「补记」这一类；补记的字段、顺序、
//       合并（_collapse）与落盘格式与 Python 逐字节一致。详见汇报「已知分叉」。

use super::util::{clip, now_ymd_hms, py_str, str_or, sv, truthy};
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Mutex;

const WL_KEEP: usize = 300;
const WL_LIMIT: usize = 90;
const WL_HOST_LIMIT: usize = 80;

fn root(ctx: &Ctx) -> PathBuf {
    ctx.allowed_roots()
        .into_iter()
        .next()
        .unwrap_or_else(|| ctx.library_dir.clone())
}

fn worklog_path(ctx: &Ctx) -> PathBuf {
    root(ctx).join("_工作台").join("worklog.json")
}

fn load(p: &std::path::Path) -> Value {
    jsonio::read_json(p).unwrap_or(json!({}))
}

/// `_iso`：`2026-11-18T16:45:09+0800` → `2026-11-18 16:45:09`
fn iso(v: Option<&Value>) -> String {
    let s = {
        let raw = v.map(|x| if truthy(x) { x.clone() } else { json!("") }).unwrap_or(json!(""));
        py_str(&raw).trim().to_string()
    };
    let s = s.replace('T', " ");
    if s.chars().count() >= 19 {
        s.chars().take(19).collect()
    } else {
        s
    }
}

fn secs(a: &str, b: &str) -> Option<i64> {
    let pa = chrono::NaiveDateTime::parse_from_str(
        &a.chars().take(19).collect::<String>(),
        "%Y-%m-%d %H:%M:%S",
    )
    .ok()?;
    let pb = chrono::NaiveDateTime::parse_from_str(
        &b.chars().take(19).collect::<String>(),
        "%Y-%m-%d %H:%M:%S",
    )
    .ok()?;
    Some((pb - pa).num_seconds().max(0))
}

fn code_src(name: &str) -> Value {
    json!({ "kind": "code", "name": name })
}

fn ai_src(name: &str) -> Value {
    json!({ "kind": "ai", "name": name })
}

fn tool_src(name: &str) -> Value {
    json!({ "kind": "tool", "name": name })
}

/// `read_worklog`：补记文件 → 页面口径（截断、默认值、来源标签都在这里定）
pub fn read_worklog(ctx: &Ctx) -> Vec<Value> {
    let d = load(&worklog_path(ctx));
    let raw = d.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for it in raw {
        if !it.is_object() {
            continue;
        }
        let task_raw = it.get("task").map(py_str).unwrap_or_default();
        if task_raw.trim().is_empty() {
            continue;
        }
        let mut src: Vec<Value> = Vec::new();
        let code = str_or(it.get("code"));
        if !code.is_empty() {
            src.push(code_src(&clip(&code, 40)));
        }
        let ai = str_or(it.get("ai"));
        if !ai.is_empty() {
            src.push(ai_src(&clip(&ai, 40)));
        }
        let st = iso(it.get("start"));
        let en = iso(it.get("end"));
        let status = match str_or(it.get("status")).as_str() {
            "ok" | "fail" | "run" | "skip" => str_or(it.get("status")),
            _ => "ok".to_string(),
        };
        let id = {
            let v = str_or(it.get("id"));
            if v.is_empty() {
                format!("w{st}")
            } else {
                v
            }
        };
        let detail = str_or(it.get("detail"));
        let client = {
            let c = str_or(it.get("client"));
            if c.is_empty() {
                "手记".to_string()
            } else {
                clip(&c, 40)
            }
        };
        out.push(json!({
            "id": id,
            "task": clip(&task_raw, 80),
            "detail": clip(&detail, 200),
            "status": status,
            "start": st,
            "end": en,
            "secs": if !st.is_empty() && !en.is_empty() { secs(&st, &en).map(|v| json!(v)).unwrap_or(Value::Null) } else { Value::Null },
            "sources": src,
            "manual": true,
            "client": client,
        }));
    }
    out
}

static NOTES_LOCK: Mutex<()> = Mutex::new(());

/// `append_worklog`：kind = code / ai / both；who 里带 `+`、`/`、`、` 会拆成两个来源。
pub fn append_worklog(ctx: &Ctx, body: &Value) -> Value {
    let task = clip(&str_or(body.get("task")).trim().to_string(), 80);
    if task.is_empty() {
        return json!({ "ok": false, "error": "先说清楚做了什么" });
    }
    let mut status = str_or(body.get("status"));
    if status.is_empty() {
        status = "ok".to_string();
    }
    if !["ok", "fail", "run", "skip"].contains(&status.as_str()) {
        status = "ok".to_string();
    }
    let mut kind = str_or(body.get("kind"));
    if kind.is_empty() {
        kind = "ai".to_string();
    }
    let who = clip(&str_or(body.get("who")).trim().to_string(), 60);
    let client = clip(&str_or(body.get("client")).trim().to_string(), 40);
    let parts: Vec<String> = who
        .split(['+', '＋', ',', '，', '/', '、'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let (mut code, mut ai) = (String::new(), String::new());
    match kind.as_str() {
        "both" => {
            if parts.len() >= 2 {
                code = parts[0].clone();
                ai = parts[1].clone();
            } else if let Some(p) = parts.first() {
                code = p.clone();
                ai = p.clone();
            }
        }
        "code" => code = if who.is_empty() { "脚本".to_string() } else { who.clone() },
        _ => ai = if who.is_empty() { "AI".to_string() } else { who.clone() },
    }

    let now = now_ymd_hms();
    let start = {
        let s = iso(body.get("start"));
        if s.is_empty() { now.clone() } else { s }
    };
    let end = if status == "run" {
        String::new()
    } else {
        let e = iso(body.get("end"));
        if e.is_empty() { now.clone() } else { e }
    };
    let iid = format!("w{}", super::util::rand_hex(10));
    let item = json!({
        "id": iid,
        "task": task,
        "detail": clip(&str_or(body.get("detail")), 200),
        "status": status,
        "start": start,
        "end": end,
        "code": code,
        "ai": ai,
        "client": client,
        "created": now,
    });

    let p = worklog_path(ctx);
    let _guard = NOTES_LOCK.lock();
    let d = load(&p);
    let mut items = d.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    items.push(item.clone());
    let start_idx = items.len().saturating_sub(WL_KEEP);
    let items: Vec<Value> = items[start_idx..].to_vec();
    let doc = json!({ "items": items, "updated_at": now });
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = jsonio::atomic_write(&p, &jsonio::dumps(&doc, 1));
    json!({ "ok": true, "item": item })
}

/// `_manual_host_items`：补记的客户端名提到来源最前面
fn manual_host_items(ctx: &Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    for it in read_worklog(ctx) {
        let client = str_or(it.get("client"));
        let mut srcs: Vec<Value> = Vec::new();
        if !client.is_empty() {
            srcs.push(tool_src(&client));
        }
        if let Some(list) = it.get("sources").and_then(|v| v.as_array()) {
            srcs.extend(list.iter().cloned());
        }
        let mut obj = it;
        if let Value::Object(m) = &mut obj {
            m.insert("sources".into(), Value::Array(srcs));
        }
        out.push(obj);
    }
    out
}

/// `_collapse`：同一天 / 同一任务 / 同一结果折成一条「×N」，明细进 children。
fn collapse(items: Vec<Value>) -> Vec<Value> {
    let mut buckets: Vec<(String, Vec<Value>)> = Vec::new();
    let mut order: Vec<String> = Vec::new();
    for it in &items {
        if str_or(it.get("status")) == "run" {
            continue;
        }
        let start = str_or(it.get("start"));
        let day: String = start.chars().take(10).collect();
        let key = format!(
            "{}|{}|{}|{}",
            day,
            str_or(it.get("task")),
            str_or(it.get("status")),
            str_or(it.get("plugin")),
        );
        match buckets.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(it.clone()),
            None => {
                order.push(key.clone());
                buckets.push((key, vec![it.clone()]));
            }
        }
    }
    let mut out: Vec<Value> = items
        .iter()
        .filter(|it| str_or(it.get("status")) == "run")
        .cloned()
        .collect();
    for key in &order {
        let Some(pos) = buckets.iter().position(|(k, _)| k == key) else {
            continue;
        };
        let mut g = buckets[pos].1.clone();
        g.sort_by_key(|x| str_or(x.get("start")));
        let Some(last) = g.last().cloned() else { continue };
        let mut head = last.clone();
        let n = g.len();
        let start = sv(&last.get("start").cloned().unwrap_or(json!("")));
        let end = {
            let e = sv(&last.get("end").cloned().unwrap_or(json!("")));
            if e.is_empty() {
                start.clone()
            } else {
                e
            }
        };
        let first = sv(&g[0].get("start").cloned().unwrap_or(json!("")));
        let head_secs = if !start.is_empty() && !end.is_empty() {
            secs(&start, &end).map(|v| json!(v)).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let tokens = if n > 1
            && g.iter()
                .any(|x| x.get("tokens").map(|v| v.is_object()).unwrap_or(false))
        {
            let mut tin = 0i64;
            let mut tout = 0i64;
            let mut tcached = 0i64;
            let mut ttotal = 0i64;
            for x in &g {
                let t = x.get("tokens").cloned().unwrap_or(json!({}));
                tin += t.get("in").and_then(|v| v.as_i64()).unwrap_or(0);
                tout += t.get("out").and_then(|v| v.as_i64()).unwrap_or(0);
                tcached += t.get("cached").and_then(|v| v.as_i64()).unwrap_or(0);
                ttotal += t.get("total").and_then(|v| v.as_i64()).unwrap_or(0);
            }
            Some(json!({"in": tin, "out": tout, "cached": tcached, "total": ttotal}))
        } else {
            None
        };
        let d0 = sv(&head.get("detail").cloned().unwrap_or(json!("")));
        let detail = if d0.is_empty() {
            String::new()
        } else {
            format!("最近：{d0}")
        };
        let tail: Vec<Value> = g
            .iter()
            .rev()
            .take(12)
            .map(|x| {
                json!({
                    "start": sv(&x.get("start").cloned().unwrap_or(json!(""))),
                    "end": sv(&x.get("end").cloned().unwrap_or(json!(""))),
                    "detail": clip(&sv(&x.get("detail").cloned().unwrap_or(json!(""))), 120),
                    "status": str_or(x.get("status")),
                    "link": match x.get("link") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => Value::Null,
                    },
                })
            })
            .collect();

        if let Value::Object(m) = &mut head {
            m.insert("start".into(), json!(start));
            m.insert("end".into(), json!(end));
            m.insert("first".into(), json!(first));
            m.insert("secs".into(), head_secs);
            if n > 1 {
                m.insert("count".into(), json!(n));
                if let Some(tk) = tokens {
                    m.insert("tokens".into(), tk);
                }
                m.insert("detail".into(), json!(clip(&detail, 120)));
                m.insert("children".into(), Value::Array(tail));
            }
        }
        out.push(head);
    }
    out
}

/// `plugin_manifest`：口袋自己 + `_工作台/plugins.json` 里登记过的
fn plugin_manifest(ctx: &Ctx) -> Vec<Value> {
    let mut out = vec![json!({
        "id": "pocket", "name": "口袋", "glyph": "", "url": "", "n": 0, "fail": 0,
    })];
    let d = load(&root(ctx).join("_工作台").join("plugins.json"));
    for p in d.get("plugins").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        if !p.is_object() {
            continue;
        }
        let pid = str_or(p.get("id")).trim().to_string();
        if pid.is_empty() || truthy(p.get("soon").unwrap_or(&Value::Null)) {
            continue;
        }
        let name = {
            let n = str_or(p.get("name"));
            if n.is_empty() { pid.clone() } else { n }
        };
        out.push(json!({
            "id": pid,
            "name": name,
            "glyph": str_or(p.get("glyph")),
            "url": str_or(p.get("url")),
            "n": 0,
            "fail": 0,
        }));
    }
    out
}

/// `_worklog` 的壳：两张卡、筛选条、合计。
pub fn worklog_view(ctx: &Ctx) -> Value {
    // 左卡：自动采集未迁（见文件头），恒为空
    let items: Vec<Value> = Vec::new();

    let mut plugins: Vec<Value> = plugin_manifest(ctx)
        .into_iter()
        .filter(|p| p.get("id").and_then(|v| v.as_str()) != Some("nest"))
        .collect();
    for it in &items {
        let pid = {
            let p = str_or(it.get("plugin"));
            if p.is_empty() { "pocket".to_string() } else { p }
        };
        let pos = plugins
            .iter()
            .position(|p| p.get("id").and_then(|v| v.as_str()) == Some(pid.as_str()));
        let idx = match pos {
            Some(i) => i,
            None => {
                plugins.push(json!({
                    "id": pid, "name": pid, "glyph": "", "url": "", "n": 0, "fail": 0,
                }));
                plugins.len() - 1
            }
        };
        if let Value::Object(m) = &mut plugins[idx] {
            let n = m.get("n").and_then(|v| v.as_i64()).unwrap_or(0) + 1;
            m.insert("n".into(), json!(n));
            if str_or(it.get("status")) == "fail" {
                let f = m.get("fail").and_then(|v| v.as_i64()).unwrap_or(0) + 1;
                m.insert("fail".into(), json!(f));
            }
        }
    }

    // 右卡：补记（自动那半未迁）
    let mut host = collapse(manual_host_items(ctx));
    host.sort_by(|a, b| {
        let ka = str_or(a.get("status")) == "run";
        let kb = str_or(b.get("status")) == "run";
        kb.cmp(&ka).then_with(|| {
            let ta = {
                let e = str_or(a.get("end"));
                if e.is_empty() { str_or(a.get("start")) } else { e }
            };
            let tb = {
                let e = str_or(b.get("end"));
                if e.is_empty() { str_or(b.get("start")) } else { e }
            };
            tb.cmp(&ta)
        })
    });
    let keep: Vec<Value> = host
        .iter()
        .filter(|x| truthy(x.get("manual").unwrap_or(&Value::Null)))
        .take(24)
        .cloned()
        .collect();
    if !keep.is_empty() {
        let keep_ids: Vec<String> = keep.iter().map(|x| str_or(x.get("id"))).collect();
        let mut rest: Vec<Value> = host
            .iter()
            .filter(|x| !keep_ids.contains(&str_or(x.get("id"))))
            .take(WL_HOST_LIMIT - keep.len())
            .cloned()
            .collect();
        let mut merged = keep;
        merged.append(&mut rest);
        merged.sort_by(|a, b| {
            let ka = str_or(a.get("status")) == "run";
            let kb = str_or(b.get("status")) == "run";
            kb.cmp(&ka).then_with(|| {
                let ta = {
                    let e = str_or(a.get("end"));
                    if e.is_empty() { str_or(a.get("start")) } else { e }
                };
                let tb = {
                    let e = str_or(b.get("end"));
                    if e.is_empty() { str_or(b.get("start")) } else { e }
                };
                tb.cmp(&ta)
            })
        });
        host = merged;
    } else {
        host.truncate(WL_HOST_LIMIT);
    }

    // 筛选条：客户端名 + 条数（稳定排序，同数按首次出现）
    let mut tally: Vec<(String, i64)> = Vec::new();
    for it in &host {
        let c = str_or(it.get("client"));
        match tally.iter_mut().find(|(k, _)| *k == c) {
            Some((_, n)) => *n += 1,
            None => tally.push((c, 1)),
        }
    }
    tally.sort_by(|a, b| b.1.cmp(&a.1));
    let clients: Vec<Value> = tally
        .into_iter()
        .map(|(name, n)| json!({ "name": name, "n": n }))
        .collect();

    let count = |st: &str| items.iter().filter(|x| str_or(x.get("status")) == st).count();
    let ai_n = items
        .iter()
        .filter(|x| {
            x.get("sources")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().any(|s| str_or(s.get("kind")) == "ai"))
                .unwrap_or(false)
        })
        .count();
    let shown: Vec<Value> = items.iter().take(WL_LIMIT).cloned().collect();
    json!({
        "items": shown,
        "host": host,
        "clients": clients,
        "plugins": plugins,
        "total": items.len(),
        "runs": count("run"),
        "ok": count("ok"),
        "fail": count("fail"),
        "skip": count("skip"),
        "ai": ai_n,
        "at": now_ymd_hms(),
    })
}

// ---------------------------------------------------------------- 路由

pub async fn worklog_read(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    Json(worklog_view(&ctx)).into_response()
}

pub async fn worklog_add(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match super::util::strict_body(&body) {
        Ok(v) => v,
        Err((code, b)) => {
            return (axum::http::StatusCode::from_u16(code).unwrap(), Json(b)).into_response()
        }
    };
    Json(append_worklog(&ctx, &body)).into_response()
}

// 字段顺序说明：`_collapse` 的 head 是 `dict(g[-1])`，新键追加在原有键之后；
// Rust 侧用 Map 插入，等价。
