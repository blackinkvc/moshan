// SPDX-License-Identifier: Apache-2.0
// 职责：工作流水（`moshan/dash.py` 的 flow_* 三件套）—— 3 条路由：
//       GET  /api/flow      某个月的完整天数
//       POST /api/flow      勾一条（写 勾选.json）
//       POST /api/flow/add  补一条（写 流水.json + 底稿 md + 当月镜像）
//
// 真源链路（Python 侧注释）：原始流水.md（手写底稿）→ _build.py → 流水.json → 卡片。
// Rust 不能执行用户目录里的 `_build.py`，所以这里自带一个**同版式**的 md 解析器：
// `## MM-DD 周X` 作日期头、`- [x] 正文  \`分类\`` 作条目、每两个空格一层。
// 这与 `_flow_md()` 铺出来的镜像格式一致，`_flow_md` 的产物能被自己读回。
// 分叉：用户若在 `_build.py` 里加了自己的规则，Rust 侧看不到 —— 见汇报。
//
// id 一律优先沿用 流水.json 里同（日期, 文本）那条的 id：`_build.py` 写 json 时
// 就是同一种 id，勾选（勾选.json 以 id 之外的东西为键）才接得上是同一条。

use super::util::{clip, py_str, str_or, truthy};
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const PERSONAL_DIR: &str = "09-个人工作台";
const FLOW_DIR: &str = "流水";
const WD: &str = "一二三四五六日";
const MD_NAME: &str = "工作流水202604-.md";
const SRC_FALLBACK: &str = "原始流水.md";

fn root(ctx: &Ctx) -> PathBuf {
    ctx.allowed_roots()
        .into_iter()
        .next()
        .unwrap_or_else(|| ctx.library_dir.clone())
}

fn flow_dir(ctx: &Ctx) -> PathBuf {
    root(ctx).join(PERSONAL_DIR).join(FLOW_DIR)
}

fn flow_path(ctx: &Ctx) -> PathBuf {
    flow_dir(ctx).join("流水.json")
}

fn flow_src_path(ctx: &Ctx) -> PathBuf {
    let live = home()
        .join("Documents")
        .join("生活区")
        .join("工作流水")
        .join(MD_NAME);
    if live.exists() {
        return live;
    }
    flow_dir(ctx).join(SRC_FALLBACK)
}

