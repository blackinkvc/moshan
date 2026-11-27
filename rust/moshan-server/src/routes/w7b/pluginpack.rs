// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/pluginpack.py` 里 `/api/updates` 用到的那一小块 —— 已装插件清单、
//       更新检查缓存、在线查最新版。只迁「读」的部分；装/卸/升级不在这条泳道。
// 不做：不做包解压安装（那会往 `_插件/` 写文件，属于另一波）。
//
// 为什么必须迁：`/api/updates` 是工作台角落那颗更新胶囊唯一的接口，本体与插件
// 的更新总账都在这一份响应里。只搬 `/api/update/*` 而不搬它，胶囊就永远是旧形状。

#![allow(dead_code)]

use crate::paths::Ctx;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const MANIFEST: &str = "plugin.json";
const PLUG_DIR: &str = "_插件";
const CACHE_TTL: f64 = 6.0 * 3600.0;
const UPDATE_TTL: u64 = 5;
const USER_AGENT: &str = "Moshan-PluginPack/1.0";

fn data_root(ctx: &Ctx) -> PathBuf {
    super::transfer::data_root(ctx)
}

fn plugins_dir(ctx: &Ctx) -> PathBuf {
    data_root(ctx).join(PLUG_DIR)
}

fn plugins_json(ctx: &Ctx) -> PathBuf {
    crate::paths::workbench_dir(ctx).join("plugins.json")
}

fn cache_file(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("plugin-updates.json")
}

fn read_json_file(p: &Path) -> Option<Value> {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn read_rows(ctx: &Ctx) -> Vec<Value> {
    let Some(d) = read_json_file(&plugins_json(ctx)) else {
        return Vec::new();
    };
    let rows = match &d {
        Value::Object(m) => m.get("plugins").cloned().unwrap_or(Value::Null),
        other => other.clone(),
    };
    match rows {
        Value::Array(a) => a.into_iter().filter(|x| x.is_object()).collect(),
        _ => Vec::new(),
    }
}

fn s(m: &Map<String, Value>, k: &str) -> String {
    match m.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => super::util::py_str(v),
    }
}

/// `str(x or "")`。
fn or_default(v: &Value) -> String {
    if super::util::py_truthy(Some(v)) {
        super::util::py_str(v)
    } else {
        String::new()
    }
}

/// `norm_manifest`：只保留 `/api/updates` 要展示的字段。
fn norm_manifest(m: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let mut pid = s(m, "id").trim().to_lowercase();
    let name = {
        let n = s(m, "name");
        if n.trim().is_empty() { s(m, "displayName") } else { n }
    };
    let name = name.trim().to_string();
    if pid.is_empty() {
        let re = regex::Regex::new(r"[^a-z0-9]+").unwrap();
        pid = re
            .replace_all(&name.to_lowercase(), "-")
            .trim_matches('-')
            .chars()
            .take(24)
            .collect();
    }
    let id_re = regex::Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").unwrap();
    if pid.is_empty() || !id_re.is_match(&pid) {
        return Err("id 不合规".into());
    }
    if name.is_empty() {
        return Err("name 必填".into());
    }
    let ver = {
        let v = s(m, "version").trim().to_string();
        let v = v.trim_start_matches(['v', 'V']);
        let v = v.trim();
        if v.is_empty() { "0.0.0".to_string() } else { v.to_string() }
    };
    let mut out = Map::new();
    out.insert("id".into(), json!(pid));
    out.insert("name".into(), json!(name.chars().take(24).collect::<String>()));
    out.insert("version".into(), json!(ver));
    out.insert("note".into(), json!(s(m, "note").trim().chars().take(60).collect::<String>()));
    out.insert("repo".into(), json!(s(m, "repo").trim()));
    out.insert("data_default".into(), json!(s(m, "data_default").trim()));
    out.insert("port".into(), m.get("port").cloned().unwrap_or(Value::Null));
    Ok(out)
}

