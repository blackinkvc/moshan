// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：工作台「在做的事」两张卡 —— 左卡（中台各插件的活）与右卡（本机的活）。
//       对应 dash.py 的 _auto_worklog / plugin_manifest / _pg_item / _device_items /
//       _game_items / _media_items / _app_items / _plugin_items / _collapse /
//       _pocket_pulse / _manual_host_items / _worklog / read_worklog / append_worklog，
//       以及 /api/worklog 的 GET 与 POST 两条路由。
// 不做：不碰 nest（虫巢，账不并进来）、不写 pocket.jsonl、不跑任何插件。
//
// 左卡的口径（用户 2026-11-21 定的）：口袋 + 设备 + 游戏 + 影音 + App 监工，各插件
// 自己的日志翻成同一种 item，多带 plugin 字段，页面按插件分栏。右卡只回答
// 「这台电脑在忙什么」：各 AI 客户端每轮一条 + pocket 折一天一条 + 补记的几条。
#![allow(unused_imports, dead_code)]

use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use super::cards;
use super::platform;
use super::sessions;
use super::util::{
    human, iso_of, load_obj, now_hms, now_ymd_hms, obj, py_str, sval, tail_by_event, tail_jsonl,
    truthy,
};
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{body_obj, http_err, peek, CtxState};

const WL_LIMIT: usize = 90;
const WL_KEEP: usize = 300;
const WL_RUN_WINDOW: i64 = 120;
const PG_LIMIT: usize = 300;
const PG_RUN_STALE: i64 = 3600;
const _WL_HOST_DAYS: i64 = 2;

// ------------------------------------------------------------------ 小工具

fn code(name: &str) -> Value {
    json!({"kind": "code", "name": name})
}
fn ai(name: &str) -> Value {
    json!({"kind": "ai", "name": name})
}
fn tool(name: &str) -> Value {
    json!({"kind": "tool", "name": name})
}

/// `_state_of`：抓取器自报的状态 → ok / skip / run / fail。
fn state_of(v: Option<&Value>) -> &'static str {
    let s = super::util::str_or(v).trim().to_lowercase();
    match s.as_str() {
        "done" | "ok" | "success" | "fine" => "ok",
        "gallery" | "dup" | "read" | "skip" | "skipped" | "unsupported" => "skip",
        "run" | "running" | "start" | "started" | "pending" => "run",
        _ => "fail",
    }
}

/// `_local_model`：config.model 取冒号前、去掉 -gguf 后缀。
fn local_model(ctx: &Ctx) -> String {
    let raw = super::util::str_or(ctx.cfg.get("model"));
    let raw = raw.split(':').next().unwrap_or("").trim().to_string();
    let re = regex::Regex::new(r"(?i)[-_](gguf|ggml)$").unwrap();
    let out = re.replace(&raw, "").to_string();
    if out.is_empty() {
        "本机模型".to_string()
    } else {
        out
    }
}

fn app_base(app_id: &str) -> String {
    for a in cards::BUILTIN_APPS {
        if a.id == app_id {
            return a.url.to_string();
        }
    }
    String::new()
}

/// `_go`：给一条记录配跳转目标。
fn go(app_id: &str, path: &str) -> Value {
    if path.starts_with('#') {
        return json!({"url": path, "same": true});
    }
    if app_id == "inspect" {
        return json!({"url": "/dash?v=ops#card=inspLog", "app": app_id});
    }
    let base = app_base(app_id);
    if base.is_empty() {
        return json!({});
    }
    let url = format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    json!({"url": url, "app": app_id})
}

/// `_ext`：存失败的那条，目标就是用户粘进来的链接。
fn ext(url: &Value) -> Value {
    let u = super::util::str_or(Some(url)).trim().to_string();
    if u.starts_with("http") {
        json!({"url": u, "ext": true})
    } else {
        json!({})
    }
}

/// `urllib.parse.quote(s)`：保留 A-Za-z0-9 `_.-~/`，其余按 UTF-8 编码成 %XX。
fn py_quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || "_.-~/".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn parse_ts(s: &str) -> f64 {
    chrono::NaiveDateTime::parse_from_str(s.get(..19).unwrap_or(""), "%Y-%m-%dT%H:%M:%S")
        .map(|d| d.and_local_timezone(chrono::Local).single().map(|t| t.timestamp() as f64).unwrap_or(0.0))
        .unwrap_or(0.0)
}

// ------------------------------------------------------------------ 口袋自己的活

struct Pusher {
    items: Vec<Value>,
}

impl Pusher {
    fn push(
        &mut self,
        task: &str,
        status: &str,
        start: &str,
        end: &str,
        detail: &str,
        sources: Vec<Value>,
        rid: &str,
        link: Option<Value>,
    ) {
        let id = if rid.is_empty() {
            format!("a{}-{}", self.items.len(), start)
        } else {
            rid.to_string()
        };
        let mut m = Map::new();
        m.insert("id".into(), sval(&id));
        m.insert("task".into(), sval(task));
        m.insert("detail".into(), sval(&detail.chars().take(160).collect::<String>()));
        m.insert("status".into(), sval(status));
        m.insert("start".into(), sval(start));
        m.insert("end".into(), sval(end));
        m.insert(
            "secs".into(),
            if !start.is_empty() && !end.is_empty() {
                match sessions::secs(start, end) {
                    Some(v) => json!(v),
                    None => Value::Null,
                }
            } else {
                Value::Null
            },
        );
        m.insert("sources".into(), Value::Array(sources));
        m.insert("plugin".into(), sval("pocket"));
        if let Some(l) = link {
            if l.get("url").map(truthy).unwrap_or(false) {
                m.insert("link".into(), l);
            }
        }
        self.items.push(Value::Object(m));
    }
}

