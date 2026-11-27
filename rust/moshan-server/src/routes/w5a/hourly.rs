// SPDX-License-Identifier: Apache-2.0
// 职责：/api/hourly —— 某一天的 24 个自然小时 token 柱状。对应
//       hostwork.hourly_bundle / hourly_tokens / hourly_days / _hour_scan_file /
//       _hour_render，外加 agentbus.hour_buckets（自报账那一层）。
//
// 不做：不迁 overview 里那份「顺便带回今天 24 根柱子」的调用（overview 本身还没迁），
//       也不碰 price 折算 —— 这里只数 token，不算钱。
//
// 口径要点（差一处就成片错）：
//   · 缓存 45 秒；磁盘缓存 <数据根>/_本机工作/缓存/小时.json（v2），
//     只存过盘的那份**不含** '@agents/usage.jsonl' 这条合成记录；
//   · 按文件大小增量扫：ino 变了或文件变小就从头来；只吃最后一个换行之前的整行，
//     半行留到下次 —— 否则 JSON 解析失败会把一条用量永久丢掉；
//   · 文件按 mtime 倒序扫（刚说过话的会话先扫），**这个顺序决定 models 的键序**，
//     接口逐字节比的是这个顺序；
//   · 只认带 `"usage"` 子串的行；`totalTokens` 为 0/缺就不算一次调用；
//   · 桶只留最近 30 天；渲染时补齐 24 个空桶。
#![allow(dead_code)]

use crate::hostwork::platform;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{Local, TimeZone};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use super::cards;
use super::util::{int_of, now_ymd, str_or, truthy};
use crate::routes::common::{peek, q, CtxState};

const HOUR_TTL: f64 = 45.0;
const HOUR_DAYS: i64 = 30;

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `_hour_key`：epoch 毫秒 → 本地 `YYYY-MM-DDTH`。
fn hour_key(ms: i64) -> String {
    match Local.timestamp_millis_opt(ms).single() {
        Some(t) => t.format("%Y-%m-%dT%H").to_string(),
        None => String::new(),
    }
}

/// `_norm_day`：只认 `YYYY-MM-DD`，别的（空串 / 乱写 / 不存在的日子）都给今天。
fn norm_day(s: &str) -> String {
    let t = s.trim();
    let ok_shape = t.len() == 10
        && t.as_bytes()[4] == b'-'
        && t.as_bytes()[7] == b'-'
        && t[0..4].bytes().all(|b| b.is_ascii_digit())
        && t[5..7].bytes().all(|b| b.is_ascii_digit())
        && t[8..10].bytes().all(|b| b.is_ascii_digit());
    if ok_shape {
        if let Ok(d) = chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d") {
            return d.format("%Y-%m-%d").to_string();
        }
    }
    now_ymd()
}

fn keep_days() -> HashSet<String> {
    let now = Local::now();
    (0..HOUR_DAYS)
        .map(|i| (now - chrono::Duration::days(i)).format("%Y-%m-%d").to_string())
        .collect()
}

fn four(n_in: i64, n_out: i64, cached: i64, calls: i64) -> Value {
    json!({"in": n_in, "out": n_out, "cached": cached, "calls": calls})
}

fn bucket_empty() -> Value {
    json!({"in": 0, "out": 0, "cached": 0, "calls": 0,
           "models": Map::new(), "apps": Map::new()})
}

/// 把一个 {in,out,cached,calls} 累加进目标桶的 models/apps 子字典。
fn add_dim(bucket: &mut Map<String, Value>, dim: &str, name: &str,
           n_in: i64, n_out: i64, cached: i64, calls: i64) {
    if !bucket.contains_key(dim) {
        bucket.insert(dim.to_string(), Value::Object(Map::new()));
    }
    let Some(Value::Object(m)) = bucket.get_mut(dim) else { return };
    let e = m.entry(name.to_string()).or_insert_with(|| four(0, 0, 0, 0));
    if let Value::Object(em) = e {
        for (k, add) in [("in", n_in), ("out", n_out), ("cached", cached), ("calls", calls)] {
            let cur = em.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            em.insert(k.to_string(), json!(cur + add));
        }
    }
}

fn add_four(bucket: &mut Map<String, Value>, n_in: i64, n_out: i64, cached: i64, calls: i64) {
    for (k, add) in [("in", n_in), ("out", n_out), ("cached", cached), ("calls", calls)] {
        let cur = bucket.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
        bucket.insert(k.to_string(), json!(cur + add));
    }
}

// ------------------------------------------------------------------ 文件增量扫