fn marks_path(ctx: &Ctx) -> PathBuf {
    flow_dir(ctx).join("勾选.json")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load(p: &Path) -> Option<Value> {
    jsonio::read_json(p)
}

struct Cache(Mutex<Option<(String, i64, u64, Value)>>);
static CACHE: Cache = Cache(Mutex::new(None));

fn cache_key(p: &Path) -> Option<(String, i64, u64)> {
    let md = std::fs::metadata(p).ok()?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Some((p.to_string_lossy().to_string(), mtime, md.len()))
}

fn invalidate() {
    if let Ok(mut g) = CACHE.0.lock() {
        *g = None;
    }
}

/// `_flow_key`：空白全去、首尾标点剥掉（勾选.json 的键就这么来的）。
fn flow_key(t: &str) -> String {
    let no_ws: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    no_ws
        .trim_matches(|c: char| "，,、：:;；。. ".contains(c))
        .to_string()
}

fn mark_key(date: &str, text: &str) -> String {
    format!("{date}|{}", flow_key(text))
}

/// `_iso` 的日期部分：`2026-11-18T16:45` → `2026-11-18`
fn day_of(v: &Value) -> String {
    let s = py_str(v).replace('T', " ");
    s.chars().take(10).collect()
}

pub fn read_flow(ctx: &Ctx) -> Value {
    let src = flow_src_path(ctx);
    let key = cache_key(&src);
    if let Ok(g) = CACHE.0.lock() {
        if let Some((p, mt, sz, data)) = g.as_ref() {
            if Some((p.clone(), *mt, *sz)) == key {
                return data.clone();
            }
        }
    }

    let mut days: Vec<Value> = Vec::new();
    if key.is_some() {
        // 底稿 md 就是真源：它在就解析它。
        // （以前这里要求 `流水/_build.py` 存在才解析 —— 那是 Python 时代的开关。
        //   builder 现在没人跑了，拿一个死脚本的存在与否决定运行时行为，
        //   等于把真源锁死在最后一次 build 出来的 json 上：卡片上的月份条
        //   会停在旧数字，点进去却看到现场解析的 md，两处永远对不上。）
        if let Ok(text) = std::fs::read_to_string(&src) {
            days = parse_md(&text, &flow_path(ctx));
        }
    }
    let note;
    if !days.is_empty() {
        merge_done(ctx, &mut days);
        note = "md";
    } else {
        let d = load(&flow_path(ctx)).unwrap_or(json!({}));
        days = d
            .get("days")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        note = "json";
    }
    for day in &mut days {
        if let Some(items) = day.get_mut("items").and_then(|v| v.as_array_mut()) {
            flow_kinds(items);
        }
    }
    let missing = days.is_empty();
    let out = json!({
        "days": days,
        "missing": missing,
        "src": note,
    });
    if let Some(k) = key {
        if let Ok(mut g) = CACHE.0.lock() {
            *g = Some((k.0, k.1, k.2, out.clone()));
        }
    }
    out
}

/// `_flow_merge_done`：勾选.json > md 的 [x] > 流水.json 的历史勾选。
fn merge_done(ctx: &Ctx, days: &mut [Value]) {
    let marks = load(&marks_path(ctx)).unwrap_or(json!({}));
    let mut hist: Map<String, Value> = Map::new();
    let d = load(&flow_path(ctx)).unwrap_or(json!({}));
    for day in d.get("days").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let date = day.get("date").map(py_str).unwrap_or_default();
        for it in day.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            if it.get("done").map(|v| *v == Value::Bool(true)).unwrap_or(false) {
                let k = mark_key(&date, &str_or(it.get("text")));
                let v = it.get("done_at").cloned().unwrap_or(json!(true));
                hist.insert(k, v);
            }
        }
    }
    for day in days.iter_mut() {
        let date = day.get("date").map(py_str).unwrap_or_default();
        let Some(items) = day.get("items").and_then(|v| v.as_array()).cloned() else {
            continue;
        };
        let mut out: Vec<Value> = Vec::new();
        for mut it in items {
            let k = mark_key(&date, &str_or(it.get("text")));
            if let Some(m) = marks.get(&k) {
                if m.is_object() {
                    let done = truthy(m.get("done").unwrap_or(&Value::Null));
                    if let Value::Object(o) = &mut it {
                        o.insert("done".into(), Value::Bool(done));
                        if done {
                            if let Some(at) = m.get("at") {
                                if truthy(at) {
                                    o.insert(
                                        "done_at".into(),
                                        json!(clip(&py_str(at), 10)),
                                    );
                                }
                            }
                        }
                    }
                    out.push(it);
                    continue;
                }
            }
            if it.get("done").map(|v| *v == Value::Bool(true)).unwrap_or(false) {
                out.push(it);
                continue;
            }
            if let Some(v) = hist.get(&k) {
                if let Value::Object(o) = &mut it {
                    o.insert("done".into(), Value::Bool(true));
                    if let Value::String(s) = v {
                        if !o.contains_key("done_at") {
                            o.insert("done_at".into(), json!(s));
                        }
                    }
                }
            }
            out.push(it);
        }
        if let Value::Object(o) = &mut *day {
            o.insert("items".into(), Value::Array(out));
        }
    }
}

/// `_flow_kinds`：group（板块/子板块）/ trip（行程）/ task（真任务）
fn flow_kinds(items: &mut [Value]) {
    let levels: Vec<i64> = items
        .iter()
        .map(|it| it.get("level").and_then(|v| v.as_i64()).unwrap_or(0))
        .collect();
    for i in 0..items.len() {
        let has_child = i + 1 < items.len() && levels[i + 1] > levels[i];
        let text = str_or(items[i].get("text")).trim().to_string();
        let cat = str_or(items[i].get("cat")).trim().to_string();
        let kind = if has_child || (!cat.is_empty() && text == cat) {
            "group"
        } else if cat == "行程" || text.starts_with("行程") {
            "trip"
        } else {
            "task"
        };
        if let Value::Object(o) = &mut items[i] {
            o.insert("kind".into(), json!(kind));
        }
    }
}

