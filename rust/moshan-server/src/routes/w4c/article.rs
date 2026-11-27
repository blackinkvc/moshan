// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/article.py` + `articlebox.py` 的 5 条路由
//       （fetch / classify / list / status / topics）。
// 不做：不在本进程里抓网页 —— Python 也是把 `article.py fetch` 丢给
//       cuntu 的 venv 跑子进程（那只 venv 里有 requests），这里照抄调用方式。

#![allow(dead_code)]

use super::jobs;
use super::py;
use super::link;
use super::naming;
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{body_obj, http_err, peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const UNCATEGORIZED: &str = "未分类";
pub const TOPICS_FILE: &str = "_主题.json";
pub const MAX_PROMPT_BODY: usize = 1400;
const TIMEOUT: u64 = 180;
const MAX_TEXT: usize = 8000;
const DUP_WINDOW: f64 = 180.0;

pub fn read_root(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("08-阅读")
}

pub fn article_meta(d: &Path) -> Value {
    match jsonio::read_json(&d.join("article.json")) {
        Some(v @ Value::Object(_)) => v,
        _ => json!({}),
    }
}

pub fn load_topics(ctx: &Ctx) -> Vec<String> {
    let p = read_root(ctx).join(TOPICS_FILE);
    match jsonio::read_json(&p) {
        Some(v) => v
            .get("topics")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

pub fn save_topics(ctx: &Ctx, topics: &[String]) {
    let root = read_root(ctx);
    let _ = std::fs::create_dir_all(&root);
    let v = json!({"topics": topics, "updated_at": py::now_cst()});
    let _ = jsonio::atomic_write(&root.join(TOPICS_FILE), &jsonio::dumps(&v, 1));
}

/// `textfmt.strip_images`：去掉独占一行的 `![](…)`，再去首尾空白。
pub fn strip_images(md: &str) -> String {
    let re = regex::Regex::new(r"(?m)^!\[[^\]]*\]\([^)]*\)[ \t\r]*$").unwrap();
    re.replace_all(md, "").trim().to_string()
}

/// Python `repr(str)`：只用于错误文案里的 `{raw!r}`。
fn py_repr(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fn ms_since(t0: std::time::Instant) -> i64 {
    (t0.elapsed().as_secs_f64() * 1000.0) as i64
}

const CLASSIFY_PROMPT: &str = "下面是一篇文章的标题和正文开头。请判断它属于哪个主题，并给出 2-4 个内容标签。

现有主题：{topics}
标签参考（可选用，也可自己起）：人工智能、政治、经济、科技、文化、社会、国际、商业、历史、教育、医疗、法律、环境、军事、体育、娱乐、生活、哲学、心理、能源、汽车、游戏、旅行、美食、摄影

要求输出一行，格式严格为：主题|标签1,标签2,标签3
- 主题：从现有主题里选一个最贴切的；都不合适就新建一个 2-6 个中文字的主题名。
- 标签：2-4 个，每个 2-6 个中文字，反映文章的具体内容（如「大模型」「美联储」「俄乌冲突」），不要和主题重复。
- 只输出这一行，不要解释、不要引号。

标题：{title}
正文：{body}

主题|标签：";

/// `article.classify_dir`：4B 判主题 → 挪目录 → 写回 meta。
/// 由 `/api/article/classify` 的后台任务调用，也由 linkbox 的「就地转存」调用。
pub fn classify_dir(ctx: &Ctx, d: &Path, model: Option<&str>) -> Value {
    let t0 = std::time::Instant::now();
    if !d.is_dir() {
        return json!({"ok": false, "error": "目录不存在"});
    }
    let meta0 = article_meta(d);
    let body_path = d.join("正文.md");
    let body = if body_path.exists() {
        let raw = std::fs::read_to_string(&body_path).unwrap_or_default();
        let s = strip_images(&raw);
        s.chars().take(MAX_PROMPT_BODY).collect::<String>()
    } else {
        String::new()
    };
    if body.chars().count() < 20 {
        let needs = meta0.get("needs_ocr").map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false);
        let st = py::s_of(meta0.get("ocr_status"));
        if needs || st == "pending" || st == "failed" {
            return json!({"ok": false, "error": "正文还在图里，等 OCR 转出文字后再分类"});
        }
        return json!({"ok": false, "error": "正文太短，没法判断主题"});
    }
    let topics = load_topics(ctx);
    let topics_s = if topics.is_empty() {
        "（暂无，按文章内容新建一个）".to_string()
    } else {
        topics.join("、")
    };
    let title = py::s_of(meta0.get("title"));
    let title = if title.is_empty() {
        d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
    } else {
        title
    };
    let prompt = CLASSIFY_PROMPT
        .replace("{topics}", &topics_s)
        .replace("{title}", &title)
        .replace("{body}", &body);
    let raw = match naming::ask_4b(&prompt, model.unwrap_or(naming::DEFAULT_MODEL), 90) {
        Ok(r) => r,
        Err(e) => {
            return json!({"ok": false, "error": format!("4B 没应答：{}", py::trunc(&e, 80))})
        }
    };
    let (raw_topic, raw_tags) = match raw.split_once('|') {
        Some((a, b)) => (a, b),
        None => (raw.as_str(), ""),
    };
    let mut topic = naming::clean_name(raw_topic);
    let n = topic.chars().count();
    if n < 2 || n > 8 {
        return json!({"ok": false,
                      "error": format!("4B 输出的主题不像话：{}", py_repr(&py::trunc(&raw, 24)))});
    }
    if let Some(i1) = topic.find('：').or_else(|| topic.find(':')) {
        topic = topic[..i1].to_string();
    }
    let mut tags: Vec<String> = Vec::new();
    let split_re = regex::Regex::new(r"[,，、;；]+").unwrap();
    for part in split_re.split(raw_tags) {
        let t = naming::clean_name(part);
        let ln = t.chars().count();
        if (2..=12).contains(&ln) && t != topic && !tags.contains(&t) {
            tags.push(t);
        }
        if tags.len() >= 4 {
            break;
        }
    }

    let mut meta = meta0;
    if !meta.is_object() {
        meta = json!({});
    }
    let old_topic = py::or_str(meta.get("topic"), UNCATEGORIZED);
    meta["topic"] = json!(topic);
    if !tags.is_empty() {
        meta["tags"] = json!(tags);
    }
    if !meta.get("milestones").map(|v| v.is_object()).unwrap_or(false) {
        meta["milestones"] = json!({});
    }
    {
        let ms = meta["milestones"].as_object_mut().unwrap();
        let submitted = ms.get("submitted_at").cloned();
        let fetched = ms.get("fetched_at").cloned().unwrap_or(json!(""));
        if submitted.is_none() {
            ms.insert("submitted_at".into(), fetched);
        }
        ms.insert("classified_at".into(), json!(py::now_cst()));
    }
    let schema = py::i64_of(meta.get("schema")).max(1).max(2);
    meta["schema"] = json!(schema);

    let mut dest = d.to_path_buf();
    if topic != old_topic {
        let target_dir = read_root(ctx).join(&topic);
        let _ = std::fs::create_dir_all(&target_dir);
        let target = py::unique_dir(
            &target_dir,
            &d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        );
        if let Err(e) = std::fs::rename(d, &target) {
            return json!({"ok": false, "error": format!("移动失败：{e}")});
        }
        remap_state(ctx, &format!("{old_topic}/{}", d_name(d)), &format!("{topic}/{}", d_name(&target)));
        dest = target;
    }
    let _ = jsonio::atomic_write(&dest.join("article.json"), &jsonio::dumps(&meta, 1));

    let created = !topics.contains(&topic) && topic != UNCATEGORIZED;
    if created {
        let mut t = topics.clone();
        t.push(topic.clone());
        save_topics(ctx, &t);
    }
    let title_out = meta
        .get("title")
        .map(|v| py::s_of(Some(v)))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| d_name(&dest));
    let tags_out = meta.get("tags").cloned().unwrap_or(json!([]));
    Json(json!({
        "ok": true,
        "title": title_out,
        "topic": topic,
        "tags": tags_out,
        "created": created,
        "rel": format!("{topic}/{}", d_name(&dest)),
        "ms": ms_since(t0),
    }))
    .0
}

fn d_name(p: &Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
}

/// `_remap_state`：分类挪了目录，「已读 / 收藏」里记的 rel 跟着改。
fn remap_state(ctx: &Ctx, old_rel: &str, new_rel: &str) {
    for name in ["_已读.json", "_收藏.json"] {
        let p = read_root(ctx).join(name);
        if !p.exists() {
            continue;
        }
        let Some(mut d) = jsonio::read_json(&p) else { continue };
        let rels: Vec<String> = d
            .get("rels")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
            .unwrap_or_default();
        if !rels.iter().any(|r| r == old_rel) {
            continue;
        }
        let mut set: Vec<String> = rels
            .iter()
            .map(|r| if r == old_rel { new_rel.to_string() } else { r.clone() })
            .collect();
        set.sort();
        set.dedup();
        d["rels"] = json!(set);
        let _ = jsonio::atomic_write(&p, &jsonio::dumps(&d, 1));
    }
}

// ---------------------------------------------------------------- 后台任务

fn art_script(ctx: &Ctx) -> PathBuf {
    ctx.app_dir.join("article.py")
}

/// `ArticleBox._call`：起子进程跑 article.py，从 stdout 里抠出 JSON。
fn call(ctx: &Ctx, cmd: &[&str]) -> Value {
    let pybin = py::cuntu_py();
    let script = art_script(ctx);
    if !pybin.exists() || !script.exists() {
        return json!({"ok": false, "error": "article.py 或 cuntu 的 .venv 不存在"});
    }
    let mut argv = vec![
        pybin.to_string_lossy().to_string(),
        script.to_string_lossy().to_string(),
    ];
    argv.extend(cmd.iter().map(|s| s.to_string()));
    let Some((_code, out, err)) = py::run_cmd(&argv, TIMEOUT, Some(&ctx.app_dir)) else {
        return json!({"ok": false, "error": format!("超时（>{TIMEOUT} 秒）")});
    };
    let s = out.trim().to_string();
    let (Some(i), Some(j)) = (s.find('{'), s.rfind('}')) else {
        return json!({"ok": false, "error": if err.trim().is_empty() {
            "无输出".to_string()
        } else {
            py::trunc(err.trim().split('\n').last().unwrap_or(""), 200)
        }});
    };
    if j <= i {
        return json!({"ok": false, "error": if err.trim().is_empty() {
            "无输出".to_string()
        } else {
            py::trunc(err.trim().split('\n').last().unwrap_or(""), 200)
        }});
    }
    match serde_json::from_str::<Value>(&s[i..=j]) {
        Ok(v) => v,
        Err(_) => json!({"ok": false, "error": "输出不是 JSON"}),
    }
}

/// `ArticleBox._run` 的后台线程体。
fn run_job(ctx: Arc<Ctx>, jid: String, text: String, hint: String) {
    let store = jobs::articles();
    // `with self._sem:` 在 `_do` 之前：拿不到槽位就停在 queued
    let _slot = store.sem().acquire();
    store.set(&jid, &[("state", json!("running"))], jobs::ARTICLE_TERMINAL);
    let r = call(&ctx, &["fetch", &text, &hint]);
    let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let kind = py::s_of(r.get("kind"));
    if !ok && kind == "ask" {
        let job = store.get(&jid).unwrap_or_default();
        let e = jobs::pend_add(
            &ctx,
            "article",
            &jid,
            &py::s_of(job.get("url")),
            &py::s_of(r.get("title")),
            &py::s_of(r.get("platform")),
            "",
            py::i64_of(r.get("chars")),
            py::i64_of(r.get("images")),
            0,
            "",
            &py::s_of(r.get("why")),
        );
        let qtext = jobs::pend_question(&e);
        store.set(
            &jid,
            &[
                ("state", json!("ask")),
                ("pid", e.get("id").cloned().unwrap_or(json!(""))),
                ("ambiguous", json!(true)),
                ("title", r.get("title").cloned().unwrap_or(json!(""))),
                ("platform", r.get("platform").cloned().unwrap_or(json!(""))),
                ("chars", r.get("chars").cloned().unwrap_or(json!(0))),
                ("images", json!(py::i64_of(r.get("images")))),
                ("error", json!(qtext)),
            ],
            jobs::ARTICLE_TERMINAL,
        );
        return;
    }
    if !ok && kind == "gallery" {
        let job = store.get(&jid).unwrap_or_default();
        let url = py::s_of(job.get("url"));
        if !url.is_empty() {
            let _ = link::start(&ctx, &url, "存图", false);
            store.set(
                &jid,
                &[
                    ("state", json!("routed")),
                    ("kind", json!("gallery")),
                    ("routed", json!("gallery")),
                    ("images", json!(py::i64_of(r.get("images")))),
                    ("chars", json!(py::i64_of(r.get("chars")))),
                    (
                        "error",
                        json!(format!("这是图集（{} 字），已改存图", py::num_str(
                            r.get("chars").unwrap_or(&json!(0))
                        ))),
                    ),
                ],
                jobs::ARTICLE_TERMINAL,
            );
            return;
        }
        store.set(&jid, &[("kind", json!("gallery"))], jobs::ARTICLE_TERMINAL);
    }
    if ok {
        store.set(
            &jid,
            &[
                ("state", json!("done")),
                ("title", r.get("title").cloned().unwrap_or(json!(""))),
                ("platform", r.get("platform").cloned().unwrap_or(json!(""))),
                ("topic", r.get("topic").cloned().unwrap_or(json!(""))),
                ("chars", r.get("chars").cloned().unwrap_or(json!(0))),
                ("rel", r.get("rel").cloned().unwrap_or(json!(""))),
                ("images", json!(py::i64_of(r.get("images")))),
            ],
            jobs::ARTICLE_TERMINAL,
        );
    } else {
        let err = py::or_str(r.get("error"), "抓取失败");
        store.set(
            &jid,
            &[
                ("state", json!("error")),
                ("platform", r.get("platform").cloned().unwrap_or(json!(""))),
                ("kind", r.get("kind").cloned().unwrap_or(json!(""))),
                ("error", json!(py::trunc(&err, 300))),
            ],
            jobs::ARTICLE_TERMINAL,
        );
    }
}

fn run_classify(ctx: Arc<Ctx>, jid: String, rel: String) {
    let store = jobs::articles();
    // `with self._sem:` 在 `_do` 之前：拿不到槽位就停在 queued
    let _slot = store.sem().acquire();
    store.set(&jid, &[("state", json!("running"))], jobs::ARTICLE_TERMINAL);
    if !rel.is_empty() {
        let target = ctx.library_dir.join("08-阅读").join(&rel);
        let r = call(&ctx, &["classify", &target.to_string_lossy()]);
        if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            store.set(
                &jid,
                &[
                    ("state", json!("done")),
                    ("title", r.get("title").cloned().unwrap_or(json!(""))),
                    ("topic", r.get("topic").cloned().unwrap_or(json!(""))),
                    ("rel", r.get("rel").cloned().unwrap_or(json!(""))),
                ],
                jobs::ARTICLE_TERMINAL,
            );
        } else {
            let err = py::or_str(r.get("error"), "分类失败");
            store.set(
                &jid,
                &[("state", json!("error")), ("error", json!(py::trunc(&err, 300)))],
                jobs::ARTICLE_TERMINAL,
            );
        }
    } else {
        let r = call(&ctx, &["classify", "--all"]);
        if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let total = py::i64_of(r.get("total"));
            let done = r.get("done").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            store.set(
                &jid,
                &[
                    ("state", json!("done")),
                    ("text", json!(format!("分了 {total} 篇，成功 {done}"))),
                    ("topic", json!("批量")),
                    ("rel", json!("")),
                ],
                jobs::ARTICLE_TERMINAL,
            );
        } else {
            let err = py::or_str(r.get("error"), "批量分类失败");
            store.set(
                &jid,
                &[("state", json!("error")), ("error", json!(py::trunc(&err, 300)))],
                jobs::ARTICLE_TERMINAL,
            );
        }
    }
}

