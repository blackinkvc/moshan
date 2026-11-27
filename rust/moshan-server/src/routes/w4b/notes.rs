// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/dash.py` 的个人工作台那一段 —— notes.json 的读、整份覆盖写、追加一条，
//       以及 `export_markdown()` 的 md 镜像（待办 / 行程 / 日记 / 备忘 + README）。
// 不做：不碰工作流水（`09-个人工作台/流水/`）、不做 `--export` 命令行入口。
//
// 为什么连 md 一起迁：write_notes 落盘的不只是 notes.json，还有四个目录的镜像。
// 只写一半，用户打开 Finder 看到的就是上一版内容 —— 那是真的坏，不是「暂时」。

use super::util::{self, get, s_or, truthy};
use chrono::Datelike;
use crate::paths::Ctx;
use regex::Regex;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const PERSONAL_DIR: &str = "09-个人工作台";
const MAX_NOTES_BYTES: usize = 512 * 1024;
const MD_HEAD: &str = "<!-- 工作台自动生成，改这里会被覆盖；真源是同目录的 notes.json -->";

const README: &str = "\
# 09-个人工作台

工作台页面里的待办 / 行程 / 日记 / 备忘，全收在这一个目录。

| 文件 | 是什么 |
| --- | --- |
| `notes.json` | 唯一真源。工作台读写的就是它，整份覆盖、原子落盘 |
| `待办.md` | 未完成 + 已完成的清单 |
| `行程/YYYY-MM-DD.md` | 一天一个文件；跨天的行程写在出发那天 |
| `日记/YYYY-MM-DD.md` | 一天一个文件，只有写过日记的日子才有 |
| `备忘/YYYY-MM.md` | 一个月一个文件，速记按日期倒序 |

`.md` 全是自动生成的镜像：工作台里改完会立刻重写对应文件。
直接手改 `.md` **不会**回流到 `notes.json`，下次保存就被覆盖；
要批量导入就改 `notes.json`，再 `python dash.py --export` 重铺一遍。
";

fn dash_root(ctx: &Ctx) -> PathBuf {
    if let Some(a) = ctx.cfg.get("allowed_roots").and_then(|x| x.as_array()) {
        if let Some(first) = a.first() {
            let s = s_or(first);
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    let inbox = s_or(get(&ctx.cfg, "inbox_dir"));
    if inbox.is_empty() {
        PathBuf::from(".")
    } else {
        match PathBuf::from(&inbox).parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        }
    }
}

pub fn personal_dir(ctx: &Ctx) -> PathBuf {
    dash_root(ctx).join(PERSONAL_DIR)
}

pub fn notes_path(ctx: &Ctx) -> PathBuf {
    personal_dir(ctx).join("notes.json")
}

fn legacy_notes_path(ctx: &Ctx) -> PathBuf {
    crate::paths::workbench_dir(ctx).join("notes.json")
}

/// 旧位置 `_工作台/notes.json` 搬一次；搬完把旧文件改名留底，不删。
fn ensure_notes(ctx: &Ctx) {
    let new = notes_path(ctx);
    if new.exists() {
        return;
    }
    let old = legacy_notes_path(ctx);
    if !old.is_file() {
        return;
    }
    if let Ok(bytes) = std::fs::read(&old) {
        util::ensure_dir(new.parent().unwrap_or(Path::new(".")));
        let tmp = with_suffix(&new, ".json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() && std::fs::rename(&tmp, &new).is_ok() {
            let stamp = util::now_local().format("%Y%m%d").to_string();
            let dest = old.with_file_name(format!("notes.json.migrated-{}", stamp));
            let _ = std::fs::rename(&old, dest);
        }
    }
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if let Some(dot) = name.rfind('.') {
        name.truncate(dot);
    }
    name.push_str(suffix);
    p.with_file_name(name)
}

fn load_json(p: &Path, default: Value) -> Value {
    util::load_json(p).unwrap_or(default)
}

pub fn read_notes(ctx: &Ctx) -> Value {
    // 工作台在媒体根上；没授权时 `open()` 会卡死，这里先退回空清单（见 paths 看门狗）。
    if !crate::paths::library_ready() {
        return json!({"todos": [], "events": [], "memo": [], "diary": {}, "updated_at": ""});
    }
    ensure_notes(ctx);
    let d = load_json(&notes_path(ctx), json!(null));
    let d = if d.is_object() { d } else { json!({}) };
    let list = |k: &str| -> Value {
        match d.get(k) {
            Some(v) if v.is_array() => v.clone(),
            _ => json!([]),
        }
    };
    let diary = match d.get("diary") {
        Some(v) if v.is_object() => v.clone(),
        _ => json!({}),
    };
    json!({
        "todos": list("todos"),
        "events": list("events"),
        "memo": list("memo"),
        "diary": diary,
        "updated_at": s_or(get(&d, "updated_at")),
    })
}

pub fn write_notes(ctx: &Ctx, body: &Value) -> Value {
    // 读不到就别写：空清单落盘会把用户真数据覆盖掉。
    if !crate::paths::library_ready() {
        return json!({"ok": false, "error": "数据目录暂时读不到（系统隐私授权未通过），这次没有保存"});
    }
    let body = if body.is_object() { body.clone() } else { json!({}) };
    let list = |k: &str| -> Value {
        match body.get(k) {
            Some(v) if v.is_array() => v.clone(),
            _ => json!([]),
        }
    };
    let diary = match body.get("diary") {
        Some(v) if v.is_object() => v.clone(),
        _ => json!({}),
    };
    let payload = json!({
        "todos": list("todos"),
        "events": list("events"),
        "memo": list("memo"),
        "diary": diary,
        "updated_at": util::now_iso(),
    });
    let raw = crate::jsonio::dumps(&payload, 1);
    let bytes = raw.len();
    if bytes > MAX_NOTES_BYTES {
        return json!({"ok": false, "error": "内容太大（超过 512KB），先清理旧备忘"});
    }
    let p = notes_path(ctx);
    util::ensure_dir(p.parent().unwrap_or(Path::new(".")));
    let tmp = with_suffix(&p, ".json.tmp");
    if std::fs::write(&tmp, raw.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
    let _ = export_markdown(ctx, &payload);
    json!({
        "ok": true,
        "bytes": bytes,
        "updated_at": s_or(get(&payload, "updated_at")),
        "dir": p.parent().map(|x| x.to_string_lossy().to_string()).unwrap_or_default(),
    })
}

fn uuid_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; (n + 1) / 2];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let mut x = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (std::process::id() as u64) << 40;
        for b in buf.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect::<String>()[..n].to_string()
}

