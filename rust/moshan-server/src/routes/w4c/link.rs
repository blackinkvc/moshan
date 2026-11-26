// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/linkbox.py` 的移植（`POST /api/link`、`GET /api/link/status`）。
// 不做：不在本进程抓网页。Python 那边是起 `cuntu.py --json` 子进程干的，
//       这里照抄命令行、超时、cwd 与输出解析；判定（judge）与就地转存
//       （_promote_local）在本进程里做，与 Python 同构。

#![allow(dead_code)]

use super::article;
use super::jobs;
use super::py;
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

const TIMEOUT: u64 = 600;
const MAX_TEXT: usize = 8000;
const KEEP_JOBS: usize = 60;
const DUP_WINDOW: f64 = 180.0;

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

pub fn archive_root(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("04-存图")
}

fn now_f() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `linkbox._url_key`：只 trim/去尾斜杠/小写（**不**像 article 那样剥协议和查询串）。
fn url_key_loose(u: &str) -> String {
    u.trim().trim_end_matches('/').to_lowercase()
}

/// `_meta_probe`：图集目录 meta.json 里的正文字数、版面、标题。
fn meta_probe(d: &Path) -> (i64, String, String) {
    let Some(meta) = jsonio::read_json(&d.join("meta.json")) else {
        return (0, String::new(), String::new());
    };
    let mut n = 0i64;
    for k in ["copy", "plain_text", "desc"] {
        let s = py::s_of(meta.get(k)).trim().to_string();
        if !s.is_empty() {
            n = s.chars().count() as i64;
            break;
        }
    }
    (
        n,
        py::s_of(meta.get("layout")),
        py::s_of(meta.get("title")).trim().to_string(),
    )
}

fn trash_dir(ctx: &Ctx, d: &Path) {
    if !d.exists() {
        return;
    }
    let stamp = py::local_str("%Y%m%d-%H%M%S");
    let dest = archive_root(ctx)
        .join("_回收站")
        .join(stamp)
        .join(d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::rename(d, &dest).or_else(|_| -> std::io::Result<()> {
        // 跨卷时 shutil.move 会退化成「复制 + 删除」；rename 失败就用复制
        copy_dir_all(d, &dest)?;
        let _ = std::fs::remove_dir_all(d);
        Ok(())
    });
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let p = e.path();
        let t = dst.join(e.file_name());
        if p.is_dir() {
            copy_dir_all(&p, &t)?;
        } else if p.is_symlink() {
            let _ = std::fs::copy(std::fs::canonicalize(&p).unwrap_or(p.clone()), &t);
        } else {
            std::fs::copy(&p, &t)?;
        }
    }
    Ok(())
}

/// `_trash_by_url`：同一链接以前存成图集的，一并挪走。
fn trash_by_url(ctx: &Ctx, url: &str, keep: &Path) -> Vec<String> {
    let key = url_key_loose(url);
    if key.is_empty() {
        return Vec::new();
    }
    let mut moved = Vec::new();
    let root = archive_root(ctx);
    let Ok(rd) = std::fs::read_dir(&root) else { return moved };
    for e in rd.flatten() {
        let dir = e.path();
        if !dir.is_dir() {
            continue;
        }
        let mp = dir.join("meta.json");
        if !mp.is_file() {
            continue;
        }
        let dname = dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if dir == keep || dname.starts_with('_') || dname.starts_with('.') {
            continue;
        }
        let Some(meta) = jsonio::read_json(&mp) else { continue };
        if url_key_loose(&py::s_of(meta.get("url"))) != key {
            continue;
        }
        trash_dir(ctx, &dir);
        moved.push(dname);
    }
    moved
}

// ---------------------------------------------------------------- 后台任务

