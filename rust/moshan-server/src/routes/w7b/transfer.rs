// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/transfer.py` 的移植 + `/api/transfer*` 六条路由。
// 不做：不实现微信媒体通道本身（那是 `crate::wechat`）。回推走后台线程：
//       add/upload/repush 立刻返回，推送结果逐次记进 `推送失败.jsonl`（`log_push`），
//       卡片红条与巡检的 `transfer_push` 都读这本账。

#![allow(dead_code)]

use super::util::{
    bad_int_query, dump_compact, dump_default, file_response, int_of, iso_with_offset,
    mime_of, missing_query, parse_body, parse_when, py_str, py_truthy, trunc,
};
use crate::paths::Ctx;
use crate::routes::common::{http_err, peek, q, query_map, CtxState};
use axum::body::Bytes;
use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::{DateTime, Local};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DIR_NAME: &str = "14-传输";
const PUSH_LOG: &str = "推送失败.jsonl";
const PUSH_KEEP: usize = 500;

// ---------------------------------------------------------------- 路径

/// `transfer._root` → `dash._root`：`allowed_roots[0]`，否则收件箱的上级。
pub fn data_root(ctx: &Ctx) -> PathBuf {
    let roots: Vec<String> = ctx
        .cfg
        .get("allowed_roots")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
        .unwrap_or_default();
    if let Some(first) = roots.first() {
        return PathBuf::from(first);
    }
    let inbox = ctx
        .cfg
        .get("inbox_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    Path::new(inbox)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn base_dir(ctx: &Ctx) -> PathBuf {
    data_root(ctx).join(DIR_NAME)
}

fn store_path(ctx: &Ctx) -> PathBuf {
    base_dir(ctx).join("消息.jsonl")
}

fn files_dir(ctx: &Ctx, at: &DateTime<Local>) -> PathBuf {
    base_dir(ctx).join("文件").join(at.format("%Y-%m").to_string())
}

fn trash_dir(ctx: &Ctx) -> PathBuf {
    base_dir(ctx).join("_回收站")
}

// ---------------------------------------------------------------- 小工具

fn new_id(at: &DateTime<Local>) -> String {
    format!("tf{}{}", at.format("%y%m%d%H%M%S"), super::util::rand_hex(4))
}

fn human(n: i64) -> String {
    let n = n.max(0);
    for (unit, size) in [("GB", 1i64 << 30), ("MB", 1i64 << 20), ("KB", 1i64 << 10)] {
        if n >= size {
            let v = n as f64 / size as f64;
            let s = format!("{v:.1}");
            let s = s.trim_end_matches('0').trim_end_matches('.');
            return format!("{s} {unit}");
        }
    }
    format!("{n} B")
}

fn safe_name(name: &str, fallback: &str) -> String {
    let re = regex::Regex::new("[\\\\/:*?\"<>|\\x00-\\x1f]").unwrap();
    let mut out = re.replace_all(name.trim(), "_").into_owned();
    if out.is_empty() {
        out = fallback.to_string();
    }
    out = out.trim_start_matches('.').to_string();
    if out.chars().count() > 90 {
        let (stem, dot, ext) = match out.rfind('.') {
            Some(i) => (&out[..i], &out[i..i + 1], &out[i + 1..]),
            None => (&out[..], "", ""),
        };
        if !dot.is_empty() && ext.chars().count() <= 8 {
            let head: String = stem.chars().take(80).collect();
            out = format!("{head}.{ext}");
        } else {
            out = out.chars().take(90).collect();
        }
    }
    out
}

const IMG_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "tiff", "tif"];
const VID_EXT: &[&str] = &["mp4", "mov", "m4v", "webm", "avi", "mkv"];
const AUD_EXT: &[&str] = &["m4a", "mp3", "wav", "aac", "amr", "silk", "ogg", "flac"];

