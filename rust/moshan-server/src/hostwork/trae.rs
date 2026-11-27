// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：TRAE 的任务清单两条路（hostwork / trae_work.session_projects + task_rows）。
//       TRAE 的对话正文在加密库里，这里只读「会话→工作区」的明文映射、
//       工作区快照的日期、以及 OCR 补录的任务名。
// 不做：不读加密库、不做 OCR、不解析命令记录（本机 command-execution 为空）。

use chrono::TimeZone;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::util::{py_double, str_or};
use crate::paths::Ctx;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn trae_dir() -> PathBuf {
    let env = std::env::var("APPDATA").unwrap_or_default();
    let cand = PathBuf::from(&env).join("Trae CN");
    let alt = home().join("AppData").join("Roaming").join("Trae CN");
    for p in [&cand, &alt] {
        if p.is_dir() {
            return p.clone();
        }
    }
    home().join("Library").join("Application Support").join("Trae CN")
}

fn state_db() -> PathBuf {
    trae_dir().join("User").join("globalStorage").join("state.vscdb")
}

fn snap_root() -> PathBuf {
    trae_dir().join("ModularData").join("ai-agent").join("snapshot")
}

fn mcp_root() -> PathBuf {
    home().join(".trae-cn").join("mcps")
}

fn own_dir(ctx: &Ctx) -> PathBuf {
    ctx.runtime_dir.join("_本机工作")
}

fn seed_path(ctx: &Ctx) -> PathBuf {
    own_dir(ctx).join("TRAE任务.jsonl")
}

fn ms_str(ts: f64) -> String {
    if !ts.is_finite() || ts <= 0.0 {
        return String::new();
    }
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ts as i64)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn id_day(sid: &str) -> String {
    let head: String = sid.chars().take(8).collect();
    let ts = match i64::from_str_radix(&head, 16) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    if !(1420070400..=2051222400).contains(&ts) {
        return String::new();
    }
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

struct StateRow {
    sid: String,
    path: String,
    dir: String,
}

static STATE: Mutex<Option<(f64, Vec<StateRow>)>> = Mutex::new(None);

/// `_state_rows()`：从 state.vscdb 读「会话 → 工作区」（60 秒缓存）。
fn state_rows() -> Vec<StateRow> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    {
        let g = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, rows)) = g.as_ref() {
            if !rows.is_empty() && now - *at < 60.0 {
                return rows
                    .iter()
                    .map(|r| StateRow { sid: r.sid.clone(), path: r.path.clone(), dir: r.dir.clone() })
                    .collect();
            }
        }
    }
    let db = state_db();
    if !db.exists() {
        return Vec::new();
    }
    let sql = "select key, value from ItemTable where key like '%content-map%' \
               or key like '%local-project-folders%' or key = 'solo-lite.project-workspace-cache'";
    let Some(rows) = super::platform::sqlite_rows(&db, sql, std::time::Duration::from_secs(2)) else {
        return Vec::new();
    };
    let mut out: Vec<StateRow> = Vec::new();
    for r in rows {
        let k = str_or(r.get("key"));
        let v = str_or(r.get("value"));
        let Ok(data) = serde_json::from_str::<Value>(&v) else { continue };
        let Some(map) = data.as_object() else { continue };
        if k.contains("local-project-folders") {
            for (sid, path) in map {
                out.push(StateRow {
                    sid: sid.clone(),
                    path: str_or(Some(path)),
                    dir: String::new(),
                });
            }
            continue;
        }
        if k.contains("project-workspace-cache") {
            for (sid, row) in map {
                let Some(o) = row.as_object() else { continue };
                let mut path = str_or(o.get("identity"));
                if path.is_empty() {
                    if let Some(Value::Array(folders)) = o.get("folders") {
                        path = folders.first().map(|x| str_or(Some(x))).unwrap_or_default();
                    }
                }
                out.push(StateRow { sid: sid.clone(), path, dir: String::new() });
            }
            continue;
        }
        for (sid, row) in map {
            let uri = row
                .as_object()
                .map(|o| str_or(o.get("uri")))
                .unwrap_or_default();
            out.push(StateRow { sid: sid.clone(), path: String::new(), dir: uri });
        }
    }
    {
        let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
        *g = Some((now, out.iter().map(|r| StateRow { sid: r.sid.clone(), path: r.path.clone(), dir: r.dir.clone() }).collect()));
    }
    out
}

