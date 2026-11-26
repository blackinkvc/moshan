// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：逐轮记录（rounds_items）与任务清单（tasks_view / wb_sessions / wb_task_group）。
//       对应 hostwork.py 的 rounds_items / _rounds_compute / _rounds_view /
//       wb_sessions / _span / wb_task_group / tasks_view。
// 不做：不重写会话解析层 —— WB/Cherry/Claude/TRAE 的逐轮都由 sessions.rs 出。

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Mutex;

use super::agg::{kick_scan, scan_at, sessions_snapshot, cache_path};
use super::sessions::{self, session_index, task_title};
use super::util::{py_int, str_or, truthy};
use crate::paths::Ctx;

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn range_days(key: &str) -> i64 {
    match key {
        "today" => 1,
        "7d" => 7,
        "30d" => 30,
        "all" => 0,
        _ => 7,
    }
}

fn day_of(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ rounds

struct RoundsSlot {
    at: f64,
    data: Value,
}

static ROUNDS: Mutex<Vec<(String, RoundsSlot)>> = Mutex::new(Vec::new());

fn rounds_view(data: &Value, range_key: &str) -> Value {
    if range_key != "today" {
        return data.clone();
    }
    let day = chrono::Local::now().format("%Y-%m-%d").to_string();
    let items: Vec<Value> = data
        .get("items")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|x| str_or(x.get("start")).starts_with(&day))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut m = data.as_object().cloned().unwrap_or_default();
    m.insert("items".into(), Value::Array(items));
    Value::Object(m)
}

fn rounds_compute(ctx: &Ctx, range_key: &str) -> Value {
    let days = range_days(range_key);
    let items = sessions::agent_items(&ctx.library_dir, if days == 0 { 365 } else { days });
    json!({"ok": true, "items": items, "at": chrono::Local::now().format("%H:%M:%S").to_string()})
}

/// `rounds_items(range_key)`
pub fn rounds_items_impl(ctx: &Ctx, range_key: &str) -> Value {
    let mut key = range_key.to_string();
    {
        let g = ROUNDS.lock().unwrap_or_else(|e| e.into_inner());
        let mut slot = g.iter().find(|(k, _)| *k == key).map(|(_, s)| (s.at, s.data.clone()));
        if slot.is_none() && key == "today" {
            if let Some((_, s)) = g.iter().find(|(k, _)| k == "7d") {
                slot = Some((s.at, s.data.clone()));
                key = "7d".to_string();
            }
        }
        if let Some((at, data)) = slot {
            if now_secs() - at >= 8.0 {
                refresh_rounds(ctx, &key);
            }
            return rounds_view(&data, range_key);
        }
    }
    let data = rounds_compute(ctx, range_key);
    {
        let mut g = ROUNDS.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|(k, _)| *k != range_key);
        g.push((range_key.to_string(), RoundsSlot { at: now_secs(), data: data.clone() }));
    }
    rounds_view(&data, range_key)
}

fn refresh_rounds(ctx: &Ctx, key: &str) {
    // 后台补算：Python 用过期旧账先回、另起线程；这里用同样的语义。
    // ctx 不可 Clone，只把数据根路径带过去。
    let root = ctx.library_dir.clone();
    let k = key.to_string();
    std::thread::spawn(move || {
        let days = range_days(&k);
        let items = sessions::agent_items(&root, if days == 0 { 365 } else { days });
        let data = json!({"ok": true, "items": items,
                          "at": chrono::Local::now().format("%H:%M:%S").to_string()});
        let mut g = ROUNDS.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|(x, _)| *x != k);
        g.push((k, RoundsSlot { at: now_secs(), data }));
    });
}

// ------------------------------------------------------------------ 任务清单

struct TasksSlot {
    at: f64,
    key: String,
    data: Value,
}

static TASKS: Mutex<Option<TasksSlot>> = Mutex::new(None);

fn wb_db() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".workbuddy")
        .join("workbuddy.db")
}