fn flow_counts(days: &[Value]) -> Value {
    let items: Vec<Value> = days
        .iter()
        .flat_map(|d| {
            d.get("items")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    let tasks: Vec<&Value> = items
        .iter()
        .filter(|it| {
            it.get("kind")
                .and_then(|v| v.as_str())
                .map(|s| s == "task")
                .unwrap_or(true)
        })
        .collect();
    json!({
        "days": days.len(),
        "items": tasks.len(),
        "done": tasks.iter().filter(|it| it.get("done").map(|v| *v == Value::Bool(true)).unwrap_or(false)).count(),
        "open": tasks.iter().filter(|it| it.get("done").map(|v| *v == Value::Bool(false)).unwrap_or(false)).count(),
        "trips": items.iter().filter(|it| it.get("kind").and_then(|v| v.as_str()) == Some("trip")).count(),
        "groups": items.iter().filter(|it| it.get("kind").and_then(|v| v.as_str()) == Some("group")).count(),
    })
}

fn undated_of(all_days: &[Value]) -> Vec<Value> {
    all_days
        .iter()
        .filter(|d| !truthy(d.get("date").unwrap_or(&Value::Null)))
        .map(|d| {
            let items = d.get("items").cloned().unwrap_or(json!([]));
            let count = items
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|it| {
                            it.get("kind")
                                .and_then(|v| v.as_str())
                                .map(|s| s == "task")
                                .unwrap_or(true)
                        })
                        .count()
                })
                .unwrap_or(0);
            json!({
                "title": str_or(d.get("title")),
                "items": items,
                "count": count,
            })
        })
        .collect()
}

pub fn flow_month(ctx: &Ctx, month: &str) -> Value {
    let data = read_flow(ctx);
    let all_days = data
        .get("days")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let days: Vec<Value> = all_days
        .iter()
        .filter(|d| truthy(d.get("date").unwrap_or(&Value::Null)))
        .cloned()
        .collect();
    let undated = undated_of(&all_days);
    let mut months: Vec<String> = days
        .iter()
        .filter_map(|d| {
            let m = str_or(d.get("month"));
            if m.is_empty() {
                None
            } else {
                Some(m)
            }
        })
        .collect();
    months.sort();
    months.dedup();
    months.reverse();

    if month == "all" {
        return json!({
            "month": "all",
            "months": months,
            "days": days,
            "undated": undated,
            "counts": flow_counts(&days),
        });
    }
    let mut m = month.to_string();
    if !months.iter().any(|x| x == &m) {
        m = months.first().cloned().unwrap_or_default();
    }
    let picked: Vec<Value> = days
        .iter()
        .filter(|d| str_or(d.get("month")) == m)
        .cloned()
        .collect();
    json!({
        "month": m,
        "months": months,
        "days": picked,
        "undated": undated,
        "counts": flow_counts(&picked),
    })
}

pub fn toggle(ctx: &Ctx, iid: &str, done: bool) -> Value {
    let data = read_flow(ctx);
    let mut target: Option<(String, String)> = None;
    for day in data.get("days").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        for it in day.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            if it.get("id").and_then(|v| v.as_str()) == Some(iid) {
                target = Some((day_of(day.get("date").unwrap_or(&Value::Null)), str_or(it.get("text"))));
                break;
            }
        }
        if target.is_some() {
            break;
        }
    }
    let Some((date, text)) = target else {
        return json!({ "ok": false, "error": "没这条流水" });
    };
    let mut marks = load(&marks_path(ctx)).unwrap_or(json!({}));
    if !marks.is_object() {
        marks = json!({});
    }
    if let Value::Object(m) = &mut marks {
        m.insert(
            mark_key(&date, &text),
            json!({
                "done": done,
                "at": chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            }),
        );
    }
    let p = marks_path(ctx);
    if let Err(e) = write_py(&p, &jsonio::dumps(&marks, 1)) {
        return json!({ "ok": false, "error": format!("写不进去：{e}") });
    }
    invalidate();
    json!({
        "ok": true,
        "id": iid,
        "done": done,
        "month": clip(&date, 7),
    })
}


/// Python 的写法：`tmp = p.with_suffix(".json.tmp"); tmp.write_text(...); tmp.replace(p)`。
/// 关键差别：**不建父目录**。目录不在时报 ENOENT —— 这正是 Python 的行为，
/// 而且错误文案里带着那个 tmp 路径，前端会原样显示。
fn write_py(p: &Path, text: &str) -> std::io::Result<()> {
    let tmp = std::path::PathBuf::from(format!("{}.tmp", p.to_string_lossy()));
    std::fs::write(&tmp, text).map_err(|e| with_path(e, &tmp))?;
    std::fs::rename(&tmp, p).map_err(|e| with_path(e, &tmp))
}