fn ext_of(p: &Path) -> String {
    p.extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn kind_of(p: &Path, mime: &str) -> &'static str {
    let ext = ext_of(p);
    if IMG_EXT.contains(&ext.as_str()) || mime.starts_with("image/") {
        return "image";
    }
    if VID_EXT.contains(&ext.as_str()) || mime.starts_with("video/") {
        return "video";
    }
    if AUD_EXT.contains(&ext.as_str()) || mime.starts_with("audio/") {
        return "voice";
    }
    "file"
}

pub fn is_image(p: &Path) -> bool {
    IMG_EXT.contains(&ext_of(p).as_str())
}

fn link_re() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r#"https?://[^\s，。、；）)\]】\"'<>]+"#).unwrap())
}

pub fn split_links(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in link_re().find_iter(text) {
        let u = m
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '}']);
        if u.is_empty() || out.iter().any(|x| x == u) {
            continue;
        }
        out.push(u.to_string());
    }
    out
}

// ---------------------------------------------------------------- 读写

fn append(ctx: &Ctx, row: &Value) {
    let p = store_path(ctx);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all((dump_compact(row) + "\n").as_bytes());
    }
}

/// `transfer._journal`：往程序目录（frozen 时状态目录）的 pocket.jsonl 追加一行。
fn journal(ctx: &Ctx, entry: Map<String, Value>) {
    let base = if ctx.frozen { ctx.state_dir.clone() } else { super::util::app_dir(ctx) };
    let p = base.join("pocket.jsonl");
    let mut rec = Map::new();
    rec.insert(
        "ts".into(),
        json!(Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string()),
    );
    for (k, v) in entry {
        rec.insert(k, v);
    }
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all((dump_default(&Value::Object(rec)) + "\n").as_bytes());
    }
}

/// 按 id 合并：后写的覆盖先写的，墓碑行剔除；顺序按首次出现。
pub fn load(ctx: &Ctx) -> Vec<Value> {
    let p = store_path(ctx);
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    let mut order: Vec<String> = Vec::new();
    let mut by_id: HashMap<String, Value> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Value::Object(m) = &row else { continue };
        let iid = py_str(m.get("id").unwrap_or(&Value::Null));
        if iid.is_empty() {
            continue;
        }
        if py_truthy(m.get("deleted")) {
            by_id.remove(&iid);
            continue;
        }
        if !by_id.contains_key(&iid) {
            order.push(iid.clone());
        }
        by_id.insert(iid, row);
    }
    order
        .into_iter()
        .filter_map(|i| by_id.get(&i).cloned())
        .collect()
}

// ---------------------------------------------------------------- 写入

pub struct AddArgs {
    pub text: String,
    pub path: String,
    pub name: String,
    pub src: String,
    pub conv: String,
    pub mime: String,
    pub at: String,
    pub mv: bool,
}

