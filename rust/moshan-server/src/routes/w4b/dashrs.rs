// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/dash.py` 里这一组路由用得到的几块 —— 服务探活、磁盘、存图索引、
//       心跳快照、本机模型调用（`_llm_json` / `_grab_json` / `_local_model_cfg`）。
// 不做：不实现 dash.payload() 的全部（新闻/阅读/去重/OCR/进程……），那几块简报不用。
//       也不重写 BUILTIN_APPS 的完整目录：只抄带端口、会被探活的那几条，
//       外加 INFRA_SERVICES 三条 —— 服务清单的分母必须与 Python 相同。
//
// 与 Python 的已知分叉：
//   · `_工作台/plugins.json` 里的自定义插件不进清单（对拍用的临时数据根里没有，
//     真实用户装了才会差一行）；
//   · 心跳优先读落盘的 `最新.json`（两边读同一份）；文件不存在时 Python 会退回
//     进程内 STATE，Rust 没有那个 STATE，只能显示「还没有跑过第一轮」。

use super::util;
use crate::paths::Ctx;
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// `(id, 显示名, 端口, 是否恒在线(self), 是否按需服务)`
/// 顺序 = Python `_all_specs()` 里 BUILTIN_APPS（有端口的）+ INFRA_SERVICES。
const SERVICES: &[(&str, &str, u16, bool, bool)] = &[
    ("pocket", "口袋（传输/抓取）", 8321, true, false),
    ("gallery", "看图", 8771, false, true),
    ("reader", "阅读", 8772, false, true),
    ("media", "媒体墙", 8773, false, true),
    ("inspect", "系统巡检", 8774, false, false),
    ("llmcost", "Token 价格追踪", 8776, false, true),
    ("follow", "关注追踪", 8780, false, false),
    ("review", "界面评审", 8785, false, true),
    ("gateway", "微信助手 gateway", 18789, false, false),
    ("ollama", "本机模型 Ollama", 11434, false, false),
    ("qdrant", "向量库 Qdrant", 6333, false, false),
];

fn port_open(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(350)).is_ok()
}

/// `_services()` 的投影：id/name/port/up/lazy。
pub fn services() -> Vec<Value> {
    // 11 个端口并发探，别串着等（远程 host 不通时每条 0.35s）。
    let ports: Vec<u16> = SERVICES.iter().map(|s| s.2).collect();
    let ups: Vec<bool> = std::thread::scope(|sc| {
        let handles: Vec<_> = ports
            .iter()
            .map(|p| sc.spawn(move || port_open(*p)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or(false)).collect()
    });
    SERVICES
        .iter()
        .zip(ups)
        .map(|((id, name, port, always, lazy), up)| {
            json!({
                "id": id, "name": name, "port": port,
                "up": bool_or(up, *always), "lazy": lazy,
            })
        })
        .collect()
}

fn bool_or(up: bool, always: bool) -> bool {
    always || up
}

/// `dash._human()`：B/KB 取整、MB 起一位小数。
pub fn human(n: f64) -> String {
    let mut n = if n.is_finite() { n } else { 0.0 };
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if n < 1024.0 || unit == "TB" {
            return if unit == "B" || unit == "KB" {
                format!("{} {}", util::f0(n), unit)
            } else {
                format!("{} {}", util::f1(n), unit)
            };
        }
        n /= 1024.0;
    }
    format!("{} TB", util::f1(n))
}

fn pct(a: f64, b: f64) -> f64 {
    if b != 0.0 {
        util::py_round(a * 100.0 / b, 1)
    } else {
        0.0
    }
}

fn disk_usage(root: &Path) -> Option<(f64, f64)> {
    // 磁盘口径只留一份：`platform::disk_usage` 是 statvfs / GetDiskFreeSpaceEx 的
    // 三平台封装，自己再抄一遍 statvfs 等于在 Windows 上留一处编译不过的代码。
    let (total, free) = crate::hostwork::platform::disk_usage(root)?;
    Some((total as f64, free as f64))
}