/// 把 io 错误补成 Python `OSError.__str__` 的形状（带被写的路径）。
fn with_path(e: std::io::Error, p: &Path) -> std::io::Error {
    let n = e.raw_os_error().unwrap_or(0);
    let msg = e.to_string();
    let tail = msg.split(" (os error").next().unwrap_or(&msg).to_string();
    std::io::Error::new(
        e.kind(),
        format!("[Errno {n}] {tail}: '{}'", p.display()),
    )
}

/// `_flow_src_append`：往底稿那天末尾补一行；找不到那天的标题就补在文件末尾。
fn flow_src_append(ctx: &Ctx, day: &str, text: &str) -> bool {
    let p = flow_src_path(ctx);
    if !p.exists() {
        return false;
    }
    let Ok(raw) = std::fs::read_to_string(&p) else {
        return false;
    };
    let mut lines: Vec<String> = raw.split('\n').map(|s| s.to_string()).collect();
    let parts: Vec<&str> = day.split('-').collect();
    if parts.len() != 3 {
        return false;
    }
    let (mo, d) = (parts[1], parts[2]);
    let mo_i = mo.parse::<i64>().unwrap_or(0);
    let d_i = d.parse::<i64>().unwrap_or(0);
    let pats = vec![
        day.to_string(),
        format!("{mo_i}-{d_i}"),
        format!("{mo_i}{d_i}"),
    ];
    let is_that_day = |line: &str| -> bool {
        if !line.starts_with("## ") {
            return false;
        }
        let body = line[3..].trim();
        if pats.iter().any(|x| body.starts_with(x.as_str())) {
            return true;
        }
        body.replace(' ', "").starts_with(day)
    };
    let head = lines.iter().position(|l| is_that_day(l));
    let row = format!("- {text}");
    match head {
        None => {
            while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                lines.pop();
            }
            let wd = weekday_of(day);
            lines.push(String::new());
            lines.push(format!("## {day}-周{wd}"));
            lines.push(String::new());
            lines.push(row);
            lines.push(String::new());
        }
        Some(head) => {
            let mut stop = lines.len();
            for i in head + 1..lines.len() {
                if lines[i].starts_with("## ") || lines[i].starts_with("# ") {
                    stop = i;
                    break;
                }
            }
            let mut last = head;
            for i in head + 1..stop {
                if !lines[i].trim().is_empty() {
                    last = i;
                }
            }
            if last == head {
                lines.insert(head + 1, String::new());
                lines.insert(head + 2, row);
            } else {
                lines.insert(last + 1, row);
            }
        }
    }
    write_py(&p, &lines.join("\n")).is_ok()
}

fn weekday_of(day: &str) -> char {
    // `一二三四五六日`[weekday]
    let d = parse_day(day);
    match d {
        Some(d) => WD
            .chars()
            .nth(chrono::Datelike::weekday(&d).num_days_from_monday() as usize)
            .unwrap_or('日'),
        None => '日',
    }
}

fn parse_day(day: &str) -> Option<chrono::NaiveDate> {
    let parts: Vec<&str> = day.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(y, m, d)
}

/// `_flow_md`：重铺一个月的 markdown 镜像。
fn write_month_md(ctx: &Ctx, month: &str, days: &[Value]) -> std::io::Result<()> {
    let mut out: Vec<String> = vec![
        format!("# {month} 工作流水"),
        String::new(),
        "> 由 `流水.json` 自动生成，手改会被覆盖。底稿是 `原始流水.md`。".to_string(),
        String::new(),
    ];
    for d in days {
        let date = str_or(d.get("date"));
        let wd = parse_day(&date)
            .map(|d| WD.chars().nth(chrono::Datelike::weekday(&d).num_days_from_monday() as usize).unwrap_or(' '))
            .unwrap_or(' ');
        if wd != ' ' {
            out.push(format!("## {} 周{}", clip(&date, 20).get(5..).unwrap_or(""), wd));
        } else {
            out.push(format!("## {date}"));
        }
        out.push(String::new());
        let items = d.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
        if items.is_empty() {
            out.push("- （无记录）".to_string());
        }
        for it in items {
            let done = it.get("done").cloned().unwrap_or(Value::Null);
            let box_ = match done {
                Value::Null => "-".to_string(),
                Value::Bool(true) => "- [x]".to_string(),
                _ => "- [ ]".to_string(),
            };
            let level = it.get("level").and_then(|v| v.as_i64()).unwrap_or(0);
            let pad = "  ".repeat(level.max(0) as usize);
            let tail = {
                let cat = str_or(it.get("cat"));
                if !cat.is_empty() && level == 0 {
                    format!("  `{cat}`")
                } else {
                    String::new()
                }
            };
            out.push(format!("{pad}{box_} {}{tail}", str_or(it.get("text"))));
        }
        out.push(String::new());
    }
    let body = out.join("\n");
    let body = body.trim_end().to_string() + "\n";
    let p = flow_dir(ctx).join(format!("{month}.md"));
    write_py(&p, &body)
}