/// `_auto_worklog`：口袋的账（pocket.jsonl 现算）。
pub fn auto_worklog(ctx: &Ctx) -> Vec<Value> {
    // paths.base_dir()：源码运行 = moshan/（账本一直跟着代码走），打包后 = 状态目录。
    let data_dir = if ctx.frozen { ctx.state_dir.clone() } else { ctx.app_dir.clone() };
    let log = data_dir.join("pocket.jsonl");
    let rows = tail_jsonl(&log, 1200);
    let model = local_model(ctx);
    let now_s = now_ymd_hms();
    let mut p = Pusher { items: Vec::new() };
    let mut open_start: Vec<(String, String)> = Vec::new();

    // —— 传输助手 → 手机：同秒同 kind 的一批 ——
    struct Batch {
        key: (String, String),
        total: i64,
        ok: i64,
        attempt: i64,
        names: Vec<String>,
        error: String,
    }
    let mut tf_batch: Vec<Batch> = Vec::new();
    for d in tail_by_event(&log, &["tf_push"], 2) {
        let key = (
            super::util::str_or(d.get("ts")),
            super::util::str_or(d.get("kind")),
        );
        let idx = match tf_batch.iter().position(|b| b.key == key) {
            Some(i) => i,
            None => {
                tf_batch.push(Batch { key: key.clone(), total: 0, ok: 0, attempt: 0, names: Vec::new(), error: String::new() });
                tf_batch.len() - 1
            }
        };
        let b = &mut tf_batch[idx];
        b.total += 1;
        if d.get("ok").map(truthy).unwrap_or(false) {
            b.ok += 1;
        } else if b.error.is_empty() {
            b.error = super::util::str_or(d.get("error"));
        }
        b.attempt = b.attempt.max(super::util::int_of(d.get("attempt")));
        let nm = super::util::str_or(d.get("name"));
        if !nm.is_empty() && !b.names.iter().any(|x| *x == nm) {
            b.names.push(nm);
        }
    }
    // 同一组文件名 30 分钟内反复出现 = 同一次投递的多次尝试，只留最后一次
    let mut alias: Vec<(String, String)> = Vec::new();
    let mut keep: Vec<((String, String), Vec<String>)> = Vec::new();
    let mut sorted_idx: Vec<usize> = (0..tf_batch.len()).collect();
    sorted_idx.sort_by(|a, b| tf_batch[*a].key.cmp(&tf_batch[*b].key));
    let mut removed: Vec<bool> = vec![false; tf_batch.len()];
    for i in sorted_idx {
        let mut sig_names = tf_batch[i].names.clone();
        sig_names.sort();
        let sig: Vec<String> = if sig_names.is_empty() {
            vec![tf_batch[i].key.0.clone()]
        } else {
            sig_names
        };
        let kind = tf_batch[i].key.1.clone();
        let prev = keep
            .iter()
            .position(|((_, k), s)| *k == kind && *s == sig);
        if let Some(pi) = prev {
            let prev_key = keep[pi].0.clone();
            if parse_ts(&tf_batch[i].key.0) - parse_ts(&prev_key.0) <= 1800.0 {
                let prev_attempt = tf_batch
                    .iter()
                    .find(|b| b.key == prev_key)
                    .map(|b| b.attempt)
                    .unwrap_or(0);
                tf_batch[i].attempt = prev_attempt.max(tf_batch[i].attempt.max(1));
                alias.push((prev_key.0.clone(), tf_batch[i].key.0.clone()));
                if let Some(pos) = tf_batch.iter().position(|b| b.key == prev_key) {
                    removed[pos] = true;
                }
                keep[pi] = (tf_batch[i].key.clone(), sig);
            }
        } else {
            keep.push((tf_batch[i].key.clone(), sig));
        }
    }
    let is_alias = |k: &(String, String)| alias.iter().any(|(old, _)| *old == k.0);

    let mut tf_seen: Vec<(String, String)> = Vec::new();
    let mut tf_push_item = |p: &mut Pusher, key: &(String, String), b: &Batch| {
        let at = iso_of(Some(&Value::String(key.0.clone())));
        let total = if b.total == 0 { 1 } else { b.total };
        let got = b.ok;
        let kind = match key.1.as_str() {
            "image" => "图片",
            "file" => "文件",
            "video" => "视频",
            "text" => "文字",
            _ => "媒体",
        };
        let unit = match key.1.as_str() {
            "image" => "张",
            "text" => "条",
            _ => "个",
        };
        let mut detail = if got >= total {
            format!("{} {}到手机", total, unit)
        } else if got > 0 {
            format!("{}/{} {}到手机", got, total, unit)
        } else {
            format!("{} {}没到", total, unit)
        };
        if !b.error.is_empty() {
            detail.push_str(" · ");
            detail.push_str(&b.error.chars().take(60).collect::<String>());
        }
        if b.attempt > 1 {
            detail.push_str(&format!("（试了 {} 次）", b.attempt));
        }
        p.push(
            &format!("传到手机 · {}", kind),
            if got >= total { "ok" } else { "fail" },
            &at,
            &at,
            &detail,
            vec![code("transfer.py")],
            &format!("tf{}", at),
            Some(go("", "#card=transfer")),
        );
    };

    let mut hb_prev_ok = true;
    for d in &rows {
        let ev = super::util::str_or(d.get("event"));
        let at = iso_of(d.get("ts"));
        let rid = super::util::str_or(d.get("id"));

        if ev == "link_start" || ev == "article_start" {
            if !rid.is_empty() {
                let k = format!("{}:{}", if ev.starts_with("link") { "link" } else { "article" }, rid);
                open_start.push((k, at.clone()));
            }
            continue;
        }

        if ev == "link_done" {
            let state = state_of(d.get("state"));
            let plat: String = super::util::str_or(d.get("platform")).chars().take(16).collect();
            let plat = if plat.is_empty() { "链接".to_string() } else { plat };
            let n = super::util::int_of(d.get("images"));
            let key = format!("link:{}", rid);
            let start = match open_start.iter().position(|(k, _)| *k == key) {
                Some(i) => open_start.remove(i).1,
                None => at.clone(),
            };
            if state == "ok" {
                let dir = super::util::str_or(d.get("dir"));
                let mut detail = dir.clone();
                if n != 0 {
                    detail.push_str(&format!("（{} 张）", n));
                }
                let link = if dir.is_empty() {
                    go("gallery", "")
                } else {
                    go("gallery", &format!("?dir={}", py_quote(&dir)))
                };
                p.push(&format!("存图 · {}", plat), "ok", &start, &at, detail.trim(), vec![code("cuntu.py")], &format!("l{}", rid), Some(link));
            } else if state == "skip" {
                p.push(
                    &format!("存图 · {}", plat),
                    "skip",
                    &start,
                    &at,
                    &format!("不支持或重复：{}", super::util::str_or(d.get("state"))),
                    vec![code("cuntu.py")],
                    &format!("l{}", rid),
                    Some(ext(d.get("url").unwrap_or(&Value::Null))),
                );
            } else {
                let st = super::util::str_or(d.get("state"));
                let why = match st.as_str() {
                    "error" => "抓取出错".to_string(),
                    "unsupported" => "不支持的链接".to_string(),
                    "" => "抓取失败".to_string(),
                    other => other.to_string(),
                };
                let detail = {
                    let e = super::util::str_or(d.get("error"));
                    if !e.is_empty() {
                        e
                    } else {
                        let m = super::util::str_or(d.get("message"));
                        if !m.is_empty() { m } else { why }
                    }
                };
                p.push(
                    &format!("存图 · {}", plat),
                    "fail",
                    &start,
                    &at,
                    &detail.chars().take(160).collect::<String>(),
                    vec![code("cuntu.py")],
                    &format!("l{}", rid),
                    Some(ext(d.get("url").unwrap_or(&Value::Null))),
                );
            }
            continue;
        }

        if ev == "article_done" {
            let kind = {
                let k = super::util::str_or(d.get("kind"));
                if k.is_empty() { "fetch".to_string() } else { k }
            };
            let state = state_of(d.get("state"));
            let chars = super::util::int_of(d.get("chars"));
            let rel = super::util::str_or(d.get("rel"));
            let key = format!("article:{}", rid);
            let start = match open_start.iter().position(|(k, _)| *k == key) {
                Some(i) => open_start.remove(i).1,
                None => at.clone(),
            };
            let mut src = vec![code("article.py")];
            if kind == "classify" {
                src.push(ai(&model));
            }
            if kind == "gallery" {
                p.push("存文章", "skip", &start, &at, "识别成图集，已分流到存图", vec![code("article.py")], &format!("a{}", rid), Some(go("gallery", "")));
            } else if kind == "classify" {
                let topic = super::util::str_or(d.get("topic"));
                let mut detail = rel.clone();
                if !topic.is_empty() && state == "ok" {
                    detail.push_str(&format!("（{}）", topic));
                }
                let link = if !rel.is_empty() && state == "ok" {
                    Some(go("reader", &format!("?a={}", py_quote(&rel))))
                } else {
                    None
                };
                p.push("文章分类", state, &start, &at, &detail, src, &format!("a{}", rid), link);
            } else if state == "ok" {
                let detail = if !rel.is_empty() {
                    format!("{}（{} 字）", rel, chars)
                } else {
                    format!("{} 字", chars)
                };
                let link = if !rel.is_empty() {
                    Some(go("reader", &format!("?a={}", py_quote(&rel))))
                } else {
                    Some(go("reader", ""))
                };
                p.push("存文章", "ok", &start, &at, &detail, src, &format!("a{}", rid), link);
            } else {
                let e = super::util::str_or(d.get("error"));
                let detail = if e.is_empty() { "抓取失败".to_string() } else { e };
                p.push(
                    "存文章",
                    "fail",
                    &start,
                    &at,
                    &detail.chars().take(160).collect::<String>(),
                    src,
                    &format!("a{}", rid),
                    Some(ext(d.get("url").unwrap_or(&Value::Null))),
                );
            }
            continue;
        }

        if ev == "tf_push" {
            let k = (
                super::util::str_or(d.get("ts")),
                super::util::str_or(d.get("kind")),
            );
            if tf_seen.contains(&k) || is_alias(&k) {
                continue;
            }
            tf_seen.push(k.clone());
            let idx = tf_batch.iter().position(|b| b.key == k);
            match idx {
                Some(i) => {
                    let b = &tf_batch[i];
                    tf_push_item(&mut p, &k, b);
                }
                None => {
                    let b = Batch { key: k.clone(), total: 1, ok: 0, attempt: 0, names: Vec::new(), error: String::new() };
                    tf_push_item(&mut p, &k, &b);
                }
            }
            continue;
        }

        if ev == "upload" {
            let saved = super::util::int_of(d.get("saved"));
            let failed = super::util::int_of(d.get("failed"));
            let names: Vec<Value> = match d.get("names") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            let one = if names.len() == 1 {
                py_str(&names[0]).chars().take(40).collect::<String>()
            } else {
                String::new()
            };
            let mut detail = if one.is_empty() {
                format!("{} 个文件", saved)
            } else {
                one
            };
            if failed != 0 {
                detail.push_str(&format!("，{} 个失败", failed));
            }
            let ms = super::util::int_of(d.get("ms"));
            if ms != 0 {
                detail.push_str(&format!(" · {}ms", ms));
            }
            p.push(
                "收件箱上传",
                if failed != 0 && saved == 0 { "fail" } else { "ok" },
                &at,
                &at,
                &detail,
                vec![code("server.py")],
                &format!("u{}", at),
                None,
            );
            continue;
        }

        if ev == "ocr_sweep" {
            let done = super::util::int_of(d.get("done"));
            let failed = super::util::int_of(d.get("failed"));
            if done == 0 && failed == 0 {
                continue;
            }
            let mut detail = format!("{} 张转文字", done);
            if failed != 0 {
                detail.push_str(&format!("，{} 张失败", failed));
            }
            if truthy(d.get("error").unwrap_or(&Value::Null)) {
                detail.push_str(&format!(" · {}", py_str(d.get("error").unwrap_or(&Value::Null)).chars().take(60).collect::<String>()));
            }
            let mut src = vec![code("ocr.py")];
            if done != 0 {
                src.push(ai(&model));
            }
            p.push(
                "长图 OCR",
                if failed != 0 && done == 0 { "fail" } else { "ok" },
                &at,
                &at,
                &detail,
                src,
                &format!("o{}", at),
                Some(go("gallery", "")),
            );
            continue;
        }

        if ev == "trends_done" {
            let ok = d.get("ok").map(truthy).unwrap_or(false);
            let partial = super::util::str_or(d.get("status")) == "partial";
            let mut detail = format!("{} 条", super::util::int_of(d.get("items")));
            if partial {
                detail.push_str("（部分源失败）");
            }
            if truthy(d.get("llm").unwrap_or(&Value::Null)) {
                detail.push_str("，模型已摘要");
            }
            if truthy(d.get("error").unwrap_or(&Value::Null)) {
                detail.push_str(&format!(" · {}", py_str(d.get("error").unwrap_or(&Value::Null)).chars().take(80).collect::<String>()));
            }
            let mut src = vec![code("ai_trends_runner.py")];
            if truthy(d.get("llm").unwrap_or(&Value::Null)) {
                src.push(ai(&model));
            }
            p.push("AI 新闻抓取", if ok { "ok" } else { "fail" }, &at, &at, &detail, src, &format!("t{}", at), Some(go("", "#card=trends")));
            continue;
        }

        if ev == "dedupe_manual" {
            let apply_ = d.get("apply").map(truthy).unwrap_or(false);
            // 详情取自 dedupe_runner.STATE —— 本进程没有那个 STATE，按「没跑过」处理
            let mut bits: Vec<String> = vec![if apply_ { "已清理".to_string() } else { "试跑".to_string() }];
            bits[0].push_str("");
            p.push("图片去重", "ok", &at, &at, &bits.join(" · "), vec![code("dedupe_runner.py")], &format!("d{}", at), Some(go("gallery", "")));
            continue;
        }

        if ev == "heartbeat_manual" {
            p.push("系统巡检（手动）", "ok", &at, &at, "点了一次「巡检并自愈」", vec![code("heartbeat.py")], &format!("hm{}", at), Some(go("inspect", "")));
            continue;
        }

        if ev == "chat" {
            let text: String = super::util::str_or(d.get("text")).chars().take(40).collect();
            let ms = super::util::int_of(d.get("ms"));
            let mut detail = format!("「{}」", text);
            if ms != 0 {
                detail.push_str(&format!(" · {:.1}s", ms as f64 / 1000.0));
            }
            p.push("微信助手对话", "ok", &at, &at, &detail, vec![ai(&model)], &format!("c{}", at), None);
            continue;
        }

        if ev == "move" {
            let n = super::util::int_of(d.get("n"));
            let dest = super::util::str_or(d.get("dest"));
            let dest = dest
                .trim_end_matches('/')
                .split('/')
                .last()
                .unwrap_or("")
                .to_string();
            p.push(
                "收件箱整理",
                if super::util::int_of(d.get("ok")) == n { "ok" } else { "fail" },
                &at,
                &at,
                &format!("{} 个文件 → {}", n, dest),
                vec![code("server.py")],
                &format!("m{}", at),
                None,
            );
            continue;
        }

        if ev == "rename" {
            let to = super::util::str_or(d.get("to"));
            let detail: String = to
                .trim_end_matches('/')
                .split('/')
                .last()
                .unwrap_or("")
                .chars()
                .take(60)
                .collect();
            p.push("重命名", "ok", &at, &at, &detail, vec![code("server.py")], &format!("rn{}", at), None);
            continue;
        }

        if ev == "heartbeat_done" {
            let healed: Vec<Value> = match d.get("healed") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            let ok_now = d.get("ok").map(truthy).unwrap_or(false);
            if ok_now && healed.is_empty() && hb_prev_ok {
                continue;
            }
            hb_prev_ok = ok_now;
            let detail = if ok_now {
                if healed.is_empty() {
                    "各项正常".to_string()
                } else {
                    format!(
                        "自愈 {}",
                        healed.iter().take(3).map(|x| py_str(x)).collect::<Vec<_>>().join("、")
                    )
                }
            } else {
                let e = super::util::str_or(d.get("error"));
                if e.is_empty() {
                    "有检查项没过".to_string()
                } else {
                    e.chars().take(140).collect()
                }
            };
            p.push("系统巡检", if ok_now { "ok" } else { "fail" }, &at, &at, &detail, vec![code("heartbeat.py")], &format!("h{}", at), Some(go("inspect", "")));
            continue;
        }
    }

    // —— 传输记录补漏：窗口外但还在两日内的 ——
    for (i, b) in tf_batch.iter().enumerate() {
        if removed[i] {
            continue;
        }
        if !tf_seen.contains(&b.key) {
            let key = b.key.clone();
            tf_push_item(&mut p, &key, b);
        }
    }

    // —— 收到的与被删的 ——
    for d in tail_by_event(&log, &["tf_in", "tf_del"], 2) {
        let ev2 = super::util::str_or(d.get("event"));
        let at2 = iso_of(d.get("ts"));
        let kind2 = match super::util::str_or(d.get("kind")).as_str() {
            "image" => "图片",
            "file" => "文件",
            "video" => "视频",
            "text" => "文字",
            "link" => "链接",
            _ => "消息",
        };
        if ev2 == "tf_in" {
            let nm = super::util::str_or(d.get("name"));
            let detail = if !nm.is_empty() {
                let mut s: String = nm.chars().take(60).collect();
                if truthy(d.get("size").unwrap_or(&Value::Null)) {
                    s.push_str(&format!(" · {}", human(super::util::int_of(d.get("size")) as f64)));
                }
                s
            } else {
                let t: String = super::util::str_or(d.get("text")).chars().take(60).collect();
                if t.is_empty() { "（空）".to_string() } else { t }
            };
            p.push(&format!("收到 · {}", kind2), "ok", &at2, &at2, &detail, vec![code("transfer.py")], &format!("in{}", at2), Some(go("", "#card=transfer")));
        } else {
            let detail: String = {
                let n = super::util::str_or(d.get("name"));
                let t = super::util::str_or(d.get("text"));
                let base = if !n.is_empty() { n } else if !t.is_empty() { t } else { "一条消息".to_string() };
                base.chars().take(60).collect()
            };
            p.push(&format!("传输助手删除 · {}", kind2), "ok", &at2, &at2, &detail, vec![code("transfer.py")], &format!("dl{}", at2), Some(go("", "#card=transfer")));
        }
    }

    // —— 进行中：只有 start 没 done ——
    let mut pending: Vec<(String, String)> = Vec::new();
    for (k, st) in open_start.iter() {
        if st.is_empty() {
            continue;
        }
        let age = sessions::secs(st, &now_s);
        match age {
            Some(a) if a <= WL_RUN_WINDOW => pending.push((k.clone(), st.clone())),
            _ => {}
        }
    }
    for (k, st) in pending {
        let (kind, rid) = k.split_once(':').unwrap_or((k.as_str(), ""));
        let link = kind == "link";
        p.push(
            if link { "正在抓取存图" } else { "正在抓取文章" },
            "run",
            &st,
            "",
            "开始了，还没收尾",
            vec![code(if link { "cuntu.py" } else { "article.py" })],
            &format!("run-{}", rid),
            None,
        );
    }
    p.items
}