pub fn add(ctx: &Ctx, a: AddArgs) -> Value {
    let text = a.text.trim().to_string();
    let src_path = if a.path.is_empty() { None } else { Some(PathBuf::from(&a.path)) };
    if let Some(p) = &src_path {
        if !p.is_file() {
            return json!({"ok": false, "error": "找不到这个文件"});
        }
    }
    if text.is_empty() && src_path.is_none() {
        return json!({"ok": false, "error": "空消息"});
    }

    let mut when = Local::now();
    if !a.at.is_empty() {
        if let Some(d) = parse_when(&a.at) {
            when = d;
        }
    }
    let mut item = Map::new();
    item.insert("id".into(), json!(new_id(&when)));
    item.insert("at".into(), json!(iso_with_offset(&when)));
    item.insert("src".into(), json!(if a.src.is_empty() { "web" } else { &a.src }));
    item.insert("conv".into(), json!(a.conv));

    let final_kind;
    if let Some(sp) = src_path {
        let raw_name = if a.name.is_empty() {
            sp.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        } else {
            a.name.clone()
        };
        let name = safe_name(&raw_name, "未命名");
        let mime = if a.mime.is_empty() {
            mime_of(Path::new(&name))
        } else {
            a.mime.clone()
        };
        let d = files_dir(ctx, &when);
        if std::fs::create_dir_all(&d).is_err() {
            return json!({"ok": false, "error": format!("存文件失败：目录建不出来")});
        }
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let dest = d.join(format!("{id}_{name}"));
        let done = if a.mv {
            std::fs::rename(&sp, &dest).or_else(|_| {
                // 跨卷时 shutil.move 会先复制再删；这里同样退一步。
                std::fs::copy(&sp, &dest).and_then(|_| std::fs::remove_file(&sp))
            })
        } else {
            std::fs::copy(&sp, &dest).map(|_| ())
        };
        if let Err(e) = done {
            return json!({"ok": false, "error": format!("存文件失败：{e}")});
        }
        let size = std::fs::metadata(&dest).map(|m| m.len() as i64).unwrap_or(0);
        final_kind = kind_of(&dest, &mime).to_string();
        let rel = dest
            .strip_prefix(base_dir(ctx))
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| dest.to_string_lossy().to_string());
        item.insert("kind".into(), json!(final_kind));
        item.insert("name".into(), json!(name));
        item.insert("size".into(), json!(size));
        item.insert("mime".into(), json!(mime));
        item.insert("path".into(), json!(rel));
        let kept = if final_kind == "file" { text.clone() } else { String::new() };
        item.insert("text".into(), json!(kept));
    } else {
        let links = split_links(&text);
        let rest = link_re().replace_all(&text, "").to_string();
        let rest = rest.trim_matches(|c| " \n,，。、".contains(c));
        final_kind = if !links.is_empty() && rest.is_empty() { "link".into() } else { "text".into() };
        item.insert("kind".into(), json!(final_kind));
        item.insert("text".into(), json!(text));
        item.insert("links".into(), json!(links));
    }

    let row = Value::Object(item.clone());
    append(ctx, &row);
    if a.src != "web" {
        let mut j = Map::new();
        j.insert("event".into(), json!("tf_in"));
        j.insert("kind".into(), item.get("kind").cloned().unwrap_or(Value::Null));
        j.insert(
            "name".into(),
            json!(item.get("name").and_then(|v| v.as_str()).unwrap_or("")),
        );
        j.insert("size".into(), json!(int_of(item.get("size"))));
        j.insert(
            "text".into(),
            json!(trunc(item.get("text").and_then(|v| v.as_str()).unwrap_or(""), 80)),
        );
        j.insert("from".into(), json!(a.src));
        journal(ctx, j);
    }
    json!({"ok": true, "item": item})
}

pub fn remove(ctx: &Ctx, iid: &str) -> Value {
    let iid = iid.trim().to_string();
    if iid.is_empty() {
        return json!({"ok": false, "error": "没给 id"});
    }
    let hit = load(ctx)
        .into_iter()
        .find(|x| x.get("id").and_then(|v| v.as_str()).unwrap_or("") == iid);
    let Some(hit) = hit else {
        return json!({"ok": false, "error": "这条已经不在了"});
    };
    let rel = hit.get("path").map(py_str).unwrap_or_default();
    if !rel.is_empty() {
        let src = base_dir(ctx).join(&rel);
        if src.is_file() {
            let t = trash_dir(ctx);
            let _ = std::fs::create_dir_all(&t);
            if let Some(name) = src.file_name() {
                let _ = std::fs::rename(&src, t.join(name));
            }
        }
    }
    let mut tomb = Map::new();
    tomb.insert("id".into(), json!(iid));
    tomb.insert("deleted".into(), json!(true));
    tomb.insert("at".into(), json!(super::util::now_iso()));
    append(ctx, &Value::Object(tomb));
    let mut j = Map::new();
    j.insert("event".into(), json!("tf_del"));
    j.insert("kind".into(), hit.get("kind").cloned().unwrap_or(Value::Null));
    j.insert("name".into(), json!(hit.get("name").map(py_str).unwrap_or_default()));
    j.insert(
        "text".into(),
        json!(trunc(hit.get("text").map(py_str).unwrap_or_default().as_str(), 60)),
    );
    journal(ctx, j);
    json!({"ok": true, "id": iid})
}

// ---------------------------------------------------------------- 读出