pub fn flow_add(ctx: &Ctx, text: &str, day: &str) -> Value {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return json!({ "ok": false, "error": "空的一行" });
    }
    let text = clip(&text, 200);
    let day = if day.is_empty() {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    } else {
        day.to_string()
    };
    if parse_day(&day).is_none() {
        return json!({ "ok": false, "error": "日期不对" });
    }

    let p = flow_path(ctx);
    let mut d = load(&p).unwrap_or(json!({}));
    if !d.is_object() {
        d = json!({});
    }
    let mut days = d.get("days").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut hit_idx = days
        .iter()
        .position(|x| x.get("date").and_then(|v| v.as_str()) == Some(day.as_str()));
    if hit_idx.is_none() {
        days.push(json!({ "date": day.clone(), "month": clip(&day, 7), "items": [] }));
        days.sort_by(|a, b| str_or(a.get("date")).cmp(&str_or(b.get("date"))));
        hit_idx = days.iter().position(|x| x.get("date").and_then(|v| v.as_str()) == Some(day.as_str()));
    }
    let idx = hit_idx.unwrap();
    let mut items = days[idx]
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut seq = items.len() + 1;
    let compact = day.replace('-', "");
    let used: Vec<String> = items
        .iter()
        .map(|it| str_or(it.get("id")))
        .collect();
    while used.iter().any(|u| u == &format!("{compact}-{seq}")) {
        seq += 1;
    }
    let iid = format!("{compact}-{seq}");
    items.push(json!({
        "id": iid,
        "text": text,
        "done": Value::Null,
        "level": 0,
        "cat": "速记",
    }));
    let count = items.len();
    if let Value::Object(o) = &mut days[idx] {
        o.insert("items".into(), Value::Array(items));
    }
    if let Value::Object(o) = &mut d {
        o.insert("days".into(), Value::Array(days.clone()));
    }
    if let Err(e) = write_py(&p, &jsonio::dumps(&d, 1)) {
        return json!({ "ok": false, "error": format!("写不进去：{e}") });
    }
    invalidate();
    let month = {
        let m = str_or(days[idx].get("month"));
        if m.is_empty() {
            clip(&day, 7)
        } else {
            m
        }
    };
    let src = flow_src_append(ctx, &day, &text);
    let picked: Vec<Value> = days
        .iter()
        .filter(|x| str_or(x.get("month")) == month)
        .cloned()
        .collect();
    let _ = write_month_md(ctx, &month, &picked);
    json!({
        "ok": true,
        "id": iid,
        "day": day,
        "text": text,
        "src": src,
        "count": count,
    })
}

// ---------------------------------------------------------------- md 解析
//
// 规则照抄 `09-个人工作台/流水/_build.py` 的 parse_lines —— 底稿是手写的，
// 同一份文件里混着三种写法（早期 `## 0427` + `1. xxx`、中期 `## 0511周一`、
// 现在 `## 2026-11-16-周一`），任何一条不认就会把整天内容丢进「无日期小节」，
// 于是卡片上切月没反应、日期也不成序（真出现过：11 月 21 天全空、47 个无日期块）。

/// 中文月份 → 数字。`# 四月` 这种一级标题只提供「当前在第几月」，不含年份。
fn cn_month(s: &str) -> Option<u32> {
    Some(match s {
        "一" => 1,
        "二" => 2,
        "三" => 3,
        "四" => 4,
        "五" => 5,
        "六" => 6,
        "七" => 7,
        "八" => 8,
        "九" => 9,
        "十" => 10,
        "十一" => 11,
        "十二" => 12,
        _ => return None,
    })
}

/// 缩进宽度：Tab 当 4 个空格（与 `_build.py` 的 `indent.replace("\t","    ")` 一致）。
/// 返回 (列宽, 去掉缩进后的正文)。
fn split_indent(line: &str) -> (usize, &str) {
    let mut col = 0usize;
    let mut idx = 0usize;
    for (i, c) in line.char_indices() {
        match c {
            ' ' => col += 1,
            '\t' => col += 4,
            _ => break,
        }
        idx = i + c.len_utf8();
    }
    (col, &line[idx..])
}

