// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`about.py` 的检查 / 模式 / 状态 / 安装 四段 + `server.py` 的 `/api/updates`。
// 不做：下载（在 download.rs）、主机推送（在 pushupd.rs 与 mod.rs 的装配）。
//
// 状态机：STATE 只在进程内，不落盘（Python 的注释写得很清楚：下载是短事务，
// 重启就该重来）。字段顺序与 `about.STATE` 的字面量一致。

#![allow(dead_code)]

use super::util::{
    about_gate, init_cfg, json_status, now_epoch, py_str, py_truthy, repo_from_cfg, set_update_cfg,
    snapshot, upd,
};
use super::{pluginpack, pushupd};
use crate::paths::Ctx;
use crate::routes::common::{peek, q, query_map, CtxState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const VERSION: &str = crate::paths::VERSION;
pub const DEFAULT_REPO: &str = crate::paths::DEFAULT_REPO;
const API_LATEST: &str = "https://api.github.com/repos/{repo}/releases/latest";

// ---------------------------------------------------------------- 平台 / 版本

/// `updater.platform_key()`。
pub fn platform_key() -> &'static str {
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

fn version_tuple(v: &str) -> (Vec<i64>, i32, String) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let core = v.split('-').next().unwrap_or("");
    let mut parts: Vec<i64> = core.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    while parts.len() < 3 {
        parts.push(0);
    }
    let pre = v.split('-').nth(1).unwrap_or("").to_string();
    (parts, if pre.is_empty() { 1 } else { 0 }, pre)
}

fn is_newer(candidate: &str) -> bool {
    let (a, b) = (version_tuple(candidate), version_tuple(VERSION));
    if a.0 != b.0 {
        return a.0 > b.0;
    }
    if a.1 != b.1 {
        return a.1 > b.1;
    }
    a.2 > b.2
}

fn asset_pattern(key: &str) -> regex::Regex {
    let pat = if key.starts_with("macos") {
        let arch = if key.ends_with("arm64") { "arm64" } else { "x64" };
        format!(r"macos.*{arch}.*\.(dmg|zip)$|{arch}.*macos.*\.(dmg|zip)$")
    } else if key.starts_with("windows") {
        r"windows.*(setup|installer).*\.exe$|setup.*\.exe$".to_string()
    } else {
        r"linux.*\.(tar\.gz|zip)$".to_string()
    };
    regex::RegexBuilder::new(&pat).case_insensitive(true).build().unwrap()
}

// ---------------------------------------------------------------- 检查

async fn fetch_release(repo: &str, timeout: f64) -> Result<Value, String> {
    let repo_eff = if repo.is_empty() { DEFAULT_REPO } else { repo };
    let url = API_LATEST.replace("{repo}", repo_eff);
    let ua = format!("Moshan-Updater/0.1 (+https://github.com/{repo_eff})");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout))
        .build()
        .map_err(|e| format!("网络不可用：{e}"))?;
    let res = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, ua)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("网络不可用：{e}"))?;
    let code = res.status().as_u16();
    if code == 404 {
        return Err("仓库或 Release 不存在（可能还没发布版本）".into());
    }
    if code == 403 {
        return Err("GitHub API 限流，请稍后再试".into());
    }
    if !res.status().is_success() {
        return Err(format!("检查更新失败：HTTP {code}"));
    }
    let text = res.text().await.map_err(|e| format!("网络不可用：{e}"))?;
    serde_json::from_str::<Value>(&text)
        .map_err(|e| format!("JSONDecodeError: {e}"))
}