/// `_hour_scan_file`：一个会话日志的小时桶。`prev` 是上次扫到的状态（可空）。
fn hour_scan_file(path: &Path, prev: Option<&Value>, app: &str) -> Option<Value> {
    let md = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return prev.cloned(),
    };
    let size = platform::file_size(&md) as i64;
    let mtime = platform::file_mtime(&md);
    let ino = platform::file_inode(&md);

    let mut off: i64 = 0;
    let mut hours: Map<String, Value> = Map::new();
    if let Some(p) = prev {
        let p_ino = p.get("ino").and_then(|v| v.as_i64()).unwrap_or(0);
        let p_size = p.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
        if p_ino == ino as i64 && p_size <= size && !p.get("hours").map(|h| h.is_null()).unwrap_or(true) {
            off = p.get("off").and_then(|v| v.as_i64()).unwrap_or(0);
            if let Some(Value::Object(h)) = p.get("hours") {
                hours = h.clone();
            }
        }
    }

    if size == off {
        if let Some(p) = prev {
            let mut q = p.clone();
            if let Value::Object(m) = &mut q {
                m.insert("size".into(), json!(size));
                m.insert("mtime".into(), json!(mtime));
                m.insert("ino".into(), json!(ino));
            }
            return Some(q);
        }
        return Some(Value::Object(Map::new()));
    }

    let mut buf = Vec::new();
    let read_ok = (|| -> Option<bool> {
        let mut f = std::fs::File::open(path).ok()?;
        if f.seek(SeekFrom::Start(off.max(0) as u64)).is_err() {
            return None;
        }
        f.read_to_end(&mut buf).ok()?;
        Some(true)
    })()
    .unwrap_or(false);
    if !read_ok {
        return prev.cloned();
    }

    let cut = match buf.iter().rposition(|b| *b == b'\n') {
        Some(c) => c,
        None => return prev.cloned(),
    };
    let chunk = &buf[..cut + 1];

    for raw in chunk.split(|b| *b == b'\n') {
        if raw.is_empty() {
            continue;
        }
        if !contains_bytes(raw, b"\"usage\"") {
            continue;
        }
        let text = String::from_utf8_lossy(raw);
        let d: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let pd = d.get("providerData").cloned().unwrap_or(Value::Null);
        let u = pd.get("usage").cloned().unwrap_or(Value::Null);
        if !u.is_object() || !truthy(u.get("totalTokens").unwrap_or(&Value::Null)) {
            continue;
        }
        let ts = int_of(d.get("timestamp"));
        if ts == 0 {
            continue;
        }
        let k = hour_key(ts);
        if k.is_empty() {
            continue;
        }
        let tin = {
            let a = int_of(u.get("inputTokens"));
            if a != 0 { a } else { int_of(u.get("input_tokens")) }
        };
        let tout = {
            let a = int_of(u.get("outputTokens"));
            if a != 0 { a } else { int_of(u.get("output_tokens")) }
        };
        let cache = match u.get("inputTokensDetails") {
            Some(Value::Array(a)) if !a.is_empty() && a[0].is_object() => int_of(a[0].get("cached_tokens")),
            _ => 0,
        };
        let model = {
            let a = str_or(pd.get("model"));
            if a.is_empty() { str_or(pd.get("requestModelName")) } else { a }
        };
        let model = if model.is_empty() { "未知模型".to_string() } else { model };

        let b = hours.entry(k).or_insert_with(bucket_empty);
        if let Value::Object(bm) = b {
            add_four(bm, tin, tout, cache, 1);
            add_dim(bm, "models", &model, tin, tout, cache, 1);
            add_dim(bm, "apps", app, tin, tout, cache, 1);
        }
    }

    let keep = keep_days();
    hours.retain(|k, _| keep.contains(&k[..k.len().min(10)]));

    Some(json!({"v": 2, "off": off + cut as i64 + 1, "size": size,
                "mtime": mtime, "ino": ino, "app": app, "hours": hours}))
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// ------------------------------------------------------------------ 自报账层