/// `[x] 正文`（有序列表和圆点列表后面都可能挂一个勾）
fn leading_box(s: &str) -> Option<(bool, &str)> {
    let b = s.trim_start().strip_prefix('[')?;
    let mut ch = b.chars();
    let mark = ch.next()?;
    let done = match mark {
        'x' | 'X' => true,
        ' ' => false,
        _ => return None,
    };
    let rest = b[mark.len_utf8()..].strip_prefix(']')?;
    Some((done, rest.trim_start()))
}

/// `- [x] 正文` / `* [ ] 正文`：返回 (是否已完成, 正文)。
fn item_box(body: &str) -> Option<(bool, &str)> {
    let b = body.strip_prefix("- ").or_else(|| body.strip_prefix("* "))?;
    leading_box(b)
}

/// `1. 正文` / `1、正文`：返回正文（后面可能还挂 `[x]`）。
fn item_num(body: &str) -> Option<&str> {
    let end = body
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(body.len());
    if end == 0 {
        return None;
    }
    let rest = &body[end..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix('、'))?;
    Some(rest.trim_start())
}

/// `- 正文` / `* 正文`
fn item_bullet(body: &str) -> Option<&str> {
    body.strip_prefix("- ")
        .or_else(|| body.strip_prefix("* "))
        .map(str::trim_start)
}

/// `_flow_clip`：分组名太泛的只在短标题上认（「咨询」写进一句长话里不是分类）。
const NARROW_CAT: &[&str] = &["咨询", "服饰", "服装", "机车", "FA", "fa"];

/// 板块：命中即归这一类，先具体后宽泛。
const CATS: &[(&str, &[&str])] = &[
    ("墨禽", &["墨禽", "咨询", "FA", "fa"]),
    ("墨一", &["墨一"]),
    ("墨襄", &["墨襄", "服饰", "服装"]),
    ("炽奔", &["炽奔", "机车"]),
    ("墨鱼", &["墨鱼"]),
    ("墨跖", &["墨跖"]),
    ("行程", &["行程"]),
    ("装修", &["装修"]),
];

/// 顶层单写一条时（如 `3. 红驼`）靠项目名反推所属板块：只在前一条无法继承时才用。
const PROJECTS: &[(&str, &[&str])] = &[
    (
        "墨禽",
        &[
            "红驼", "威华", "晶澜", "海越", "驭风拓界", "中木", "超力", "瑞诵", "江门健雅",
            "金雨滴", "桐乡", "崇福镇", "天境生物", "百雀", "增程",
        ],
    ),
    ("墨一", &["磐德", "磐徳", "万银"]),
    ("炽奔", &["军刀", "小象"]),
];

fn categorise(text: &str, inherit: &str, level: usize) -> String {
    let head = text.trim();
    for (name, keys) in CATS {
        for k in *keys {
            if head.contains(k) {
                if NARROW_CAT.contains(k) && head.chars().count() > 12 {
                    continue;
                }
                return (*name).to_string();
            }
        }
    }
    if level == 0 && inherit == "其他" {
        for (name, keys) in PROJECTS {
            if keys.iter().any(|k| head.contains(k)) {
                return (*name).to_string();
            }
        }
    }
    inherit.to_string()
}

/// `clean`：摘掉 ✅完成日期 / 🆔块号 / ~~删除线~~ / @提及，只留正文。
fn clean(text: &str) -> (String, Option<String>, Option<String>) {
    let mut s = text.to_string();
    let mut done_at: Option<String> = None;
    let mut block: Option<String> = None;

    if let Some(pos) = s.find('✅') {
        let after = s[pos + '✅'.len_utf8()..].trim_start();
        let cand: String = after.chars().take(10).collect();
        let is_date = cand.len() == 10
            && cand.chars().filter(|c| *c == '-').count() == 2
            && cand.chars().all(|c| c.is_ascii_digit() || c == '-');
        if is_date {
            done_at = Some(cand.clone());
            let tail = &after[cand.len()..];
            s = format!("{}{}", &s[..pos], tail);
        }
    }
    if let Some(pos) = s.find('🆔') {
        let after = s[pos + '🆔'.len_utf8()..].trim().to_string();
        if !after.is_empty() {
            block = Some(after);
        }
        s.truncate(pos);
    }

    s = s.replace("~~", "");
    // `@反重力猫头鹰⏰2026-11-20` 这类提醒尾巴：整段（含前面空格）去掉
    const TAG: &str = "@反重力猫头鹰⏰";
    while let Some(pos) = s.find(TAG) {
        let mut start = pos;
        while start > 0 {
            let c = s[..start].chars().next_back().unwrap_or(' ');
            if c.is_whitespace() {
                start -= c.len_utf8();
            } else {
                break;
            }
        }
        let mut end = pos + TAG.len();
        for (i, c) in s[end..].char_indices() {
            if c.is_whitespace() {
                break;
            }
            end = pos + TAG.len() + i + c.len_utf8();
        }
        s = format!("{}{}", &s[..start], &s[end..]);
    }

    let t = s
        .trim()
        .trim_matches(|c: char| " ，,、".contains(c))
        .to_string();
    (t, done_at, block)
}