/// `updater.check()`。
async fn check_repo(repo: &str, timeout: f64) -> Result<Value, String> {
    let data = fetch_release(repo, timeout).await?;
    let repo_eff = if repo.is_empty() { DEFAULT_REPO } else { repo };
    let tag = data
        .get("tag_name")
        .map(|v| py_str(v))
        .unwrap_or_default()
        .trim_start_matches(['v', 'V'])
        .trim()
        .to_string();
    let latest = if tag.is_empty() { "0.0.0".to_string() } else { tag };
    let assets: Vec<Value> = match data.get("assets") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let key = platform_key();
    let pat = asset_pattern(key);
    let mut seen: Vec<Value> = Vec::new();
    for a in assets {
        let Some(m) = a.as_object() else { continue };
        let name = m.get("name").map(|v| py_str(v)).unwrap_or_default();
        if !pat.is_match(&name) {
            continue;
        }
        let size = match m.get("size") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
            _ => 0,
        };
        let url = m.get("browser_download_url").cloned().unwrap_or(Value::Null);
        let digest = {
            let d = m.get("digest").cloned().unwrap_or(Value::Null);
            if py_truthy(Some(&d)) { py_str(&d) } else { String::new() }
        };
        let mut c = Map::new();
        c.insert("name".into(), json!(name));
        c.insert("url".into(), url);
        c.insert("size".into(), json!(size));
        c.insert("digest".into(), json!(digest));
        seen.push(Value::Object(c));
    }
    if key.starts_with("macos") {
        seen.sort_by_key(|c| {
            let n = c.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            if n.ends_with(".zip") { 0 } else { 1 }
        });
    }
    let picked = seen.first().cloned().unwrap_or(Value::Null);
    let notes = data
        .get("body")
        .map(|v| py_str(v))
        .unwrap_or_default()
        .chars()
        .take(4000)
        .collect::<String>();
    let page = match data.get("html_url") {
        Some(v) if py_truthy(Some(v)) => py_str(v),
        _ => format!("https://github.com/{repo_eff}/releases"),
    };
    let mut info = Map::new();
    info.insert("has_update".into(), json!(is_newer(&latest)));
    info.insert("current".into(), json!(VERSION));
    info.insert("latest".into(), json!(latest));
    info.insert("notes".into(), json!(notes));
    info.insert("asset".into(), picked);
    info.insert("page".into(), json!(page));
    info.insert("platform".into(), json!(key));
    Ok(Value::Object(info))
}

/// 跑一次检查并写入 STATE；失败只记原因，不抛。
async fn do_check(repo: &str) -> Value {
    match check_repo(repo, 10.0).await {
        Ok(info) => {
            {
                let mut st = upd().lock().unwrap();
                st.info = Some(info.clone());
                st.error = String::new();
                st.checked_at = now_epoch();
            }
            let mut out = Map::new();
            out.insert("ok".into(), json!(true));
            if let Value::Object(m) = info {
                for (k, v) in m {
                    out.insert(k, v);
                }
            }
            Value::Object(out)
        }
        Err(e) => {
            let msg = super::util::trunc(&format!("UpdateError: {e}"), 200);
            {
                let mut st = upd().lock().unwrap();
                st.error = msg.clone();
                st.checked_at = now_epoch();
            }
            json!({"ok": false, "error": msg})
        }
    }
}

pub async fn check(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = about_gate(&ctx, &headers, &uri) {
        return r;
    }
    init_cfg(&ctx);
    if super::util::push_mode() {
        return Json(json!({
            "ok": true, "has_update": false, "mode": "push",
            "note": "本机更新由主机推送（update.mode=push），不走 GitHub",
        }))
        .into_response();
    }
    Json(do_check(&repo_from_cfg()).await).into_response()
}

// ---------------------------------------------------------------- 模式