fn do_job(ctx: Arc<Ctx>, jid: String, text: String, hint: String) {
    let store = jobs::links();
    // `with self._sem:` 在 `_do` 之前：拿不到槽位就停在 queued
    let _slot = store.sem().acquire();
    store.set(&jid, &[("state", json!("running"))], jobs::LINK_TERMINAL);
    let pybin = py::cuntu_py();
    let script = py::cuntu_dir().join("cuntu.py");
    if !pybin.exists() || !script.exists() {
        store.set(
            &jid,
            &[
                ("state", json!("error")),
                (
                    "message",
                    json!(format!(
                        "cuntu 未就绪：{} 或 .venv 不存在",
                        script.to_string_lossy()
                    )),
                ),
            ],
            jobs::LINK_TERMINAL,
        );
        return;
    }
    let argv = vec![
        pybin.to_string_lossy().to_string(),
        script.to_string_lossy().to_string(),
        "--json".to_string(),
        "--no-dedup".to_string(),
        "--root".to_string(),
        archive_root(&ctx).to_string_lossy().to_string(),
        text.clone(),
    ];
    let cuntu = py::cuntu_dir();
    let Some((_code, out, err)) = py::run_cmd(&argv, TIMEOUT, Some(&cuntu)) else {
        store.set(
            &jid,
            &[
                ("state", json!("error")),
                ("message", json!(format!("抓取超时（>{TIMEOUT} 秒）"))),
            ],
            jobs::LINK_TERMINAL,
        );
        return;
    };
    let data = parse_array(&out);
    if data.is_empty() {
        let detail = {
            let f = py::first_line(&err);
            if f.is_empty() { "cuntu 没有返回结果".to_string() } else { f }
        };
        store.set(
            &jid,
            &[("state", json!("error")), ("message", json!(py::trunc(&detail, 300)))],
            jobs::LINK_TERMINAL,
        );
        return;
    }
    let r = &data[0];
    let platform = py::s_of(r.get("platform"));
    let err_s = py::s_of(r.get("error"));
    if !py::s_of(r.get("url")).is_empty() {
        store.set(&jid, &[("url", r.get("url").cloned().unwrap_or(json!("")))], jobs::LINK_TERMINAL);
    }
    if r.get("stub").and_then(|v| v.as_bool()).unwrap_or(false) {
        let msg = err_s
            .split('\n')
            .next()
            .unwrap_or("")
            .replace("【占位】", "")
            .trim()
            .to_string();
        store.set(
            &jid,
            &[
                ("state", json!("unsupported")),
                ("platform", json!(platform)),
                ("message", json!(msg)),
            ],
            jobs::LINK_TERMINAL,
        );
        return;
    }
    if !r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        let msg = if err_s.is_empty() { "抓取失败".to_string() } else { err_s };
        store.set(
            &jid,
            &[
                ("state", json!("error")),
                ("platform", json!(platform)),
                ("message", json!(py::trunc(&msg, 300))),
            ],
            jobs::LINK_TERMINAL,
        );
        return;
    }
    let saved = r.get("saved").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let dir_name = py::s_of(r.get("dir"));
    let total = py::i64_of(r.get("bytes"));
    let n_video = saved
        .iter()
        .filter(|s| py::s_of(s.get("kind")) == "video")
        .count() as i64;

    if !dir_name.is_empty() && !saved.is_empty() {
        let (n_txt, layout, title) = meta_probe(&archive_root(&ctx).join(&dir_name));
        let layout2 = if layout.is_empty() { py::s_of(r.get("layout")) } else { layout };
        let hint_text = if hint.is_empty() { text.clone() } else { hint.clone() };
        let v = jobs::decide(
            n_txt,
            saved.len() as i64,
            &platform,
            &hint_text,
            &layout2,
            &title,
            n_video,
        );
        if v.ask {
            let e = jobs::pend_add(
                &ctx, "link", &jid, &py::s_of(r.get("url")), &title, &platform,
                &dir_name, n_txt, saved.len() as i64, n_video, v.mode, &v.why,
            );
            let qtext = jobs::pend_question(&e);
            store.set(
                &jid,
                &[
                    ("state", json!("ask")),
                    ("platform", json!(platform)),
                    ("dir_name", json!(dir_name)),
                    ("dir", json!(archive_root(&ctx).join(&dir_name).to_string_lossy())),
                    ("images", json!(saved.len())),
                    ("bytes", json!(total)),
                    ("size", json!(py::human_link(total as u64))),
                    ("routed", json!("")),
                    ("pid", e.get("id").cloned().unwrap_or(json!(""))),
                    ("ambiguous", json!(true)),
                    ("why", json!(v.why)),
                    ("title", json!(title)),
                    ("text_len", json!(n_txt)),
                    ("n_video", json!(n_video)),
                    ("message", json!(qtext)),
                ],
                jobs::LINK_TERMINAL,
            );
            return;
        }
        if v.mode == "article" {
            promote(&ctx, &jid, &py::s_of(r.get("url")), &dir_name, &platform, &v.why);
            return;
        }
    }

    let what = if n_video != 0 && (saved.len() as i64 - n_video) == 0 {
        format!("{n_video} 段视频")
    } else {
        format!("{} 张", saved.len())
    };
    let message = if saved.is_empty() {
        "没有新文件".to_string()
    } else {
        format!("{what}已存入 {dir_name}")
    };
    store.set(
        &jid,
        &[
            ("state", json!("done")),
            ("platform", json!(platform)),
            ("dir_name", json!(dir_name)),
            ("routed", json!("gallery")),
            (
                "dir",
                json!(if dir_name.is_empty() {
                    "".to_string()
                } else {
                    archive_root(&ctx).join(&dir_name).to_string_lossy().to_string()
                }),
            ),
            ("images", json!(saved.len())),
            ("n_video", json!(n_video)),
            ("bytes", json!(total)),
            ("size", json!(py::human_link(total as u64))),
            ("message", json!(message)),
        ],
        jobs::LINK_TERMINAL,
    );
}

