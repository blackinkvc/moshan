// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：W2 文件层只读端点（files / browse / usage / trash / shares），响应逐字节对齐 Python。
// 不做：不写盘（除了缩略图缓存）。缩略图是这一层唯一的副作用，且只写
//       `~/.cache/pocket-thumbs`；缓存键与 Python 一致，两边共用同一份缓存。

use crate::auth;
use crate::paths::Ctx;
use crate::storage;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

type CtxState = State<Arc<Ctx>>;

/// FastAPI 的 `HTTPException(code, msg)` → `{"detail": msg}`。
fn http_err(code: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(json!({ "detail": msg })),
    )
        .into_response()
}

fn deny() -> Response {
    auth::deny()
}

#[derive(Deserialize)]
pub struct FilesQ {
    #[serde(default)]
    scope: String,
    #[serde(default)]
    q: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    300
}

/// 与 `Path.rglob("*")` 同一口径：**不进入符号链接目录**。
/// 数据根下到处是链接，进不进得去直接决定这个列表给出什么。
fn rglob_nofollow(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        // rglob 是 DFS 且「遇到目录就立刻钻进去」，深度优先要看原始顺序；
        // read_dir 的顺序就是 readdir 顺序，与 Python 的 scandir 同源。
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for e in rd.flatten() {
            let p = PathBuf::from(e.path());
            let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            let is_dir = std::fs::metadata(&p).map(|m| m.is_dir()).unwrap_or(false);
            if is_dir && !is_link {
                subdirs.push(p.clone());
            }
            out.push(p);
            if out.len() >= limit {
                break;
            }
        }
        for d in subdirs.into_iter().rev() {
            stack.push(d);
        }
    }
    out
}

pub async fn files(State(ctx): CtxState, headers: HeaderMap, uri: Uri, Query(q): Query<FilesQ>) -> Response {
    if !auth::token_ok(&ctx, &headers, &uri) {
        return deny();
    }
    let root = if q.scope != "archive" {
        ctx.inbox_dir()
    } else {
        ctx.allowed_roots()
            .first()
            .cloned()
            .unwrap_or_else(|| ctx.library_dir.clone())
            .join("04-归档")
    };
    if !root.exists() {
        return Json(json!({ "items": [], "root": root.to_string_lossy() })).into_response();
    }

    let ql = q.q.to_lowercase();
    // rglob 可能遍历很多文件，这里按 limit 提前截断——与 Python 的
    // `if len(items) >= limit: break` 等价（只数已收下的条目）。
    let mut items: Vec<serde_json::Value> = Vec::new();
    for p in rglob_nofollow(&root, q.limit * 4 + 64) {
        if items.len() >= q.limit {
            break;
        }
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        if name.starts_with('.') || name == "index.jsonl" {
            continue;
        }
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if !md.is_file() {
            continue;
        }
        if !q.q.is_empty() {
            let path_str = p.to_string_lossy().to_string();
            if !name.to_lowercase().contains(&ql) && !path_str.contains(&q.q) {
                continue;
            }
        }
        let size = md.len();
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let rel = p
            .strip_prefix(&root)
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_else(|_| name.clone());
        items.push(json!({
            "name": name,
            "path": p.to_string_lossy(),
            "rel": rel,
            "bytes": size,
            "size": storage::human(size as f64),
            "mtime": mtime,
            "category": storage::category(&name),
        }));
    }
    items.sort_by(|a, b| {
        let x = a.get("mtime").and_then(|v| v.as_i64()).unwrap_or(0);
        let y = b.get("mtime").and_then(|v| v.as_i64()).unwrap_or(0);
        y.cmp(&x)
    });
    let count = items.len();
    Json(json!({ "items": items, "root": root.to_string_lossy(), "count": count })).into_response()
}

#[derive(Deserialize)]
pub struct BrowseQ {
    #[serde(default)]
    path: String,
    #[serde(default)]
    q: String,
    #[serde(default = "default_sort")]
    sort: String,
    #[serde(default = "one")]
    inline: i64,
}

fn default_sort() -> String {
    "name".to_string()
}
fn one() -> i64 {
    1
}

