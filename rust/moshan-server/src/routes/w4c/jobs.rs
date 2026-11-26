// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：异步任务队列（articlebox / linkbox）+ 内容判定（judge）+ 挂起单（pending）。
// 不做：不起 HTTP 客户端，不碰路由。
//
// 这三个模块在 Python 里互相引用（articlebox→linkbox→judge→pending），
// 合成一个文件反而让「任务字典的键顺序」只有一处定义 —— 前端/轮询端点读的
// 就是这些字典，键的顺序变了，`/api/article/status` 的字节就变了。

#![allow(dead_code)]

use super::py;
use crate::jsonio;
use crate::paths::Ctx;
use serde_json::{json, Map, Value};
use std::sync::{Condvar, Mutex, OnceLock};

pub type Job = Map<String, Value>;

/// Python 的 `threading.Semaphore(MAX_CONCURRENT)`：同时最多两篇。
/// 第 3 个任务进来时 worker 卡在 `with self._sem:` 之前，任务就停在 queued ——
/// 这个状态是**能看见的**（`/api/link` 的响应直接带出去），所以必须建模，
/// 不能只当成内部限流。
pub struct Sem {
    slots: Mutex<usize>,
    cv: Condvar,
}

pub struct SemGuard<'a>(&'a Sem);

impl Sem {
    fn new(n: usize) -> Self {
        Self { slots: Mutex::new(n), cv: Condvar::new() }
    }

    pub fn acquire(&self) -> SemGuard<'_> {
        let mut g = self.slots.lock().unwrap();
        while *g == 0 {
            g = self.cv.wait(g).unwrap();
        }
        *g -= 1;
        SemGuard(self)
    }
}

impl Drop for SemGuard<'_> {
    fn drop(&mut self) {
        *self.0.slots.lock().unwrap() += 1;
        self.0.cv.notify_one();
    }
}

// ---------------------------------------------------------------- 任务表

/// `ArticleBox._jobs` / `LinkBox._jobs` 的共同部分。
/// Python 用 dict（保插入序），trim 时按 started 排序丢最早的。
pub struct JobStore {
    jobs: Mutex<Vec<(String, Job)>>,
    keep: usize,
    sem: Sem,
}

impl JobStore {
    pub fn new(keep: usize) -> Self {
        Self { jobs: Mutex::new(Vec::new()), keep, sem: Sem::new(2) }
    }

    pub fn sem(&self) -> &Sem {
        &self.sem
    }