pub fn file_of(ctx: &Ctx, iid: &str) -> Option<PathBuf> {
    let hit = load(ctx)
        .into_iter()
        .find(|x| x.get("id").and_then(|v| v.as_str()).unwrap_or("") == iid)?;
    let rel = hit.get("path").map(py_str).unwrap_or_default();
    if rel.is_empty() {
        return None;
    }
    let p = base_dir(ctx).join(rel);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

pub fn items(ctx: &Ctx, limit: i64, qs: &str, kind: &str) -> Value {
    let mut rows = load(ctx);
    if !qs.is_empty() {
        let needle = qs.to_lowercase();
        rows.retain(|r| {
            let text = r.get("text").map(py_str).unwrap_or_default().to_lowercase();
            let name = r.get("name").map(py_str).unwrap_or_default().to_lowercase();
            if text.contains(&needle) || name.contains(&needle) {
                return true;
            }
            r.get("links")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .any(|u| py_str(u).to_lowercase().contains(&needle))
                })
                .unwrap_or(false)
        });
    }
    if !kind.is_empty() && kind != "all" {
        rows.retain(|r| r.get("kind").and_then(|v| v.as_str()).unwrap_or("") == kind);
    }
    let total = rows.len();
    let lim = limit.max(1) as usize;
    let start = rows.len().saturating_sub(lim);
    let tail = &rows[start..];
    let mut out: Vec<Value> = Vec::new();
    for r in tail.iter().rev() {
        let mut x: Map<String, Value> = match r {
            Value::Object(m) => m.clone(),
            _ => Map::new(),
        };
        let size = int_of(x.get("size"));
        let sh = if py_truthy(x.get("size")) { human(size) } else { String::new() };
        x.insert("size_h".into(), json!(sh));
        out.push(Value::Object(x));
    }
    json!({"ok": true, "items": out, "total": total, "at": super::util::now_iso()})
}

pub fn stats(ctx: &Ctx) -> Value {
    let rows = load(ctx);
    let today = Local::now().format("%Y-%m-%d").to_string();
    let sizes: i64 = rows.iter().map(|r| int_of(r.get("size"))).sum();
    let mut n_today = 0;
    let mut n_files = 0;
    let mut n_images = 0;
    let mut n_wechat = 0;
    for r in &rows {
        let at = r.get("at").map(py_str).unwrap_or_default();
        if at.starts_with(&today) {
            n_today += 1;
        }
        let k = r.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(k, "file" | "image" | "video" | "voice") {
            n_files += 1;
        }
        if k == "image" {
            n_images += 1;
        }
        if r.get("src").and_then(|v| v.as_str()).unwrap_or("") == "wechat" {
            n_wechat += 1;
        }
    }
    json!({
        "total": rows.len(),
        "today": n_today,
        "files": n_files,
        "images": n_images,
        "wechat": n_wechat,
        "bytes_h": if sizes > 0 { human(sizes) } else { "0 B".to_string() },
    })
}

fn push_log_path(ctx: &Ctx) -> PathBuf {
    base_dir(ctx).join(PUSH_LOG)
}

fn push_rows(path: &Path) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ln in text.lines() {
        let ln = ln.trim();
        if ln.is_empty() {
            continue;
        }
        if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(ln) {
            if py_truthy(v.get("at")) {
                out.push(v);
            }
        }
    }
    out
}