/// `wb_sessions(limit=80)`
pub fn wb_sessions(limit: usize) -> Vec<Value> {
    let db = wb_db();
    if !db.exists() {
        return Vec::new();
    }
    let sql = format!(
        "select id, coalesce(nullif(custom_title, ''), title) nm, status, \
         coalesce(last_activity_at, updated_at, created_at) ts, space_id, \
         coalesce(nullif(group_title, ''), '') grp \
         from sessions \
         where coalesce(nullif(custom_title, ''), title) is not null \
         and (deleted_at is null or deleted_at < 0) \
         order by ts desc limit {}",
        limit as i64
    );
    let Some(rows) = super::platform::sqlite_rows(&db, &sql, std::time::Duration::from_secs(2)) else {
        return Vec::new();
    };
    let spaces: HashMap<String, String> = super::platform::sqlite_rows(
        &db,
        "select id, name from spaces",
        std::time::Duration::from_secs(2),
    )
    .unwrap_or_default()
    .into_iter()
    .map(|r| (str_or(r.get("id")), str_or(r.get("name"))))
    .collect();
    let mut out = Vec::new();
    for r in rows {
        let name = str_or(r.get("nm")).trim().to_string();
        if name.is_empty() {
            continue;
        }
        out.push(json!({
            "id": str_or(r.get("id")),
            "name": name,
            "status": str_or(r.get("status")),
            "ts": py_int(r.get("ts").unwrap_or(&Value::Null)),
            "space": spaces.get(&str_or(r.get("space_id"))).cloned().unwrap_or_default(),
            "group": str_or(r.get("grp")),
        }));
    }
    out
}

/// `_span(days)`
fn span(days: &Map<String, Value>) -> Value {
    let mut ds: Vec<String> = days.keys().filter(|d| !d.is_empty()).cloned().collect();
    ds.sort();
    let list: Vec<Value> = ds
        .iter()
        .map(|d| json!({"d": d, "a": py_int(days.get(d).unwrap_or(&json!(1))).max(1)}))
        .collect();
    json!({
        "from": ds.first().cloned().unwrap_or_default(),
        "to": ds.last().cloned().unwrap_or_default(),
        "n": ds.len(),
        "days": list,
    })
}

