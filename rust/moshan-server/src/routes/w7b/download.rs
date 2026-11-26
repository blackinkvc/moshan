// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`POST /api/update/download` —— 从 GitHub Release 下载资产、边下边算 sha256、
//       校验、写 `.meta`、更新进度快照、清理旧包。
// 不做：不跑安装（apply 在 update.rs）。
//
// Python 的下载没有重试：`updater.download` 一次 urlopen，出错就删半截文件并
// 让 STATE 进 error。这里照抄（不加重试）。差异只有一处：urllib 的 timeout 是
// **每次 socket 读**的超时，reqwest 是整次请求的总超时 —— 大包慢网下 Rust 会
// 更早放弃。已知分叉，汇报里写清楚。

#![allow(dead_code)]

use super::util::{about_gate, upd};
use crate::routes::common::CtxState;
use crate::paths::Ctx;
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn truncated(msg: &str) -> String {
    msg.chars().take(200).collect()
}

fn set_state(f: impl FnOnce(&mut super::util::Upd)) {
    let mut st = upd().lock().unwrap();
    f(&mut st);
}

pub async fn download(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = about_gate(&ctx, &headers, &uri) {
        return r;
    }
    let info = {
        let st = upd().lock().unwrap();
        st.info.clone()
    };
    let info_has = info
        .as_ref()
        .map(|v| super::util::py_truthy(v.get("has_update")))
        .unwrap_or(false);
    if info.is_none() || !info_has {
        return super::util::json_status(
            400,
            json!({"ok": false, "error": "没有可下载的新版本，先检查一次"}),
        );
    }
    let asset = info
        .as_ref()
        .and_then(|v| v.get("asset"))
        .cloned()
        .unwrap_or(Value::Null);
    if !super::util::py_truthy(Some(&asset)) {
        return super::util::json_status(
            400,
            json!({"ok": false, "error": "这个版本没有当前平台的安装包"}),
        );
    }
    {
        let mut b = super::util::busy().lock().unwrap();
        if *b {
            return Json(json!({"ok": true, "started": false, "note": "已经在下载了"}))
                .into_response();
        }
        *b = true;
    }
    let c = ctx.clone();
    tokio::spawn(async move {
        work(c, asset).await;
        *super::util::busy().lock().unwrap() = false;
    });
    Json(json!({"ok": true, "started": true})).into_response()
}

async fn work(ctx: Arc<Ctx>, asset: Value) {
    let total0 = asset
        .get("size")
        .map(|v| super::util::int_of(Some(v)))
        .unwrap_or(0);
    set_state(|st| {
        st.phase = "downloading".into();
        st.done = 0;
        st.total = total0;
        st.msg = String::new();
        st.path = String::new();
    });
    match download_asset(&ctx, &asset).await {
        Ok(p) => {
            let verified = {
                let meta = PathBuf::from(super::util::append_suffix(&p, ".meta"));
                std::fs::read_to_string(meta)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .map(|v| super::util::py_truthy(v.get("verified")))
                    .unwrap_or(false)
            };
            let note = if verified {
                String::new()
            } else {
                "已下载，但 GitHub 未提供校验值，未做完整性校验".to_string()
            };
            set_state(|st| {
                st.phase = "done".into();
                st.path = p.to_string_lossy().to_string();
                st.msg = note;
                st.done = if st.total != 0 { st.total } else { st.done };
            });
            let _ = cleanup(&ctx, 2);
        }
        Err(e) => {
            set_state(|st| {
                st.phase = "error".into();
                st.msg = truncated(&format!("UpdateError: {e}"));
            });
        }
    }
}