pub fn add_note(ctx: &Ctx, body: &Value) -> Value {
    let body = if body.is_object() { body.clone() } else { json!({}) };
    let kind = if s_or(get(&body, "kind")) == "memo" { "memo" } else { "todo" };
    let mut text = s_or(get(&body, "text")).trim().to_string();
    if text.is_empty() {
        return json!({"ok": false, "error": "写点什么吧"});
    }
    if text.chars().count() > 200 {
        text = util::cut(&text, 200);
    }
    let key = util::cut(s_or(get(&body, "key")).trim(), 80);
    let src = util::cut(s_or(get(&body, "src")).trim(), 40);
    let nows = util::now_local();
    let p = notes_path(ctx);
    ensure_notes(ctx);
    let d = match load_json(&p, json!({})) {
        v if v.is_object() => v,
        _ => json!({}),
    };
    let list = |k: &str| -> Vec<Value> {
        match d.get(k) {
            Some(v) if v.is_array() => v.as_array().cloned().unwrap_or_default(),
            _ => Vec::new(),
        }
    };
    let mut todos = list("todos");
    let events = list("events");
    let mut memo = list("memo");
    let diary = match d.get("diary") {
        Some(v) if v.is_object() => v.clone(),
        _ => json!({}),
    };
    let lst: &mut Vec<Value> = if kind == "todo" { &mut todos } else { &mut memo };
    if !key.is_empty()
        && lst.iter().any(|x| x.is_object() && s_or(get(x, "key")) == key)
    {
        return json!({"ok": true, "dup": true, "notes": read_notes(ctx)});
    }
    let mut item = json!({
        "id": format!("m{}", uuid_hex(11)),
        "text": text,
        "created": util::utc_iso_ms(),
    });
    if kind == "todo" {
        set(&mut item, "done", json!(false));
        let due = s_or(get(&body, "due"));
        let due = if due.is_empty() { nows.format("%Y-%m-%d").to_string() } else { due };
        set(&mut item, "due", json!(due));
    } else {
        let at = s_or(get(&body, "at"));
        let at = if at.is_empty() { nows.format("%Y-%m-%d").to_string() } else { at };
        set(&mut item, "at", json!(at));
    }
    if !key.is_empty() {
        set(&mut item, "key", json!(key));
    }
    if !src.is_empty() {
        set(&mut item, "src", json!(src));
    }
    lst.insert(0, item.clone());
    let payload = json!({
        "todos": todos,
        "events": events,
        "memo": memo,
        "diary": diary,
        "updated_at": util::now_iso(),
    });
    let raw = crate::jsonio::dumps(&payload, 1);
    let bytes = raw.len();
    if bytes > MAX_NOTES_BYTES {
        return json!({"ok": false, "error": "内容太大（超过 512KB），先清理旧备忘"});
    }
    util::ensure_dir(p.parent().unwrap_or(Path::new(".")));
    let tmp = with_suffix(&p, ".json.tmp");
    if std::fs::write(&tmp, raw.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
    let _ = export_markdown(ctx, &payload);
    json!({"ok": true, "dup": false, "item": item, "notes": read_notes(ctx)})
}

fn set(v: &mut Value, k: &str, val: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(k.to_string(), val);
    }
}