/// `wb_task_group()`
pub fn wb_task_group(ctx: &Ctx) -> Option<Value> {
    let sess = wb_sessions(80);
    if sess.is_empty() {
        return None;
    }
    let mut by_id: HashMap<String, Value> = HashMap::new();
    for a in sessions_snapshot(ctx) {
        let sid = str_or(a.get("id"));
        if !sid.is_empty() {
            by_id.insert(sid, a);
        }
    }
    let mut tasks: Vec<Value> = Vec::new();
    for s in &sess {
        let a = by_id.get(&str_or(s.get("id"))).cloned().unwrap_or_else(|| json!({}));
        let mut utt: Vec<(i64, String)> = a
            .get("utt")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|it| {
                        let pair = it.as_array()?;
                        Some((py_int(pair.first().unwrap_or(&Value::Null)), str_or(pair.get(1))))
                    })
                    .collect()
            })
            .unwrap_or_default();
        utt.sort_by(|a, b| a.0.cmp(&b.0));
        let mut kids: Vec<Value> = Vec::new();
        let start = utt.len().saturating_sub(14);
        for (ts, txt) in &utt[start..] {
            if txt.is_empty() {
                continue;
            }
            let at = if *ts != 0 {
                use chrono::{Local, TimeZone};
                Local
                    .timestamp_millis_opt(*ts)
                    .single()
                    .map(|t| t.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            kids.push(json!({"at": at, "text": txt}));
        }
        let asks = py_int(a.get("utts").unwrap_or(&Value::Null));
        let mut dc: Map<String, Value> = Map::new();
        let mut t_in = 0i64;
        let mut t_out = 0i64;
        let mut t_cached = 0i64;
        let mut t_day: Map<String, Value> = Map::new();
        if let Some(Value::Object(days)) = a.get("days") {
            for (d, row) in days {
                if d.is_empty() || !row.is_object() {
                    continue;
                }
                let n = py_int(row.get("asks").unwrap_or(&Value::Null));
                let v = if n != 0 { n.max(1) } else { py_int(row.get("calls").unwrap_or(&Value::Null)).max(1) };
                dc.insert(d.clone(), json!(v));
                let i0 = py_int(row.get("in").unwrap_or(&Value::Null));
                let o0 = py_int(row.get("out").unwrap_or(&Value::Null));
                t_in += i0;
                t_out += o0;
                t_cached += py_int(row.get("cached").unwrap_or(&Value::Null));
                if i0 != 0 || o0 != 0 {
                    t_day.insert(d.clone(), json!(i0 + o0));
                }
            }
        }
        let mut sp = span(&dc);
        let n = py_int(sp.get("n").unwrap_or(&Value::Null));
        let ts = py_int(s.get("ts").unwrap_or(&Value::Null));
        if n == 0 && ts != 0 {
            let one = day_of(ts);
            sp = json!({"from": one, "to": one, "n": 1, "days": [{"d": one, "a": 1}]});
        }
        let date = {
            let to = str_or(sp.get("to"));
            if !to.is_empty() {
                to
            } else if ts != 0 {
                day_of(ts)
            } else {
                String::new()
            }
        };
        let raw = kids.last().map(|k| str_or(k.get("text"))).unwrap_or_default();
        tasks.push(json!({
            "name": s.get("name").cloned().unwrap_or(Value::Null),
            "date": date,
            "source": "会话",
            "raw": raw,
            "count": kids.len(),
            "asks": asks.max(kids.len() as i64),
            "children": kids,
            "state": s.get("status").cloned().unwrap_or(Value::Null),
            "space": s.get("space").cloned().unwrap_or(Value::Null),
            "span": sp,
            "tok": t_in + t_out,
            "tok_in": t_in,
            "tok_out": t_out,
            "tok_cached": t_cached,
            "tok_day": Value::Object(t_day),
        }));
    }
    Some(json!({"agent": "WorkBuddy", "glyph": "◉", "count": tasks.len(), "tasks": tasks}))
}

/// tasks_view 的桶
struct Bucket {
    name: String,
    raw: String,
    date: String,
    source: String,
    asks: i64,
    count: i64,
    children: Vec<Value>,
    days: Map<String, Value>,
    coarse: bool,
}

fn put(
    buckets: &mut Vec<(String, Vec<Bucket>)>,
    agent: &str,
    name: &str,
    when: &str,
    source: &str,
    raw: &str,
    utt: &str,
    sp: Option<(&str, &str)>,
) {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return;
    }
    let d: String = when.chars().take(10).collect();
    let g = match buckets.iter_mut().find(|(a, _)| a == agent) {
        Some((_, v)) => v,
        None => {
            buckets.push((agent.to_string(), Vec::new()));
            &mut buckets.last_mut().unwrap().1
        }
    };
    let idx = match g.iter().position(|b| b.name == name) {
        Some(i) => i,
        None => {
            g.push(Bucket {
                name: name.to_string(),
                raw: String::new(),
                date: String::new(),
                source: source.to_string(),
                asks: 0,
                count: 0,
                children: Vec::new(),
                days: Map::new(),
                coarse: false,
            });
            g.len() - 1
        }
    };
    let b = &mut g[idx];
    let mut days: Vec<String> = if !d.is_empty() { vec![d] } else { Vec::new() };
    if let Some((f, t)) = sp {
        let a: String = f.chars().take(10).collect();
        let c: String = t.chars().take(10).collect();
        days = [a, c].into_iter().filter(|x| !x.is_empty()).collect();
        let uniq: std::collections::HashSet<&String> = days.iter().collect();
        if uniq.len() >= 2 {
            b.coarse = true;
        }
    }
    for one in days {
        let cur = py_int(b.days.get(&one).unwrap_or(&Value::Null));
        b.days.insert(one.clone(), json!(cur + 1));
        if one > b.date {
            b.date = one;
        }
    }
    if !raw.is_empty() && b.raw.is_empty() {
        b.raw = raw.chars().take(300).collect();
    }
    let one = if !utt.is_empty() { super::agg::utt_text(utt, 110) } else { String::new() };
    if !one.is_empty() {
        let at = {
            let chars: Vec<char> = when.chars().collect();
            if chars.len() >= 16 {
                chars[5..16].iter().collect::<String>()
            } else {
                when.to_string()
            }
        };
        b.children.push(json!({"at": at, "text": one}));
        if b.children.len() > 14 {
            let cut = b.children.len() - 14;
            b.children.drain(..cut);
        }
        b.asks = b.children.len() as i64;
        // Python 的 put 在这时就把 count 插进字典，字段顺序落在 children 之后、
        // _days 之前 —— 少了这一笔，JSON 里 count 会跑到 span 后面。
        b.count = b.children.len() as i64;
    }
}