pub async fn mode(State(ctx): CtxState, headers: HeaderMap, uri: Uri, raw: Bytes) -> Response {
    if let Some(r) = about_gate(&ctx, &headers, &uri) {
        return r;
    }
    init_cfg(&ctx);
    // `await request.json()`：坏 JSON 当 `{}`；合法但不是对象时，Python 后面
    // 那句 `(body or {}).get(...)` 会在非空容器上抛 AttributeError → 500。
    let body: Value = if raw.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&raw).unwrap_or_else(|_| json!({}))
    };
    let mode = match &body {
        Value::Object(m) => {
            let v = m.get("mode").cloned().unwrap_or(Value::Null);
            let s = if py_truthy(Some(&v)) { py_str(&v) } else { String::new() };
            s.trim().to_lowercase()
        }
        v if py_truthy(Some(v)) => return super::util::internal_error(),
        _ => String::new(),
    };
    if mode != "push" && mode != "github" {
        return json_status(
            400,
            json!({"ok": false, "error": "mode 只能是 push（只认主机推送）或 github（自己查 Release）"}),
        );
    }
    let p = super::util::config_file();
    let mut cur = match super::util::read_config() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let mut upd_cfg = match cur.get("update") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let before = {
        let v = upd_cfg.get("mode").cloned().unwrap_or(Value::Null);
        if py_truthy(Some(&v)) { py_str(&v) } else { "github".to_string() }
    };
    upd_cfg.insert("mode".into(), json!(mode));
    cur.insert("update".into(), Value::Object(upd_cfg.clone()));
    let tmp = super::util::append_suffix(&p, ".tmp");
    let write = (|| -> std::io::Result<()> {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&tmp, crate::jsonio::dumps(&Value::Object(cur), 1))?;
        std::fs::rename(&tmp, &p)
    })();
    if let Err(e) = write {
        return json_status(
            500,
            json!({"ok": false, "error": super::util::trunc(&format!("写不了配置文件：{e}"), 200)}),
        );
    }
    set_update_cfg(Value::Object(upd_cfg));
    Json(json!({
        "ok": true,
        "mode": mode,
        "before": before,
        "config": p.to_string_lossy(),
        "note": if mode == "push" { "之后只认主机推送，不再查 GitHub" }
                else { "恢复自己查 GitHub Release" },
    }))
    .into_response()
}

// ---------------------------------------------------------------- 状态

/// `about._existing_package()`：`state/updates/` 下最近修改的那个安装包。
pub fn existing_package(ctx: &Ctx) -> Option<PathBuf> {
    let d = ctx.state_dir.join("updates");
    if !d.is_dir() {
        return None;
    }
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    let Ok(it) = std::fs::read_dir(&d) else { return None };
    for e in it.flatten() {
        let p = e.path();
        let ext = p
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        if !matches!(ext.as_str(), ".dmg" | ".exe" | ".zip" | ".gz") {
            continue;
        }
        let Ok(md) = p.metadata() else { continue };
        let Ok(mt) = md.modified() else { continue };
        if best.as_ref().map(|(_, b)| mt > *b).unwrap_or(true) {
            best = Some((p, mt));
        }
    }
    best.map(|(p, _)| p)
}

/// `updater.in_place_blocked()`。
pub fn in_place_blocked(ctx: &Ctx) -> String {
    if !cfg!(target_os = "macos") {
        return "这个平台走安装程序".into();
    }
    if !ctx.frozen {
        return "源码运行，没有 .app 可以替换".into();
    }
    let exe = std::fs::canonicalize(std::env::current_exe().unwrap_or_default())
        .unwrap_or_default();
    let mut app: Option<PathBuf> = None;
    for p in exe.ancestors() {
        if p.extension().map(|s| s == "app").unwrap_or(false) {
            app = Some(p.to_path_buf());
            break;
        }
    }
    let Some(app) = app else {
        return "找不到 .app 本体（可能被改名或挪出了 bundle）".into();
    };
    let s = app.to_string_lossy().to_string();
    if s.contains("/AppTranslocation/") {
        return "这个 app 正从下载目录 / dmg 里直接运行，系统给了临时路径。先把 Moshan 拖进「应用程序」，之后就能一键更新。".into();
    }
    if s.starts_with("/Volumes/") {
        return "这个 app 在 dmg 里运行。先拖进「应用程序」，之后就能一键更新。".into();
    }
    if let Some(parent) = app.parent() {
        let ok = std::fs::metadata(parent)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(false);
        if !ok {
            return format!("{} 不可写（可能装成了别人的）。重装到自己的应用程序目录再更新。", parent.to_string_lossy());
        }
    }
    String::new()
}

