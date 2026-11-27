// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：W1（契约地基）的五个只读端点，响应逐字节对齐 Python 侧。
// 不做：不查网关、不扫进程、不读运行期缓存——这几条一旦掺进来，响应就随环境变化，
//       对拍基线不成立（`/api/status`、`/api/llm` 正是因为这一点被排除在首批之外）。

use crate::auth;
use crate::paths::{self, Ctx};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

type CtxState = State<Arc<Ctx>>;

/// `{"ok":true,"service":"pocket","inbox":...}` —— 桌面壳与 CI 冒烟都靠它判断"起来了"。
pub async fn ping(State(ctx): CtxState) -> Json<Value> {
    Json(json!({ "ok": true, "service": "pocket", "inbox": ctx.inbox() }))
}

pub async fn versions(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if !auth::token_ok(&ctx, &headers, &uri) {
        return auth::deny();
    }
    let raw: Value = std::fs::read_to_string(ctx.state_dir.join("versions.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|v: &Value| v.is_object())
        .unwrap_or_else(|| json!({}));

    let seen: Vec<Value> = raw
        .get("seen")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|x| x.is_object() && truthy(x.get("v")))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    let s = |k: &str| -> String {
        raw.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let first_at = raw.get("first_at").filter(|v| truthy(Some(v))).cloned().unwrap_or(json!(""));

    Json(json!({
        "ok": true,
        "first_at": first_at,
        "first_guess": truthy(raw.get("first_guess")),
        "first_basis": s("first_basis"),
        "first_version": s("first_version"),
        "last_at": raw.get("last_at").filter(|v| truthy(Some(v))).cloned().unwrap_or(json!("")),
        "last_version": s("last_version"),
        "current": paths::VERSION,
        "n_versions": seen.len(),
        "versions": seen,
    }))
    .into_response()
}

// W1 只收「响应与运行环境无关」的端点。
//
// 反面清单（都试过，对拍时露馅）：
//   /api/heartbeat  STATE 是进程内字典，Python 启动后立刻跑过一轮，`last_at` 已填。
//                   Rust 此刻没有自己的调度器，只能返回空状态——差异真实存在，
//                   所以它归 W5：等 Rust 接管心跳调度，再把状态一并迁过来。
//   /api/status     `GW.tools()` 实时探网关。
//   /api/sys        ctypes/psutil 采样，两次请求就不同。
//   /api/llm        llmwatch 扫本机端口。
// 判据不是 LOC 大小，而是「同一份数据环境下，两次请求是否给出同样的字节」。


pub async fn about(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if !auth::token_ok(&ctx, &headers, &uri) {
        // 注意：这条的拒绝形状与 auth() 不同（`error` 而非 `detail`），
        // 照搬 Python 的实现，别"统一"——前端的两个分支各读各的字段。
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "ok": false, "error": "需要访问令牌" })),
        )
            .into_response();
    }

    let mut app_dir = ctx.app_dir.display().to_string();
    if ctx.frozen {
        // 打包后真正的可执行文件在 .app/Contents/Frameworks 下，对人没意义，要给 .app 的路径。
        let p = ctx.app_dir.clone();
        if p.file_name().map(|n| n == "Frameworks").unwrap_or(false)
            && p.parent()
                .and_then(|x| x.file_name())
                .map(|n| n == "Contents")
                .unwrap_or(false)
        {
            if let Some(grand) = p.parent().and_then(|x| x.parent()) {
                app_dir = grand.display().to_string();
            }
        }
    }

    let repo = ctx
        .cfg
        .get("update")
        .and_then(|u| u.get("repo"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(paths::DEFAULT_REPO);

    Json(json!({
        "ok": true,
        "version": paths::VERSION,
        "build": "",
        "channel": "stable",
        "repo": repo,
        "platform": platform_key(),
        "frozen": ctx.frozen,
        "app_dir": app_dir,
        "state_dir": ctx.state_dir.display().to_string(),
        "library_dir": ctx.library_dir.display().to_string(),
        "config": paths::config_file().display().to_string(),
    }))
    .into_response()
}

pub async fn appearance(State(ctx): CtxState) -> Json<Value> {
    // 清单、默认值、壁纸 has/ver 只在 `appearance::state` 里维护一份：
    // 保存接口回的也是同一个结构，读和写不可能各长一套字段。
    Json(crate::routes::u::appearance::state(&ctx.cfg))
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// 与 `updater.platform_key()` 一致：命名要跟 CI 产出的资产对上。
fn platform_key() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "macos-arm64"
        } else {
            "macos-x64"
        }
    } else if cfg!(windows) {
        "windows-x64"
    } else {
        "linux-x64"
    }
}