// ---------------------------------------------------------------- markdown 镜像

fn s_of(v: &Value) -> String {
    // dash._s()：只有 str/int/float 才转，其余当空（bool 在 Python 里是 int 的子类）
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Number(_) | Value::Bool(_) => util::pystr(v).trim().to_string(),
        _ => String::new(),
    }
}

fn md_doc(body: &str) -> String {
    format!("{}\n\n{}\n", body.trim_end(), MD_HEAD)
}

fn md_write(p: &Path, text: &str) -> bool {
    if p.is_file() {
        if let Ok(cur) = std::fs::read_to_string(p) {
            if cur == text {
                return false;
            }
        }
    }
    util::ensure_dir(p.parent().unwrap_or(Path::new(".")));
    let tmp = with_suffix(p, ".md.tmp");
    if std::fs::write(&tmp, text.as_bytes()).is_ok() {
        return std::fs::rename(&tmp, p).is_ok();
    }
    false
}

fn md_prune(d: &Path, keep: &std::collections::HashSet<String>, pat: &Regex) -> i64 {
    if !d.is_dir() {
        return 0;
    }
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(d) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_file() && !keep.contains(&name) && pat.is_match(&name) {
                if std::fs::remove_file(&p).is_ok() {
                    n += 1;
                }
            }
        }
    }
    n
}

fn re_day_file() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\d{4}-\d{2}-\d{2}\.md$").unwrap())
}
fn re_mon_file() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\d{4}-\d{2}\.md$").unwrap())
}
fn re_day() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap())
}
fn re_mon() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^\d{4}-\d{2}$").unwrap())
}

fn due_hint(due: &str) -> String {
    let m = match Regex::new(r"^(\d{4})-(\d{2})-(\d{2})").unwrap().captures(due) {
        Some(m) => m,
        None => return String::new(),
    };
    let y = m.get(1).unwrap().as_str().parse::<i32>().unwrap_or(0);
    let mo = m.get(2).unwrap().as_str().parse::<u32>().unwrap_or(0);
    let d = m.get(3).unwrap().as_str().parse::<u32>().unwrap_or(0);
    let Some(date) = chrono::NaiveDate::from_ymd_opt(y, mo, d) else {
        return String::new();
    };
    let delta = (date - util::now_local().date_naive()).num_days();
    match delta {
        0 => "今天".to_string(),
        1 => "明天".to_string(),
        -1 => "昨天".to_string(),
        d if d < 0 => format!("逾期 {} 天", -d),
        d if d <= 7 => format!("{} 天后", d),
        _ => String::new(),
    }
}

fn money(v: &Value) -> String {
    let s = Regex::new(r"^[¥￥]\s*").unwrap().replace(&s_of(v), "").to_string();
    if s.is_empty() {
        String::new()
    } else {
        format!("¥{}", s)
    }
}