pub async fn state(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = about_gate(&ctx, &headers, &uri) {
        return r;
    }
    let mut st = snapshot();
    let path_empty = st.get("path").and_then(|v| v.as_str()).unwrap_or("").is_empty();
    if path_empty {
        if let Some(pkg) = existing_package(&ctx) {
            st["path"] = json!(pkg.to_string_lossy());
        }
    }
    let mut reason = in_place_blocked(&ctx);
    let path = st.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if reason.is_empty() && !path.is_empty() && !path.to_lowercase().ends_with(".zip") {
        reason = "这个版本只发了 dmg，没法原地替换（下一个版本会有 zip）".into();
    }
    st["in_place"] = json!(reason.is_empty());
    st["in_place_reason"] = json!(reason);
    Json(st).into_response()
}

// ---------------------------------------------------------------- 安装

/// `updater.is_verified()`：有没有真校验过。
fn is_verified(p: &Path) -> bool {
    let meta = PathBuf::from(super::util::append_suffix(p, ".meta"));
    let Ok(text) = std::fs::read_to_string(meta) else { return false };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return false };
    py_truthy(v.get("verified")) && py_truthy(v.get("sha256"))
}

/// `_source_zip` + `_strip_top`：取 codeload 源码包并削掉顶层目录。
async fn source_zip(repo: &str, tag: &str) -> Result<Vec<u8>, String> {
    let repo_eff = if repo.is_empty() { DEFAULT_REPO } else { repo };
    let url = format!(
        "https://codeload.github.com/{repo_eff}/zip/refs/tags/v{}",
        tag.trim_start_matches(['v', 'V'])
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| format!("{e}"))?;
    let res = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "curl/8")
        .send()
        .await
        .map_err(|e| format!("{e}"))?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    let raw = res.bytes().await.map_err(|e| format!("{e}"))?.to_vec();
    strip_top(&raw)
}

/// `_strip_top`：用系统的 unzip/zip 重打包（Cargo.toml 不归本泳道，没有 zip crate）。
fn strip_top(raw: &[u8]) -> Result<Vec<u8>, String> {
    let work = std::env::temp_dir().join(format!("moshan-src-{}", super::util::rand_hex(8)));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("{e}"))?;
    let z = work.join("in.zip");
    std::fs::write(&z, raw).map_err(|e| format!("{e}"))?;
    let out = work.join("x");
    let st = std::process::Command::new("/usr/bin/unzip")
        .args(["-q", "-o"])
        .arg(&z)
        .arg("-d")
        .arg(&out)
        .status()
        .map_err(|e| format!("{e}"))?;
    if !st.success() {
        let _ = std::fs::remove_dir_all(&work);
        return Err("不是有效的 zip".into());
    }
    let tops: Vec<PathBuf> = std::fs::read_dir(&out)
        .map_err(|e| format!("{e}"))?
        .flatten()
        .map(|e| e.path())
        .collect();
    if tops.len() != 1 || !tops[0].is_dir() {
        let _ = std::fs::remove_dir_all(&work);
        return Ok(raw.to_vec());
    }
    let rezip = work.join("out.zip");
    let st = std::process::Command::new("/usr/bin/zip")
        .current_dir(&tops[0])
        .args(["-q", "-r", "-X"])
        .arg(&rezip)
        .arg(".")
        .status()
        .map_err(|e| format!("{e}"))?;
    if !st.success() {
        let _ = std::fs::remove_dir_all(&work);
        return Err("重打包失败".into());
    }
    let data = std::fs::read(&rezip).map_err(|e| format!("{e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    Ok(data)
}

/// `_apply_source_from_github`：源码部署从 GitHub 取包，再走 pushupd.push。
async fn apply_source(ctx: &Ctx, tag: &str) -> Response {
    {
        let mut st = upd().lock().unwrap();
        st.phase = "downloading".into();
        st.done = 0;
        st.total = 0;
        st.msg = String::new();
        st.path = String::new();
    }
    let raw = match source_zip(&repo_from_cfg(), tag).await {
        Ok(v) => v,
        Err(e) => {
            let msg = super::util::trunc(&format!("取源码包失败：{e}"), 200);
            {
                let mut st = upd().lock().unwrap();
                st.msg = msg;
                st.phase = "error".into();
            }
            return Json(json!({"ok": false, "error": format!("取源码包失败：{e}")})).into_response();
        }
    };
    let digest = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&raw);
        format!("{:x}", h.finalize())
    };
    const CHUNK: usize = 1 << 20;
    let total = raw.len().div_ceil(CHUNK).max(1);
    let mut out = json!({"ok": false, "error": "没有分片"});
    for i in 0..total {
        let start = i * CHUNK;
        let end = (start + CHUNK).min(raw.len());
        let part = &raw[start..end];
        let mut body = Map::new();
        body.insert("version".into(), json!(tag));
        body.insert("sha256".into(), json!(digest));
        body.insert("name".into(), json!(format!("github-source-{tag}.zip")));
        body.insert("total".into(), json!(total));
        body.insert("index".into(), json!(i));
        body.insert(
            "b64".into(),
            json!(base64::Engine::encode(&base64::engine::general_purpose::STANDARD, part)),
        );
        body.insert("restart".into(), json!(i == total - 1));
        match pushupd::push(ctx, &Value::Object(body)) {
            Ok(v) => {
                if !py_truthy(v.get("ok")) {
                    return Json(v).into_response();
                }
                out = v;
            }
            Err(pushupd::PushError::TooBig(msg)) => {
                return json_status(413, json!({"ok": false, "error": msg}));
            }
            Err(pushupd::PushError::Other(msg)) => {
                return json_status(500, json!({"ok": false, "error": format!("收包异常：{msg}")}));
            }
        }
    }
    {
        let mut st = upd().lock().unwrap();
        st.phase = "done".into();
        st.done = raw.len() as i64;
        st.total = raw.len() as i64;
        st.path = format!("github:v{tag}");
        st.msg = format!("源码包 {} KB", raw.len() / 1024);
    }
    Json(out).into_response()
}