/// `_disk(root)`：只回简报用得到的四个键。
pub fn disk(root: &Path) -> Value {
    match disk_usage(root) {
        Some((total, free)) => {
            let used = total - free;
            json!({
                "used_pct": pct(used, total),
                "free_h": human(free),
                "total_h": human(total),
                "used_h": human(used),
            })
        }
        None => json!({"used_pct": 0, "free_h": "", "total_h": "", "used_h": ""}),
    }
}

/// `_gallery(root)`：只取 articles / images / bytes_h（简报那三行用）。
pub fn gallery(root: &Path) -> Value {
    let d = util::load_json(&root.join("04-存图").join("_索引.json")).unwrap_or(json!({}));
    let total = d.get("total").cloned().unwrap_or(json!({}));
    let arts = d.get("articles").and_then(|a| a.as_array()).cloned().unwrap_or_default();
    let articles = match util::int_or(util::get(&total, "articles")) {
        Some(n) if n != 0 => n,
        _ => arts.len() as i64,
    };
    json!({
        "articles": articles,
        "images": util::int0(util::get(&total, "images")),
        "bytes_h": human(util::as_f64(util::get(&total, "bytes")).unwrap_or(0.0)),
    })
}

/// `_heartbeat()` 的落盘那一支：`10-系统记录/心跳/最新.json`。
pub fn heartbeat(root: &Path) -> Value {
    let hb = util::load_json(&root.join("10-系统记录").join("心跳").join("最新.json"))
        .unwrap_or(json!({}));
    if !util::truthy(&hb) {
        // Python 这里会退回进程内 STATE；Rust 没有，只能如实说没跑过第一轮。
        return json!({"ok": Value::Null, "at": "", "bad": []});
    }
    let checks = hb.get("checks").and_then(|c| c.as_array()).cloned().unwrap_or_default();
    let bad: Vec<Value> = checks
        .iter()
        .filter(|c| !util::truthy(util::get(c, "ok")))
        .cloned()
        .collect();
    json!({
        "ok": hb.get("ok").cloned().unwrap_or(Value::Null),
        "at": util::s_or(util::get(&hb, "at")),
        "bad": bad,
    })
}

// ---------------------------------------------------------------- 本机模型

/// `_local_model_cfg(cfg)`：config.model 是 ollama 里的完整名。
pub fn local_model_cfg(ctx: &Ctx) -> String {
    let m = util::s_or(util::get(&ctx.cfg, "model")).trim().to_string();
    if m.is_empty() {
        "qwen3.5-4b-gguf:q4km".to_string()
    } else {
        m
    }
}

/// `_llm_json(prompt, model, num_predict, timeout)`：ollama 兼容接口。
/// 返回 `response` 文本；网络/解析失败一律 Err（调用方降级成同一句人话）。
pub async fn llm_json(
    prompt: &str,
    model: &str,
    num_predict: i64,
    timeout_secs: u64,
) -> Result<String, String> {
    let body = json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "think": false,
        "format": "json",
        "options": {"temperature": 0.1, "num_predict": num_predict, "top_p": 0.9},
    });
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post("http://127.0.0.1:11434/api/generate")
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let text = resp.text().await.map_err(|e| e.to_string())?;
    let d: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(util::s_or(util::get(&d, "response")))
}

/// `_grab_json(raw)`：剥掉 ```json 围栏与前后废话，再解析。
pub fn grab_json(raw: &str) -> Value {
    let mut s = raw.trim().to_string();
    s = strip_fence(&s);
    if let (Some(i), Some(j)) = (s.find('{'), s.rfind('}')) {
        if j > i {
            s = s[i..=j].to_string();
        }
    }
    match serde_json::from_str::<Value>(&s) {
        Ok(v) if v.is_object() => v,
        _ => json!({}),
    }
}

fn strip_fence(s: &str) -> String {
    let mut t = s.trim();
    // ^```[a-zA-Z]*\s*
    if t.starts_with("```") {
        let rest = &t[3..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        t = rest[end..].trim_start();
    }
    if t.ends_with("```") {
        t = t[..t.len() - 3].trim_end();
    }
    t.trim().to_string()
}