// ------------------------------------------------------------------ 各插件的账

fn pg_item(
    plugin: &str,
    task: &str,
    status: &str,
    start: &str,
    end: &str,
    detail: &str,
    sources: Vec<Value>,
    rid: &str,
    link: Option<Value>,
) -> Value {
    let mut m = Map::new();
    m.insert("id".into(), sval(&format!("{}-{}", plugin, rid)));
    m.insert("plugin".into(), sval(plugin));
    m.insert("task".into(), sval(task));
    m.insert("detail".into(), sval(&detail.chars().take(160).collect::<String>()));
    m.insert("status".into(), sval(status));
    m.insert("start".into(), sval(start));
    m.insert("end".into(), sval(end));
    m.insert(
        "secs".into(),
        if !start.is_empty() && !end.is_empty() {
            match sessions::secs(start, end) {
                Some(v) => json!(v),
                None => Value::Null,
            }
        } else {
            Value::Null
        },
    );
    m.insert("sources".into(), Value::Array(sources));
    if let Some(l) = link {
        if l.get("url").map(truthy).unwrap_or(false) {
            m.insert("link".into(), l);
        }
    }
    Value::Object(m)
}

/// `plugin_manifest`：中台里在跑的插件 + 口袋自己。
pub fn plugin_manifest(ctx: &Ctx) -> Vec<Value> {
    let mut out = vec![json!({"id": "pocket", "name": "口袋", "glyph": "", "url": "", "n": 0, "fail": 0})];
    let d = load_obj(&cards::workbench(ctx).join("plugins.json"));
    if let Some(Value::Array(list)) = d.get("plugins") {
        for p in list {
            if !p.is_object() {
                continue;
            }
            let pid = super::util::str_or(p.get("id")).trim().to_string();
            if pid.is_empty() || p.get("soon").map(truthy).unwrap_or(false) {
                continue;
            }
            let name = {
                let n = super::util::str_or(p.get("name"));
                if n.is_empty() { pid.clone() } else { n }
            };
            out.push(json!({
                "id": pid,
                "name": name,
                "glyph": super::util::str_or(p.get("glyph")),
                "url": super::util::str_or(p.get("url")),
                "n": 0,
                "fail": 0,
            }));
        }
    }
    out
}