fn parse_md(text: &str, json_path: &Path) -> Vec<Value> {
    // json 里同（日期, 文本）的 id 优先复用 —— _build.py 写 json 用的就是那种 id
    let mut id_of: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(d) = load(json_path) {
        for day in d.get("days").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
            let date = str_or(day.get("date"));
            for it in day.get("items").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
                let id = str_or(it.get("id"));
                if !id.is_empty() {
                    id_of.insert(mark_key(&date, &str_or(it.get("text"))), id);
                }
            }
        }
    }

    let mut days: Vec<Value> = Vec::new();
    let mut cur: Option<Value> = None;
    let mut month_no: u32 = 0;      // `# 十一月` 给的下文月份，MMDD 靠它推年份
    let mut stack: Vec<usize> = Vec::new();   // 缩进栈，决定 level
    let mut day_cat = String::from("其他");    // 当天继承的分类
    let mut seq: usize = 0;

    let push_cur = |days: &mut Vec<Value>, cur: &mut Option<Value>| {
        if let Some(d) = cur.take() {
            days.push(d);
        }
    };

    for raw in text.split('\n') {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        // 一级标题：`# 四月` 只提供月份上下文，`# 重要待办事项` 才另起一个无日期块
        if line.starts_with("# ") && !line.starts_with("## ") {
            let name = line[2..].trim();
            if let Some(m) = name.strip_suffix('月') {
                if let Some(n) = cn_month(m) {
                    month_no = n;
                } else if let Ok(n) = m.parse::<u32>() {
                    month_no = n;
                }
            }
            if name.contains("待办") {
                push_cur(&mut days, &mut cur);
                cur = Some(json!({ "title": name.to_string(), "items": [] }));
                stack.clear();
                day_cat = "其他".to_string();
                seq = 0;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("## ") {
            push_cur(&mut days, &mut cur);
            let title = rest.trim();
            cur = Some(match head_date(title, month_no) {
                Some(date) => json!({
                    "date": date,
                    "month": clip(&date, 7),
                    "title": title.to_string(),
                    "items": [],
                }),
                // 无日期小节（AI项目 / 重要待办）：单独成块，前端排在日历之后
                None => json!({ "title": title.to_string(), "items": [] }),
            });
            stack.clear();
            day_cat = "其他".to_string();
            seq = 0;
            continue;
        }
        let Some(day) = cur.as_mut() else { continue };

        let (indent, body) = split_indent(line);
        let mut done: Option<bool> = None;
        let mut raw_text: Option<String> = None;
        if let Some((d, r)) = item_box(body) {
            done = Some(d);
            raw_text = Some(r.to_string());
        } else if let Some(r) = item_num(body) {
            // `1. 行程：…` / `1. [x] …` —— 底稿 8 月之后全是这种有序列表
            match leading_box(r) {
                Some((d, x)) => {
                    done = Some(d);
                    raw_text = Some(x.to_string());
                }
                None => raw_text = Some(r.to_string()),
            }
        } else if let Some(r) = item_bullet(body) {
            match leading_box(r) {
                Some((d, x)) => {
                    done = Some(d);
                    raw_text = Some(x.to_string());
                }
                None => raw_text = Some(r.to_string()),
            }
        }

        if raw_text.is_none() {
            // 纯文本段落：接到上一条后面（底稿里长句常被手动折行）
            let seg = line.trim().to_string();
            let mut appended = false;
            if let Some(items) = day.get_mut("items").and_then(|v| v.as_array_mut()) {
                if let Some(last) = items.last_mut() {
                    let prev = str_or(last.get("text"));
                    if let Value::Object(m) = last {
                        let joined = format!("{prev} {seg}");
                        m.insert("text".into(), json!(joined.trim()));
                    }
                    appended = true;
                }
            }
            if appended {
                continue;
            }
            raw_text = Some(seg);
        }

        let (mut text, done_at, block) = clean(&raw_text.unwrap_or_default());
        // 按月镜像的写法是 `正文  \`分类\``；底稿没有，但读回来要认
        let mut cat_hint = String::new();
        if let Some(pos) = text.rfind('`') {
            if text[..pos].ends_with("  ") {
                cat_hint = text[pos + 1..].trim_end_matches('`').trim().to_string();
                text = text[..pos].trim_end().to_string();
            }
        }
        if text.is_empty() || text == "（无记录）" {
            continue;
        }

        // level 来自缩进栈：只有比前一行更深才算下一层，不是「缩进 ÷ 2」
        while stack.last().map(|t| indent <= *t).unwrap_or(false) {
            stack.pop();
        }
        let level = stack.len();
        stack.push(indent);

        let cat = if cat_hint.is_empty() {
            categorise(&text, &day_cat, level)
        } else {
            cat_hint
        };
        if level == 0 {
            day_cat = cat.clone();
        }

        seq += 1;
        let date = str_or(day.get("date"));
        let mut id = id_of
            .get(&mark_key(&date, &text))
            .cloned()
            .unwrap_or_default();
        if id.is_empty() {
            // `_build.py` 的写法：20260427-001
            id = format!("{}-{:03}", date.replace('-', ""), seq);
        }
        let mut item = Map::new();
        item.insert("id".into(), json!(id));
        item.insert("text".into(), json!(text));
        item.insert(
            "done".into(),
            done.map(Value::Bool).unwrap_or(Value::Null),
        );
        item.insert("level".into(), json!(level));
        item.insert("cat".into(), json!(cat));
        if let Some(v) = done_at {
            item.insert("done_at".into(), json!(v));
        }
        if let Some(v) = block {
            item.insert("block".into(), json!(v));
        }
        if let Some(items) = day.get_mut("items").and_then(|v| v.as_array_mut()) {
            items.push(Value::Object(item));
        }
    }
    push_cur(&mut days, &mut cur);
    days
}

/// `_build.py:parse_date` —— 标题 → `YYYY-MM-DD`。
/// 认 `2026-11-16-周一`、`0427`、`0511周一`、`0622 周一`、`11-16`。
fn head_date(title: &str, cur_month: u32) -> Option<String> {
    let head: String = title
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    if head.is_empty() {
        return None;
    }
    let parts: Vec<&str> = head.split('-').filter(|s| !s.is_empty()).collect();
    let now = chrono::Datelike::year(&chrono::Local::now());
    let fmt = |y: i32, m: u32, d: u32| {
        chrono::NaiveDate::from_ymd_opt(y, m, d).map(|x| x.format("%Y-%m-%d").to_string())
    };
    match parts.len() {
        3 => fmt(
            parts[0].parse().ok()?,
            parts[1].parse().ok()?,
            parts[2].parse().ok()?,
        ),
        // MMDD / MDD：`## 0427`、`## 0511周一`（周几直接粘在后面）
        // 年份按「不早于当前段月份就是今年」推 —— 底稿是越写越新的流水账
        1 => {
            let t = parts[0];
            let (ms, ds) = match t.len() {
                4 => (&t[0..2], &t[2..4]),
                3 => (&t[0..1], &t[1..3]),
                _ => return None,
            };
            field_ymd(ms, ds, cur_month, now, &fmt)
        }
        2 => field_ymd(parts[0], parts[1], cur_month, now, &fmt),
        _ => None,
    }
}

fn field_ymd(
    ms: &str,
    ds: &str,
    cur_month: u32,
    now: i32,
    fmt: &dyn Fn(i32, u32, u32) -> Option<String>,
) -> Option<String> {
    let m: u32 = ms.parse().ok()?;
    let d: u32 = ds.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if cur_month > 0 && m < cur_month { now + 1 } else { now };
    fmt(y, m, d)
}

// ---------------------------------------------------------------- 路由

pub async fn get_month(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let month = crate::routes::common::q(&uri, "month");
    Json(flow_month(&ctx, month.trim())).into_response()
}

pub async fn post_toggle(
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
    let iid = str_or(body.get("id"));
    let done = truthy(body.get("done").unwrap_or(&Value::Null));
    Json(toggle(&ctx, &iid, done)).into_response()
}

pub async fn post_add(
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
    let text = str_or(body.get("text"));
    let day = str_or(body.get("day"));
    Json(flow_add(&ctx, &text, &day)).into_response()
}