/// `_uri_path`：file-workspace:?file%3Dfile%253A… 两层转义 → 本地路径。
/// 照 `urllib.parse` 的三步走：`unquote(q)` → `parse_qs`（**值再解一次码**，
/// `+` 当空格）→ 认得 `file:`/`remote-sandbox:` 就取 `urlparse().path`，末尾再 `unquote`。
fn uri_path(uri: &str) -> String {
    let q = match uri.split_once('?') {
        Some((_, r)) => r,
        None => "",
    };
    let dec = percent_decode(q);
    let mut file = String::new();
    for pair in dec.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        if k == "file" {
            // parse_qs 会把值再解一次码；`+` 是空格
            file = unquote_plus(it.next().unwrap_or(""));
        }
    }
    if file.starts_with("file:") || file.starts_with("remote-sandbox:") {
        // urlparse(f).path：去掉 scheme://netloc，从第一个 '/' 起算
        if let Some(idx) = file.find("//") {
            let rest = &file[idx + 2..];
            let path = match rest.find('/') {
                Some(i) => &rest[i..],
                None => "",
            };
            return percent_decode(path);
        }
        if let Some(rest) = file.strip_prefix("file:") {
            return percent_decode(rest);
        }
    }
    percent_decode(&file)
}

fn unquote_plus(s: &str) -> String {
    percent_decode(&s.replace('+', " "))
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().to_string()
}

/// `_snap_days()`：{sid: {from,to}} —— 快照目录 git 提交日期。
fn snap_days() -> HashMap<String, (String, String)> {
    let mut out = HashMap::new();
    let root = snap_root();
    let Ok(rd) = std::fs::read_dir(&root) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let mut one = String::new();
        if let Ok(md) = std::fs::metadata(&p) {
            if let Ok(mt) = md.modified() {
                use chrono::{Local, TimeZone};
                if let Some(dt) = mt.duration_since(std::time::UNIX_EPOCH).ok().and_then(|d| Local.timestamp_opt(d.as_secs() as i64, 0).single()) {
                    one = dt.format("%Y-%m-%d").to_string();
                }
            }
        }
        let mut first = one.clone();
        let mut last = one;
        let g = p.join("v2");
        if g.join(".git").exists() {
            let o = super::platform::run_cmd(
                &["git", "-C", &g.to_string_lossy(), "log", "--format=%ad", "--date=short"],
                std::time::Duration::from_secs(4),
            );
            let ds: Vec<String> = o.lines().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
            if !ds.is_empty() {
                first = ds.iter().min().cloned().unwrap_or_default();
                last = ds.iter().max().cloned().unwrap_or_default();
            }
        }
        let name = e.file_name().to_string_lossy().to_string();
        out.insert(name, (first, last));
    }
    out
}