fn urls_of(ctx: &Ctx) -> Vec<(String, String)> {
    plugin_manifest(ctx)
        .into_iter()
        .filter_map(|p| {
            let u = super::util::str_or(p.get("url"));
            if u.is_empty() {
                None
            } else {
                Some((super::util::str_or(p.get("id")), u))
            }
        })
        .collect()
}

fn url_of(urls: &[(String, String)], id: &str, dflt: &str) -> String {
    urls.iter()
        .find(|(k, _)| k == id)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| dflt.to_string())
}

/// `_device_items`：智能设备的使用记录。
fn device_items(ctx: &Ctx, urls: &[(String, String)]) -> Vec<Value> {
    let rows = tail_jsonl(&cards::root(ctx).join("13-设备").join("使用记录.jsonl"), PG_LIMIT);
    if rows.is_empty() {
        return Vec::new();
    }
    let link = json!({"url": url_of(urls, "devices", "http://127.0.0.1:8778/"), "pg": true});
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let act = super::util::str_or(r.get("action"));
        let name = {
            let n = super::util::str_or(r.get("name"));
            if n.is_empty() { "（没名字）".to_string() } else { n }
        };
        let det = super::util::str_or(r.get("detail")).trim().to_string();
        let at = iso_of(r.get("at"));
        let d = if act == "add" {
            if det.is_empty() { name.clone() } else { format!("{}（{}）", name, det) }
        } else if det.is_empty() {
            name.clone()
        } else {
            format!("{} · {}", name, det)
        };
        let task = match act.as_str() {
            "add" => "接入设备",
            "delete" => "移除设备",
            "on" => "打开设备",
            "off" => "关闭设备",
            "set" => "改设备设置",
            "use" => "记录使用",
            "" => "操作",
            other => other,
        };
        let id = {
            let rid = super::util::str_or(r.get("id"));
            if rid.is_empty() { format!("{}", i) } else { rid }
        };
        out.push(pg_item("devices", task, "ok", &at, &at, &d, vec![tool("智能设备")], &format!("{}-{}", id, at), Some(link.clone())));
    }
    out
}