pub fn push_summary_at(path: &Path, hours: i64) -> Value {
    let rows = push_rows(path);
    if rows.is_empty() {
        return json!({
            "hours": hours, "total": 0, "failed": 0, "ok": 0,
            "error": "", "at": "", "since": "", "names": [], "ids": [],
        });
    }
    let cut = super::util::now_epoch() - hours.max(1) as f64 * 3600.0;
    let mut order: Vec<String> = Vec::new();
    let mut last: HashMap<String, Value> = HashMap::new();
    let mut first: HashMap<String, String> = HashMap::new();
    for r in rows {
        let at = r.get("at").map(py_str).unwrap_or_default();
        let Some(ts) = super::util::iso_epoch(&at) else { continue };
        if ts < cut {
            continue;
        }
        let key = {
            let id = r.get("id").map(py_str).unwrap_or_default();
            if id.is_empty() { at.clone() } else { id }
        };
        if !last.contains_key(&key) {
            order.push(key.clone());
        }
        last.insert(key.clone(), r);
        first.entry(key).or_insert(at);
    }
    let mut bad: Vec<Value> = Vec::new();
    let mut good = 0;
    for k in &order {
        if let Some(r) = last.get(k) {
            if py_truthy(r.get("ok")) {
                good += 1;
            } else {
                bad.push(r.clone());
            }
        }
    }
    bad.sort_by(|a, b| {
        a.get("at").map(py_str).unwrap_or_default().cmp(&b.get("at").map(py_str).unwrap_or_default())
    });
    let since = if bad.is_empty() {
        String::new()
    } else {
        bad.iter()
            .map(|r| {
                let id = r.get("id").map(py_str).unwrap_or_default();
                let key = if id.is_empty() { r.get("at").map(py_str).unwrap_or_default() } else { id };
                first.get(&key).cloned().unwrap_or_default()
            })
            .min()
            .unwrap_or_default()
    };
    let names: Vec<String> = if bad.is_empty() {
        Vec::new()
    } else {
        let start = bad.len().saturating_sub(3);
        bad[start..]
            .iter()
            .map(|r| r.get("name").map(py_str).unwrap_or_default())
            .collect()
    };
    let ids: Vec<String> = bad
        .iter()
        .take(20)
        .map(|r| r.get("id").map(py_str).unwrap_or_default())
        .collect();
    let (err, at) = if bad.is_empty() {
        (String::new(), String::new())
    } else {
        let last_bad = bad.last().unwrap();
        (
            last_bad.get("error").map(py_str).unwrap_or_default(),
            last_bad.get("at").map(py_str).unwrap_or_default(),
        )
    };
    json!({
        "hours": hours,
        "total": last.len(),
        "failed": bad.len(),
        "ok": good,
        "error": err,
        "at": at,
        "since": since,
        "names": names,
        "ids": ids,
    })
}

pub fn push_summary(ctx: &Ctx) -> Value {
    push_summary_at(&push_log_path(ctx), 6)
}

// ---------------------------------------------------------------- 网页 → 微信回推

/// 一次推送最多试三回，间隔越试越久（微信媒体通道会整段坏几分钟）。
const TF_PUSH_BACKOFF: [u64; 3] = [0, 20, 90];

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "image" => "图片",
        "video" => "视频",
        "voice" => "语音",
        _ => "文件",
    }
}

/// `_tf_push_try`：推一次，返回 (本体到了吗, 渠道原话)。
///
/// **「本体到了」不等于「这次调用成功」**：媒体推不出去时退回一句话
/// （［图片］xxx.png），手机上算收到了一条，但那不是人想要的东西 ——
/// 账本按「本体」算，红条才说得清「图没到」。
fn tf_push_try(ctx: &Ctx, item: &Value, fallback: bool) -> (bool, String) {
    let kind_raw = item.get("kind").map(py_str).unwrap_or_default();
    let kind = if kind_raw.is_empty() { "text".to_string() } else { kind_raw };
    if kind == "text" || kind == "link" {
        let text = item.get("text").map(py_str).unwrap_or_default().trim().to_string();
        if text.is_empty() {
            return (false, "空文本".to_string());
        }
        let ok = crate::wechat::send(&text);
        return if ok {
            (true, String::new())
        } else {
            let e = crate::wechat::last_error();
            (false, if e.is_empty() { "文本没发出去".to_string() } else { e })
        };
    }

    let label = kind_label(&kind);
    let name = {
        let n = item.get("name").map(py_str).unwrap_or_default();
        if n.is_empty() { "未命名".to_string() } else { n }
    };
    let caption = format!("［{}］{}", label, name);
    let p = file_of(ctx, &item.get("id").map(py_str).unwrap_or_default());
    let mut ok = false;
    if let Some(p) = p {
        // 媒体本体不带标题行（2026-11-21 用户要求）：caption 只留给失败兜底。
        ok = crate::wechat::send_media(&p, "");
    }
    if ok {
        return (true, String::new());
    }
    let mut err = crate::wechat::last_error();
    if err.is_empty() {
        err = "本体没发出去".to_string();
    }
    if fallback {
        // 只试第一回 —— 重试时再补一句会刷屏。
        if crate::wechat::send(&caption) {
            err.push_str("；只补了一句话");
        }
    }
    (false, err)
}