/// `ArticleBox.start`：同一链接短时间内重复进来就复用上次结果。
pub fn start(ctx: &Arc<Ctx>, text: &str, hint: &str) -> Value {
    let store = jobs::articles();
    let text: String = text.trim().chars().take(MAX_TEXT).collect();
    if text.is_empty() {
        return json!({"error": "缺少链接"});
    }
    let key = py::url_key(&text);
    if !key.is_empty() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        for old in store.snapshot_all() {
            if py::s_of(old.get("url_key")) != key {
                continue;
            }
            if py::s_of(old.get("kind")) == "classify" {
                continue;
            }
            let st = py::s_of(old.get("state"));
            let finished = py::f64_of(old.get("finished"));
            if ["queued", "running", "ask"].contains(&st.as_str())
                || now - finished < DUP_WINDOW
            {
                let mut snap = old.clone();
                snap.insert("dup".into(), json!(true));
                return Value::Object(snap);
            }
        }
    }
    let jid = py::rand_hex(12);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let hint_v = if hint.is_empty() { text.clone() } else { hint.to_string() };
    let mut job = Map::new();
    job.insert("id".into(), json!(jid));
    job.insert("state".into(), json!("queued"));
    job.insert("text".into(), json!(py::trunc(&text, 120)));
    job.insert("title".into(), json!(""));
    job.insert("platform".into(), json!(""));
    job.insert("topic".into(), json!(""));
    job.insert("chars".into(), json!(0));
    job.insert("images".into(), json!(0));
    job.insert("rel".into(), json!(""));
    job.insert("error".into(), json!(""));
    job.insert("started".into(), json!(started));
    job.insert("routed".into(), json!(""));
    job.insert("hint".into(), json!(py::trunc(&hint_v, 300)));
    job.insert("url".into(), json!(key));
    job.insert("url_key".into(), json!(key));
    job.insert("finished".into(), json!(0.0));
    store.insert(&jid, job);

    let ctx2 = ctx.clone();
    let jid2 = jid.clone();
    let text2 = text.clone();
    let hint2 = hint.to_string();
    std::thread::spawn(move || run_job(ctx2, jid2, text2, hint2));
    store.settle(&jid);
    store.get(&jid).map(Value::Object).unwrap_or_else(|| json!({"error": "任务创建失败"}))
}

