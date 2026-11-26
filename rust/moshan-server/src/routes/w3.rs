// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：W3 文件层写族（mkdir / rename / move / delete / 回收站 恢复与清空 / 分享 建与撤）。
// 不做：不碰 `/api/upload` 的 multipart、不碰壁纸与配置写。那几条各自需要
//       独立的地基（流式上传、appearance.sync 的 CSS 生成），混进这一波会让
//       "写文件"这件小事被两个大依赖拖住。
//
// 顺序敏感的地方都按 Python 的判定链照抄：**先比同名再比越界**，还是反过来，
// 决定返回 409 还是 403，而前端是按状态码分支的。

use crate::auth;
use crate::paths::Ctx;
use crate::storage;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

type CtxState = State<Arc<Ctx>>;

fn http_err(code: u16, msg: &str) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
        Json(json!({ "detail": msg })),
    )
        .into_response()
}

fn peek(ctx: &Ctx, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if auth::token_ok(ctx, headers, uri) {
        None
    } else {
        Some(auth::deny())
    }
}

/// 取 `payload["paths"]`：Python 里 `payload.get("paths") or []`，
/// 所以 null / 缺失 / 空数组的语义完全一样（都是"没有选中"）。
fn paths_of(body: &Value) -> Vec<String> {
    body.get("paths")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

fn safe_each(ctx: &Ctx, raw: &[String]) -> Result<Vec<PathBuf>, Response> {
    let mut out = Vec::new();
    for x in raw {
        match storage::safe_path(ctx, x, false) {
            Ok(p) => out.push(p),
            Err(e) => return Err(http_err(e.code, &e.msg)),
        }
    }
    Ok(out)
}

pub async fn mkdir(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let raw = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let parent = if raw.trim().is_empty() {
        ctx.inbox_dir()
    } else {
        match storage::safe_path(&ctx, raw, false) {
            Ok(p) => p,
            Err(e) => return http_err(e.code, &e.msg),
        }
    };
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    match storage::mkdir(&ctx, &parent, name) {
        Ok(v) => Json(v).into_response(),
        Err(e) => http_err(e.code, &e.msg),
    }
}

pub async fn rename(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let raw = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let p = match storage::safe_path(&ctx, raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("");
    match storage::rename(&ctx, &p, name) {
        Ok(item) => {
            storage::log(
                &ctx,
                json!({"event": "rename", "from": p.to_string_lossy(),
                       "to": item.get("path").and_then(|v| v.as_str()).unwrap_or("")}),
            );
            Json(item).into_response()
        }
        Err(e) => http_err(e.code, &e.msg),
    }
}

pub async fn move_(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let raw = paths_of(&body);
    let paths = match safe_each(&ctx, &raw) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if paths.is_empty() {
        return http_err(400, "没有选中文件");
    }
    let dest_raw = body.get("dest").and_then(|v| v.as_str()).unwrap_or("");
    let dest = match storage::safe_path(&ctx, dest_raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    let copy = body.get("copy").map(truthy).unwrap_or(false);
    let results = match storage::move_paths(&ctx, &paths, &dest, copy) {
        Ok(v) => v,
        Err(e) => return http_err(e.code, &e.msg),
    };
    let ok = results
        .iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    storage::log(
        &ctx,
        json!({"event": if copy { "copy" } else { "move" },
               "n": paths.len(), "ok": ok, "dest": dest.to_string_lossy()}),
    );
    Json(json!({
        "results": results,
        "ok": ok,
        "failed": results.len() - ok,
        "dest": dest.to_string_lossy(),
    }))
    .into_response()
}

pub async fn delete(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let raw = paths_of(&body);
    let paths = match safe_each(&ctx, &raw) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if paths.is_empty() {
        return http_err(400, "没有选中文件");
    }
    let results = match storage::delete_to_trash(&ctx, &paths) {
        Ok(v) => v,
        Err(e) => return http_err(e.code, &e.msg),
    };
    let ok = results
        .iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    storage::log(&ctx, json!({"event": "trash", "n": paths.len(), "ok": ok}));
    let mut reply = format!("已移入回收站 {ok} 项");
    if results.len() > ok {
        reply.push_str(&format!("，{} 项失败", results.len() - ok));
    }
    Json(json!({
        "results": results,
        "ok": ok,
        "failed": results.len() - ok,
        "reply": reply,
    }))
    .into_response()
}

pub async fn trash_restore(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let names: Vec<String> = body
        .get("names")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let out = storage::restore(&ctx, &names);
    let ok = out
        .iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    storage::log(&ctx, json!({"event": "restore", "n": out.len(), "ok": ok}));
    Json(json!({"results": out, "ok": ok, "reply": format!("已恢复 {ok} 项")})).into_response()
}

pub async fn trash_purge(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    // `names` 缺失或 null = 全部清空；给了数组就只清这些。
    let names: Option<Vec<String>> = body
        .get("names")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| s.to_string()).collect());
    let n = storage::purge(&ctx, names.as_deref());
    storage::log(&ctx, json!({"event": "purge", "n": n, "all": names.is_none()}));
    Json(json!({"ok": true, "purged": n, "reply": format!("已永久删除 {n} 项")})).into_response()
}

pub async fn share_create(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body.map(|j| j.0).unwrap_or(Value::Null);
    let raw = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let p = match storage::safe_path(&ctx, raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    // `hours or 24`：0 与缺失都当 24 小时。
    let hours = body
        .get("hours")
        .and_then(|v| v.as_f64())
        .filter(|h| *h != 0.0)
        .unwrap_or(24.0);
    match storage::share_create(&ctx, &p, hours) {
        Ok(mut rec) => {
            let sid = rec.get("sid").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let name = rec.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Some(o) = rec.as_object_mut() {
                o.insert("url".into(), json!(format!("/s/{sid}/{name}")));
            }
            storage::log(&ctx, json!({"event": "share", "path": p.to_string_lossy(), "sid": sid}));
            Json(rec).into_response()
        }
        Err(e) => http_err(e.code, &e.msg),
    }
}

pub async fn share_revoke(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    AxumPath(sid): AxumPath<String>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    Json(json!({"ok": storage::share_revoke(&ctx, &sid)})).into_response()
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `POST /api/upload`：多文件流式落盘。
///
/// 三个上限都必须真的生效，任何一个漏了都是「一个请求打满内存/磁盘」：
///   单文件 `max_upload_mb`、整批 `2×max_upload_mb`、以及**边写边数**（不是先收完再判）。
/// 失败的文件要删残件——半截文件留在收件箱里，用户会以为传成功了。
pub async fn upload(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<UploadQ>,
    mut mp: axum::extract::Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let day = if !q.dir.trim().is_empty() {
        match storage::safe_path(&ctx, &q.dir, false) {
            Ok(p) => {
                if !p.is_dir() {
                    return http_err(400, "目标不是目录");
                }
                p
            }
            Err(e) => return http_err(e.code, &e.msg),
        }
    } else {
        ctx.inbox_dir().join(chrono::Local::now().format("%Y-%m-%d").to_string())
    };
    if std::fs::create_dir_all(&day).is_err() {
        return http_err(500, "建不了目标目录");
    }

    let max_mb = ctx.max_upload_mb();
    let max_single = (max_mb * 1048576.0) as u64;
    let max_batch = max_single * 2;

    let mut saved: Vec<Value> = Vec::new();
    let mut failed: Vec<Value> = Vec::new();
    let mut batch: u64 = 0;

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return http_err(400, &format!("表单解析失败：{e}")),
        };
        // 只收带文件名的段：前端可能同时塞普通字段。
        let Some(name) = field.file_name().map(|s| s.to_string()) else { continue };
        let name = if name.is_empty() {
            format!("上传_{}", chrono::Local::now().timestamp())
        } else {
            name
        };
        let dest = storage::unique_path(&day, &name);
        let mut written: u64 = 0;
        let mut err: Option<String> = None;
        let mut file = match std::fs::File::create(&dest) {
            Ok(f) => f,
            Err(e) => {
                failed.push(json!({"name": name, "error": e.to_string().chars().take(200).collect::<String>()}));
                continue;
            }
        };
        {
            use std::io::Write;
            let mut field = field;
            loop {
                match field.chunk().await {
                    Ok(Some(chunk)) => {
                        written += chunk.len() as u64;
                        if written > max_single {
                            err = Some(format!("超过 {}MB 上限", max_mb));
                            break;
                        }
                        if batch + written > max_batch {
                            err = Some("这一批文件总量超过上限，剩下的文件请分批传".to_string());
                            break;
                        }
                        if let Err(e) = file.write_all(&chunk) {
                            err = Some(e.to_string().chars().take(200).collect::<String>());
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        err = Some(e.to_string().chars().take(200).collect::<String>());
                        break;
                    }
                }
            }
        }
        drop(file);
        if let Some(msg) = err {
            let _ = std::fs::remove_file(&dest);
            failed.push(json!({"name": name, "error": msg}));
            continue;
        }
        batch += written;
        let fname = dest
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        saved.push(json!({
            "name": fname,
            "path": dest.to_string_lossy(),
            "bytes": written,
            "category": storage::category(&fname),
            "size": storage::human(written as f64),
        }));
    }

    // 归档清单写进「日期目录」而不是「组目录」：组目录里只该有图，
    // 否则列表页第一行永远顶着个 index.jsonl。
    let rec_dir = if day.parent().and_then(|p| p.parent()) == Some(ctx.inbox_dir().as_path()) {
        day.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| day.clone())
    } else {
        day.clone()
    };
    if !saved.is_empty() {
        use std::io::Write;
        let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(rec_dir.join("index.jsonl"))
        {
            for s in &saved {
                let line = json!({
                    "ts": ts,
                    "name": s.get("name"),
                    "path": s.get("path"),
                    "bytes": s.get("bytes"),
                    "category": s.get("category"),
                    "source": "pocket_upload",
                });
                let _ = writeln!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
            }
        }
    }

    storage::log(
        &ctx,
        json!({"event": "upload", "saved": saved.len(), "failed": failed.len(), "ms": 0,
               "names": saved.iter().filter_map(|s| s.get("name").cloned()).collect::<Vec<_>>()}),
    );
    Json(json!({"saved": saved, "failed": failed, "dir": day.to_string_lossy()})).into_response()
}

#[derive(Deserialize)]
pub struct UploadQ {
    #[serde(default)]
    dir: String,
}
