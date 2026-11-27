// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：对话模型来源（moshan/modelcfg.py 的 /api/model）+ 本机在跑什么大模型
//       （moshan/llmwatch.py，给 /api/overview 的 llm 那一格）。
// 不做：不写 config.json —— 改模型那条路由不在本泳道。

use super::util::{fnum, round1, str_or};
use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);

fn preset(id: &str) -> (&'static str, &'static str, &'static str, &'static str) {
    match id {
        "gateway" => ("gateway", "中台本地网关", "gateway", "http://127.0.0.1:8080"),
        "ollama" => ("ollama", "本机 Ollama", "ollama", "http://127.0.0.1:11434"),
        "lmstudio" => ("lmstudio", "LM Studio", "openai", "http://127.0.0.1:1234/v1"),
        "deepseek" => ("deepseek", "DeepSeek", "openai", "https://api.deepseek.com/v1"),
        "openai" => ("openai", "OpenAI 官方", "openai", "https://api.openai.com/v1"),
        _ => ("custom", "自定义端点", "openai", ""),
    }
}

fn hint_of(id: &str) -> &'static str {
    match id {
        "gateway" => "自带工具，能用中台的文件、检索、抓网页",
        "ollama" => "模型跑在本机，不联网",
        "lmstudio" => "先在 LM Studio 里加载模型，这里才看得见",
        "deepseek" => "填上 API Key 即可，按量计费",
        "openai" => "需要能连出去的网络",
        _ => "任何 OpenAI 兼容地址：vLLM、one-api、中转站都行",
    }
}

pub fn norm_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_string()
}

fn openai_root(base: &str) -> String {
    let b = norm_base(base);
    if b.is_empty() {
        return String::new();
    }
    if b.ends_with("/v1") { b } else { format!("{b}/v1") }
}

fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn why(e: &str) -> String {
    let low = e.to_lowercase();
    if low.contains("401") || low.contains("403") {
        return "鉴权没过，检查 API Key".to_string();
    }
    if low.contains("404") {
        return "这个地址没有 /v1/models".to_string();
    }
    if low.contains("timed out") || low.contains("timeout") {
        return "连接超时".to_string();
    }
    if low.contains("connection refused") || low.contains("10061") {
        return "端口没人应答（服务没启动？）".to_string();
    }
    for code in [500u16, 502, 503, 504, 400, 405, 429] {
        if low.contains(&format!("{code}")) {
            return format!("HTTP {code}");
        }
    }
    super::util::clip(e, 80)
}

async fn get_json(url: &str, key: &str, timeout: Duration) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .no_proxy()
        .build()
        .map_err(|e| why(&e.to_string()))?;
    let mut req = client.get(url).header("Accept", "application/json");
    if !key.is_empty() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                if status.as_u16() == 401 || status.as_u16() == 403 {
                    return Err("鉴权没过，检查 API Key".to_string());
                }
                if status.as_u16() == 404 {
                    return Err("这个地址没有 /v1/models".to_string());
                }
                return Err(format!("HTTP {}", status.as_u16()));
            }
            let text = resp.text().await.map_err(|e| why(&e.to_string()))?;
            serde_json::from_str::<Value>(&text).map_err(|_| "返回的不是 JSON".to_string())
        }
        Err(e) => {
            if e.is_connect() {
                return Err("端口没人应答（服务没启动？）".to_string());
            }
            if e.is_timeout() {
                return Err("连接超时".to_string());
            }
            Err(why(&e.to_string()))
        }
    }
}

async fn probe(base: &str, key: &str, timeout: Duration) -> (bool, Vec<String>, String) {
    let base = norm_base(base);
    if base.is_empty() {
        return (false, Vec::new(), "没填地址".to_string());
    }
    let reason;
    match get_json(&format!("{}/models", openai_root(&base)), key, timeout).await {
        Ok(data) => {
            let models: Vec<String> = data
                .get("data")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.get("id").and_then(|x| x.as_str()))
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();
            if !models.is_empty() {
                return (true, models.into_iter().take(200).collect(), "已连通".to_string());
            }
            reason = "连上了，但没列出模型".to_string();
        }
        Err(msg) => reason = msg,
    }
    match get_json(&format!("{base}/api/tags"), key, timeout).await {
        Ok(data) => {
            let models: Vec<String> = data
                .get("models")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.get("name").and_then(|x| x.as_str()))
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default();
            if !models.is_empty() {
                (true, models.into_iter().take(200).collect(), "Ollama 原生接口".to_string())
            } else {
                (true, Vec::new(), "Ollama 在跑，但还没有模型".to_string())
            }
        }
        Err(_) => (false, Vec::new(), reason),
    }
}