pub fn start_classify(ctx: &Arc<Ctx>, rel: &str) -> Value {
    let store = jobs::articles();
    let jid = py::rand_hex(12);
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut job = Map::new();
    job.insert("id".into(), json!(jid));
    job.insert("state".into(), json!("queued"));
    job.insert("kind".into(), json!("classify"));
    job.insert(
        "text".into(),
        json!(if rel.is_empty() { "未分类全部".to_string() } else { rel.to_string() }),
    );
    job.insert("title".into(), json!(""));
    job.insert("platform".into(), json!(""));
    job.insert("topic".into(), json!(""));
    job.insert("chars".into(), json!(0));
    job.insert("rel".into(), json!(rel));
    job.insert("error".into(), json!(""));
    job.insert("started".into(), json!(started));
    store.insert(&jid, job);
    let ctx2 = ctx.clone();
    let jid2 = jid.clone();
    let rel2 = rel.to_string();
    std::thread::spawn(move || run_classify(ctx2, jid2, rel2));
    store.settle(&jid);
    store.get(&jid).map(Value::Object).unwrap_or_else(|| json!({"error": "任务创建失败"}))
}

// ---------------------------------------------------------------- 路由

fn missing_query(field: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "missing",
            "loc": ["query", field],
            "msg": "Field required",
            "input": null,
            "url": "https://errors.pydantic.dev/2.14/v/missing",
        }]})),
    )
        .into_response()
}