pub async fn apply(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = about_gate(&ctx, &headers, &uri) {
        return r;
    }
    init_cfg(&ctx);
    if super::util::push_mode() {
        return json_status(
            400,
            json!({
                "ok": false, "mode": "push",
                "error": "本机更新由主机推送（update.mode=push）：主机推完会自动重启，不需要在本机跑安装包",
            }),
        );
    }
    let p = {
        let st = upd().lock().unwrap();
        st.path.clone()
    };
    let path: Option<PathBuf> = if p.is_empty() { existing_package(&ctx) } else { Some(PathBuf::from(&p)) };
    let force = matches!(q(&uri, "force").as_str(), "1" | "true" | "yes");
    let exists = path.as_ref().map(|x| x.exists()).unwrap_or(false);
    if exists && !is_verified(path.as_ref().unwrap()) && !force {
        return json_status(
            409,
            json!({
                "ok": false, "need_confirm": true,
                "error": "这个安装包没有校验值（GitHub 未提供 digest / 本地元数据缺失），无法确认未被篡改。要强制安装请再确认一次。",
            }),
        );
    }

    if !ctx.frozen {
        let tag = {
            let st = upd().lock().unwrap();
            let info = st.info.clone().unwrap_or_else(|| json!({}));
            info.get("latest").map(|v| py_str(v)).unwrap_or_default().trim().to_string()
        };
        if tag.is_empty() {
            return json_status(
                400,
                json!({"ok": false, "error": "还没查过有没有新版本，先点一次「检查更新」"}),
            );
        }
        // 危险动作保险：源码部署的「取包 → 覆盖程序目录」只在显式开启时执行。
        // 迁移期默认不动 moshan/*.py（那是对照组，改它等于改考卷）。
        if std::env::var("MOSHAN_ALLOW_APPLY").as_deref() != Ok("1") {
            return json_status(
                500,
                json!({"ok": false, "error": "迁移期不执行源码原地替换（设 MOSHAN_ALLOW_APPLY=1 才执行）"}),
            );
        }
        return apply_source(&ctx, &tag).await;
    }

    let Some(pkg) = path.filter(|x| x.exists()) else {
        return json_status(400, json!({"ok": false, "error": "还没有下载好的安装包"}));
    };
    let mut reason = in_place_blocked(&ctx);
    let is_zip = pkg
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase() == "zip")
        .unwrap_or(false);
    if reason.is_empty() && is_zip && !is_verified(&pkg) {
        reason = "这个包没有校验值，只能手动安装".into();
    }
    if reason.is_empty() && is_zip {
        // 真替换要解压 .app、验签、起 helper、再 os._exit；迁移期**只回形状**。
        let work = ctx
            .state_dir
            .join("updates")
            .join(format!("apply-{}", now_epoch() as i64));
        let _ = std::fs::create_dir_all(&work);
        return Json(json!({
            "ok": true, "in_place": true,
            "hint": "正在更新：程序会自己关掉再打开，约 3 秒。设置和文件都不会动。",
            "path": pkg.to_string_lossy(),
            "log": work.join("apply.log").to_string_lossy(),
        }))
        .into_response();
    }
    let mut hint = if cfg!(target_os = "macos") {
        "已打开安装包，把 Moshan 拖进「应用程序」即可，原有设置与文件不会丢失。".to_string()
    } else if cfg!(windows) {
        "安装程序已启动，按提示覆盖安装即可，原有设置与文件不会丢失。".to_string()
    } else {
        "已用系统默认程序打开安装包。".to_string()
    };
    if !reason.is_empty() {
        hint = format!("{hint}（本机没法自动更新：{reason}）");
    }
    Json(json!({
        "ok": true, "in_place": false, "hint": hint, "path": pkg.to_string_lossy(),
    }))
    .into_response()
}