async fn local_source(id: &str, cur: &str) -> Value {
    let (pid, name, kind, base) = preset(id);
    let (ok, models, reason) = probe(base, "", PROBE_TIMEOUT).await;
    json!({
        "id": pid,
        "name": name,
        "kind": kind,
        "base": base,
        "hint": hint_of(pid),
        "models": models,
        "running": ok,
        "note": reason,
        "current": norm_base(base) == cur,
    })
}

/// `sources()`：三条本机来源并发探。
pub async fn sources(cur_base: &str) -> Vec<Value> {
    let cur = norm_base(cur_base);
    let (a, b, c) = tokio::join!(
        local_source("gateway", &cur),
        local_source("ollama", &cur),
        local_source("lmstudio", &cur),
    );
    let mut rows = vec![a, b, c];
    for id in ["deepseek", "openai", "custom"] {
        let (pid, name, kind, base) = preset(id);
        let current = !norm_base(base).is_empty() && norm_base(base) == cur;
        rows.push(json!({
            "id": pid,
            "name": name,
            "kind": kind,
            "base": base,
            "hint": hint_of(pid),
            "models": [],
            "running": false,
            "note": "",
            "current": current,
        }));
    }
    rows
}

/// GET /api/model
pub async fn model_state(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let cfg = &ctx.cfg;
    let kind = {
        let k = str_or(cfg.get("model_kind"));
        let k = if k.is_empty() { "gateway".to_string() } else { k };
        if ["gateway", "ollama", "openai"].contains(&k.as_str()) { k } else { "gateway".to_string() }
    };
    let base = norm_base(&str_or(cfg.get("gateway_base")));
    let key = str_or(cfg.get("gateway_api_key"));
    let model = str_or(cfg.get("model"));
    let rows = sources(&base).await;
    Json(json!({
        "current": {
            "kind": kind,
            "base": base,
            "model": model,
            "key_set": !key.is_empty(),
            "key_mask": mask_key(&key),
        },
        "sources": rows,
    }))
    .into_response()
}

// ------------------------------------------------------------------ llmwatch

const SERVICE_FAMS: &[&str] = &["llama.cpp", "Ollama", "LM Studio", "vLLM", "SGLang",
                                "KoboldCpp", "llama-box", "Jan", "GPT4All", "LocalAI",
                                "MLX-LM", "text-generation-webui"];
const WORKER_FAMS: &[&str] = &["llama.cpp", "llama-box"];

fn family_by_name(name: &str) -> &'static str {
    let low = name.trim().to_lowercase();
    let table: &[(&str, &str)] = &[
        ("llama-server", "llama.cpp"),
        ("llama_server", "llama.cpp"),
        ("server.exe", "llama.cpp"),
        ("ollama_llama_server", "Ollama"),
        ("ollama", "Ollama"),
        ("lm studio.exe", "LM Studio"),
        ("lmstudio", "LM Studio"),
        ("koboldcpp", "KoboldCpp"),
        ("llama-box", "llama-box"),
        ("jan.exe", "Jan"),
        ("gpt4all", "GPT4All"),
        ("localai", "LocalAI"),
    ];
    for (k, v) in table {
        if k.ends_with(".exe") {
            if low == *k {
                return v;
            }
            continue;
        }
        if low.contains(k) {
            // 精确名优先：先全等再看子串
            if low == *k {
                return v;
            }
        }
    }
    for (k, v) in table {
        if k.ends_with(".exe") {
            if low == *k {
                return v;
            }
            continue;
        }
        if low.contains(k) {
            return v;
        }
    }
    ""
}

fn is_listening(port: u16) -> bool {
    super::platform::port_open(port, "127.0.0.1", Duration::from_millis(120))
}

async fn probe_llama_cpp(port: u16) -> Option<Value> {
    let v = get_json(&format!("http://127.0.0.1:{port}/props"), "", Duration::from_millis(800))
        .await
        .ok()?;
    let path = v.get("model_path").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let ctx = v.get("default_generation_settings")
        .and_then(|g| g.get("n_ctx"))
        .and_then(|x| x.as_i64())
        .or_else(|| v.get("n_ctx").and_then(|x| x.as_i64()));
    json!({"path": path, "ctx": ctx, "source": "props"}).into()
}