pub async fn fetch(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body_obj(body);
    let text = py::s_of(body.get("text")).trim().to_string();
    if text.is_empty() {
        return http_err(400, "缺少链接");
    }
    let hint = py::trunc(&py::s_of(body.get("hint")), 300);
    let urls = py::split_links(&text);
    if urls.len() > 1 {
        let mut out: Vec<Value> = Vec::new();
        for u in urls {
            let hint2 = if hint.is_empty() { text.clone() } else { hint.clone() };
            let j = start(&ctx, &u, &hint2);
            // Python 是 `if not j.get("error")`：空串也算没有错
            if py::s_of(j.get("error")).is_empty()
                && !j.get("dup").and_then(|v| v.as_bool()).unwrap_or(false)
            {
                crate::storage::log(
                    &ctx,
                    json!({"event": "article_start", "id": j.get("id"),
                           "head": py::trunc(&u, 80), "url": u}),
                );
            }
            out.push(j);
        }
        return Json(json!({"multi": out.len(), "jobs": out})).into_response();
    }
    let job = start(&ctx, &text, &hint);
    let err = py::s_of(job.get("error"));
    if !err.is_empty() {
        return http_err(400, &err);
    }
    if job.get("dup").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Json(job).into_response();
    }
    crate::storage::log(
        &ctx,
        json!({"event": "article_start", "id": job.get("id"),
               "head": py::trunc(&text, 80), "url": job.get("url")}),
    );
    Json(job).into_response()
}