/// `_game_items`：游戏记录的流水。
fn game_items(ctx: &Ctx, urls: &[(String, String)]) -> Vec<Value> {
    let root = cards::root(ctx);
    let rows = tail_jsonl(&root.join("17-游戏").join("记录.jsonl"), PG_LIMIT);
    if rows.is_empty() {
        return Vec::new();
    }
    let lib = load_obj(&root.join("17-游戏").join("游戏.json"));
    let mut names: Vec<(String, String)> = Vec::new();
    if let Some(Value::Array(gs)) = lib.get("games") {
        for g in gs {
            names.push((super::util::str_or(g.get("id")), super::util::str_or(g.get("name"))));
        }
    }
    let link = json!({"url": url_of(urls, "games", "http://127.0.0.1:8779/"), "pg": true});
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let kind = {
            let k = super::util::str_or(r.get("kind"));
            if k.is_empty() { "play".to_string() } else { k }
        };
        let gid = super::util::str_or(r.get("gid"));
        let gname = {
            let n = names.iter().find(|(k, _)| *k == gid).map(|(_, v)| v.clone()).unwrap_or_default();
            if n.is_empty() { "某个游戏".to_string() } else { n }
        };
        let at = iso_of(r.get("ts"));
        let note = super::util::str_or(r.get("note")).trim().to_string();
        let title = super::util::str_or(r.get("title")).trim().to_string();
        let mut d = match kind.as_str() {
            "play" => format!("{} · {} 分钟", gname, super::util::int_of(r.get("min"))),
            "milestone" => format!(
                "{} · {}（{}%）",
                gname,
                if title.is_empty() { "里程碑" } else { &title },
                super::util::int_of(r.get("progress"))
            ),
            "match" => {
                let mut s = format!(
                    "{} · {} · {}",
                    gname,
                    if title.is_empty() { "一局" } else { &title },
                    if truthy(r.get("win").unwrap_or(&Value::Null)) { "胜" } else { "负" }
                );
                if truthy(r.get("kda").unwrap_or(&Value::Null)) {
                    s.push_str(&format!(" · {}", super::util::str_or(r.get("kda"))));
                }
                if truthy(r.get("rank").unwrap_or(&Value::Null)) {
                    s.push_str(&format!(" · {}", super::util::str_or(r.get("rank"))));
                }
                s
            }
            "buy" => format!(
                "{} · {} · ¥{}",
                gname,
                if title.is_empty() { "购入" } else { &title },
                super::util::str_or(r.get("cost"))
            ),
            _ => format!(
                "{} · {}",
                gname,
                if !title.is_empty() {
                    title.clone()
                } else if !note.is_empty() {
                    note.clone()
                } else {
                    "笔记".to_string()
                }
            ),
        };
        if !note.is_empty() && kind != "note" && kind != "play" {
            d.push_str(&format!("：{}", note));
        } else if !note.is_empty() && kind == "play" {
            d.push_str(&format!(" · {}", note));
        }
        let task = match kind.as_str() {
            "play" => "玩游戏",
            "milestone" => "游戏里程碑",
            "match" => "游戏战绩",
            "buy" => "游戏花费",
            "note" => "游戏笔记",
            other => other,
        };
        let rid = {
            let g = super::util::str_or(r.get("gid"));
            if g.is_empty() { format!("{}", i) } else { g }
        };
        out.push(pg_item("games", task, "ok", &at, &at, &d, vec![tool("游戏记录")], &format!("{}-{}", rid, at), Some(link.clone())));
    }
    out
}

/// `_media_items`：影音室的重扫 / 刮削 / 移动 / 已看 / 删除。
fn media_items(ctx: &Ctx, urls: &[(String, String)]) -> Vec<Value> {
    let base = cards::root(ctx).join("02-媒体");
    let rows = tail_jsonl(&base.join("_墙").join("记录.jsonl"), 400);
    if rows.is_empty() {
        return Vec::new();
    }
    let reg = load_obj(&base.join("_墙").join("库.json"));
    let mut names: Vec<(String, String)> = Vec::new();
    if let Some(Value::Array(ls)) = reg.get("libraries") {
        for l in ls {
            if l.is_object() {
                names.push((super::util::str_or(l.get("id")), super::util::str_or(l.get("name"))));
            }
        }
    }
    let link = json!({"url": url_of(urls, "media", "http://127.0.0.1:8773/"), "pg": true});
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let kind = {
            let k = super::util::str_or(r.get("kind"));
            if k.is_empty() { "refresh".to_string() } else { k }
        };
        let at = iso_of(r.get("ts"));
        let ok = r.get("ok").map(|v| truthy(v)).unwrap_or(true);
        let mut bits: Vec<String> = Vec::new();
        let lib = super::util::str_or(r.get("lib"));
        if let Some((_, n)) = names.iter().find(|(k, _)| *k == lib) {
            if !n.is_empty() {
                bits.push(n.clone());
            }
        }
        for k in ["item", "note"] {
            let v = super::util::str_or(r.get(k));
            if !v.is_empty() {
                bits.push(v);
            }
        }
        let mut detail = bits.join(" · ");
        if kind == "boot_timeout" || kind == "launch_timeout" {
            let secs = super::util::int_of(r.get("secs"));
            if kind == "boot_timeout" {
                detail = if secs != 0 { format!("等了 {}s 还没通", secs) } else { "等待超时".to_string() };
                if truthy(r.get("error").unwrap_or(&Value::Null)) {
                    detail.push_str(&format!(" · {}", super::util::str_or(r.get("error")).chars().take(80).collect::<String>()));
                }
            } else {
                let e = super::util::str_or(r.get("error"));
                detail = if e.is_empty() { "端口一直没通".to_string() } else { e }.chars().take(120).collect();
            }
        } else if kind == "refresh" || kind == "scrape" || kind == "move" {
            let dn = super::util::int_of(r.get("done"));
            let tt = super::util::int_of(r.get("total"));
            if tt != 0 {
                if !detail.is_empty() {
                    detail.push_str(" · ");
                }
                detail.push_str(&format!("{}/{}", dn, tt));
            }
            let secs = super::util::int_of(r.get("secs"));
            if secs != 0 {
                if !detail.is_empty() {
                    detail.push_str(" · ");
                }
                detail.push_str(&format!("{}s", secs));
            }
            if !ok && truthy(r.get("error").unwrap_or(&Value::Null)) {
                detail = super::util::str_or(r.get("error")).chars().take(120).collect();
            }
        }
        let status = if kind == "boot_timeout" || kind == "launch_timeout" {
            "warn"
        } else if ok {
            "ok"
        } else {
            "fail"
        };
        let task = match kind.as_str() {
            "refresh" => "重扫库",
            "scrape" => "刮削海报",
            "move" => "移动文件",
            "watch" => "标记已看",
            "delete" => "删除",
            "undo" => "撤销删除",
            "boot_timeout" => "启动超时",
            "launch_timeout" => "拉起超时",
            other => other,
        };
        let rid = {
            let ts = super::util::str_or(r.get("ts"));
            if ts.is_empty() { format!("{}", i) } else { ts }
        };
        out.push(pg_item("media", task, status, &at, &at, &detail, vec![tool("影音室")], &format!("{}-{}", rid, kind), Some(link.clone())));
    }
    out
}