fn md_todos(todos: &[Value]) -> String {
    let mut open_: Vec<(String, String)> = Vec::new();
    let mut done: Vec<(String, String)> = Vec::new();
    for t in todos {
        if !t.is_object() {
            continue;
        }
        let text = s_of(get(t, "text"));
        if text.is_empty() {
            continue;
        }
        let due = util::cut(&s_of(get(t, "due")), 10);
        let hint = if due.is_empty() { String::new() } else { due_hint(&due) };
        let meta = if !due.is_empty() && !hint.is_empty() {
            format!(" — {}（{}）", due, hint)
        } else if !due.is_empty() {
            format!(" — {}", due)
        } else {
            String::new()
        };
        let line = format!("- [{}] {}{}", if truthy(get(t, "done")) { "x" } else { " " }, text, meta);
        let key = if due.is_empty() { "\u{ffff}".to_string() } else { due };
        if truthy(get(t, "done")) {
            done.push((key, line));
        } else {
            open_.push((key, line));
        }
    }
    open_.sort_by(|a, b| a.0.cmp(&b.0));
    done.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out: Vec<String> = vec![
        "# 待办".to_string(),
        String::new(),
        format!("未完成 {} / 已完成 {}", open_.len(), done.len()),
        String::new(),
        "## 未完成".to_string(),
        String::new(),
    ];
    if open_.is_empty() {
        out.push("（没有待办）".to_string());
    } else {
        out.extend(open_.iter().map(|x| x.1.clone()));
    }
    if !done.is_empty() {
        out.push(String::new());
        out.push("## 已完成".to_string());
        out.push(String::new());
        out.extend(done.iter().map(|x| x.1.clone()));
    }
    md_doc(&out.join("\n"))
}

fn ev_lines(e: &Value) -> Vec<String> {
    let day = util::cut(&s_of(get(e, "date")), 10);
    let t = s_of(get(e, "time"));
    let mut title = s_of(get(e, "title"));
    let frm = s_of(get(e, "from"));
    let to = s_of(get(e, "to"));
    let end_d = util::cut(&s_of(get(e, "endDate")), 10);
    let end_t = s_of(get(e, "endTime"));
    let stops: Vec<String> = get(e, "stops")
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x.is_string())
                .map(|x| x.as_str().unwrap_or("").trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if title.is_empty() {
        let joined = [frm.clone(), to.clone()]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>()
            .join(" → ");
        title = if joined.is_empty() { "未命名行程".to_string() } else { joined };
    }
    let mut out: Vec<String> = vec![format!("## {} {}", t, title).trim().to_string(), String::new()];
    if !frm.is_empty() {
        out.push(format!("- 出发：{}{}", frm, if t.is_empty() { String::new() } else { format!(" {}", t) }));
    }
    if !stops.is_empty() {
        out.push(format!("- 途经：{}", stops.join(" → ")));
    }
    if !to.is_empty() {
        let mut arrive = format!("- 到达：{}", to);
        if !end_d.is_empty() && end_d != day {
            let tail: Vec<&str> = [&end_d[5.min(end_d.len())..], end_t.as_str()]
                .into_iter()
                .filter(|x| !x.is_empty())
                .collect();
            arrive.push_str(&format!(" · {}", tail.join(" ")));
        } else if !end_t.is_empty() {
            arrive.push_str(&format!(" {}", end_t));
        }
        out.push(arrive);
    } else if !end_d.is_empty() && end_d != day {
        let tail: Vec<&str> = [&end_d[5.min(end_d.len())..], end_t.as_str()]
            .into_iter()
            .filter(|x| !x.is_empty())
            .collect();
        out.push(format!("- 结束：{}", tail.join(" ")));
    }
    let flight = s_of(get(e, "flight"));
    if !flight.is_empty() {
        out.push(format!("- 航班：{}", flight));
    }
    let fee = money(get(e, "fee"));
    if !fee.is_empty() {
        out.push(format!("- 路费：{}", fee));
    }
    let note = s_of(get(e, "delayNote"));
    let delay_raw = s_of(get(e, "delay"));
    let dm: i64 = if delay_raw.is_empty() {
        0
    } else {
        delay_raw.parse::<f64>().map(|f| f as i64).unwrap_or(0)
    };
    if dm > 0 {
        out.push(format!("- 延误：{} 分钟{}", dm, if note.is_empty() { String::new() } else { format!("（{}）", note) }));
    } else if !note.is_empty() {
        out.push(format!("- 备注：{}", note));
    }
    if truthy(get(e, "done")) {
        out.push("- 状态：已完成".to_string());
    }
    out.push(String::new());
    out
}

fn weekday_cn(day: &str) -> String {
    match chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") {
        Ok(d) => {
            let names = ["一", "二", "三", "四", "五", "六", "日"];
            format!("周{}", names[d.weekday().num_days_from_monday() as usize])
        }
        Err(_) => String::new(),
    }
}

fn md_trip(day: &str, evs: &mut Vec<Value>) -> String {
    let wd = weekday_cn(day);
    let mut out: Vec<String> =
        vec![format!("# 行程 · {}{}", day, if wd.is_empty() { String::new() } else { format!(" {}", wd) }), String::new()];
    evs.sort_by(|a, b| (s_of(get(a, "time")), s_of(get(a, "created"))).cmp(&(s_of(get(b, "time")), s_of(get(b, "created")))));
    for e in evs.iter() {
        out.extend(ev_lines(e));
    }
    md_doc(&out.join("\n"))
}