pub async fn classify(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body_obj(body);
    let rel = py::s_of(body.get("rel"));
    if !rel.is_empty() && (rel.contains("..") || rel.starts_with('/')) {
        return http_err(400, "路径不合法");
    }
    let job = start_classify(&ctx, &rel);
    let err = py::s_of(job.get("error"));
    if !err.is_empty() {
        return http_err(400, &err);
    }
    crate::storage::log(
        &ctx,
        json!({"event": "article_classify_start", "id": job.get("id"), "rel": rel}),
    );
    Json(job).into_response()
}

pub async fn list(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let root = read_root(&ctx);
    let mut groups = Map::new();
    if !root.exists() {
        return Json(json!({"groups": {}})).into_response();
    }
    let Ok(rd) = std::fs::read_dir(&root) else {
        return Json(json!({"groups": {}})).into_response();
    };
    let mut topics: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    // Python `sorted(root.iterdir())`：PosixPath 比的是 casefold 后的整串路径。
    topics.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    for topic_dir in topics {
        let name = d_name(&topic_dir);
        if !topic_dir.is_dir() || name.starts_with('_') {
            continue;
        }
        let Ok(rd2) = std::fs::read_dir(&topic_dir) else { continue };
        let mut entries: Vec<(f64, PathBuf)> = rd2
            .flatten()
            .map(|e| {
                let p = e.path();
                let mt = std::fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                (mt, p)
            })
            .collect();
        // `sorted(..., key=-mtime)`：稳定排序，递减
        entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut arts: Vec<Value> = Vec::new();
        for (_, d) in entries {
            if !d.is_dir() {
                continue;
            }
            let meta = article_meta(&d);
            let dname = d_name(&d);
            let title = meta.get("title").cloned().unwrap_or(json!(dname));
            let author = meta.get("author").cloned().unwrap_or(json!(""));
            let platform = meta.get("platform").cloned().unwrap_or(json!(""));
            let chars = meta.get("chars").cloned().unwrap_or(json!(0));
            let url = meta.get("url").cloned().unwrap_or(json!(""));
            let saved_at = meta.get("saved_at").cloned().unwrap_or(json!(""));
            let has_body = d.join("正文.md").exists();
            arts.push(json!({
                "rel": format!("{name}/{dname}"),
                "title": title,
                "author": author,
                "platform": platform,
                "chars": chars,
                "url": url,
                "saved_at": saved_at,
                "has_body": has_body,
            }));
        }
        if !arts.is_empty() {
            groups.insert(name, json!(arts));
        }
    }
    Json(json!({"groups": groups})).into_response()
}

pub async fn status(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let map = crate::routes::common::query_map(&uri);
    let Some((_, id)) = map.into_iter().find(|(k, _)| k == "id") else {
        return missing_query("id");
    };
    match jobs::articles().get(&id) {
        Some(j) => Json(Value::Object(j)).into_response(),
        None => http_err(404, "任务不存在或已过期"),
    }
}

pub async fn topics(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let root = read_root(&ctx);
    let topics = load_topics(&ctx);
    let mut counts = Map::new();
    let mut all: Vec<String> = vec![UNCATEGORIZED.to_string()];
    all.extend(topics.iter().cloned());
    for t in all {
        let d = root.join(&t);
        let n = if d.exists() {
            std::fs::read_dir(&d)
                .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).count())
                .unwrap_or(0)
        } else {
            0
        };
        counts.insert(t, json!(n));
    }
    Json(json!({"topics": topics, "counts": counts})).into_response()
}