/// `_app_items`：本机 App 的开关 / 闪退 / 卡住。
fn app_items(ctx: &Ctx, _urls: &[(String, String)]) -> Vec<Value> {
    let rows = tail_jsonl(&cards::root(ctx).join("10-系统记录").join("App事件.jsonl"), 400);
    if rows.is_empty() {
        return Vec::new();
    }
    let mut ended: Vec<String> = Vec::new();
    for r in rows.iter() {
        let ev = super::util::str_or(r.get("event"));
        if ev == "app_close" || ev == "app_crash" || ev == "app_kill" {
            ended.push(super::util::str_or(r.get("pid")));
        }
    }
    let mut out = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let ev = super::util::str_or(r.get("event"));
        if ev == "app_rec" {
            continue;
        }
        let app = {
            let a = super::util::str_or(r.get("app"));
            if a.is_empty() { "（不知道名字）".to_string() } else { a }
        };
        let at = iso_of(r.get("ts"));
        let pid = super::util::str_or(r.get("pid"));
        let mut status = "ok";
        let mut detail = super::util::str_or(r.get("note"));
        let task;
        if ev == "app_open" {
            if !pid.is_empty() && ended.contains(&pid) {
                continue;
            }
            task = format!("打开 {}", app);
            if detail.is_empty() {
                detail = "这次还没退出".to_string();
            }
        } else if ev == "app_close" {
            task = format!("退出 {}", app);
        } else if ev == "app_crash" {
            task = format!("{} 闪退", app);
            status = "fail";
        } else if ev == "app_hang" {
            task = format!("{} 卡住", app);
            status = "warn";
        } else if ev == "app_kill" {
            task = format!("{} 卡住后强制退出", app);
            status = "fail";
        } else {
            task = format!("{} {}", app, ev);
        }
        let rid = {
            let ts = super::util::str_or(r.get("ts"));
            if ts.is_empty() { format!("{}", i) } else { ts }
        };
        out.push(pg_item("apps", &task, status, &at, &at, &detail, vec![tool("App 监工")], &format!("{}-{}-{}", rid, ev, pid), None));
        if out.len() >= 60 && (ev == "app_open" || ev == "app_close") {
            break;
        }
    }
    out
}

/// `_plugin_items`：各插件的账并成一份（进行中同任务只留最新一条）。
pub fn plugin_items(ctx: &Ctx) -> Vec<Value> {
    let urls = urls_of(ctx);
    let mut out: Vec<Value> = Vec::new();
    out.extend(device_items(ctx, &urls));
    out.extend(game_items(ctx, &urls));
    out.extend(media_items(ctx, &urls));
    out.extend(app_items(ctx, &urls));
    let mut sorted = out;
    sorted.sort_by(|a, b| {
        super::util::str_or(b.get("start")).cmp(&super::util::str_or(a.get("start")))
    });
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut keep = Vec::new();
    for it in sorted {
        if super::util::str_or(it.get("status")) == "run" {
            let k = (super::util::str_or(it.get("plugin")), super::util::str_or(it.get("task")));
            if seen.contains(&k) {
                continue;
            }
            seen.push(k);
        }
        keep.push(it);
    }
    keep
}

// ------------------------------------------------------------------ 折叠