fn parse_array(stdout: &str) -> Vec<Value> {
    let s = stdout.trim();
    if s.is_empty() {
        return Vec::new();
    }
    let (Some(i), Some(j)) = (s.find('['), s.rfind(']')) else {
        return Vec::new();
    };
    if j <= i {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(&s[i..=j]) {
        Ok(Value::Array(a)) => a,
        _ => Vec::new(),
    }
}

/// `_fetch_article`：用 cuntu 的 python 跑 `article.py fetch`。
fn fetch_article(ctx: &Ctx, url: &str) -> Value {
    let pybin = py::cuntu_py();
    if url.is_empty() || !pybin.exists() {
        return json!({"ok": false, "error": "缺少链接或 cuntu 未就绪"});
    }
    let script = ctx.app_dir.join("article.py");
    let argv = vec![
        pybin.to_string_lossy().to_string(),
        script.to_string_lossy().to_string(),
        "fetch".to_string(),
        url.to_string(),
    ];
    let Some((_code, out, err)) = py::run_cmd(&argv, TIMEOUT, Some(&ctx.app_dir)) else {
        return json!({"ok": false, "error": "转存文章失败：timeout"});
    };
    let out = out.trim().to_string();
    let Some(i) = out.find('{') else {
        let f = py::first_line(&err);
        return json!({"ok": false, "error": if f.is_empty() { "没有返回结果".to_string() } else { f }});
    };
    match serde_json::from_str::<Value>(&out[i..]) {
        Ok(v) => v,
        Err(_) => json!({"ok": false, "error": "返回内容不是 JSON"}),
    }
}

/// `_move_images`：图集的图挪进阅读条目目录；heic 用 sips 转 jpg。
fn move_images(src: &Path, dest: &Path) -> Vec<String> {
    if !src.is_dir() {
        return Vec::new();
    }
    let Ok(rd) = std::fs::read_dir(src) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && !p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(false))
        .collect();
    files.sort();
    if files.is_empty() {
        return Vec::new();
    }
    let _ = std::fs::create_dir_all(dest);
    let mut out = Vec::new();
    for p in files {
        let ext = p
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if ![".jpg", ".jpeg", ".png", ".gif", ".webp", ".heic", ".heif"].contains(&ext.as_str()) {
            continue;
        }
        let seq = out.len() + 1;
        if [".jpg", ".jpeg", ".png", ".gif", ".webp"].contains(&ext.as_str()) {
            let name = format!("{seq:02}.{ext}");
            if std::fs::copy(&p, dest.join(&name)).is_err() {
                continue;
            }
            out.push(format!("images/{name}"));
        } else {
            let name = format!("{seq:02}.jpg");
            let argv = vec![
                "/usr/bin/sips".to_string(),
                "-s".to_string(),
                "format".to_string(),
                "jpeg".to_string(),
                "-s".to_string(),
                "formatOptions".to_string(),
                "85".to_string(),
                p.to_string_lossy().to_string(),
                "--out".to_string(),
                dest.join(&name).to_string_lossy().to_string(),
            ];
            let _ = py::run_cmd(&argv, 60, None);
            if !dest.join(&name).exists() {
                continue;
            }
            out.push(format!("images/{name}"));
        }
    }
    out
}