const TASK_GLYPH: [(&str, &str); 6] = [
    ("TRAE", "◈"),
    ("WorkBuddy", "◉"),
    ("Cherry Studio", "❀"),
    ("Claude Code", "✦"),
    ("Cursor", "▸"),
    ("pocket", "◆"),
];

/// `tasks_view(range_key)`
pub fn tasks_view(ctx: &Ctx, range_key: &str) -> Value {
    let mut now = now_secs();
    {
        let g = TASKS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_ref() {
            if c.key == range_key && now - c.at < 6.0 {
                return c.data.clone();
            }
        }
    }
    let _ = kick_scan(&cache_path(ctx), 6.0);
    now = now_secs();

    let days = range_days(range_key);
    let since_ms = if days != 0 { (now - days as f64 * 86400.0) * 1000.0 } else { 0.0 };

    let mut groups: Vec<Value> = Vec::new();
    if let Some(wb) = wb_task_group(ctx) {
        groups.push(wb);
    }

    let mut buckets: Vec<(String, Vec<Bucket>)> = Vec::new();

    // 1) TRAE 项目会话
    let mut trae_named: std::collections::HashSet<String> = std::collections::HashSet::new();
    for row in super::trae::session_projects() {
        let task = str_or(row.get("task"));
        trae_named.insert(task.clone());
        let when = {
            let to = str_or(row.get("to"));
            if !to.is_empty() { to } else { str_or(row.get("from")) }
        };
        let source = {
            let s = str_or(row.get("source"));
            if s.is_empty() { "项目会话".to_string() } else { s }
        };
        let f = str_or(row.get("from"));
        let t = str_or(row.get("to"));
        put(&mut buckets, "TRAE", &task, &when, &source, "", "", Some((&f, &t)));
    }
    // 2) OCR 侧栏任务名
    for row in super::trae::task_rows(ctx, 0.0) {
        let task = str_or(row.get("task")).trim().to_string();
        if trae_named.contains(&task) {
            continue;
        }
        put(&mut buckets, "TRAE", &task, "", &str_or(row.get("source")), "", "", None);
    }
    // 3) TRAE 逐轮
    for it in sessions::trae_items(&ctx.library_dir, since_ms) {
        let srcs = it.get("sources").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let has_cmd = srcs.iter().any(|s| str_or(s.get("name")) == "命令记录");
        let task = str_or(it.get("task"));
        put(
            &mut buckets,
            "TRAE",
            &task,
            &str_or(it.get("start")),
            if has_cmd { "命令" } else { "进程" },
            "",
            &task,
            None,
        );
    }
    // 4) 其余 agent（有逐轮记录的）
    let rd = rounds_items_impl(ctx, range_key);
    if let Some(items) = rd.get("items").and_then(|v| v.as_array()) {
        for it in items {
            let agent = str_or(it.get("client")).trim().to_string();
            if agent.is_empty() || agent == "TRAE" || agent == "WorkBuddy" {
                continue;
            }
            let raw = str_or(it.get("task"));
            let title = {
                let t = str_or(it.get("title"));
                if t.is_empty() { task_title(&raw) } else { t }
            };
            put(&mut buckets, &agent, &title, &str_or(it.get("start")), "会话", &raw, &raw, None);
        }
    }

    for (agent, tasks) in buckets {
        let mut rows: Vec<Value> = tasks
            .iter()
            .map(|b| {
                let mut m = Map::new();
                m.insert("name".into(), json!(b.name));
                m.insert("raw".into(), json!(b.raw));
                m.insert("date".into(), json!(b.date));
                m.insert("source".into(), json!(b.source));
                m.insert("asks".into(), json!(b.asks));
                m.insert("children".into(), Value::Array(b.children.clone()));
                if b.count != 0 {
                    m.insert("count".into(), json!(b.count));
                }
                m.insert("_days".into(), Value::Object(b.days.clone()));
                m.insert("_coarse".into(), json!(b.coarse));
                Value::Object(m)
            })
            .collect();
        rows.sort_by(|a, b| {
            let ka = (str_or(a.get("date")), a.get("children").and_then(|v| v.as_array()).map(|x| x.len()).unwrap_or(0));
            let kb = (str_or(b.get("date")), b.get("children").and_then(|v| v.as_array()).map(|x| x.len()).unwrap_or(0));
            kb.cmp(&ka)
        });
        let mut out_rows: Vec<Value> = Vec::new();
        for r in rows {
            let mut m = r.as_object().cloned().unwrap_or_default();
            let days = match m.remove("_days") {
                Some(Value::Object(d)) => d,
                _ => Map::new(),
            };
            let mut sp = span(&days);
            let coarse = m.remove("_coarse").map(|v| truthy(&v)).unwrap_or(false);
            if let Some(o) = sp.as_object_mut() {
                o.insert("coarse".into(), json!(coarse));
            }
            m.insert("span".into(), sp);
            if !m.contains_key("count") {
                m.insert("count".into(), json!(m.get("children").and_then(|v| v.as_array()).map(|x| x.len()).unwrap_or(0)));
            }
            if !m.contains_key("asks") {
                m.insert("asks".into(), m.get("count").cloned().unwrap_or(json!(0)));
            }
            let date = str_or(m.get("date"));
            if date.is_empty() {
                let to = str_or(m.get("span").and_then(|s| s.get("to")));
                m.insert("date".into(), json!(to));
            }
            out_rows.push(Value::Object(m));
        }
        let glyph = TASK_GLYPH
            .iter()
            .find(|(a, _)| *a == agent)
            .map(|(_, g)| *g)
            .unwrap_or("●");
        groups.push(json!({"agent": agent, "glyph": glyph,
                           "count": out_rows.len(), "tasks": out_rows}));
    }
    // 稳定排序：count 降序
    groups.sort_by(|a, b| {
        py_int(b.get("count").unwrap_or(&Value::Null)).cmp(&py_int(a.get("count").unwrap_or(&Value::Null)))
    });

    let scan_at_s = {
        let t = scan_at();
        if t != 0.0 {
            use chrono::{Local, TimeZone};
            Local
                .timestamp_opt(t as i64, 0)
                .single()
                .map(|x| x.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_default()
        } else {
            String::new()
        }
    };
    let data = json!({
        "ok": true,
        "groups": groups,
        "at": chrono::Local::now().format("%H:%M:%S").to_string(),
        "scan_at": scan_at_s,
    });
    let mut g = TASKS.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some(TasksSlot { at: now, key: range_key.to_string(), data: data.clone() });
    let _ = session_index;
    data
}