/// `_collapse`：同任务、同结果、同一天 → 合成一条「×N」。
pub fn collapse(items: Vec<Value>) -> Vec<Value> {
    let mut order: Vec<(String, String, String, String)> = Vec::new();
    let mut buckets: Vec<((String, String, String, String), Vec<Value>)> = Vec::new();
    for it in items.iter() {
        if super::util::str_or(it.get("status")) == "run" {
            continue;
        }
        let key = (
            super::util::str_or(it.get("start")).chars().take(10).collect::<String>(),
            super::util::str_or(it.get("task")),
            super::util::str_or(it.get("status")),
            super::util::str_or(it.get("plugin")),
        );
        match buckets.iter().position(|(k, _)| *k == key) {
            Some(i) => buckets[i].1.push(it.clone()),
            None => {
                order.push(key.clone());
                buckets.push((key, vec![it.clone()]));
            }
        }
    }
    let mut out: Vec<Value> = items
        .iter()
        .filter(|it| super::util::str_or(it.get("status")) == "run")
        .cloned()
        .collect();
    for key in order {
        let idx = match buckets.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => continue,
        };
        let mut g = buckets[idx].1.clone();
        g.sort_by(|a, b| super::util::str_or(a.get("start")).cmp(&super::util::str_or(b.get("start"))));
        let last = g.last().unwrap().clone();
        let mut head = match last.clone() {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        let g_last_start = super::util::str_or(g.last().unwrap().get("start"));
        let g_last_end = {
            let e = super::util::str_or(g.last().unwrap().get("end"));
            if e.is_empty() { g_last_start.clone() } else { e }
        };
        let g_first_start = super::util::str_or(g.first().unwrap().get("start"));
        head.insert("start".into(), sval(&g_last_start));
        head.insert("end".into(), sval(&g_last_end));
        head.insert("first".into(), sval(&g_first_start));
        head.insert(
            "secs".into(),
            if !g_last_start.is_empty() && !g_last_end.is_empty() {
                match sessions::secs(&g_last_start, &g_last_end) {
                    Some(v) => json!(v),
                    None => Value::Null,
                }
            } else {
                Value::Null
            },
        );
        if g.len() > 1 {
            head.insert("count".into(), json!(g.len()));
            // token 是消耗量，合并行必须求和
            if g.iter().any(|x| x.get("tokens").map(|t| t.is_object()).unwrap_or(false)) {
                let mut tin = 0i64;
                let mut tout = 0i64;
                let mut cached = 0i64;
                let mut total = 0i64;
                for x in g.iter() {
                    let t = x.get("tokens").cloned().unwrap_or_else(|| json!({}));
                    tin += super::util::int_of(t.get("in"));
                    tout += super::util::int_of(t.get("out"));
                    cached += super::util::int_of(t.get("cached"));
                    total += super::util::int_of(t.get("total"));
                }
                head.insert("tokens".into(), json!({"in": tin, "out": tout, "cached": cached, "total": total}));
            }
            let d0 = super::util::str_or(head.get("detail"));
            let detail = if d0.is_empty() {
                String::new()
            } else {
                format!("最近：{}", d0)
            };
            head.insert("detail".into(), sval(&detail.chars().take(120).collect::<String>()));
            let start_take = g.len().saturating_sub(12);
            let children: Vec<Value> = g[start_take..]
                .iter()
                .rev()
                .map(|x| {
                    json!({
                        "start": super::util::str_or(x.get("start")),
                        "end": super::util::str_or(x.get("end")),
                        "detail": super::util::str_or(x.get("detail")).chars().take(120).collect::<String>(),
                        "status": super::util::str_or(x.get("status")),
                        "link": x.get("link").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect();
            head.insert("children".into(), Value::Array(children));
        }
        out.push(Value::Object(head));
    }
    out
}

// ------------------------------------------------------------------ 右卡：pocket 折一天一条

/// `_pocket_pulse`：口袋的一堆零碎活折成「一天一条」。
pub fn pocket_pulse(items: &[Value]) -> Vec<Value> {
    let mut days: Vec<(String, Vec<Value>)> = Vec::new();
    for it in items {
        let day: String = super::util::str_or(it.get("start")).chars().take(10).collect();
        match days.iter().position(|(d, _)| *d == day) {
            Some(i) => days[i].1.push(it.clone()),
            None => days.push((day, vec![it.clone()])),
        }
    }
    let mut out = Vec::new();
    for (day, its) in days {
        if day.is_empty() {
            continue;
        }
        let okn = its.iter().filter(|x| super::util::str_or(x.get("status")) == "ok").count();
        let _ = okn;
        let failn = its.iter().filter(|x| super::util::str_or(x.get("status")) == "fail").count();
        let runn = its.iter().filter(|x| super::util::str_or(x.get("status")) == "run").count();
        let mut kinds: Vec<(String, i64)> = Vec::new();
        for x in its.iter() {
            let k = super::util::str_or(x.get("task")).split(" · ").next().unwrap_or("").to_string();
            match kinds.iter().position(|(kk, _)| *kk == k) {
                Some(i) => kinds[i].1 += 1,
                None => kinds.push((k, 1)),
            }
        }
        kinds.sort_by(|a, b| b.1.cmp(&a.1));
        let mut starts: Vec<String> = its
            .iter()
            .map(|x| super::util::str_or(x.get("start")))
            .filter(|s| !s.is_empty())
            .collect();
        starts.sort();
        let mut ends: Vec<String> = its
            .iter()
            .map(|x| super::util::str_or(x.get("end")))
            .filter(|s| !s.is_empty())
            .collect();
        ends.sort();
        let mut bits: Vec<String> = vec![format!("{} 条", its.len())];
        for (k, n) in kinds.iter().take(3) {
            bits.push(format!("{} {}", k, n));
        }
        if failn != 0 {
            bits.push(format!("失败 {}", failn));
        }
        let detail: String = bits.join(" · ").chars().take(160).collect();
        let mut m = Map::new();
        m.insert("id".into(), sval(&format!("pp{}", day)));
        m.insert("task".into(), sval("pocket"));
        m.insert("detail".into(), sval(&detail));
        m.insert("status".into(), sval(if runn > 0 { "run" } else { "ok" }));
        m.insert("start".into(), sval(starts.first().map(|s| s.as_str()).unwrap_or("")));
        m.insert("end".into(), sval(ends.last().map(|s| s.as_str()).unwrap_or("")));
        m.insert("secs".into(), Value::Null);
        m.insert("sources".into(), Value::Array(vec![tool("pocket")]));
        m.insert("client".into(), sval("pocket"));
        out.push(Value::Object(m));
    }
    out
}

// ------------------------------------------------------------------ 补记

/// `read_worklog`：worklog.json 里手写/导入的账。
pub fn read_worklog(ctx: &Ctx) -> Vec<Value> {
    let p = cards::workbench(ctx).join("worklog.json");
    let d = load_obj(&p);
    let raw: Vec<Value> = match d.get("items") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for it in raw {
        if !it.is_object() || super::util::str_or(it.get("task")).trim().is_empty() {
            continue;
        }
        let mut src = Vec::new();
        if truthy(it.get("code").unwrap_or(&Value::Null)) {
            src.push(code(&super::util::str_or(it.get("code")).chars().take(40).collect::<String>()));
        }
        if truthy(it.get("ai").unwrap_or(&Value::Null)) {
            src.push(ai(&super::util::str_or(it.get("ai")).chars().take(40).collect::<String>()));
        }
        let st = iso_of(it.get("start"));
        let en = iso_of(it.get("end"));
        let status = {
            let s = super::util::str_or(it.get("status"));
            if ["ok", "fail", "run", "skip"].contains(&s.as_str()) { s } else { "ok".to_string() }
        };
        let client = {
            let c = super::util::str_or(it.get("client"));
            if c.is_empty() { "手记".to_string() } else { c }
        };
        let mut m = Map::new();
        let id = super::util::str_or(it.get("id"));
        let rid_out = if id.is_empty() { format!("w{}", st) } else { id.clone() };
        m.insert("id".into(), sval(&rid_out));
        m.insert("task".into(), sval(&super::util::str_or(it.get("task")).chars().take(80).collect::<String>()));
        m.insert("detail".into(), sval(&super::util::str_or(it.get("detail")).chars().take(200).collect::<String>()));
        m.insert("status".into(), sval(&status));
        m.insert("start".into(), sval(&st));
        m.insert("end".into(), sval(&en));
        m.insert(
            "secs".into(),
            if !st.is_empty() && !en.is_empty() {
                match sessions::secs(&st, &en) {
                    Some(v) => json!(v),
                    None => Value::Null,
                }
            } else {
                Value::Null
            },
        );
        m.insert("sources".into(), Value::Array(src));
        m.insert("manual".into(), Value::Bool(true));
        m.insert("client".into(), sval(&client.chars().take(40).collect::<String>()));
        out.push(Value::Object(m));
    }
    out
}

/// `_manual_host_items`：补记进右卡（客户端放最前）。
fn manual_host_items(ctx: &Ctx) -> Vec<Value> {
    let mut out = Vec::new();
    for it in read_worklog(ctx) {
        let client = super::util::str_or(it.get("client"));
        let mut m = match it {
            Value::Object(m) => m,
            _ => continue,
        };
        let mut src: Vec<Value> = Vec::new();
        if !client.is_empty() {
            src.push(tool(&client));
        }
        if let Some(Value::Array(old)) = m.get("sources") {
            src.extend(old.clone());
        }
        m.insert("sources".into(), Value::Array(src));
        out.push(Value::Object(m));
    }
    out
}

// ------------------------------------------------------------------ 组装

/// `_worklog`：两张卡、两份账，不互相掺。
pub fn worklog(ctx: &Ctx) -> Value {
    let auto = auto_worklog(ctx);
    let mut merged = auto.clone();
    merged.extend(plugin_items(ctx));
    let mut items = collapse(merged);

    // 「系统巡检」是状态卡，不是流水：只留最近一条
    let hb: Vec<Value> = items
        .iter()
        .filter(|x| super::util::str_or(x.get("task")) == "系统巡检")
        .cloned()
        .collect();
    if hb.len() > 1 {
        let keep_start = hb
            .iter()
            .map(|x| super::util::str_or(x.get("start")))
            .max()
            .unwrap_or_default();
        items = items
            .into_iter()
            .filter(|x| {
                super::util::str_or(x.get("task")) != "系统巡检"
                    || super::util::str_or(x.get("start")) == keep_start
            })
            .collect();
    }
    items.sort_by(|a, b| {
        let ka = (super::util::str_or(a.get("status")) == "run", super::util::str_or(a.get("start")));
        let kb = (super::util::str_or(b.get("status")) == "run", super::util::str_or(b.get("start")));
        kb.cmp(&ka)
    });

    let mut plugins: Vec<Value> = plugin_manifest(ctx)
        .into_iter()
        .filter(|p| super::util::str_or(p.get("id")) != "nest")
        .collect();
    for it in items.iter() {
        let pid = {
            let p = super::util::str_or(it.get("plugin"));
            if p.is_empty() { "pocket".to_string() } else { p }
        };
        let idx = plugins.iter().position(|p| super::util::str_or(p.get("id")) == pid);
        let i = match idx {
            Some(i) => i,
            None => {
                let label = match pid.as_str() {
                    "apps" => "App 监工",
                    "media" => "影音室",
                    other => other,
                };
                plugins.push(json!({"id": pid, "name": label, "glyph": "", "url": "", "n": 0, "fail": 0}));
                plugins.len() - 1
            }
        };
        if let Value::Object(m) = &mut plugins[i] {
            let n = super::util::int_of(m.get("n")) + 1;
            m.insert("n".into(), json!(n));
            if super::util::str_or(it.get("status")) == "fail" {
                let f = super::util::int_of(m.get("fail")) + 1;
                m.insert("fail".into(), json!(f));
            }
        }
    }

    // 右卡
    let mut host_src = pocket_pulse(&auto);
    host_src.extend(sessions::agent_items(&cards::root(ctx), _WL_HOST_DAYS));
    host_src.extend(manual_host_items(ctx));
    let mut host = collapse(host_src);
    for it in host.iter_mut() {
        if super::util::str_or(it.get("client")) == "pocket" {
            if let Value::Object(m) = it {
                m.insert("secs".into(), Value::Null);
            }
        }
    }
    let sort_host = |v: &mut Vec<Value>| {
        v.sort_by(|a, b| {
            let ka = (
                super::util::str_or(a.get("status")) == "run",
                {
                    let e = super::util::str_or(a.get("end"));
                    if e.is_empty() { super::util::str_or(a.get("start")) } else { e }
                },
            );
            let kb = (
                super::util::str_or(b.get("status")) == "run",
                {
                    let e = super::util::str_or(b.get("end"));
                    if e.is_empty() { super::util::str_or(b.get("start")) } else { e }
                },
            );
            kb.cmp(&ka)
        });
    };
    sort_host(&mut host);
    let keep: Vec<Value> = host
        .iter()
        .filter(|x| x.get("manual").map(truthy).unwrap_or(false))
        .take(24)
        .cloned()
        .collect();
    if !keep.is_empty() {
        let mut rest: Vec<Value> = host
            .iter()
            .filter(|x| !x.get("manual").map(truthy).unwrap_or(false))
            .cloned()
            .collect();
        let limit = sessions::WL_HOST_LIMIT.saturating_sub(keep.len());
        rest.truncate(limit);
        let mut all = keep;
        all.extend(rest);
        all.sort_by(|a, b| {
            let ka = (
                super::util::str_or(a.get("status")) == "run",
                {
                    let e = super::util::str_or(a.get("end"));
                    if e.is_empty() { super::util::str_or(a.get("start")) } else { e }
                },
            );
            let kb = (
                super::util::str_or(b.get("status")) == "run",
                {
                    let e = super::util::str_or(b.get("end"));
                    if e.is_empty() { super::util::str_or(b.get("start")) } else { e }
                },
            );
            kb.cmp(&ka)
        });
        host = all;
    } else {
        host.truncate(sessions::WL_HOST_LIMIT);
    }

    // 右卡筛选条
    let mut tally: Vec<(String, i64)> = Vec::new();
    for it in host.iter() {
        let c = super::util::str_or(it.get("client"));
        match tally.iter().position(|(k, _)| *k == c) {
            Some(i) => tally[i].1 += 1,
            None => tally.push((c, 1)),
        }
    }
    tally.sort_by(|a, b| b.1.cmp(&a.1));
    let clients: Vec<Value> = tally
        .iter()
        .map(|(k, v)| json!({"name": k, "n": v}))
        .collect();

    let counts = |st: &str| items.iter().filter(|x| super::util::str_or(x.get("status")) == st).count();
    let ai_n = items
        .iter()
        .filter(|x| match x.get("sources") {
            Some(Value::Array(a)) => a
                .iter()
                .any(|s| s.get("kind").and_then(|k| k.as_str()) == Some("ai")),
            _ => false,
        })
        .count();

    let mut m = Map::new();
    m.insert(
        "items".into(),
        Value::Array(items.iter().take(WL_LIMIT).cloned().collect()),
    );
    m.insert("host".into(), Value::Array(host));
    m.insert("clients".into(), Value::Array(clients));
    m.insert("plugins".into(), Value::Array(plugins));
    m.insert("total".into(), json!(items.len()));
    m.insert("runs".into(), json!(counts("run")));
    m.insert("ok".into(), json!(counts("ok")));
    m.insert("fail".into(), json!(counts("fail")));
    m.insert("skip".into(), json!(counts("skip")));
    m.insert("ai".into(), json!(ai_n));
    m.insert("at".into(), sval(&now_ymd_hms()));
    Value::Object(m)
}

// ------------------------------------------------------------------ 路由

/// `GET /api/worklog`
pub async fn worklog_read(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let c = ctx.clone();
    match tokio::task::spawn_blocking(move || worklog(&c)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => http_err(500, &format!("internal error: {e}")),
    }
}

/// `POST /api/worklog`：补记一条。
pub async fn worklog_add(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    let task: String = super::util::str_or(b.get("task")).trim().chars().take(80).collect();
    if task.is_empty() {
        return Json(json!({"ok": false, "error": "先说清楚做了什么"})).into_response();
    }
    let mut status = super::util::str_or(b.get("status"));
    if status.is_empty() {
        status = "ok".to_string();
    }
    if !["ok", "fail", "run", "skip"].contains(&status.as_str()) {
        status = "ok".to_string();
    }
    let kind = {
        let k = super::util::str_or(b.get("kind"));
        if k.is_empty() { "ai".to_string() } else { k }
    };
    let who: String = super::util::str_or(b.get("who")).trim().chars().take(60).collect();
    let client: String = super::util::str_or(b.get("client")).trim().chars().take(40).collect();
    let parts: Vec<String> = regex::Regex::new(r"[+＋,，/、]")
        .unwrap()
        .split(&who)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mut code_s = String::new();
    let mut ai_s = String::new();
    if kind == "both" {
        if parts.len() >= 2 {
            code_s = parts[0].clone();
            ai_s = parts[1].clone();
        } else if let Some(p) = parts.first() {
            code_s = p.clone();
            ai_s = p.clone();
        }
    } else if kind == "code" {
        code_s = if who.is_empty() { "脚本".to_string() } else { who.clone() };
    } else {
        ai_s = if who.is_empty() { "AI".to_string() } else { who.clone() };
    }
    let now = now_ymd_hms();
    let start = {
        let s = iso_of(b.get("start"));
        if s.is_empty() { now.clone() } else { s }
    };
    let end = if status == "run" {
        String::new()
    } else {
        let e = iso_of(b.get("end"));
        if e.is_empty() { now.clone() } else { e }
    };
    // uuid4().hex[:10] 的等价物：随机 16 进制十位
    let id = format!("w{}", random_hex(10));
    let item = {
        let mut m = Map::new();
        m.insert("id".into(), sval(&id));
        m.insert("task".into(), sval(&task));
        m.insert("detail".into(), sval(&super::util::str_or(b.get("detail")).chars().take(200).collect::<String>()));
        m.insert("status".into(), sval(&status));
        m.insert("start".into(), sval(&start));
        m.insert("end".into(), sval(&end));
        m.insert("code".into(), sval(&code_s));
        m.insert("ai".into(), sval(&ai_s));
        m.insert("client".into(), sval(&client));
        m.insert("created".into(), sval(&now));
        Value::Object(m)
    };

    let p = cards::workbench(ctx.as_ref()).join("worklog.json");
    let mut d = load_obj(&p);
    let mut items: Vec<Value> = match d.get("items") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    items.push(item.clone());
    let keep_from = items.len().saturating_sub(WL_KEEP);
    let items: Vec<Value> = items[keep_from..].to_vec();
    let payload = json!({"items": items, "updated_at": now});
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let text = jsonio::dumps(&payload, 1);
    let tmp = p.with_extension("json.tmp");
    let _ = std::fs::write(&tmp, text);
    let _ = std::fs::rename(&tmp, &p);
    Json(json!({"ok": true, "item": item})).into_response()
}

fn random_hex(n: usize) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut out = String::new();
    for _ in 0..n {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let v = (seed >> 33) as u32 & 0xF;
        out.push(std::char::from_digit(v, 16).unwrap());
    }
    out
}