/// `session_projects()`
pub fn session_projects() -> Vec<Value> {
    let snap = snap_days();
    let mut rows: Vec<Value> = Vec::new();
    for r in state_rows() {
        let path = if !r.path.is_empty() { r.path.clone() } else { uri_path(&r.dir) };
        let mut name = String::new();
        if !path.is_empty() && path.contains('/') {
            let p = Path::new(&path);
            let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let fname = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            name = if p.extension().is_some() && !stem.is_empty() { stem } else { fname };
        }
        if name.is_empty() || name.starts_with("remote-sandbox") || name.starts_with('.') {
            continue;
        }
        if name.chars().count() > 60 || name == "assistant" || name == "temp" || name.starts_with("temp_") {
            continue;
        }
        let d = id_day(&r.sid);
        let (mut f, mut t) = snap.get(&r.sid).cloned().unwrap_or_default();
        if f.is_empty() {
            f = d.clone();
        }
        if t.is_empty() {
            t = d;
        }
        rows.push(json!({"task": name, "path": path, "sid": r.sid,
                         "from": f, "to": t, "source": "项目会话"}));
    }
    // mcps 目录：s_<任务名>-<hash>
    if let Ok(rd) = std::fs::read_dir(mcp_root()) {
        let mut dirs: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            let fname = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if !fname.starts_with("s_") {
                continue;
            }
            let raw = &fname[2..];
            let name = trim_hash_suffix(raw);
            if name.is_empty() {
                continue;
            }
            let day = std::fs::metadata(&d)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|x| chrono::Local.timestamp_opt(x.as_secs() as i64, 0).single())
                .map(|t| t.format("%Y-%m-%d").to_string())
                .unwrap_or_default();
            rows.push(json!({"task": name, "path": "", "sid": fname,
                             "from": day, "to": day, "source": "代理缓存"}));
        }
    }
    // 同名合并
    let mut merged: Vec<Value> = Vec::new();
    for r in rows {
        let task = str_or(r.get("task"));
        if let Some(old) = merged.iter_mut().find(|x| str_or(x.get("task")) == task) {
            let of = str_or(old.get("from"));
            let rf = str_or(r.get("from"));
            if !rf.is_empty() && (of.is_empty() || rf < of) {
                old["from"] = json!(rf);
            }
            let ot = str_or(old.get("to"));
            let rt = str_or(r.get("to"));
            if !rt.is_empty() && (ot.is_empty() || rt > ot) {
                old["to"] = json!(rt);
            }
            if str_or(r.get("source")) == "项目会话" {
                old["source"] = json!("项目会话");
            }
        } else {
            merged.push(r);
        }
    }
    merged.sort_by(|a, b| {
        let ka = (str_or(a.get("to")), str_or(a.get("task")));
        let kb = (str_or(b.get("to")), str_or(b.get("task")));
        kb.cmp(&ka)
    });
    merged
}

fn trim_hash_suffix(raw: &str) -> String {
    // re.sub(r"-[\da-f]{6,12}$", "", raw)
    let chars: Vec<char> = raw.chars().collect();
    for n in (6..=12).rev() {
        if chars.len() > n + 1 {
            let idx = chars.len() - n - 1;
            if chars[idx] == '-' && chars[idx + 1..].iter().all(|c| c.is_ascii_hexdigit()) {
                return chars[..idx].iter().collect::<String>().trim().to_string();
            }
        }
    }
    raw.trim().to_string()
}

/// `task_rows(since_ms)`：OCR 补录的任务名。
pub fn task_rows(ctx: &Ctx, since_ms: f64) -> Vec<Value> {
    let p = seed_path(ctx);
    if !p.exists() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&p) else { return Vec::new() };
    let mut out = Vec::new();
    for ln in text.lines() {
        let Ok(d) = serde_json::from_str::<Value>(ln) else { continue };
        let t = py_double(d.get("at").unwrap_or(&Value::Null)) * 1000.0;
        if t < since_ms {
            continue;
        }
        let task = str_or(d.get("task")).trim().to_string();
        if task.is_empty() {
            continue;
        }
        let ms = ms_str(t);
        let date: String = ms.chars().take(10).collect();
        let image = Path::new(&str_or(d.get("image")))
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let source = {
            let s = str_or(d.get("source"));
            if s.is_empty() { "截图 OCR".to_string() } else { s }
        };
        out.push(json!({"task": task, "at": ms, "date": date, "source": source, "image": image}));
    }
    out
}