/// `installed_packs`：以 `_插件/<id>/plugin.json` 为准。
fn installed_packs(ctx: &Ctx) -> Vec<Map<String, Value>> {
    let d = plugins_dir(ctx);
    if !d.is_dir() {
        return Vec::new();
    }
    let mut rows_map: Map<String, Value> = Map::new();
    for r in read_rows(ctx) {
        if let Value::Object(m) = &r {
            rows_map.insert(s(m, "id"), r.clone());
        }
    }
    let mut names: Vec<String> = std::fs::read_dir(&d)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        if name.starts_with('.') {
            continue;
        }
        let p = d.join(&name);
        if !p.is_dir() {
            continue;
        }
        let mf = p.join(MANIFEST);
        if !mf.is_file() {
            continue;
        }
        let Some(v) = read_json_file(&mf) else { continue };
        let Value::Object(raw) = v else { continue };
        let Ok(m) = norm_manifest(&raw) else { continue };
        let row = rows_map.get(&s(&m, "id")).and_then(|v| v.as_object()).cloned().unwrap_or_default();
        let mut item = Map::new();
        item.insert("id".into(), m.get("id").cloned().unwrap_or(Value::Null));
        item.insert("name".into(), m.get("name").cloned().unwrap_or(Value::Null));
        item.insert("version".into(), m.get("version").cloned().unwrap_or(Value::Null));
        let repo = {
            let a = s(&m, "repo");
            if a.is_empty() { s(&row, "repo") } else { a }
        };
        item.insert("repo".into(), json!(repo));
        let note = {
            let a = s(&m, "note");
            if a.is_empty() { s(&row, "note") } else { a }
        };
        item.insert("note".into(), json!(note));
        item.insert("dir".into(), json!(p.to_string_lossy().to_string()));
        item.insert("registered".into(), json!(rows_map.contains_key(&s(&m, "id"))));
        let dd = {
            let a = s(&m, "data_default");
            if a.is_empty() { s(&row, "data_default") } else { a }
        };
        item.insert("data_default".into(), json!(dd));
        let port = match m.get("port") {
            Some(v) if super::util::py_truthy(Some(v)) => v.clone(),
            _ => row.get("port").cloned().unwrap_or(Value::Null),
        };
        item.insert("port".into(), port);
        out.push(item);
    }
    out
}

fn version_key(v: &str) -> Vec<i64> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let core = v.split('-').next().unwrap_or("");
    let mut out: Vec<i64> = core.split('.').map(|s| s.trim().parse().unwrap_or(0)).collect();
    while out.len() < 3 {
        out.push(0);
    }
    out
}

fn version_gt(a: &str, b: &str) -> bool {
    version_key(a) > version_key(b)
}