/// `log_push`：每次尝试都记一行（巡检靠同一个 id 的最后一行判断有没有恢复）。
fn log_push(ctx: &Ctx, item: &Value, ok: bool, error: &str, attempts: usize) {
    let kind = {
        let k = item.get("kind").map(py_str).unwrap_or_default();
        if k.is_empty() { "text".to_string() } else { k }
    };
    let row = json!({
        "at": super::util::now_iso(),
        "id": item.get("id").map(py_str).unwrap_or_default(),
        "name": item.get("name").map(py_str).unwrap_or_default(),
        "kind": kind,
        "ok": ok,
        "attempts": attempts,
        "error": trunc(error, 200),
    });
    let p = push_log_path(ctx);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all((dump_default(&row) + "\n").as_bytes());
    }
    if std::fs::metadata(&p).map(|m| m.len() > 200_000).unwrap_or(false) {
        if let Ok(text) = std::fs::read_to_string(&p) {
            let lines: Vec<&str> = text.lines().collect();
            if lines.len() > PUSH_KEEP {
                let tail = lines[lines.len() - PUSH_KEEP..].join("\n") + "\n";
                let tmp = p.with_file_name(format!(
                    "{}.tmp",
                    p.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default()
                ));
                if std::fs::write(&tmp, tail).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
            }
        }
    }
}

/// `_tf_push_wechat`：三回（0 / 20 / 90 秒），每回一行账。
fn tf_push_wechat(ctx: &Ctx, item: &Value) {
    for (i, delay) in TF_PUSH_BACKOFF.iter().enumerate() {
        if *delay > 0 {
            std::thread::sleep(std::time::Duration::from_secs(*delay));
        }
        let (ok, err) = tf_push_try(ctx, item, i == 0);
        log_push(ctx, item, ok, &err, i + 1);
        let mut ev = Map::new();
        ev.insert("event".into(), json!("tf_push"));
        ev.insert("ok".into(), json!(ok));
        ev.insert(
            "kind".into(),
            item.get("kind").cloned().unwrap_or(Value::Null),
        );
        ev.insert(
            "name".into(),
            json!(trunc(&item.get("name").map(py_str).unwrap_or_default(), 60)),
        );
        ev.insert("attempt".into(), json!(i + 1));
        ev.insert(
            "error".into(),
            json!(if ok { String::new() } else { trunc(&err, 160) }),
        );
        journal(ctx, ev);
        eprintln!(
            "[moshan-rs] {}",
            json!({"event": "tf_push", "ok": ok,
                   "kind": item.get("kind").cloned().unwrap_or(Value::Null),
                   "name": trunc(&item.get("name").map(py_str).unwrap_or_default(), 60),
                   "attempt": i + 1,
                   "error": if ok { String::new() } else { trunc(&err, 160) }})
        );
        if ok {
            return;
        }
    }
}

/// Python 的 `threading.Thread(target=_tf_push_wechat, daemon=True)`。
fn spawn_push(ctx: &std::sync::Arc<Ctx>, item: Value) {
    let ctx = ctx.clone();
    std::thread::spawn(move || tf_push_wechat(&ctx, &item));
}

pub fn payload(ctx: &Ctx, limit: i64, qs: &str, kind: &str) -> Value {
    let mut d = items(ctx, limit, qs, kind);
    d["stats"] = stats(ctx);
    d["push"] = push_summary(ctx);
    d
}

// ---------------------------------------------------------------- 路由