/// `updater.download()`。
async fn download_asset(ctx: &Ctx, asset: &Value) -> Result<PathBuf, String> {
    let url = asset.get("url").cloned().unwrap_or(Value::Null);
    let url_s = if super::util::py_truthy(Some(&url)) { super::util::py_str(&url) } else { String::new() };
    let name = {
        let n = asset.get("name").map(super::util::py_str).unwrap_or_default();
        if n.is_empty() { "moshan-update.bin".to_string() } else { n }
    };
    if url_s.is_empty() {
        return Err("该版本没有可下载的安装包".into());
    }
    let dest_dir = ctx.state_dir.join("updates");
    let _ = std::fs::create_dir_all(&dest_dir);
    let target = dest_dir.join(&name);
    let partial = super::util::append_suffix(&target, ".part");

    let expected = {
        let d = asset.get("digest").map(super::util::py_str).unwrap_or_default();
        d.replace("sha256:", "").trim().to_lowercase()
    };
    let total = asset.get("size").map(|v| super::util::int_of(Some(v))).unwrap_or(0);
    let started = Instant::now();
    let mut hasher = Sha256::new();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("下载中断：{e}"))?;
    // 注意：下载用的 UA 里的 repo 是**默认仓库**（updater.download 那一行写死了
    // version.DEFAULT_REPO），不是 config 里覆盖过的。
    let ua = format!("Moshan-Updater/0.1 (+https://github.com/{})", crate::paths::DEFAULT_REPO);
    let mut resp = client
        .get(&url_s)
        .header(reqwest::header::USER_AGENT, ua)
        .send()
        .await
        .map_err(|e| {
            let _ = std::fs::remove_file(&partial);
            format!("下载中断：{e}")
        })?;
    if !resp.status().is_success() {
        let _ = std::fs::remove_file(&partial);
        return Err(format!("下载中断：HTTP {}", resp.status().as_u16()));
    }
    let declared = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v != 0)
        .unwrap_or(if total != 0 { total } else { 0 });

    let mut f = std::fs::File::create(&partial).map_err(|e| {
        let _ = std::fs::remove_file(&partial);
        format!("下载中断：{e}")
    })?;
    let mut done: i64 = 0;
    loop {
        let chunk = match resp.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                return Err(format!("下载中断：{e}"));
            }
        };
        if chunk.is_empty() {
            continue;
        }
        if let Err(e) = f.write_all(&chunk) {
            let _ = std::fs::remove_file(&partial);
            return Err(format!("下载中断：{e}"));
        }
        hasher.update(&chunk);
        done += chunk.len() as i64;
        // `on_progress(done, declared)`：每块都写一次，页面轮询才跟得上。
        set_state(|st| {
            st.done = done;
            st.total = declared;
        });
    }
    let _ = f.flush();
    drop(f);

    let got = format!("{:x}", hasher.finalize());
    let verified = !expected.is_empty();
    if verified && got != expected {
        let _ = std::fs::remove_file(&partial);
        return Err(format!(
            "校验失败（期望 {}… 实际 {}…），已丢弃",
            &expected[..expected.len().min(12)],
            &got[..got.len().min(12)]
        ));
    }

    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let size = std::fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
    std::fs::rename(&partial, &target).map_err(|e| format!("下载中断：{e}"))?;
    let seconds = format!("{elapsed:.1}").parse::<f64>().unwrap_or(elapsed);
    let rate = format!("{:.2}", size as f64 / elapsed / 1048576.0)
        .parse::<f64>()
        .unwrap_or(0.0);
    let mut meta = Map::new();
    meta.insert("name".into(), json!(name));
    meta.insert("size".into(), json!(size));
    meta.insert("sha256".into(), json!(got));
    meta.insert("verified".into(), json!(verified));
    meta.insert("seconds".into(), json!(seconds));
    meta.insert("rate_mb_s".into(), json!(rate));
    let _ = std::fs::write(
        super::util::append_suffix(&target, ".meta"),
        crate::jsonio::dumps(&Value::Object(meta), 2),
    );
    Ok(target)
}

/// `updater.cleanup(keep)`：旧包与 apply-* 目录各留最近 keep 份。
fn cleanup(ctx: &Ctx, keep: usize) -> i64 {
    let d = ctx.state_dir.join("updates");
    if !d.is_dir() {
        return 0;
    }
    let mut removed = 0i64;
    let mut files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    let mut dirs: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(it) = std::fs::read_dir(&d) {
        for e in it.flatten() {
            let p = e.path();
            let Ok(md) = p.metadata() else { continue };
            let Ok(mt) = md.modified() else { continue };
            if md.is_dir() {
                if p.file_name()
                    .map(|n| n.to_string_lossy().starts_with("apply-"))
                    .unwrap_or(false)
                {
                    dirs.push((p, mt));
                }
            } else {
                let ext = p.extension().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                if matches!(ext.as_str(), "dmg" | "exe" | "zip" | "gz") {
                    files.push((p, mt));
                }
            }
        }
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    dirs.sort_by(|a, b| b.1.cmp(&a.1));
    for (p, _) in files.iter().skip(keep) {
        if std::fs::remove_file(p).is_ok() {
            let _ = std::fs::remove_file(super::util::append_suffix(p, ".meta"));
            removed += 1;
        }
    }
    for (p, _) in dirs.iter().skip(keep) {
        if std::fs::remove_dir_all(p).is_ok() {
            removed += 1;
        }
    }
    removed
}