    pub fn insert(&self, id: &str, job: Job) {
        let mut g = self.jobs.lock().unwrap();
        g.push((id.to_string(), job));
        if g.len() > self.keep {
            let mut order: Vec<(i64, usize)> = g
                .iter()
                .enumerate()
                .map(|(i, (_, j))| (py::i64_of(j.get("started")), i))
                .collect();
            order.sort_by_key(|x| x.0);
            let drop_n = g.len() - self.keep;
            let mut drop_idx: Vec<usize> = order.iter().take(drop_n).map(|x| x.1).collect();
            drop_idx.sort_unstable();
            for i in drop_idx.into_iter().rev() {
                g.remove(i);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Job> {
        let g = self.jobs.lock().unwrap();
        g.iter().find(|(k, _)| k == id).map(|(_, j)| j.clone())
    }

    /// `Thread.start()` 之后立刻 `get(jid)` 的那个瞬间：worker 的第一句就是
    /// `_set(running)`，所以外面看到的基本都是 running 而不是 queued。
    /// 这里照同样的顺序：spawn 完先等它离开 queued（最多 50ms）再取快照。
    /// 只影响「刚受理」那一次响应，后续轮询本来就是实时状态。
    pub fn settle(&self, id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(50);
        loop {
            let st = self.get(id).map(|j| py::s_of(j.get("state"))).unwrap_or_default();
            if st != "queued" || std::time::Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Python `_set(jid, **kw)`：存在的键原位覆盖，新键追加。
    /// 终态时补 `finished`（判重要靠它算「多久之内算重复」）。
    pub fn set(&self, id: &str, kv: &[(&str, Value)], terminals: &[&str]) {
        let mut g = self.jobs.lock().unwrap();
        let Some((_, job)) = g.iter_mut().find(|(k, _)| k == id) else { return };
        let mut hit_terminal = false;
        for (k, v) in kv {
            job.insert((*k).to_string(), v.clone());
            if *k == "state" {
                if let Some(s) = v.as_str() {
                    if terminals.contains(&s) {
                        hit_terminal = true;
                    }
                }
            }
        }
        if hit_terminal {
            job.insert("finished".to_string(), json!(now_f()));
        }
    }

    pub fn snapshot_all(&self) -> Vec<Job> {
        self.jobs.lock().unwrap().iter().map(|(_, j)| j.clone()).collect()
    }
}

fn now_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn articles() -> &'static JobStore {
    static S: OnceLock<JobStore> = OnceLock::new();
    S.get_or_init(|| JobStore::new(40))
}

pub fn links() -> &'static JobStore {
    static S: OnceLock<JobStore> = OnceLock::new();
    S.get_or_init(|| JobStore::new(60))
}

pub const ARTICLE_TERMINAL: &[&str] = &["done", "error", "routed"];
pub const LINK_TERMINAL: &[&str] = &["done", "error", "unsupported"];

// ---------------------------------------------------------------- judge

const ARTICLE_KEYWORDS: &[&str] = &["文章", "长文", "正文"];
const GALLERY_KEYWORDS: &[&str] = &["存图", "图集", "存图片"];
const VISUAL_KEYWORDS: &[&str] = &["写真", "人像"];
const MIN_TEXT: i64 = 30;
const FEW_IMAGES: i64 = 3;
const FEW_RATIO: f64 = 0.6;
const ASK_TEXT_LO: f64 = 0.5;
const ASK_TEXT_HI: f64 = 3.5;
const ASK_MIN_IMG: i64 = 4;
const ASK_FEW_LO: f64 = 0.6;
const ASK_FEW_HI: f64 = 1.0;

pub fn keyword_of(text: &str) -> Option<&'static str> {
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

pub fn limit_for(platform: &str) -> i64 {
    if platform.contains("公众号") {
        300
    } else if platform.contains("微博") || platform.contains("小红书") {
        500
    } else {
        500
    }
}

fn ambiguous(text_len: i64, n_img: i64, platform: &str, layout: &str) -> bool {
    if layout == "article" {
        return false;
    }
    let limit = limit_for(platform) as f64;
    let r = (text_len.max(0) as f64) / limit;
    if n_img >= ASK_MIN_IMG {
        return ASK_TEXT_LO <= r && r <= ASK_TEXT_HI;
    }
    if n_img >= 1 {
        return ASK_FEW_LO <= r && r <= ASK_FEW_HI;
    }
    false
}

pub fn question(text_len: i64, n_img: i64, _platform: &str, title: &str) -> String {
    let name = if title.trim().is_empty() {
        "这条".to_string()
    } else {
        format!("《{}》", title.trim())
    };
    format!("{name}{n_img} 张图、{text_len} 字，看着两边都像。回「存图」还是「存文章」？")
}

/// `judge.decide`：返回 (mode, why, ask)。与 Python 的判定链逐条对应 —— 顺序
/// 本身就是契约（视频、写真都必须在「问一句」之前）。
pub struct Verdict {
    pub mode: &'static str,
    pub why: String,
    pub ask: bool,
}

pub fn decide(
    text_len: i64,
    n_img: i64,
    platform: &str,
    hint: &str,
    layout: &str,
    title: &str,
    n_video: i64,
) -> Verdict {
    let text_len = text_len.max(0);
    let n_img = n_img.max(0);
    let n_video = n_video.max(0);
    match keyword_of(hint) {
        Some("article") => {
            return Verdict { mode: "article", why: "你说了「文章」".into(), ask: false }
        }
        Some("gallery") => {
            return Verdict { mode: "gallery", why: "你说了「存图」".into(), ask: false }
        }
        _ => {}
    }
    if n_video >= 1 {
        return Verdict {
            mode: "gallery",
            why: format!("视频作品（{n_video} 段），不按文章转存"),
            ask: false,
        };
    }
    let who = format!("{title} {hint}");
    if VISUAL_KEYWORDS.iter().any(|k| who.contains(k)) && n_img >= 1 {
        return Verdict {
            mode: "gallery",
            why: "写真：图才是内容，配文不算数".into(),
            ask: false,
        };
    }
    if ambiguous(text_len, n_img, platform, layout) {
        let limit = limit_for(platform);
        let lean = if text_len >= limit { "article" } else { "gallery" };
        return Verdict {
            mode: lean,
            why: format!("{n_img} 张图、{text_len} 字，两边都像"),
            ask: true,
        };
    }
    if layout == "picture" {
        let limit = limit_for(platform);
        if text_len >= limit && n_img < 9 {
            return Verdict {
                mode: "article",
                why: format!("正文 {text_len} 字、{n_img} 张图，是文章配图"),
                ask: false,
            };
        }
        return Verdict {
            mode: "gallery",
            why: format!("图片消息（{n_img} 张图、配文 {text_len} 字），图才是内容"),
            ask: false,
        };
    }
    if layout == "article" {
        return Verdict {
            mode: "article",
            why: format!("正文文章（{text_len} 字）"),
            ask: false,
        };
    }
    let limit = limit_for(platform);
    if text_len >= limit {
        return Verdict {
            mode: "article",
            why: format!("正文 {text_len} 字，过 {limit} 字线"),
            ask: false,
        };
    }
    if n_img == 0 && text_len >= MIN_TEXT {
        return Verdict {
            mode: "article",
            why: format!("没有图、正文 {text_len} 字"),
            ask: false,
        };
    }
    if n_img <= FEW_IMAGES && (text_len as f64) >= (limit as f64) * FEW_RATIO {
        return Verdict {
            mode: "article",
            why: format!("正文 {text_len} 字（够 {}）、图才 {n_img} 张", (limit as f64 * FEW_RATIO) as i64),
            ask: false,
        };
    }
    Verdict { mode: "gallery", why: format!("正文 {text_len} 字 / {n_img} 张图"), ask: false }
}

// ---------------------------------------------------------------- pending

/// `04-存图/_待定.json`：图集/文章两可时挂起的那一条。
///
/// 迁移说明：这里只做「挂起 + 生成问句 + 落盘」，**不做**微信投递 ——
/// 投递属于 pending 那条泳道（`/api/pending*` 与 wechat_push）。后台抓取
/// 走到 ask 分支时，落盘的记录与 Python 一致，只是不会真的发一条微信。
fn pend_path(ctx: &Ctx) -> std::path::PathBuf {
    ctx.library_dir.join("04-存图").join("_待定.json")
}

fn pend_load(ctx: &Ctx) -> Vec<Value> {
    let raw = jsonio::read_json(&pend_path(ctx)).unwrap_or(Value::Null);
    let items = match &raw {
        Value::Object(o) => o.get("items").cloned(),
        Value::Array(_) => Some(raw.clone()),
        _ => None,
    };
    items
        .and_then(|v| v.as_array().cloned())
        .map(|a| a.into_iter().filter(|x| x.is_object()).collect())
        .unwrap_or_default()
}

fn pend_save(ctx: &Ctx, items: &[Value]) {
    let p = pend_path(ctx);
    let _ = jsonio::atomic_write(&p, &jsonio::dumps(&json!({"v": 1, "items": items}), 1));
}

fn pend_prune(items: Vec<Value>) -> Vec<Value> {
    let now = now_f();
    let mut keep: Vec<Value> = Vec::new();
    let mut done = 0;
    for mut i in items.into_iter().rev() {
        let is_open = i.get("state").and_then(|v| v.as_str()) == Some("open");
        if !is_open {
            done += 1;
            if done > 40 {
                continue;
            }
        } else if now - py::f64_of(i.get("asked_at")) > 7.0 * 86400.0 {
            if let Some(o) = i.as_object_mut() {
                o.insert("state".into(), json!("expired"));
            }
        }
        keep.push(i);
    }
    keep.reverse();
    keep
}

pub fn pend_add(
    ctx: &Ctx,
    kind: &str,
    job: &str,
    url: &str,
    title: &str,
    platform: &str,
    dir_name: &str,
    text_len: i64,
    n_img: i64,
    n_video: i64,
    suggest: &str,
    why: &str,
) -> Value {
    let now = now_f();
    let e = json!({
        "id": format!("p{}", py::rand_hex(8)),
        "kind": kind,
        "job": job,
        "url": url,
        "title": title,
        "platform": platform,
        "dir_name": dir_name,
        "text_len": text_len,
        "n_img": n_img,
        "n_video": n_video,
        "suggest": suggest,
        "why": why,
        "agent": "main",
        "channel": "openclaw-weixin",
        "conv": "",
        "state": "open",
        "asked_at": now,
        "answered_at": 0.0,
        "pushed_at": 0.0,
        "push_tries": 0,
        "next_try": 0.0,
    });
    let mut items = pend_prune(pend_load(ctx));
    items.push(e.clone());
    pend_save(ctx, &items);
    e
}

fn one_line(e: &Value) -> String {
    let title = py::s_of(e.get("title"));
    let title = title.trim();
    let name = if title.is_empty() {
        "这条链接".to_string()
    } else {
        format!("《{title}》")
    };
    let n = py::i64_of(e.get("n_img"));
    let c = py::i64_of(e.get("text_len"));
    let v = py::i64_of(e.get("n_video"));
    let what = if v != 0 && n <= v {
        format!("{v} 段视频")
    } else if v != 0 {
        format!("{} 张图 + {v} 段视频", n - v)
    } else {
        format!("{n} 张图")
    };
    format!("{name}：{what}、{c} 字")
}

pub fn pend_question(e: &Value) -> String {
    format!(
        "{}，两边都像 —— 回1「存图」；回2「存文章」；回3「算了」就当没存；",
        one_line(e)
    )
}