pub async fn list(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let qmap = query_map(&uri);
    let raw_limit = qmap.iter().find(|(k, _)| k == "limit").map(|(_, v)| v.clone());
    let limit = match raw_limit {
        None => 40,
        Some(s) if s.trim().is_empty() => return bad_int_query("limit", &s),
        Some(s) => match s.trim().parse::<i64>() {
            Ok(v) => v,
            Err(_) => return bad_int_query("limit", &s),
        },
    };
    let qs = q(&uri, "q").trim().to_string();
    let kind = q(&uri, "kind").trim().to_string();
    Json(payload(&ctx, limit.clamp(1, 300), &qs, &kind)).into_response()
}

pub async fn add_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match parse_body(&raw) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let src = {
        let s = body.get("src").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default();
        if s.is_empty() { "web".to_string() } else { s }
    };
    let args = AddArgs {
        text: body.get("text").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default(),
        path: body.get("path").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default(),
        name: body.get("name").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default(),
        mime: body.get("mime").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default(),
        src: src.clone(),
        conv: body.get("conv").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default(),
        at: body.get("at").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default(),
        mv: py_truthy(body.get("move")),
    };
    let r = add(&ctx, args);
    let ok = py_truthy(r.get("ok"));
    let push_default = body.get("push").map(|v| py_truthy(Some(v))).unwrap_or(true);
    if ok && src == "web" && push_default {
        // 微信来的不回推，否则自己发的东西自己再收一遍。
        spawn_push(&ctx, r.get("item").cloned().unwrap_or_else(|| json!({})));
    }
    Json(r).into_response()
}