/// `agentbus._parse_usage_line` + `hour_buckets`：自报账折成同样的小时桶。
fn agent_hour_buckets(usage: &Path) -> Map<String, Value> {
    let mut out: Map<String, Value> = Map::new();
    let mut f = match std::fs::File::open(usage) {
        Ok(f) => f,
        Err(_) => return out,
    };
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return out;
    }
    let cut = match buf.iter().rposition(|b| *b == b'\n') {
        Some(c) => c,
        None => return out,
    };
    let text = String::from_utf8_lossy(&buf[..cut + 1]).to_string();
    for raw in text.split('\n') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let d: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !d.is_object() {
            continue;
        }
        let ts = int_of(d.get("ts"));
        if ts == 0 {
            continue;
        }
        let k = hour_key(ts);
        if k.is_empty() {
            continue;
        }
        let tin = int_nonneg(d.get("in"));
        let tout = int_nonneg(d.get("out"));
        let cached = int_nonneg(d.get("cached"));
        let calls = {
            let c = int_nonneg(d.get("calls"));
            if c != 0 { c } else { 1 }
        };
        let model = {
            let m = str_or(d.get("model"));
            let m = if m.is_empty() { "未知模型".to_string() } else { m };
            m.chars().take(60).collect::<String>()
        };
        let app = {
            let c = str_or(d.get("client"));
            let c = if c.is_empty() { "agent".to_string() } else { c };
            c.chars().take(24).collect::<String>()
        };
        let b = out.entry(k).or_insert_with(bucket_empty);
        if let Value::Object(bm) = b {
            add_four(bm, tin, tout, cached, calls);
            add_dim(bm, "models", &model, tin, tout, cached, calls);
            add_dim(bm, "apps", &app, tin, tout, cached, calls);
        }
    }
    let keep = keep_days();
    out.retain(|k, _| keep.contains(&k[..k.len().min(10)]));
    out
}

/// `agentbus._int`：int(float(v))，失败或 ≤0 都算 0。
fn int_nonneg(v: Option<&Value>) -> i64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.trim().parse::<f64>().unwrap_or(0.0),
        Some(Value::Bool(b)) => {
            if *b { 1.0 } else { 0.0 }
        }
        _ => 0.0,
    };
    let n = n as i64;
    if n > 0 { n } else { 0 }
}

// ------------------------------------------------------------------ 进程内缓存

struct HourCache {
    files: Map<String, Value>,
    at: f64,
}

static HOUR: Mutex<Option<HourCache>> = Mutex::new(None);

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn hourly_path(ctx: &crate::paths::Ctx) -> PathBuf {
    crate::paths::rt_join(ctx, "_本机工作").join("缓存").join("小时.json")
}

fn agents_usage_path(ctx: &crate::paths::Ctx) -> PathBuf {
    crate::paths::rt_join(ctx, "_工作台").join("agents").join("usage.jsonl")
}

/// `_hour_render`：补 24 个空桶，按文件迭代顺序聚合（键序照扫到的先后）。
fn hour_render(files: &Map<String, Value>, day: &str) -> Vec<Value> {
    let keys: Vec<String> = (0..24).map(|h| format!("{day}T{h:02}")).collect();
    let want: HashSet<&str> = keys.iter().map(|s| s.as_str()).collect();
    let mut agg: Map<String, Value> = Map::new();
    for e in files.values() {
        let Some(Value::Object(hours)) = e.get("hours") else { continue };
        for (k, v) in hours {
            if !want.contains(k.as_str()) {
                continue;
            }
            let cur = agg.entry(k.clone()).or_insert_with(bucket_empty);
            let Value::Object(cm) = cur else { continue };
            let (a, b, c, d) = (
                v.get("in").and_then(|x| x.as_i64()).unwrap_or(0),
                v.get("out").and_then(|x| x.as_i64()).unwrap_or(0),
                v.get("cached").and_then(|x| x.as_i64()).unwrap_or(0),
                v.get("calls").and_then(|x| x.as_i64()).unwrap_or(0),
            );
            add_four(cm, a, b, c, d);
            for dim in ["models", "apps"] {
                let Some(Value::Object(dv)) = v.get(dim) else { continue };
                for (name, mv) in dv {
                    let (a, b, c, d) = (
                        mv.get("in").and_then(|x| x.as_i64()).unwrap_or(0),
                        mv.get("out").and_then(|x| x.as_i64()).unwrap_or(0),
                        mv.get("cached").and_then(|x| x.as_i64()).unwrap_or(0),
                        mv.get("calls").and_then(|x| x.as_i64()).unwrap_or(0),
                    );
                    add_dim(cm, dim, name, a, b, c, d);
                }
            }
        }
    }
    let mut out = Vec::with_capacity(24);
    for k in keys {
        let v = agg.get(&k).cloned().unwrap_or_else(bucket_empty);
        out.push(json!({
            "key": k,
            "h": &k[k.len() - 2..],
            "in": v.get("in").cloned().unwrap_or(json!(0)),
            "out": v.get("out").cloned().unwrap_or(json!(0)),
            "cached": v.get("cached").cloned().unwrap_or(json!(0)),
            "calls": v.get("calls").cloned().unwrap_or(json!(0)),
            "models": v.get("models").cloned().unwrap_or_else(|| Value::Object(Map::new())),
            "apps": v.get("apps").cloned().unwrap_or_else(|| Value::Object(Map::new())),
        }));
    }
    out
}