async fn probe_openai(port: u16) -> Option<Value> {
    let v = get_json(&format!("http://127.0.0.1:{port}/v1/models"), "", Duration::from_millis(800))
        .await
        .ok()?;
    let models: Vec<String> = v
        .get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|x| x.as_str()))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    if models.is_empty() {
        return None;
    }
    Some(json!({"path": models[0], "models": models, "source": "v1"}))
}

async fn probe_ollama(port: u16) -> Option<Vec<Value>> {
    let v = get_json(&format!("http://127.0.0.1:{port}/api/ps"), "", Duration::from_millis(800))
        .await
        .ok()?;
    let models = v.get("models").and_then(|m| m.as_array())?;
    if models.is_empty() {
        return None;
    }
    Some(
        models
            .iter()
            .map(|m| {
                let name = m.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let vram = m.get("size_vram").and_then(|x| x.as_f64()).map(|b| round1(b / 1073741824.0));
                let expires = m.get("expires_at").cloned().unwrap_or(Value::Null);
                json!({"path": name, "vram_gb": vram, "expires": expires})
            })
            .collect(),
    )
}

/// 从名字里挑「7B / 4.5B / 30M」这种参数量。
///
/// Python 版是 `(?<![\d.])(\d{1,3}(?:\.\d)?)\s*([BM])(?![a-z])`，但 `regex` crate
/// **不支持前后瞻**，`Regex::new` 会 panic（不是返回 Err，是 panic —— 整个请求线程跟着死，
/// 表现成连接被直接掐断、curl 拿到空响应）。所以改成「先按主体匹配，再在代码里判边界」。
fn size_of(stem: &str) -> String {
    use std::sync::OnceLock;
    static R: OnceLock<regex::Regex> = OnceLock::new();
    let re = R.get_or_init(|| regex::Regex::new(r"(?i)(\d{1,3}(?:\.\d)?)\s*([BM])").unwrap());
    for c in re.captures_iter(stem) {
        let m = c.get(0).unwrap();
        // 前一个字符不能是数字或点：`4.5B` 里的 `5B` 不该再算一次。
        let before_ok = m.start() == 0
            || !matches!(stem[..m.start()].chars().next_back(), Some(ch) if ch.is_ascii_digit() || ch == '.');
        // 后一个字符不能是小写字母：否则 `1bit` 会被当成 `1B`。
        let after_ok = m.end() == stem.len()
            || !matches!(stem[m.end()..].chars().next(), Some(ch) if ch.is_ascii_lowercase());
        if before_ok && after_ok {
            return format!("{}{}", &c[1], c[2].to_uppercase());
        }
    }
    String::new()
}

fn pretty(path: &str) -> (String, String, String) {
    let base = path.trim().rsplit(['/', '\\']).next().unwrap_or(path.trim()).to_string();
    let stem = regex::Regex::new(r"(?i)\.(gguf|safetensors|bin|pt|pth|mlx)$")
        .unwrap()
        .replace(&base, "")
        .to_string();
    let quant = regex::Regex::new(r"(?i)(IQ\d[\w]*|Q\d[\w]*|BF16|FP16|F16|F32|FP32)")
        .unwrap()
        .captures(&stem)
        .map(|c| c.get(1).unwrap().as_str().to_uppercase())
        .unwrap_or_default();
    let size = size_of(&stem);
    let name = if stem.is_empty() { path.to_string() } else { stem };
    (name, quant, size)
}

