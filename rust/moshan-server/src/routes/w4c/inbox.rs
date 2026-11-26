// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：收件箱的 3 条路由（开一组 / 起名 / 在 Finder 里打开）。
// 不做：不改上传逻辑（那是 W3）。这里只碰「目录」这一层。
//
// `/api/inbox/group` 的重名规则直接用 storage.unique_path（`组-120000-2`），
// `/api/inbox/name` 的改名规则用 naming.uniq（`名字 (2)`）—— 两套规则**不一样**，
// 各自照抄，别为了「统一」把其中一个改掉：对拍时 diff 的是目录列表。

#![allow(dead_code)]

use super::py;
use super::naming;
use crate::storage;
use crate::routes::common::{body_obj, http_err, peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};

/// `_NAME_LOCK`：起名要读图 + 问 4B，几秒到十几秒；同一时刻只允许一组。
fn name_lock() -> &'static Mutex<bool> {
    static L: OnceLock<Mutex<bool>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(false))
}

/// `POST /api/inbox/group`：在今天的日期目录下建 `组-HHMMSS/`。
pub async fn group(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body_obj(body);
    let day = ctx.inbox_dir().join(py::local_str("%Y-%m-%d"));
    if let Err(e) = std::fs::create_dir_all(&day) {
        return http_err(500, &py::trunc(&format!("建不了日期目录：{e}"), 120));
    }
    let want = py::s_of(body.get("name")).trim().to_string();
    let base = if want.is_empty() {
        format!("组-{}", py::local_str("%H%M%S"))
    } else {
        want
    };
    let mut target: Option<std::path::PathBuf> = None;
    for _ in 0..4 {
        let cand = storage::unique_path(&day, &base);
        match std::fs::create_dir(&cand) {
            Ok(_) => {
                target = Some(cand);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return http_err(500, &py::trunc(&format!("建不了组目录：{e}"), 120));
            }
        }
    }
    let Some(target) = target else {
        return http_err(500, "建不了组目录：一直撞名");
    };
    storage::log(&ctx, json!({"event": "inbox_group", "path": target.to_string_lossy()}));
    Json(json!({
        "name": target.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "path": target.to_string_lossy(),
        "day": day.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
    }))
    .into_response()
}

/// `POST /api/inbox/name`：读图 → 4B 概括 → 目录改名。
pub async fn name(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = body_obj(body);
    let raw = py::s_of(body.get("path"));
    let p = match storage::safe_path(&ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    if !p.is_dir() {
        return http_err(400, "不是目录");
    }
    let inbox = storage::py_resolve(&ctx.inbox_dir());
    let target = storage::py_resolve(&p);
    let inside = target != inbox && target.starts_with(&inbox);
    if !inside {
        return http_err(403, "只能给收件箱里的目录起名");
    }
    {
        let mut l = name_lock().lock().unwrap();
        if *l {
            return http_err(429, "上一组还在读图起名，等它跑完");
        }
        *l = true;
    }
    let force = match body.get("force") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => false,
    };
    let model = ctx.cfg.get("model").and_then(|v| v.as_str()).map(|s| s.to_string());
    let ctx2 = ctx.clone();
    let p2 = p.clone();
    let res = tokio::task::spawn_blocking(move || {
        names_guard(|| naming::name_folder(&p2, model.as_deref(), force))
    })
    .await;
    *name_lock().lock().unwrap() = false;
    let res = match res {
        Ok(v) => v,
        Err(e) => return http_err(500, &py::trunc(&format!("起名失败：{e}"), 120)),
    };
    storage::log(
        &ctx2,
        json!({"event": "inbox_name",
               "from": p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
               "to": res.get("name"),
               "ok": res.get("ok"), "renamed": res.get("renamed"),
               "images": res.get("images"), "chars": res.get("chars"),
               "ms": res.get("ms")}),
    );
    Json(res).into_response()
}

/// `name_folder` 在 Python 里「永不抛异常」；这里兜一层，别让 panic 穿过线程边界。
fn names_guard<F: FnOnce() -> Value>(f: F) -> Value {
    f()
}

/// `GET /api/inbox/open`：在 Finder 里打开收件箱。
/// 与 Python 的 `os.system('open "…"')` 同效：起 `open`、等它退出、忽略结果。
pub async fn open(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let dir = ctx.inbox_dir();
    let _ = std::process::Command::new("open")
        .arg(dir.to_string_lossy().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    Json(json!({"ok": true, "path": dir.to_string_lossy()})).into_response()
}