pub async fn upload(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    mut mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let limit = (ctx.max_upload_mb() * 1048576.0) as usize;
    struct Part {
        name: String,
        ctype: String,
        data: Vec<u8>,
        over: bool,
    }
    let mut parts: Vec<Part> = Vec::new();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return http_err(400, &format!("表单解析失败：{e}")),
        };
        if field.name().unwrap_or("") != "files" {
            continue;
        }
        let fname = field.file_name().map(|s| s.to_string()).unwrap_or_default();
        let ctype = field.content_type().map(|s| s.to_string()).unwrap_or_default();
        let mut field = field;
        let mut buf: Vec<u8> = Vec::new();
        let mut over = false;
        loop {
            match field.chunk().await {
                Ok(Some(c)) => {
                    buf.extend_from_slice(&c);
                    if buf.len() > limit {
                        over = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => return http_err(400, &format!("表单解析失败：{e}")),
            }
        }
        parts.push(Part { name: fname, ctype, data: buf, over });
    }
    if parts.is_empty() {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": [{
                "type": "missing",
                "loc": ["body", "files"],
                "msg": "Field required",
                "input": null,
                "url": "https://errors.pydantic.dev/2.14/v/missing",
            }]})),
        )
            .into_response();
    }

    let mut ok: Vec<Value> = Vec::new();
    let mut bad: Vec<Value> = Vec::new();
    for p in parts {
        if p.over {
            bad.push(json!({
                "name": p.name,
                "error": format!("这个文件超过 {}MB 上限", (limit / 1048576)),
            }));
            continue;
        }
        let dir = std::env::temp_dir().join(format!("tf_{}", super::util::rand_hex(12)));
        let fname = if p.name.is_empty() { "未命名".to_string() } else { p.name.clone() };
        let tmp = dir.join(&fname);
        let r;
        if std::fs::create_dir_all(&dir).is_err() {
            r = json!({"ok": false, "error": "写不进去：临时目录建不出来"});
        } else if let Err(e) = std::fs::write(&tmp, &p.data) {
            r = json!({"ok": false, "error": format!("写不进去：{e}")});
        } else {
            r = add(
                &ctx,
                AddArgs {
                    text: String::new(),
                    path: tmp.to_string_lossy().to_string(),
                    name: fname.clone(),
                    src: "web".into(),
                    conv: String::new(),
                    mime: p.ctype,
                    at: String::new(),
                    mv: true,
                },
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        if py_truthy(r.get("ok")) {
            ok.push(r.get("item").cloned().unwrap_or(Value::Null));
        } else {
            bad.push(json!({"name": p.name, "error": r.get("error").cloned().unwrap_or(Value::Null)}));
        }
    }
    if !ok.is_empty() {
        // 网页传上来的文件逐个回推（本体，不是只发一句话）；超过 6 个补一句汇总。
        let n = ok.len();
        for it in ok.iter().take(6) {
            spawn_push(&ctx, it.clone());
        }
        if n > 6 {
            spawn_push(
                &ctx,
                json!({"kind": "text",
                       "text": format!("另外 {} 个文件已存进传输助手，去网页看", n - 6)}),
            );
        }
    }
    Json(json!({"ok": !ok.is_empty(), "items": ok, "failed": bad})).into_response()
}

pub async fn repush(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match parse_body(&raw) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mut want: Vec<String> = Vec::new();
    if let Some(v) = body.get("ids") {
        if py_truthy(Some(v)) {
            match v {
                Value::Array(a) => {
                    for x in a {
                        if py_truthy(Some(x)) {
                            want.push(py_str(x));
                        }
                    }
                }
                Value::String(s) => {
                    for c in s.chars() {
                        want.push(c.to_string());
                    }
                }
                Value::Object(m) => want.extend(m.keys().cloned()),
                _ => {}
            }
        }
    }
    if want.is_empty() {
        if let Some(ids) = push_summary(&ctx).get("ids").and_then(|v| v.as_array()) {
            want = ids.iter().filter(|x| py_truthy(Some(x))).map(py_str).collect();
        }
    }
    let rows_val = items(&ctx, 400, "", "");
    let mut rows: HashMap<String, Value> = HashMap::new();
    if let Some(arr) = rows_val.get("items").and_then(|v| v.as_array()) {
        for r in arr {
            rows.insert(r.get("id").map(py_str).unwrap_or_default(), r.clone());
        }
    }
    let mut queued = 0;
    let mut missing: Vec<String> = Vec::new();
    for i in &want {
        if let Some(row) = rows.get(i) {
            queued += 1;
            // Python 是 daemon 线程重推；这里同样放后台，接口立刻返回。
            spawn_push(&ctx, row.clone());
        } else if missing.len() < 10 {
            missing.push(i.clone());
        }
    }
    Json(json!({"ok": true, "queued": queued, "missing": missing})).into_response()
}

pub async fn del(State(ctx): CtxState, headers: HeaderMap, uri: Uri, raw: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match parse_body(&raw) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let id = body.get("id").map(|v| super::util::py_or_str(Some(v), "")).unwrap_or_default();
    Json(remove(&ctx, &id)).into_response()
}

pub async fn file(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let qmap = query_map(&uri);
    let get = |k: &str| qmap.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone());
    let Some(id) = get("id") else {
        return missing_query("id");
    };
    let parse_i = |k: &str, default: i64| -> Result<i64, Response> {
        match get(k) {
            None => Ok(default),
            Some(s) if s.trim().is_empty() => Err(bad_int_query(k, &s)),
            Some(s) => s.trim().parse::<i64>().map_err(|_| bad_int_query(k, &s)),
        }
    };
    let w = match parse_i("w", 0) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let dl = match parse_i("dl", 0) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(p) = file_of(&ctx, &id) else {
        return http_err(404, "这个条目没有文件（或已经被删掉）");
    };
    let mime = mime_of(&p);
    let base_name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let name = if !id.is_empty() && base_name.starts_with(&format!("{id}_")) {
        base_name[id.len() + 1..].to_string()
    } else {
        base_name
    };
    if dl != 0 {
        return file_response(&p, &mime, Some("private, max-age=60"), Some(&name));
    }
    if w != 0 && is_image(&p) {
        let w = w.clamp(60, 1600) as u32;
        if let Some(t) = crate::storage::make_thumb(&p, w) {
            return file_response(&t, "image/jpeg", Some("private, max-age=600"), None);
        }
    }
    file_response(&p, &mime, Some("private, max-age=120"), None)
}