/// `_promote_local`：用已抓到的图集目录直接落成阅读条目。返回 Some 表示收尾。
fn promote_local(
    ctx: &Ctx,
    jid: &str,
    url: &str,
    dir_name: &str,
    platform: &str,
    why: &str,
) -> Option<()> {
    let store = jobs::links();
    let gdir = archive_root(ctx).join(dir_name);
    let Some(meta) = jsonio::read_json(&gdir.join("meta.json")) else { return None };
    let copy = {
        let a = py::s_of(meta.get("copy"));
        let b = if a.is_empty() { py::s_of(meta.get("desc")) } else { a };
        b.trim().to_string()
    };
    if copy.chars().count() < 80 {
        return None;
    }
    let effective_url = if url.is_empty() { py::s_of(meta.get("url")) } else { url.to_string() };
    if read_hit(ctx, &effective_url).is_some() {
        trash_dir(ctx, &gdir);
        let moved = trash_by_url(ctx, &effective_url, &gdir);
        store.set(
            jid,
            &[
                ("state", json!("done")),
                ("platform", json!(platform)),
                ("routed", json!("article")),
                ("dir_name", json!("")),
                ("dir", json!("")),
                ("images", json!(0)),
                ("trashed", json!(moved)),
                (
                    "message",
                    json!(format!("这篇已经在阅读库里了（{why}），刚建的图集已撤掉")),
                ),
            ],
            jobs::LINK_TERMINAL,
        );
        return Some(());
    }
    let raw_title = {
        let t = py::s_of(meta.get("title"));
        if t.is_empty() {
            dir_name.split('_').last().unwrap_or("").to_string()
        } else {
            t
        }
    };
    let title = py::safe_name(&raw_title, 60);
    let read_uncat = ctx.library_dir.join("08-阅读").join(article::UNCATEGORIZED);
    let dest = py::unique_dir(&read_uncat, &title);
    if std::fs::create_dir_all(&dest).is_err() {
        return None;
    }
    let imgs = move_images(&gdir.join("images"), &dest.join("images"));
    let n_img = imgs.len();
    let mut body = copy.clone();
    if !imgs.is_empty() {
        body = format!(
            "{}\n\n{}",
            copy,
            imgs.iter().map(|r| format!("![]({r})")).collect::<Vec<_>>().join("\n\n")
        );
    }
    let stamp = py::now_cst();
    let author = py::s_of(meta.get("author"));
    let source = format!("{platform} · {author}").trim_end_matches([' ', '·']).to_string();
    let meta_out = json!({
        "schema": 2,
        "title": title,
        "platform": platform,
        "source": source,
        "url": effective_url,
        "canonical": py::s_of(meta.get("canonical")),
        "author": author,
        "published_at": py::s_of(meta.get("published_at")),
        "topic": article::UNCATEGORIZED,
        "saved_at": stamp,
        "chars": copy.chars().count(),
        "source_text": effective_url,
        "tags": [],
        "origin": meta.get("origin").cloned().unwrap_or(json!({})),
        "milestones": {"submitted_at": stamp, "fetched_at": stamp, "archived_at": stamp},
        "duration_sec": 0,
        "inline_images": n_img,
        "needs_ocr": false,
        "ocr_status": "skipped",
        "images": n_img,
        "note": "由存图就地转存（未重抓网页）",
    });
    if std::fs::write(dest.join("正文.md"), &body).is_err() {
        return None;
    }
    if jsonio::atomic_write(&dest.join("article.json"), &jsonio::dumps(&meta_out, 1)).is_err() {
        return None;
    }
    // 4B 分类失败也只是「未分类」，不影响东西存下来
    let _ = article::classify_dir(ctx, &dest, None);
    trash_dir(ctx, &gdir);
    let moved = trash_by_url(ctx, &effective_url, &gdir);
    // 分类可能把目录挪走，位置按实际落点算
    let parent = dest
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let rel = format!("{parent}/{}", dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    let chars = copy.chars().count() as i64;
    store.set(
        jid,
        &[
            ("state", json!("done")),
            ("platform", json!(platform)),
            ("routed", json!("article")),
            ("dir_name", json!(rel)),
            ("dir", json!("")),
            ("images", json!(0)),
            ("chars", json!(chars)),
            ("size", json!(format!("{chars} 字"))),
            ("trashed", json!(moved)),
            (
                "message",
                json!(format!("这是一篇文章（{why}），已存入阅读《{title}》，{chars} 字")),
            ),
        ],
        jobs::LINK_TERMINAL,
    );
    Some(())
}

/// `article.read_hit`：这条链接是不是已经在 08-阅读 里了（取正文最长的一份）。
fn read_hit(ctx: &Ctx, url: &str) -> Option<Value> {
    let key = url_norm_key(url);
    if key.is_empty() {
        return None;
    }
    let root = ctx.library_dir.join("08-阅读");
    let Ok(rd) = std::fs::read_dir(&root) else { return None };
    let mut best: Option<Value> = None;
    let mut best_chars = i64::MIN;
    for e in rd.flatten() {
        let td = e.path();
        let tn = td.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if !td.is_dir() || tn.starts_with('_') || tn.starts_with('.') {
            continue;
        }
        let Ok(rd2) = std::fs::read_dir(&td) else { continue };
        for e2 in rd2.flatten() {
            let d = e2.path();
            if !d.is_dir() {
                continue;
            }
            let meta = article::article_meta(&d);
            let k1 = url_norm_key(&py::s_of(meta.get("url")));
            let k2 = url_norm_key(&py::s_of(meta.get("canonical")));
            if key != k1 && key != k2 {
                continue;
            }
            let chars = py::i64_of(meta.get("chars"));
            let dname = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if best.is_none() || chars > best_chars {
                best_chars = chars;
                let title = {
                    let t = py::s_of(meta.get("title"));
                    if t.is_empty() { dname.clone() } else { t }
                };
                best = Some(json!({"rel": format!("{tn}/{dname}"), "chars": chars, "title": title}));
            }
        }
    }
    best
}

/// `article._url_key`：剥协议、去查询串与尾斜杠、小写。
fn url_norm_key(u: &str) -> String {
    let mut s = u.trim().to_string();
    let low = s.to_lowercase();
    if low.starts_with("http://") {
        s = s[7..].to_string();
    } else if low.starts_with("https://") {
        s = s[8..].to_string();
    }
    if let Some(i) = s.find(['?', '#']) {
        s.truncate(i);
    }
    s.trim_end_matches('/').to_lowercase()
}

/// `_promote`：判成文章 → 本地转存，办不了再重抓。
fn promote(ctx: &Ctx, jid: &str, url: &str, dir_name: &str, platform: &str, why: &str) {
    if promote_local(ctx, jid, url, dir_name, platform, why).is_some() {
        return;
    }
    let store = jobs::links();
    let r = fetch_article(ctx, url);
    let dup = py::s_of(r.get("kind")) == "dup";
    let ok = r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok && !dup {
        store.set(
            jid,
            &[
                ("state", json!("done")),
                ("platform", json!(platform)),
                ("dir_name", json!(dir_name)),
                ("routed", json!("gallery")),
                ("dir", json!(archive_root(ctx).join(dir_name).to_string_lossy())),
                (
                    "message",
                    json!(format!(
                        "已存入 {dir_name}（判成文章但转存失败：{}）",
                        py::trunc(&py::s_of(r.get("error")), 60)
                    )),
                ),
            ],
            jobs::LINK_TERMINAL,
        );
        return;
    }
    if dup {
        trash_dir(ctx, &archive_root(ctx).join(dir_name));
        let moved = trash_by_url(ctx, url, &archive_root(ctx).join(dir_name));
        store.set(
            jid,
            &[
                ("state", json!("done")),
                ("platform", json!(platform)),
                ("routed", json!("article")),
                ("dir_name", json!("")),
                ("dir", json!("")),
                ("images", json!(0)),
                ("trashed", json!(moved)),
                ("message", json!(format!("这篇已经在阅读库里了（{why}），刚建的图集已撤掉"))),
            ],
            jobs::LINK_TERMINAL,
        );
        return;
    }
    trash_dir(ctx, &archive_root(ctx).join(dir_name));
    let moved = trash_by_url(ctx, url, &archive_root(ctx).join(dir_name));
    let title = py::s_of(r.get("title"));
    let chars = py::i64_of(r.get("chars"));
    let rel = {
        let r2 = py::s_of(r.get("rel"));
        if r2.is_empty() { title.clone() } else { r2 }
    };
    store.set(
        jid,
        &[
            ("state", json!("done")),
            ("platform", json!(platform)),
            ("routed", json!("article")),
            ("dir_name", json!(rel)),
            ("dir", json!("")),
            ("images", json!(0)),
            ("chars", json!(chars)),
            ("size", json!(format!("{chars} 字"))),
            ("trashed", json!(moved)),
            (
                "message",
                json!(format!("这是一篇文章（{why}），已存入阅读《{title}》，{chars} 字")),
            ),
        ],
        jobs::LINK_TERMINAL,
    );
}

/// `LinkBox.start`：返回任务快照（或 `{"error": ...}`）。
pub fn start(ctx: &Arc<Ctx>, text: &str, hint: &str) -> Value {
    let store = jobs::links();
    let text: String = text.trim().chars().take(MAX_TEXT).collect();
    if text.is_empty() {
        return json!({"error": "缺少链接"});
    }
    let key = py::url_key(&text);
    if !key.is_empty() {
        let now = now_f();
        for old in store.snapshot_all() {
            if py::s_of(old.get("url_key")) != key {
                continue;
            }
            let st = py::s_of(old.get("state"));
            if ["queued", "running", "ask"].contains(&st.as_str())
                || now - py::f64_of(old.get("finished")) < DUP_WINDOW
            {
                let mut snap = old.clone();
                snap.insert("dup".into(), json!(true));
                return Value::Object(snap);
            }
        }
    }
    let jid = py::rand_hex(12);
    let hint_v = if hint.is_empty() { text.clone() } else { hint.to_string() };
    let mut job = Map::new();
    job.insert("id".into(), json!(jid));
    job.insert("state".into(), json!("queued"));
    job.insert("platform".into(), Value::Null);
    job.insert("dir_name".into(), json!(""));
    job.insert("dir".into(), json!(""));
    job.insert("images".into(), json!(0));
    job.insert("bytes".into(), json!(0));
    job.insert("size".into(), json!(""));
    job.insert("message".into(), json!(""));
    job.insert("text".into(), json!(py::trunc(&text, 160)));
    job.insert("started".into(), json!(now_f()));
    job.insert("hint".into(), json!(py::trunc(&hint_v, 300)));
    job.insert("url".into(), json!(key));
    job.insert("url_key".into(), json!(key));
    job.insert("finished".into(), json!(0.0));
    job.insert("routed".into(), json!(""));
    store.insert(&jid, job);

    let ctx2 = ctx.clone();
    let jid2 = jid.clone();
    let text2 = text.clone();
    let hint2 = hint.to_string();
    std::thread::spawn(move || do_job(ctx2, jid2, text2, hint2));
    store.settle(&jid);
    store.get(&jid).map(Value::Object).unwrap_or_else(|| json!({"error": "任务创建失败"}))
}

// ---------------------------------------------------------------- 路由

pub async fn create(
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
        let mut jobs_out: Vec<Value> = Vec::new();
        for u in urls {
            let hint2 = if hint.is_empty() { text.clone() } else { hint.clone() };
            let j = start(&ctx, &u, &hint2);
            // Python 是 `if not j.get("error")`：空串也算没有错
            if py::s_of(j.get("error")).is_empty()
                && !j.get("dup").and_then(|v| v.as_bool()).unwrap_or(false)
            {
                crate::storage::log(
                    &ctx,
                    json!({"event": "link_start", "id": j.get("id"),
                           "head": py::trunc(&u, 80), "url": u}),
                );
            }
            jobs_out.push(j);
        }
        return Json(json!({"multi": jobs_out.len(), "jobs": jobs_out})).into_response();
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
        json!({"event": "link_start", "id": job.get("id"),
               "head": py::trunc(&text, 80), "url": job.get("url")}),
    );
    Json(job).into_response()
}

pub async fn status(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let map = crate::routes::common::query_map(&uri);
    let Some((_, id)) = map.into_iter().find(|(k, _)| k == "id") else {
        return missing_query("id");
    };
    match jobs::links().get(&id) {
        Some(j) => Json(Value::Object(j)).into_response(),
        None => http_err(404, "任务不存在或已过期"),
    }
}
