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
use serde_json::{json, Map, Value};
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
    let folder = paths::workbench_dir(&ctx);
    let wall_dir = folder.join("wallpapers");

    // load()：只认 skin/accent/bg 三个键，其余忽略；文件读不出来就整份用默认值。
    let data: Map<String, Value> = std::fs::read_to_string(folder.join("appearance.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();

    const SKINS: [&str; 4] = ["classic", "g1", "g2", "g3"];
    let skin = data
        .get("skin")
        .and_then(|v| v.as_str())
        .filter(|s| SKINS.contains(s))
        .unwrap_or("classic");
    let accent = norm_hex(data.get("accent").and_then(|v| v.as_str()).unwrap_or(""))
        .unwrap_or_else(|| "#0a84ff".to_string());
    let bg_raw = data.get("bg").and_then(|v| v.as_str()).unwrap_or("");
    const BG_KEYS: [&str; 7] = ["dawn", "mist", "glass", "dusk", "moss", "graphite", "custom"];
    let bg = if bg_raw == "auto" || BG_KEYS.contains(&bg_raw) {
        bg_raw
    } else {
        "auto"
    };

    let has_wall = |key: &str| -> bool {
        if !BG_KEYS.contains(&key) {
            return false;
        }
        wall_dir.join(format!("{key}.jpg")).is_file()
    };

    let presets = [
        ("系统蓝", "#0a84ff"),
        ("靛紫", "#5451e8"),
        ("青", "#0d8f86"),
        ("绿", "#0f8f56"),
        ("琥珀", "#a86a04"),
        ("朱红", "#cf3a3a"),
        ("玫红", "#cc2e76"),
        ("石墨", "#3b4252"),
    ];
    let preset_bgs = [
        ("dawn", "晨光", "暖米金主光，拟物默认"),
        ("mist", "冷雾", "冷蓝紫雾，毛玻璃默认"),
        ("glass", "玻璃", "冷色折射，液态玻璃默认"),
        ("dusk", "暮色", "深靛紫，暗色模式最好看"),
        ("moss", "苔原", "灰绿米白，低饱和"),
        ("graphite", "石墨", "中性灰，最不抢戏"),
    ];

    let mut bgs: Vec<Value> = preset_bgs
        .iter()
        .map(|(k, n, d)| {
            json!({ "key": k, "name": n, "note": d, "has": has_wall(k), "ver": "" })
        })
        .collect();
    // custom 那张是替换式的（同名覆盖），带 mtime 做查询串才能破浏览器一天的缓存。
    let custom_ver = std::fs::metadata(wall_dir.join("custom.jpg"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("?v={}", d.as_secs()))
        .unwrap_or_default();
    bgs.push(json!({
        "key": "custom", "name": "自定义", "note": "自己传图，只留这一张",
        "has": has_wall("custom"), "ver": custom_ver
    }));

    let skin_bg = json!({ "classic": "", "g1": "dawn", "g2": "mist", "g3": "glass" });
    let eff_key = if bg == "auto" {
        match skin {
            "g1" => "dawn",
            "g2" => "mist",
            "g3" => "glass",
            _ => "",
        }
    } else {
        bg
    };
    let bg_eff = if !eff_key.is_empty() && has_wall(eff_key) {
        eff_key
    } else {
        ""
    };

    Json(json!({
        "skin": skin,
        "accent": accent,
        "bg": bg,
        "v": 2,
        "skins": [
            { "id": "classic", "name": "经典", "note": "工作台原本的靛紫 + 中性 slate" },
            { "id": "g1", "name": "拟物", "note": "Aqua / iOS 6：亚麻底、斜面高光、小圆角" },
            { "id": "g2", "name": "扁平毛玻璃", "note": "iOS 7–18：色块 + vibrancy，密度最高" },
            { "id": "g3", "name": "液态玻璃", "note": "iOS 26：镜面白边、大圆角、字重更粗" },
        ],
        "presets": presets.iter().map(|(n, h)| json!({"name": n, "hex": h})).collect::<Vec<_>>(),
        "bgs": bgs,
        "bg_eff": bg_eff,
        "skin_bg": skin_bg,
    }))
}

/// 强调色只接受 `#rgb` / `#rrggbb`，统一小写 —— 与 `appearance._norm_hex` 同一套规则。
fn norm_hex(s: &str) -> Option<String> {
    let t = s.trim();
    let hex = t.strip_prefix('#')?;
    if !(hex.len() == 3 || hex.len() == 6) || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", hex.to_lowercase()))
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