fn md_diary(day: &str, text: &str) -> String {
    let wd = weekday_cn(day);
    let head = format!("# 日记 · {}{}", day, if wd.is_empty() { String::new() } else { format!(" {}", wd) });
    md_doc(&format!("{}\n\n{}", head, text))
}

fn md_memo(month: &str, items: &[Value]) -> String {
    let mut out = vec![format!("# 备忘 · {}", month), String::new()];
    let mut sorted: Vec<&Value> = items.iter().collect();
    // Python: sorted(..., reverse=True) —— 倒序但仍是稳定排序
    sorted.sort_by(|a, b| {
        (s_of(get(b, "at")), s_of(get(b, "id")))
            .cmp(&(s_of(get(a, "at")), s_of(get(a, "id"))))
    });
    for m in sorted {
        let at = s_of(get(m, "at"));
        let stamp = if at.chars().count() >= 10 { at.chars().skip(5).take(5).collect::<String>() } else { at };
        let text = s_of(get(m, "text")).replace('\n', "\n  ");
        out.push(format!("- **{}** {}", stamp, text));
    }
    md_doc(&out.join("\n"))
}

fn ensure_readme(base: &Path) {
    let p = base.join("README.md");
    if p.exists() {
        return;
    }
    util::ensure_dir(base);
    let _ = std::fs::write(&p, README);
}

pub fn export_markdown(ctx: &Ctx, data: &Value) -> std::io::Result<Value> {
    let base = personal_dir(ctx);
    let mut written = 0;
    let mut pruned = 0;

    if md_write(&base.join("待办.md"), &md_todos(data.get("todos").and_then(|x| x.as_array()).map(|a| a.as_slice()).unwrap_or(&[]))) {
        written += 1;
    }

    let mut trips: Vec<(String, Vec<Value>)> = Vec::new();
    for e in data.get("events").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        if !e.is_object() {
            continue;
        }
        let mut day = util::cut(&s_of(get(&e, "date")), 10);
        if !re_day().is_match(&day) {
            day = util::cut(&s_of(get(&e, "created")), 10);
        }
        if !re_day().is_match(&day) {
            continue;
        }
        match trips.iter_mut().find(|(k, _)| *k == day) {
            Some(slot) => slot.1.push(e),
            None => trips.push((day, vec![e])),
        }
    }
    let keep: std::collections::HashSet<String> = trips.iter().map(|(k, _)| format!("{}.md", k)).collect();
    for (day, mut evs) in trips {
        if md_write(&base.join("行程").join(format!("{}.md", day)), &md_trip(&day, &mut evs)) {
            written += 1;
        }
    }
    pruned += md_prune(&base.join("行程"), &keep, re_day_file());

    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Some(Value::Object(diary)) = data.get("diary") {
        for (day, text) in diary {
            let day = util::cut(day, 10);
            if !re_day().is_match(&day) || s_of(text).is_empty() {
                continue;
            }
            keep.insert(format!("{}.md", day));
            if md_write(&base.join("日记").join(format!("{}.md", day)), &md_diary(&day, &s_of(text))) {
                written += 1;
            }
        }
    }
    pruned += md_prune(&base.join("日记"), &keep, re_day_file());

    let mut months: Vec<(String, Vec<Value>)> = Vec::new();
    for m in data.get("memo").and_then(|x| x.as_array()).cloned().unwrap_or_default() {
        if !m.is_object() || s_of(get(&m, "text")).is_empty() {
            continue;
        }
        let mut month = util::cut(&s_of(get(&m, "at")), 7);
        if !re_mon().is_match(&month) {
            month = util::now_local().format("%Y-%m").to_string();
        }
        match months.iter_mut().find(|(k, _)| *k == month) {
            Some(slot) => slot.1.push(m),
            None => months.push((month, vec![m])),
        }
    }
    let keep: std::collections::HashSet<String> = months.iter().map(|(k, _)| format!("{}.md", k)).collect();
    for (month, items) in &months {
        if md_write(&base.join("备忘").join(format!("{}.md", month)), &md_memo(month, items)) {
            written += 1;
        }
    }
    pruned += md_prune(&base.join("备忘"), &keep, re_mon_file());

    ensure_readme(&base);
    Ok(json!({"ok": true, "dir": base.to_string_lossy(), "written": written, "pruned": pruned}))
}