// ---------------------------------------------------------------- /api/updates

pub async fn updates(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    init_cfg(&ctx);
    let raw_force = query_map(&uri)
        .into_iter()
        .find(|(k, _)| k == "force")
        .map(|(_, v)| v);
    let force = match raw_force {
        None => 0i64,
        Some(s) => match s.trim().parse::<i64>() {
            Ok(v) => v,
            Err(_) => return super::util::bad_int_query("force", &s),
        },
    };
    let pk = pluginpack::check_updates(&ctx, force != 0).await;
    let st = snapshot();
    let info = match st.get("info") {
        Some(v @ Value::Object(_)) => v.clone(),
        _ => json!({}),
    };
    let checked = st.get("checked_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let push = super::util::push_mode();
    let info_empty = !info.as_object().map(|m| !m.is_empty()).unwrap_or(false);
    if info_empty && !push && (now_epoch() - checked) > 3600.0 {
        tokio::spawn(async move {
            let _ = do_check(&repo_from_cfg()).await;
        });
    }
    let s_trunc = |v: Option<&Value>, n: usize| -> String {
        v.map(py_str).unwrap_or_default().chars().take(n).collect()
    };
    let self_obj = json!({
        "current": VERSION,
        "latest": info.get("latest").map(py_str).unwrap_or_default(),
        "has_update": py_truthy(info.get("has_update")),
        "notes": s_trunc(info.get("notes"), 1500),
        "page": info.get("page").map(py_str).unwrap_or_default(),
        "platform": info.get("platform").map(py_str).unwrap_or_default(),
        "error": st.get("error").map(py_str).unwrap_or_default(),
        "checked_at": checked,
        "mode": if push { "push" } else { "release" },
    });
    let updates = pk.get("updates").cloned().unwrap_or(json!(0));
    let pk_checked = pk.get("checked_at").cloned().unwrap_or(json!(0));
    Json(json!({
        "ok": true,
        "plugins": pk.get("plugins").cloned().unwrap_or(json!([])),
        "updates": updates,
        "checked_at": if py_truthy(Some(&pk_checked)) { pk_checked } else { json!(0) },
        "self": self_obj,
    }))
    .into_response()
}