fn hourly_days_from(files: &Map<String, Value>) -> Vec<String> {
    let mut ds: HashSet<String> = HashSet::new();
    for e in files.values() {
        let Some(Value::Object(hours)) = e.get("hours") else { continue };
        for k in hours.keys() {
            ds.insert(k[..k.len().min(10)].to_string());
        }
    }
    let mut v: Vec<String> = ds.into_iter().collect();
    v.sort();
    v.reverse();
    v
}

/// `hourly_tokens`：某一天的 24 根柱子（day 为空就是今天）。
fn hourly_tokens(ctx: &crate::paths::Ctx, day: &str, fresh: bool) -> Vec<Value> {
    let day = norm_day(day);
    let mut guard = match HOUR.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    let now = now_secs();

    if !fresh {
        if let Some(c) = guard.as_ref() {
            if now - c.at < HOUR_TTL {
                return hour_render(&c.files, &day);
            }
        }
    }

    // 没有内存态就先吃磁盘缓存
    if guard.as_ref().map(|c| c.files.is_empty()).unwrap_or(true) {
        if let Some(disk) = crate::jsonio::read_json(&hourly_path(ctx)) {
            if disk.get("v").and_then(|v| v.as_i64()) == Some(2) {
                let files = match disk.get("files") {
                    Some(Value::Object(m)) => m.clone(),
                    _ => Map::new(),
                };
                *guard = Some(HourCache { files, at: 0.0 });
            }
        }
    }

    let prev_files: Map<String, Value> = guard.as_ref().map(|c| c.files.clone()).unwrap_or_default();
    let mut files: Map<String, Value> = Map::new();

    // 候选：~/.workbuddy/projects/*/*.jsonl，30 天内动过的必扫；
    // 没动过但缓存里也没有的补扫一次（否则往回翻永远空白）。
    let root = home_dir().join(".workbuddy").join("projects");
    let mut cand: Vec<(f64, PathBuf)> = Vec::new();
    if root.exists() {
        for (dname, is_dir, _) in super::util::scandir(&root) {
            if !is_dir || dname.starts_with('.') {
                continue;
            }
            let d = root.join(&dname);
            for (fname, f_dir, _) in super::util::scandir(&d) {
                if f_dir || !fname.ends_with(".jsonl") {
                    continue;
                }
                let p = d.join(&fname);
                let mt = match std::fs::metadata(&p) {
                    Ok(m) => platform::file_mtime(&m),
                    Err(_) => continue,
                };
                let key = p.to_string_lossy().to_string();
                if now - mt < (HOUR_DAYS * 86400) as f64 || !prev_files.contains_key(&key) {
                    cand.push((mt, p));
                }
            }
        }
    }
    // 刚说过话的会话先扫：这个顺序决定 models 的键序
    cand.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    for (_, p) in &cand {
        let key = p.to_string_lossy().to_string();
        let prev = prev_files.get(&key);
        if let Some(e) = hour_scan_file(p, prev, "workbuddy") {
            if e.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
                files.insert(key, e);
            }
        }
    }

    // 自报账：单独折成同样的小时桶，key 以 '@' 开头
    let ah = agent_hour_buckets(&agents_usage_path(ctx));
    if !ah.is_empty() {
        files.insert(
            "@agents/usage.jsonl".to_string(),
            json!({"v": 2, "off": 0, "size": 0, "mtime": 0, "ino": 0,
                   "app": "agents", "hours": ah}),
        );
    }

    *guard = Some(HourCache { files: files.clone(), at: now });
    drop(guard);

    // 落盘缓存：剔掉 '@' 那条（每次现算，存进去只会变成没人更新的旧账）
    let mut disk_files = Map::new();
    for (k, v) in &files {
        if !k.starts_with('@') {
            disk_files.insert(k.clone(), v.clone());
        }
    }
    let payload = json!({"v": 2, "files": disk_files});
    if let Ok(text) = serde_json::to_string(&payload) {
        let p = hourly_path(ctx);
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = crate::jsonio::atomic_write(&p, &text);
    }

    hour_render(&files, &day)
}

// ------------------------------------------------------------------ 路由

/// `hourly_bundle`：`?d=YYYY-MM-DD&fresh=1`。
pub async fn hourly_api(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let day = q(&uri, "d");
    let fresh = q(&uri, "fresh");
    let fresh = truthy(&Value::String(fresh));
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || {
        let d = norm_day(&day);
        let hours = hourly_tokens(&ctx2, &d, fresh);
        let files = HOUR
            .lock()
            .map(|g| g.as_ref().map(|c| c.files.clone()).unwrap_or_default())
            .unwrap_or_default();
        json!({"day": d, "today": now_ymd(), "hours": hours,
               "days": hourly_days_from(&files)})
    })
    .await;
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(json!({"ok": false, "error": format!("{}", e), "hours": []})).into_response(),
    }
}