async fn fetch_ver(client: &reqwest::Client, url: &str, kind: &str) -> Result<(String, String), String> {
    let res = client
        .get(url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await
        .map_err(|e| format!("{e}"))?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    let text = res.text().await.map_err(|e| format!("{e}"))?;
    let d: Value = serde_json::from_str(&text).map_err(|e| format!("{e}"))?;
    let mut ver = d.get("version").map(or_default).unwrap_or_default();
    ver = ver.trim_start_matches(['v', 'V']).trim().to_string();
    if ver.is_empty() && kind == "release" {
        let tag = d.get("tag_name").map(or_default).unwrap_or_default();
        ver = tag.trim_start_matches(['v', 'V']).trim().to_string();
    }
    let notes = {
        let raw = ["body", "notes", "summary"]
            .iter()
            .find_map(|k| d.get(*k).filter(|v| super::util::py_truthy(Some(v))).cloned())
            .unwrap_or(Value::String(String::new()));
        super::util::py_str(&raw).chars().take(2000).collect()
    };
    Ok((ver, notes))
}

async fn latest_online(client: &reqwest::Client, row: &Map<String, Value>) -> Result<(String, String, String), String> {
    let upd = row.get("update").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let manifest = {
        let m = s(&upd, "manifest");
        m.trim().to_string()
    };
    if !manifest.is_empty() {
        match tokio::time::timeout(Duration::from_secs(UPDATE_TTL), fetch_ver(client, &manifest, "manifest")).await {
            Ok(Ok((ver, notes))) => {
                if ver.is_empty() {
                    return Err("update.manifest 里没有 version".into());
                }
                return Ok((ver, notes, manifest));
            }
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("请求超时".into()),
        }
    }
    let mut repo = {
        let a = s(&upd, "repo");
        if a.is_empty() { s(row, "repo") } else { a }
    };
    repo = repo.trim().to_string();
    let re = regex::Regex::new(r"^https?://github\.com/").unwrap();
    repo = re.replace(&repo, "").trim_matches('/').to_string();
    if repo.ends_with(".git") {
        repo = repo[..repo.len() - 4].to_string();
    }
    if repo.is_empty() || !repo.contains('/') {
        return Err("这个插件没写 repo / update.manifest，查不了新版".into());
    }
    let (owner, name) = repo.split_once('/').unwrap();
    let cands = [
        (format!("https://api.github.com/repos/{repo}/releases/latest"), "release"),
        (format!("https://{}.github.io/{}/{MANIFEST}", owner.to_lowercase(), name), "pages"),
        (format!("https://raw.githubusercontent.com/{repo}/main/{MANIFEST}"), "raw"),
        (format!("https://raw.githubusercontent.com/{repo}/master/{MANIFEST}"), "raw"),
    ];
    let mut last_err = String::new();
    for (u, k) in cands {
        match tokio::time::timeout(Duration::from_secs(UPDATE_TTL), fetch_ver(client, &u, k)).await {
            Ok(Ok((ver, notes))) => {
                if !ver.is_empty() {
                    return Ok((ver, notes, u));
                }
            }
            Ok(Err(e)) => last_err = if last_err.is_empty() { e } else { last_err },
            Err(_) => last_err = if last_err.is_empty() { "请求超时".into() } else { last_err },
        }
    }
    Err(format!("查不到新版：{}", if last_err.is_empty() { "地址里都没有 version".into() } else { last_err }))
}

/// `pluginpack.check_updates(cfg, force=...)`。
pub async fn check_updates(ctx: &Ctx, force: bool) -> Value {
    let cache_path = cache_file(ctx);
    let mut cache: Map<String, Value> = match read_json_file(&cache_path) {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let now = super::util::now_epoch();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(UPDATE_TTL))
        .build()
        .unwrap_or_default();
    let mut items: Vec<Value> = Vec::new();
    for pk in installed_packs(ctx) {
        let id = s(&pk, "id");
        let mut rec: Map<String, Value> = cache
            .get(&id)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let fresh = !rec.is_empty()
            && (now - rec.get("checked_at").and_then(|v| Some(super::util::int_of(Some(v)))).unwrap_or(0) as f64) < CACHE_TTL;
        if force || !fresh {
            let dir = s(&pk, "dir");
            let mm = read_json_file(&Path::new(&dir).join(MANIFEST))
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            let mut merged = mm.clone();
            for (k, v) in &pk {
                merged.insert(k.clone(), v.clone());
            }
            match latest_online(&client, &merged).await {
                Ok((ver, notes, url)) => {
                    rec = Map::new();
                    rec.insert("checked_at".into(), json!(now));
                    rec.insert("latest".into(), json!(ver));
                    rec.insert("notes".into(), json!(notes));
                    rec.insert("url".into(), json!(url));
                    rec.insert("error".into(), json!(""));
                }
                Err(e) => {
                    rec.insert("checked_at".into(), json!(now));
                    rec.insert("error".into(), json!(super::util::trunc(&e, 160)));
                }
            }
        }
        cache.insert(id.clone(), Value::Object(rec.clone()));
        let latest = s(&rec, "latest");
        let cur_ver = s(&pk, "version");
        let has = !latest.is_empty() && version_gt(&latest, &cur_ver);
        let mut item = pk.clone();
        item.insert("latest".into(), json!(latest));
        item.insert("has_update".into(), json!(has));
        item.insert("notes".into(), json!(s(&rec, "notes")));
        item.insert("page".into(), json!(s(&rec, "url")));
        item.insert("error".into(), json!(s(&rec, "error")));
        let ca = rec.get("checked_at").cloned().unwrap_or(Value::Null);
        item.insert("checked_at".into(), if super::util::py_truthy(Some(&ca)) { ca } else { json!(0) });
        items.push(Value::Object(item));
    }
    // `save_cache`：缩进 1 + 末尾换行（与 Python 逐字节一致）。
    let cache_v = Value::Object(cache);
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::jsonio::atomic_write(
        &cache_path,
        &(crate::jsonio::dumps(&cache_v, 1) + "\n"),
    );
    let updates = items
        .iter()
        .filter(|x| x.get("has_update").map(|v| super::util::py_truthy(Some(v))).unwrap_or(false))
        .count();
    json!({
        "ok": true,
        "checked_at": now,
        "plugins": items,
        "updates": updates,
    })
}