/// Starlette 的 `RedirectResponse` 会对 URL 做 quote（safe 集合如下），
/// 直接拼原串会让带空格/中文的路径在浏览器里裂开。
fn quote_url(s: &str) -> String {
    const SAFE: &[u8] = b":/%#?=@[]!$&'()*+,;";
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || SAFE.contains(b) || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(*b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// 内联缩略图的并发闸：`sips` 是子进程，几十张图同时开会把机器压死。
static THUMB_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
/// 超过这个数就退回逐张 `/api/thumb`：响应体塞太大反而更慢（base64 还要涨 33%）。
const INLINE_THUMB_MAX: usize = 12;
const INLINE_THUMB_W: u32 = 200;

pub async fn browse(State(ctx): CtxState, headers: HeaderMap, uri: Uri, Query(q): Query<BrowseQ>) -> Response {
    if !auth::token_ok(&ctx, &headers, &uri) {
        return deny();
    }
    let p = if q.path.trim().is_empty() {
        ctx.inbox_dir()
    } else {
        match storage::safe_path(&ctx, &q.path, false) {
            Ok(p) => p,
            Err(e) => return http_err(e.code, &e.msg),
        }
    };
    if !p.exists() {
        return http_err(404, "目录不存在");
    }
    // 指向文件时跳到它所在目录，方便从预览里「在文件夹中显示」。
    if p.is_file() {
        let target = p.parent().map(|x| x.to_string_lossy().to_string()).unwrap_or_default();
        let loc = format!("/api/browse?path={}", quote_url(&target));
        return (
            StatusCode::TEMPORARY_REDIRECT,
            [(axum::http::header::LOCATION, loc)],
            "",
        )
            .into_response();
    }

    let mut out = match storage::list_dir(&ctx, &p, &q.q, &q.sort) {
        Ok(v) => v,
        Err(e) => return http_err(e.code, &e.msg),
    };

    // 走隧道时每张缩略图都要一次往返（中继下 0.7 秒/次），图不多的目录干脆
    // 把缩略图内联进这次响应。只在没搜索时做——搜索结果可能是四次下钻的散落文件。
    if q.inline != 0 && q.q.trim().is_empty() {
        if let Some(files) = out.get_mut("files").and_then(|v| v.as_array_mut()) {
            let imgs: Vec<usize> = files
                .iter()
                .enumerate()
                .filter(|(_, f)| f.get("kind").and_then(|k| k.as_str()) == Some("image"))
                .map(|(i, _)| i)
                .collect();
            if !imgs.is_empty() && imgs.len() <= INLINE_THUMB_MAX {
                for i in imgs {
                    let path = files[i]
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let permit = THUMB_SLOTS.acquire().await.ok();
                    let got = tokio::task::spawn_blocking(move || -> Option<String> {
                        let t = crate::imaging::thumb(Path::new(&path), INLINE_THUMB_W)?;
                        let bytes = std::fs::read(t).ok()?;
                        use base64::Engine;
                        Some(format!(
                            "data:image/jpeg;base64,{}",
                            base64::engine::general_purpose::STANDARD.encode(bytes)
                        ))
                    })
                    .await
                    .ok()
                    .flatten();
                    drop(permit);
                    if let Some(d) = got {
                        if let Some(o) = files[i].as_object_mut() {
                            o.insert("thumb".into(), json!(d));
                        }
                    }
                }
            }
        }
    }

    Json(out).into_response()
}

pub async fn usage(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if !auth::token_ok(&ctx, &headers, &uri) {
        return deny();
    }
    Json(storage::usage(&ctx.inbox_dir())).into_response()
}

pub async fn trash(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if !auth::token_ok(&ctx, &headers, &uri) {
        return deny();
    }
    let rows = storage::list_trash(&ctx);
    let total: u64 = rows
        .iter()
        .map(|r| r.get("bytes").and_then(|v| v.as_u64()).unwrap_or(0))
        .sum();
    let count = rows.len();
    Json(json!({
        "items": rows,
        "count": count,
        "bytes": total,
        "size": storage::human(total as f64),
        "days": ctx.trash_days(),
    }))
    .into_response()
}

pub async fn shares(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if !auth::token_ok(&ctx, &headers, &uri) {
        return deny();
    }
    let mut rows = storage::share_list(&ctx);
    for r in rows.iter_mut() {
        let sid = r.get("sid").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if let Some(o) = r.as_object_mut() {
            o.insert("url".into(), json!(format!("/s/{sid}/{name}")));
        }
    }
    let count = rows.len();
    Json(json!({ "items": rows, "count": count })).into_response()
}