async fn build() -> Value {
    let procs = super::procs::collect_procs();
    let mut cands: Vec<(i32, i32, String, f64)> = Vec::new(); // pid, ppid, family, rss_gb
    for p in &procs {
        let fam = family_by_name(&p.name);
        if fam.is_empty() {
            continue;
        }
        // ollama 的 blob 子进程（命令行走 ollama runner）也算 Ollama
        let fam = if fam == "Ollama" || p.path.to_lowercase().contains("ollama") {
            "Ollama"
        } else {
            fam
        };
        cands.push((p.pid, p.ppid, fam.to_string(), round1(p.rss as f64 / 1073741824.0)));
    }

    // 探哪些端口
    let mut ports: Vec<u16> = Vec::new();
    if cands.iter().any(|c| c.2 == "Ollama") {
        ports.push(11434);
    }
    for p in [8080u16, 11434, 1234, 8000, 5001, 8081] {
        if !ports.contains(&p) {
            ports.push(p);
        }
    }
    let mut probes: Vec<(u16, Value)> = Vec::new();
    for p in ports {
        if !is_listening(p) {
            continue;
        }
        let mut got = serde_json::Map::new();
        if let Some(v) = probe_llama_cpp(p).await {
            if let Value::Object(m) = v {
                for (k, v) in m {
                    if !v.is_null() {
                        got.insert(k, v);
                    }
                }
            }
        }
        if let Some(v) = probe_openai(p).await {
            if let Value::Object(m) = v {
                for (k, v) in m {
                    got.entry(k).or_insert(v);
                }
            }
        }
        if p == 11434 {
            if let Some(ps) = probe_ollama(p).await {
                got.insert("ollama".into(), Value::Array(ps));
            }
        }
        if !got.is_empty() {
            probes.push((p, Value::Object(got)));
        }
    }

    let mut models: Vec<Value> = Vec::new();
    let mut used_ports: Vec<u16> = Vec::new();

    // 1) 宿主型服务拉起来的子进程
    for c in &cands {
        let par = cands.iter().find(|x| x.0 == c.1);
        let Some(par) = par else { continue };
        if !SERVICE_FAMS.contains(&par.2.as_str()) || !WORKER_FAMS.contains(&c.2.as_str()) {
            continue;
        }
        let probe = probes.iter().find(|(_, g)| g.get("path").map(|p| !p.is_null()).unwrap_or(false));
        let path = probe
            .and_then(|(_, g)| g.get("path").and_then(|x| x.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        if path.is_empty() {
            continue;
        }
        let (name, quant, size) = pretty(&path);
        models.push(json!({
            "name": name, "family": par.2, "quant": quant, "size": size,
            "pid": c.0, "port": Value::Null, "ctx": Value::Null, "ngl": Value::Null,
            "rss_gb": fnum(c.3), "source": "argv",
        }));
    }

    // 2) Ollama /api/ps 的人读名字
    let ollama_ps = probes
        .iter()
        .find(|(p, _)| *p == 11434)
        .and_then(|(_, g)| g.get("ollama"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !ollama_ps.is_empty() {
        used_ports.push(11434);
        let group_rss = round1(cands.iter().filter(|c| c.2 == "Ollama").map(|c| c.3).sum::<f64>());
        let main = cands.iter().find(|c| c.2 == "Ollama");
        models.retain(|m| m.get("family").and_then(|f| f.as_str()) != Some("Ollama"));
        for (i, m) in ollama_ps.iter().enumerate() {
            let path = m.get("path").and_then(|x| x.as_str()).unwrap_or("");
            let (name, quant, size) = pretty(path);
            models.push(json!({
                "name": name, "family": "Ollama", "quant": quant, "size": size,
                "pid": main.map(|c| c.0).unwrap_or(0), "port": 11434,
                "ctx": Value::Null, "ngl": Value::Null,
                "rss_gb": fnum(if i == 0 { group_rss } else { 0.0 }),
                "source": "ps",
                "vram_gb": m.get("vram_gb").cloned().unwrap_or(Value::Null),
                "expires": m.get("expires").cloned().unwrap_or(Value::Null),
            }));
        }
    }

    // 3) 独立推理服务
    for c in &cands {
        if SERVICE_FAMS.contains(&c.2.as_str()) {
            continue;
        }
        let probe = probes.iter().find(|(p, _)| !used_ports.contains(p));
        let path = probe
            .and_then(|(_, g)| g.get("path").and_then(|x| x.as_str()).map(|s| s.to_string()))
            .unwrap_or_default();
        let (name, quant, size) = pretty(&path);
        if !path.is_empty() {
            models.push(json!({
                "name": name, "family": c.2, "quant": quant, "size": size,
                "pid": c.0, "port": probe.map(|(p, _)| json!(*p)).unwrap_or(Value::Null),
                "ctx": Value::Null, "ngl": Value::Null,
                "rss_gb": fnum(c.3), "source": "argv",
            }));
        } else {
            models.push(json!({
                "name": c.2, "family": c.2, "quant": "", "size": "",
                "pid": c.0, "port": Value::Null, "ctx": Value::Null, "ngl": Value::Null,
                "rss_gb": fnum(c.3), "source": "proc",
            }));
        }
        if let Some((p, _)) = probe {
            used_ports.push(*p);
        }
    }

    // 4) 探得到但没对上进程的
    if cands.iter().any(|c| c.2 == "Ollama") && !used_ports.contains(&11434) {
        used_ports.push(11434);
    }
    let mut covered: Vec<String> = models
        .iter()
        .map(|m| str_or(m.get("name")).to_lowercase())
        .collect();
    for (p, got) in &probes {
        if used_ports.contains(p) || got.get("ollama").is_some() {
            continue;
        }
        let fam = if *p == 11434 { "Ollama" } else { "OpenAI 兼容" };
        if let Some(list) = got.get("models").and_then(|m| m.as_array()) {
            for mid in list {
                let nm = str_or(Some(mid)).to_lowercase();
                if nm.is_empty() || covered.iter().any(|c| !c.is_empty() && (nm.contains(c) || c.contains(&nm))) {
                    continue;
                }
                let (name, quant, size) = pretty(&str_or(Some(mid)));
                models.push(json!({
                    "name": name, "family": fam, "quant": quant, "size": size,
                    "pid": Value::Null, "port": p, "ctx": Value::Null, "ngl": Value::Null,
                    "rss_gb": 0.0, "source": "v1",
                }));
                covered.push(nm);
            }
        }
    }

    // 5) 在跑但没加载模型的服务
    let fam_with_models: Vec<String> = models
        .iter()
        .map(|m| str_or(m.get("family")))
        .collect();
    let mut services: Vec<Value> = Vec::new();
    let mut seen_fam: Vec<String> = Vec::new();
    for c in &cands {
        if !SERVICE_FAMS.contains(&c.2.as_str())
            || fam_with_models.contains(&c.2)
            || seen_fam.contains(&c.2)
        {
            continue;
        }
        seen_fam.push(c.2.clone());
        let port = if c.2 == "Ollama" { json!(11434) } else { Value::Null };
        services.push(json!({
            "family": c.2, "pid": c.0, "port": port, "rss_gb": fnum(c.3),
        }));
    }

    // 去重 + 排序
    let mut dedup: Vec<Value> = Vec::new();
    for m in models {
        let key = (str_or(m.get("name")).to_lowercase(), m.get("port").and_then(|p| p.as_i64()).unwrap_or(0));
        let mut found = None;
        for (i, d) in dedup.iter().enumerate() {
            let dk = (str_or(d.get("name")).to_lowercase(), d.get("port").and_then(|p| p.as_i64()).unwrap_or(0));
            if dk == key {
                found = Some(i);
                break;
            }
        }
        match found {
            None => dedup.push(m),
            Some(i) => {
                let old_rss = dedup[i].get("rss_gb").and_then(|x| x.as_f64()).unwrap_or(0.0);
                let new_rss = m.get("rss_gb").and_then(|x| x.as_f64()).unwrap_or(0.0);
                if new_rss > old_rss {
                    dedup[i] = m;
                }
            }
        }
    }
    dedup.sort_by(|a, b| {
        let ra = a.get("rss_gb").and_then(|x| x.as_f64()).unwrap_or(0.0);
        let rb = b.get("rss_gb").and_then(|x| x.as_f64()).unwrap_or(0.0);
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| str_or(a.get("name")).cmp(&str_or(b.get("name"))))
    });
    let total: f64 = dedup.iter().map(|m| m.get("rss_gb").and_then(|x| x.as_f64()).unwrap_or(0.0)).sum();
    let mut probed: Vec<u16> = probes.iter().map(|(p, _)| *p).collect();
    probed.sort_unstable();
    json!({
        "ok": true,
        "at": fnum(round1(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0))),
        "models": dedup,
        "total_rss_gb": if total == 0.0 { json!(0) } else { fnum(round1(total)) },
        "services": services,
        "procs": cands.len(),
        "probed_ports": probed,
    })
}

static LLM_CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

/// `llmwatch.snapshot()`：20 秒缓存。
pub async fn llm_snapshot() -> Value {
    if let Ok(g) = LLM_CACHE.lock() {
        if let Some((at, v)) = g.as_ref() {
            if at.elapsed() < Duration::from_secs(20) {
                return v.clone();
            }
        }
    }
    let v = build().await;
    if let Ok(mut g) = LLM_CACHE.lock() {
        *g = Some((Instant::now(), v.clone()));
    }
    v
}
