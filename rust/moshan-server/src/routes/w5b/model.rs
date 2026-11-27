// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：模型两条路由
//       GET  /api/llm        本机在跑什么大模型（`moshan/llmwatch.py`）
//       POST /api/model/test 探一个 OpenAI 兼容端点（`moshan/modelcfg.probe`）
// 不做：不落盘、不改配置（`/api/model/save` 不在本泳道）。
//
// 进程从哪儿来：`hostwork::procs::collect_procs()` —— libproc 拿 pid/ppid/name/rss，
// 再按平台补 argv（macOS KERN_PROCARGS2、Linux /proc/<pid>/cmdline）。
// **不再走 `ps`**：沙箱里 `ps` 会被拒，退化分支只有 pid + 命令行、rss 全 0，
// 最后只能按名字排序 —— 那正是「模型 ↔ 进程」归属错位的根源。
// `_port_owner` 主路径 `lsof`，Linux 退 `/proc/net/tcp*`，Windows 退 `netstat -ano`。

use super::util::{fnum, round1, str_or, sv, truthy};
use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- /api/model/test

const PROBE_TIMEOUT: f64 = 2.5;

fn norm_base(base: &str) -> String {
    base.trim().trim_end_matches('/').to_string()
}

fn openai_root(base: &str) -> String {
    let b = norm_base(base);
    if b.is_empty() {
        return String::new();
    }
    if b.ends_with("/v1") {
        b
    } else {
        format!("{b}/v1")
    }
}

async fn get_json(url: &str, key: &str, timeout: f64) -> Result<Value, String> {
    let c = reqwest::Client::builder()
        .timeout(Duration::from_secs_f64(timeout))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = c.get(url).header("Accept", "application/json");
    if !key.is_empty() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.map_err(|e| why(&e))?;
    let status = resp.status();
    if !status.is_success() {
        // urllib 在 >=400 时抛 HTTPError：_why 只看状态码，不读正文
        return Err(format!("\u{0}HTTP {}", status.as_u16()));
    }
    let text = resp.text().await.map_err(|e| why(&e))?;
    serde_json::from_str::<Value>(&text).map_err(|_| "返回的不是 JSON".to_string())
}

/// `_why()`：把异常翻成一句人话。约定以 `\u{0}HTTP` 前缀传状态码。
fn why(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "连接超时".to_string();
    }
    let text = format!("{e}");
    if text.contains("refused") || text.contains("61") || e.is_connect() {
        return "端口没人应答（服务没启动？）".to_string();
    }
    text.chars().take(80).collect()
}

pub async fn probe(base: &str, key: &str) -> Value {
    let base = norm_base(base);
    if base.is_empty() {
        return json!({ "ok": false, "models": [], "reason": "没填地址" });
    }
    let models_url = format!("{}/models", openai_root(&base));
    let reason = match get_json(&models_url, key, PROBE_TIMEOUT).await {
        Ok(data) => {
            let models = ids_of(&data);
            if !models.is_empty() {
                return json!({ "ok": true, "models": take(models, 200), "reason": "已连通" });
            }
            "连上了，但没列出模型".to_string()
        }
        Err(e) => finish_why(e),
    };
    match get_json(&format!("{base}/api/tags"), key, PROBE_TIMEOUT).await {
        Ok(data) => {
            let models = names_of(&data);
            if !models.is_empty() {
                return json!({ "ok": true, "models": take(models, 200), "reason": "Ollama 原生接口" });
            }
            json!({ "ok": true, "models": [], "reason": "Ollama 在跑，但还没有模型" })
        }
        Err(_) => json!({ "ok": false, "models": [], "reason": reason }),
    }
}

fn take(v: Vec<String>, n: usize) -> Vec<String> {
    v.into_iter().take(n).collect()
}

fn finish_why(e: String) -> String {
    if let Some(code) = e.strip_prefix('\u{0}') {
        let n: u16 = code.trim_start_matches("HTTP ").trim().parse().unwrap_or(0);
        return match n {
            401 | 403 => "鉴权没过，检查 API Key".to_string(),
            404 => "这个地址没有 /v1/models".to_string(),
            _ => format!("HTTP {n}"),
        };
    }
    e
}

fn ids_of(data: &Value) -> Vec<String> {
    data.get("data")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m.get("id").map(sv).unwrap_or_default();
                    if id.is_empty() {
                        None
                    } else {
                        Some(id)
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn names_of(data: &Value) -> Vec<String> {
    data.get("models")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let n = m.get("name").map(sv).unwrap_or_default();
                    if n.is_empty() {
                        None
                    } else {
                        Some(n)
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub async fn model_test(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    // Python 这里是 `await request.json()` + try/except：坏 JSON 也算空体
    let body = super::util::lenient_body(&body);
    let base = {
        let b = body.get("base").map(sv).unwrap_or_default();
        if b.is_empty() {
            ctx.cfg.get("gateway_base").map(sv).unwrap_or_default()
        } else {
            b
        }
    };
    let key = {
        let k = body.get("key").map(sv).unwrap_or_default().trim().to_string();
        if k.is_empty() {
            ctx.cfg
                .get("gateway_api_key")
                .map(sv)
                .unwrap_or_default()
        } else {
            k
        }
    };
    Json(probe(&base, &key).await).into_response()
}

// ---------------------------------------------------------------- /api/llm

// 认得的推理服务：进程名（小写）→ 族名
const FAMILY_BY_NAME: &[(&str, &str)] = &[
    ("llama-server", "llama.cpp"),
    ("llama-server.exe", "llama.cpp"),
    ("llama_server", "llama.cpp"),
    ("server.exe", "llama.cpp"),
    ("ollama", "Ollama"),
    ("ollama.exe", "Ollama"),
    ("ollama_llama_server", "Ollama"),
    ("ollama_llama_server.exe", "Ollama"),
    ("lm studio.exe", "LM Studio"),
    ("lmstudio", "LM Studio"),
    ("koboldcpp.exe", "KoboldCpp"),
    ("koboldcpp", "KoboldCpp"),
    ("llama-box.exe", "llama-box"),
    ("llama-box", "llama-box"),
    ("jan.exe", "Jan"),
    ("gpt4all", "GPT4All"),
    ("localai", "LocalAI"),
];

const FAMILY_BY_ARGV: &[(&str, &str)] = &[
    ("vllm.entrypoints", "vLLM"),
    ("vllm serve", "vLLM"),
    ("sglang.launch_server", "SGLang"),
    ("mlx_lm.server", "MLX-LM"),
    ("mlx_lm", "MLX-LM"),
    ("text_generation.launch", "text-generation-webui"),
    ("oobabooga", "text-generation-webui"),
];

const MODEL_FLAGS: &[&str] = &[
    "-m",
    "--model",
    "--model-path",
    "--model_file",
    "--gguf",
    "-hf",
    "--hf-repo",
    "-hfr",
    "--repo-id",
];
const PORT_FLAGS: &[&str] = &["--port", "-p", "--api-port", "--serve-port"];
const CTX_FLAGS: &[&str] = &[
    "-c",
    "--ctx-size",
    "--ctx",
    "--max-model-len",
    "--context-length",
    "-contextlength",
];
const NGL_FLAGS: &[&str] = &["-ngl", "--n-gpu-layers", "--n_gpu_layers"];
const ALIAS_FLAGS: &[&str] = &["-a", "--alias", "--model-alias"];
const DEFAULT_PORTS: &[u16] = &[8080, 11434, 1234, 8000, 5001, 8081];
const SERVICE_FAMS: &[&str] = &["LM Studio", "Ollama", "Jan", "GPT4All", "LocalAI"];
const WORKER_FAMS: &[&str] = &["llama.cpp", "llama-box"];
const PROBE_TIMEOUT_S: f64 = 0.8;
const CACHE_TTL: f64 = 20.0;

struct Proc {
    pid: i64,
    ppid: i64,
    name: String,
    argv: Vec<String>,
    /// RSS，单位 MB —— 与 Python `psutil.memory_info().rss / 1048576` 同口径，
    /// 进 cands 时再 `/1024` 变 GB。`ProcRow.rss` 是字节，这里只做一次换算。
    rss: f64,
    /// Python 的 `age_s`（`create_time` 换算）。`_build` 把它放进 cands 但不进
    /// /api/llm 响应；保留同口径字段，避免下一段再自己算。
    #[allow(dead_code)]
    age_s: Option<u64>,
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn iter_procs() -> Vec<Proc> {
    let now = epoch_secs();
    let mut procs = Vec::new();
    for row in crate::hostwork::procs::collect_procs() {
        // Python `if not name and not argv: continue`
        if row.name.is_empty() && row.argv.is_empty() {
            continue;
        }
        procs.push(Proc {
            pid: row.pid as i64,
            ppid: row.ppid as i64,
            name: row.name,
            argv: row.argv,
            rss: row.rss as f64 / 1048576.0,
            age_s: if row.started > 0 {
                Some(now.saturating_sub(row.started))
            } else {
                None
            },
        });
    }
    procs
}

fn family_of(name: &str, argv: &[String]) -> String {
    let low = name.trim().to_lowercase();
    // 1) 进程名精确命中
    for (key, fam) in FAMILY_BY_NAME {
        if *key == low {
            return fam.to_string();
        }
    }
    // 2) 进程名子串（`llama-server-3.2` 这类带版本号的产物）
    for (key, fam) in FAMILY_BY_NAME {
        if key.ends_with(".exe") {
            continue;
        }
        if low.contains(key) {
            return fam.to_string();
        }
    }
    // 3) 命令行里的模块名（vllm / sglang / mlx_lm …）
    let hay = argv.join(" ").to_lowercase();
    for (key, fam) in FAMILY_BY_ARGV {
        if hay.contains(key) {
            return fam.to_string();
        }
    }
    String::new()
}

fn take_flag(argv: &[String], flags: &[&str]) -> String {
    for (i, a) in argv.iter().enumerate() {
        let low = a.to_lowercase();
        if flags.contains(&low.as_str()) && i + 1 < argv.len() {
            let nxt = &argv[i + 1];
            if !nxt.starts_with('-') {
                return nxt.clone();
            }
        }
        for f in flags {
            if low.starts_with(&format!("{f}=")) {
                return a[f.len() + 1..].to_string();
            }
        }
    }
    String::new()
}

fn num_of(s: &str) -> Option<i64> {
    let t = s.trim();
    if let Ok(v) = t.parse::<i64>() {
        return Some(v);
    }
    t.parse::<f64>().ok().map(|f| f as i64)
}

/// Python 的 `probe.get("ctx") or cand["ctx"]`：整数 0 在 Python 里是假值，
/// 必须落到 fallback（Rust 的 `Option::or` 只认 None）。
fn or_nonzero(v: Option<i64>, fallback: Option<i64>) -> Option<i64> {
    v.filter(|x| *x != 0).or(fallback)
}

struct Pretty {
    name: String,
    quant: String,
    size: String,
}

fn pretty(path: &str) -> Pretty {
    let trimmed = path.trim();
    let base = trimmed
        .rsplit(['\\', '/'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed);
    let stem = Regex::new(r"(?i)\.(gguf|safetensors|bin|pt|pth|mlx)$")
        .ok()
        .map(|re| re.replace(base, "").to_string())
        .unwrap_or_else(|| base.to_string());
    let quant = Regex::new(r"(?i)(IQ\d[\w]*|Q\d[\w]*|BF16|FP16|F16|F32|FP32)")
        .ok()
        .and_then(|re| re.find(&stem).map(|m| m.as_str().to_uppercase()))
        .unwrap_or_default();
    Pretty {
        name: if stem.is_empty() {
            trimmed.to_string()
        } else {
            stem.clone()
        },
        quant,
        size: size_tag(&stem),
    }
}

/// `_SIZE`（`re.I` + 前后界断言）。Rust 正则不支持 lookaround，手工判边界：
/// 前一个字符不能是数字或 `.`；后一个字符不能是字母（Python `(?![a-z])` 带
/// IGNORECASE，所以大写字母同样要挡）。单位本身也按 `re.I` 收大小写。
fn size_tag(stem: &str) -> String {
    let Ok(re) = Regex::new(r"(?i)(\d{1,3}(?:\.\d)?)\s*([BM])") else {
        return String::new();
    };
    for caps in re.captures_iter(stem) {
        let m = caps.get(0).unwrap();
        let prev = stem[..m.start()].chars().next_back();
        if prev.map(|c| c.is_ascii_digit() || c == '.').unwrap_or(false) {
            continue;
        }
        let next = stem[m.end()..].chars().next();
        if next.map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
            continue;
        }
        let num = caps.get(1).map(|x| x.as_str()).unwrap_or("");
        let unit = caps.get(2).map(|x| x.as_str()).unwrap_or("");
        return format!("{}{}", num, unit.to_uppercase());
    }
    String::new()
}

fn http_json(port: u16, path: &str) -> Option<Value> {
    // 走裸 TCP：只需要一小段 JSON，不值得为它引 reqwest 的 runtime。
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs_f64(PROBE_TIMEOUT_S)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs_f64(PROBE_TIMEOUT_S)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 262144 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.lines().next()?.split_whitespace().nth(1)?;
    if status != "200" {
        return None;
    }
    let body = if head.to_lowercase().contains("transfer-encoding: chunked") {
        dechunk(body)
    } else {
        body.to_string()
    };
    serde_json::from_str(&body).ok()
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    loop {
        let Some((size_line, tail)) = rest.split_once("\r\n") else {
            break;
        };
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if size == 0 {
            break;
        }
        if tail.len() < size {
            out.push_str(tail);
            break;
        }
        out.push_str(&tail[..size]);
        rest = tail[size..].trim_start_matches("\r\n");
    }
    out
}

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(250),
    )
    .is_ok()
}

fn probe_llama_cpp(port: u16) -> Option<Value> {
    let d = http_json(port, "/props")?;
    let path = {
        let p = d.get("model_path").map(sv).unwrap_or_default();
        if p.is_empty() {
            d.get("model_alias").map(sv).unwrap_or_default()
        } else {
            p
        }
    };
    if path.is_empty() && !truthy(d.get("default_generation_settings").unwrap_or(&Value::Null)) {
        return None;
    }
    let gen = d.get("default_generation_settings").cloned().unwrap_or(json!({}));
    let ctx = if gen.is_object() {
        gen.get("n_ctx").map(sv).and_then(|s| num_of(&s))
    } else {
        None
    };
    let ngl = if gen.is_object() {
        gen.get("n_gpu_layers").map(sv).and_then(|s| num_of(&s))
    } else {
        None
    };
    Some(json!({
        "path": path, "ctx": ctx, "ngl": ngl, "source": "props",
    }))
}

fn probe_openai(port: u16) -> Option<Value> {
    let d = http_json(port, "/v1/models")?;
    let data = d.get("data")?.as_array()?;
    if data.is_empty() {
        return None;
    }
    let ids: Vec<String> = data
        .iter()
        .map(|x| {
            if x.is_object() {
                x.get("id").map(sv).unwrap_or_default().trim().to_string()
            } else {
                String::new()
            }
        })
        .filter(|s| !s.is_empty())
        .collect();
    if ids.is_empty() {
        return None;
    }
    Some(json!({
        "path": ids[0],
        "models": ids,
        "source": "v1",
    }))
}

fn probe_ollama(port: u16) -> Vec<Value> {
    let Some(d) = http_json(port, "/api/ps") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in d.get("models").and_then(|v| v.as_array()).cloned().unwrap_or_default() {
        let nm = {
            let a = m.get("name").map(sv).unwrap_or_default().trim().to_string();
            if a.is_empty() {
                m.get("model").map(sv).unwrap_or_default().trim().to_string()
            } else {
                a
            }
        };
        if nm.is_empty() {
            continue;
        }
        let vram = m
            .get("size_vram")
            .and_then(|v| super::util::py_float(v))
            .unwrap_or(0.0);
        out.push(json!({
            "path": nm,
            "vram_gb": fnum(round1(vram / 1073741824.0)),
            "expires": m.get("expires_at").map(py_expires).unwrap_or_default(),
            "source": "ps",
        }));
    }
    out
}

fn py_expires(v: &Value) -> String {
    super::util::str_or(Some(v))
}

/// 监听端口 → pid。三条取数路径，按平台与可用性逐级降级：
///
/// 1. `lsof -nP -iTCP -sTCP:LISTEN` —— mac / 多数 Linux 发行版都有；
/// 2. Linux：`/proc/net/tcp{,6}` 找 LISTEN 的 inode，再扫 `/proc/<pid>/fd`
///    把 `socket:[inode]` 反查成 pid；
/// 3. Windows：`netstat -ano` 的 LISTENING 行（末列就是 pid）。
///
/// 三者都拿不到就回空表 —— Python 的 `_port_owner()` 也是这样降级。
#[derive(Default)]
struct PortOwners {
    map: HashMap<u16, i64>,
    /// 插入顺序（第一次见到这个端口时记下）。Python 的 dict 也按这个顺序
    /// 遍历，`services` 挑端口时依赖它。
    order: Vec<(u16, i64)>,
}

impl PortOwners {
    /// Python 的 `out.setdefault(port, pid)`：同一个端口只认第一个。
    fn insert_first(&mut self, port: u16, pid: i64) {
        if !self.map.contains_key(&port) {
            self.map.insert(port, pid);
            self.order.push((port, pid));
        }
    }
}

fn port_owner() -> PortOwners {
    let mut owners = PortOwners::default();
    fill_from_lsof(&mut owners);
    #[cfg(target_os = "linux")]
    if owners.map.is_empty() {
        fill_from_proc_net(&mut owners);
    }
    #[cfg(windows)]
    if owners.map.is_empty() {
        fill_from_netstat(&mut owners);
    }
    owners
}

fn fill_from_lsof(owners: &mut PortOwners) {
    let out = std::process::Command::new("lsof")
        .args(["-nP", "-iTCP", "-sTCP:LISTEN"])
        .output();
    let Ok(o) = out else { return };
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    for line in text.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 9 {
            continue;
        }
        let Some(pid) = cols.get(1).and_then(|s| s.parse::<i64>().ok()) else {
            continue;
        };
        // NAME 形如 127.0.0.1:11434、*:8080、[::1]:1234
        let name = cols[8];
        let Some((_, port)) = name.rsplit_once(':') else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        owners.insert_first(port, pid);
    }
}

/// Linux：`/proc/net/tcp{,6}` 的 LISTEN 行里，local_address 是
/// `0100007F:1F90`（十六进制小端地址 : 十六进制端口），第 10 列是 socket inode。
/// 再扫 `/proc/<pid>/fd` 的符号链接，`socket:[inode]` 对上的就是占用者。
#[cfg(target_os = "linux")]
fn fill_from_proc_net(owners: &mut PortOwners) {
    let mut inode_port: HashMap<u64, u16> = HashMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 || cols[3] != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            let Some((_, port_hex)) = cols[1].rsplit_once(':') else {
                continue;
            };
            let (Ok(port), Ok(inode)) = (
                u16::from_str_radix(port_hex, 16),
                cols[9].parse::<u64>(),
            ) else {
                continue;
            };
            inode_port.insert(inode, port);
        }
    }
    if inode_port.is_empty() {
        return;
    }
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return;
    };
    for ent in procs.flatten() {
        let Ok(pid) = ent.file_name().to_string_lossy().parse::<i64>() else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(ent.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(link) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let s = link.to_string_lossy();
            let Some(inode) = s
                .strip_prefix("socket:[")
                .and_then(|x| x.strip_suffix(']'))
                .and_then(|x| x.parse::<u64>().ok())
            else {
                continue;
            };
            if let Some(port) = inode_port.get(&inode) {
                owners.insert_first(*port, pid);
            }
        }
    }
}

/// Windows：`netstat -ano` 的 LISTENING 行：
///   `  TCP    0.0.0.0:8080    0.0.0.0:0    LISTENING    1234`
#[cfg(windows)]
fn fill_from_netstat(owners: &mut PortOwners) {
    let out = std::process::Command::new("netstat").args(["-ano"]).output();
    let Ok(o) = out else { return };
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 5 || !cols[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        let Some((_, port)) = cols[1].rsplit_once(':') else {
            continue;
        };
        let (Ok(port), Ok(pid)) = (port.parse::<u16>(), cols[4].parse::<i64>()) else {
            continue;
        };
        owners.insert_first(port, pid);
    }
}

/// `_is_ollama_blob(argv, name)`：Ollama 拉起的 llama-server 子进程。
/// 除了 argv 里的 blob 路径，进程名本身是 `ollama_llama_server` 也算 ——
/// 老版本 Ollama 的路径不在 `~/.ollama/models/blobs` 里。
fn is_ollama_blob(argv: &[String], name: &str) -> bool {
    let hay = argv.join(" ").to_lowercase();
    hay.contains(".ollama/models/blobs")
        || hay.contains("lib/ollama/llama-server")
        || name.to_lowercase().contains("ollama_llama_server")
}

fn norm_name(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .collect()
}

fn best_api_port(
    name: &str,
    probes: &HashMap<u16, Value>,
    probes_order: &[u16],
    used: &HashSet<u16>,
) -> Option<u16> {
    let n = norm_name(name);
    if n.is_empty() {
        return None;
    }
    // Python 遍历 `probes.items()`（按探到的顺序），不是按端口号排序
    for p in probes_order {
        if used.contains(p) {
            continue;
        }
        let Some(got) = probes.get(p) else {
            continue;
        };
        for mid in got
            .get("models")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let m = norm_name(&sv(&mid));
            if !m.is_empty() && (n.contains(&m) || n.starts_with(&m)) {
                return Some(*p);
            }
        }
    }
    None
}

#[derive(Clone)]
struct Cand {
    pid: i64,
    ppid: i64,
    family: String,
    blob: bool,
    rss_gb: f64,
    model_hint: String,
    alias: String,
    port: Option<u16>,
    ctx: Option<i64>,
    ngl: Option<i64>,
    child: bool,
    parent: Option<i64>,
}

fn build() -> Value {
    let mut cands: Vec<Cand> = Vec::new();
    for p in iter_procs() {
        let fam = family_of(&p.name, &p.argv);
        if fam.is_empty() {
            continue;
        }
        let blob = is_ollama_blob(&p.argv, &p.name);
        cands.push(Cand {
            pid: p.pid,
            ppid: p.ppid,
            family: if blob { "Ollama".to_string() } else { fam },
            blob,
            rss_gb: round1(p.rss / 1024.0),
            model_hint: take_flag(&p.argv, MODEL_FLAGS),
            alias: take_flag(&p.argv, ALIAS_FLAGS),
            port: num_of(&take_flag(&p.argv, PORT_FLAGS))
                .filter(|v| *v != 0) // Python: `if c["port"]`，0 视为没写
                .and_then(|v| u16::try_from(v).ok()),
            ctx: num_of(&take_flag(&p.argv, CTX_FLAGS)),
            ngl: num_of(&take_flag(&p.argv, NGL_FLAGS)),
            child: false,
            parent: None,
        });
    }
    let mut by_pid: HashMap<i64, usize> = HashMap::new();
    for (i, c) in cands.iter().enumerate() {
        by_pid.insert(c.pid, i);
    }
    // Ollama 的子进程：blob 路径或父进程是 ollama 本体
    let ollama_main: HashSet<i64> = cands
        .iter()
        .filter(|c| c.family == "Ollama" && !c.blob)
        .map(|c| c.pid)
        .collect();
    for c in cands.iter_mut() {
        if c.blob || ollama_main.contains(&c.ppid) {
            c.family = "Ollama".to_string();
            c.child = true;
        }
    }
    // 宿主型服务的子进程归到父族
    for i in 0..cands.len() {
        let par_idx = by_pid.get(&cands[i].ppid).copied();
        if let Some(pi) = par_idx {
            if SERVICE_FAMS.contains(&cands[pi].family.as_str())
                && WORKER_FAMS.contains(&cands[i].family.as_str())
            {
                cands[i].parent = Some(cands[pi].pid);
                cands[i].family = cands[pi].family.clone();
            }
        }
    }

    // 探哪些端口
    let mut ports: Vec<u16> = Vec::new();
    for c in &cands {
        if let Some(p) = c.port {
            if c.parent.is_none() {
                ports.push(p);
            }
        }
    }
    if cands.iter().any(|c| c.family == "Ollama") && !ports.contains(&11434) {
        ports.push(11434);
    }
    for p in DEFAULT_PORTS {
        if !ports.contains(p) {
            ports.push(*p);
        }
    }

    let mut probes: HashMap<u16, Value> = HashMap::new();
    // Python 的 probes 是 dict，后面几段都按**插入顺序**遍历；HashMap 没有顺序，
    // 所以单独留一份探到的端口序列，避免用排序后的键改变「先撞上谁」。
    let mut probes_order: Vec<u16> = Vec::new();
    for p in ports {
        if !listening(p) {
            continue;
        }
        let mut got = serde_json::Map::new();
        if let Some(props) = probe_llama_cpp(p) {
            if let Some(m) = props.as_object() {
                for (k, v) in m {
                    if !v.is_null() {
                        got.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if let Some(openai) = probe_openai(p) {
            for k in ["path", "models", "source"] {
                if !got.contains_key(k) {
                    if let Some(v) = openai.get(k) {
                        got.insert(k.to_string(), v.clone());
                    }
                }
            }
        }
        if p == 11434 {
            let ps = probe_ollama(p);
            if !ps.is_empty() {
                got.insert("ollama".into(), Value::Array(ps));
            }
        }
        if !got.is_empty() {
            probes.insert(p, Value::Object(got));
            probes_order.push(p);
        }
    }

    let mut models: Vec<Value> = Vec::new();
    let mut used_ports: HashSet<u16> = HashSet::new();
    let mut used_pids: HashSet<i64> = HashSet::new();

    let add = |models: &mut Vec<Value>,
                   path: &str,
                   family: &str,
                   pid: Option<i64>,
                   port: Option<u16>,
                   rss: f64,
                   ctx: Option<i64>,
                   ngl: Option<i64>,
                   source: &str,
                   extra: Option<Value>| {
        let info = pretty(path);
        let name = if info.name.is_empty() {
            // Python: `info["name"] or (family or "未知模型")`
            if family.is_empty() {
                "未知模型".to_string()
            } else {
                family.to_string()
            }
        } else {
            info.name
        };
        let mut row = json!({
            "name": name,
            "family": if family.is_empty() { "未知".to_string() } else { family.to_string() },
            "quant": info.quant,
            "size": info.size,
            "pid": pid,
            "port": port,
            "ctx": ctx,
            "ngl": ngl,
            "rss_gb": fnum(round1(rss)),
            "source": source,
        });
        if let (Some(Value::Object(ex)), Value::Object(m)) = (extra, &mut row) {
            for (k, v) in ex {
                m.insert(k.clone(), v);
            }
        }
        models.push(row);
    };

    // 1) 宿主型服务拉起来的子进程
    for c in &cands {
        if c.parent.is_none() {
            continue;
        }
        used_pids.insert(c.pid);
        used_pids.insert(c.ppid);
        let probe = c.port.and_then(|p| probes.get(&p));
        let path = match probe.and_then(|p| p.get("path")).map(sv) {
            Some(s) if !s.is_empty() => s,
            _ => {
                if !c.model_hint.is_empty() {
                    c.model_hint.clone()
                } else {
                    c.alias.clone()
                }
            }
        };
        if path.is_empty() {
            continue;
        }
        let api_port = best_api_port(&path, &probes, &probes_order, &used_ports);
        if let Some(ap) = api_port {
            used_ports.insert(ap);
        }
        let ctx = or_nonzero(
            probe.and_then(|p| p.get("ctx")).and_then(|v| v.as_i64()),
            c.ctx,
        );
        let ngl = or_nonzero(
            probe.and_then(|p| p.get("ngl")).and_then(|v| v.as_i64()),
            c.ngl,
        );
        let source = probe
            .and_then(|p| p.get("source"))
            .map(sv)
            .unwrap_or_else(|| "argv".to_string());
        add(
            &mut models,
            &path,
            &c.family,
            Some(c.pid),
            api_port,
            c.rss_gb,
            ctx,
            ngl,
            &source,
            None,
        );
    }

    // 2) Ollama：/api/ps 给的是人能读的名字
    let ollama_ps = probes
        .get(&11434)
        .and_then(|p| p.get("ollama"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if !ollama_ps.is_empty() {
        used_ports.insert(11434);
        let group_rss = round1(
            cands
                .iter()
                .filter(|c| c.family == "Ollama")
                .map(|c| c.rss_gb)
                .sum(),
        );
        let main = cands.iter().find(|c| c.family == "Ollama" && !c.child);
        let kid = cands.iter().find(|c| c.family == "Ollama" && c.child);
        models.retain(|m| m.get("family").and_then(|v| v.as_str()) != Some("Ollama"));
        for (i, m) in ollama_ps.iter().enumerate() {
            let path = m.get("path").map(sv).unwrap_or_default();
            let extra = json!({
                "vram_gb": m.get("vram_gb").cloned().unwrap_or(Value::Null),
                "expires": m.get("expires").cloned().unwrap_or(Value::Null),
            });
            add(
                &mut models,
                &path,
                "Ollama",
                main.map(|c| c.pid),
                Some(11434),
                if i == 0 { group_rss } else { 0.0 },
                if i == 0 { kid.and_then(|c| c.ctx) } else { None },
                None,
                "ps",
                Some(extra),
            );
        }
    }

    // 3) 独立的推理服务（没有子进程）
    for c in &cands {
        if c.parent.is_some() || SERVICE_FAMS.contains(&c.family.as_str()) {
            continue;
        }
        let mut p = c.port;
        let mut probe = p.and_then(|port| probes.get(&port).cloned());
        if probe.is_none() {
            // Python 按 probes 的插入顺序挑第一个「有 path 且没被用过」的
            for cp in &probes_order {
                let got = &probes[cp];
                if used_ports.contains(cp) || truthy(got.get("ollama").unwrap_or(&Value::Null)) {
                    continue;
                }
                if truthy(got.get("path").unwrap_or(&Value::Null)) {
                    probe = Some(got.clone());
                    p = Some(*cp);
                    break;
                }
            }
        }
        let path = match probe.as_ref().and_then(|x| x.get("path")).map(sv) {
            Some(s) if !s.is_empty() => s,
            _ => {
                if !c.model_hint.is_empty() {
                    c.model_hint.clone()
                } else {
                    c.alias.clone()
                }
            }
        };
        if !path.is_empty() {
            let ctx = or_nonzero(
                probe.as_ref().and_then(|x| x.get("ctx")).and_then(|v| v.as_i64()),
                c.ctx,
            );
            let ngl = or_nonzero(
                probe.as_ref().and_then(|x| x.get("ngl")).and_then(|v| v.as_i64()),
                c.ngl,
            );
            let source = probe
                .as_ref()
                .and_then(|x| x.get("source"))
                .map(sv)
                .unwrap_or_else(|| "argv".to_string());
            add(
                &mut models,
                &path,
                &c.family,
                Some(c.pid),
                p,
                c.rss_gb,
                ctx,
                ngl,
                &source,
                None,
            );
        } else {
            add(
                &mut models,
                &c.family,
                &c.family,
                Some(c.pid),
                p,
                c.rss_gb,
                None,
                None,
                "proc",
                None,
            );
        }
        if let Some(pp) = p {
            used_ports.insert(pp);
        }
    }

    // 4) 探得到但没对上进程的
    if cands.iter().any(|c| c.family == "Ollama") {
        used_ports.insert(11434);
    }
    let owner = port_owner();
    let mut covered: HashSet<String> = models
        .iter()
        .map(|m| norm_name(&str_or(m.get("name"))))
        .collect();
    // 同 Python `for p, got in probes.items()`：按探到的顺序，不排序
    for p in &probes_order {
        let got = &probes[p];
        if used_ports.contains(p) || truthy(got.get("ollama").unwrap_or(&Value::Null)) {
            continue;
        }
        let mut fam = "OpenAI 兼容".to_string();
        if let Some(opid) = owner.map.get(p) {
            let ofam = by_pid
                .get(opid)
                .map(|i| cands[*i].family.clone())
                .unwrap_or_default();
            if SERVICE_FAMS.contains(&ofam.as_str()) {
                fam = ofam;
            } else if *p == 11434 {
                fam = "Ollama".to_string();
            }
        } else if *p == 11434 {
            fam = "Ollama".to_string();
        }
        for mid in got
            .get("models")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let nm = norm_name(&sv(&mid));
            if nm.is_empty()
                || covered.iter().any(|c| !c.is_empty() && (nm.contains(c) || c.contains(&nm)))
            {
                continue;
            }
            let ctx = got.get("ctx").and_then(|v| v.as_i64());
            add(
                &mut models,
                &sv(&mid),
                &fam,
                None,
                Some(*p),
                ctx.map(|_| 0.0).unwrap_or(0.0),
                ctx,
                None,
                "v1",
                None,
            );
            covered.insert(nm);
        }
    }

    // 5) 活着但没加载模型的服务
    let fam_with_models: HashSet<String> = models
        .iter()
        .map(|m| str_or(m.get("family")))
        .collect();
    let mut services: Vec<Value> = Vec::new();
    let mut seen_fam: HashSet<String> = HashSet::new();
    for c in &cands {
        if !SERVICE_FAMS.contains(&c.family.as_str())
            || fam_with_models.contains(&c.family)
            || seen_fam.contains(&c.family)
        {
            continue;
        }
        seen_fam.insert(c.family.clone());
        let mut port: Option<u16> = if c.family == "Ollama" { Some(11434) } else { None };
        // Python `for p, pid in owner.items()`：按 lsof/psutil 给的顺序
        for (p, pid) in &owner.order {
            if probes.contains_key(p)
                && by_pid
                    .get(pid)
                    .map(|i| cands[*i].family == c.family)
                    .unwrap_or(false)
            {
                port = Some(*p);
                break;
            }
        }
        services.push(json!({
            "family": c.family,
            "pid": c.pid,
            "port": port,
            "rss_gb": fnum(round1(c.rss_gb)),
        }));
    }

    // 去重：同一模型被进程层和服务层各说一遍
    let mut dedup: HashMap<(String, u16), Value> = HashMap::new();
    let mut order: Vec<(String, u16)> = Vec::new();
    for m in models {
        let k = (
            norm_name(&str_or(m.get("name"))),
            m.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16,
        );
        match dedup.get(&k) {
            None => {
                order.push(k.clone());
                dedup.insert(k, m);
            }
            Some(old) => {
                let a = m.get("rss_gb").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let b = old.get("rss_gb").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if a > b {
                    let mut m2 = m.clone();
                    if let Value::Object(o) = &mut m2 {
                        let keep = old.get("pid").cloned().unwrap_or(Value::Null);
                        if o.get("pid").map(|v| v.is_null()).unwrap_or(true) && !keep.is_null() {
                            o.insert("pid".into(), keep);
                        }
                    }
                    dedup.insert(k, m2);
                } else {
                    let mut old2 = old.clone();
                    if let Value::Object(o) = &mut old2 {
                        let keep = m.get("pid").cloned().unwrap_or(Value::Null);
                        if o.get("pid").map(|v| v.is_null()).unwrap_or(true) && !keep.is_null() {
                            o.insert("pid".into(), keep);
                        }
                        let ctx = m.get("ctx").cloned().unwrap_or(Value::Null);
                        if o.get("ctx").map(|v| v.is_null()).unwrap_or(true) && !ctx.is_null() {
                            o.insert("ctx".into(), ctx);
                        }
                    }
                    dedup.insert(k, old2);
                }
            }
        }
    }
    let mut out: Vec<Value> = order
        .into_iter()
        .filter_map(|k| dedup.remove(&k))
        .collect();
    out.sort_by(|a, b| {
        let ra = a.get("rss_gb").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let rb = b.get("rss_gb").and_then(|v| v.as_f64()).unwrap_or(0.0);
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| str_or(a.get("name")).cmp(&str_or(b.get("name"))))
    });
    let total: f64 = round1(
        out.iter()
            .map(|m| m.get("rss_gb").and_then(|v| v.as_f64()).unwrap_or(0.0))
            .sum(),
    );
    let mut probed: Vec<u16> = probes.keys().cloned().collect();
    probed.sort();
    let total_rss = if out.is_empty() {
        json!(0)
    } else {
        fnum(total)
    };
    json!({
        "ok": true,
        "at": fnum(epoch_round1()),
        "models": out,
        "total_rss_gb": total_rss,
        "services": services,
        "procs": cands.len(),
        "probed_ports": probed,
    })
}

fn epoch_round1() -> f64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    round1(t)
}

static CACHE: Mutex<Option<(f64, Value)>> = Mutex::new(None);

pub fn snapshot(fresh: bool) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if !fresh {
        if let Ok(g) = CACHE.lock() {
            if let Some((at, val)) = g.as_ref() {
                if now - at < CACHE_TTL {
                    return val.clone();
                }
            }
        }
    }
    let val = match std::panic::catch_unwind(build) {
        Ok(v) => v,
        Err(_) => json!({
            "ok": false, "at": fnum(round1(now)), "models": [],
            "total_rss_gb": 0.0, "error": "RuntimeError: 探测失败",
        }),
    };
    if let Ok(mut g) = CACHE.lock() {
        *g = Some((now, val.clone()));
    }
    val
}

pub async fn llm_api(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let fresh = crate::routes::common::q_int(&uri, "fresh", 0).unwrap_or(0) != 0;
    // 进程扫描 + 端口探测都是阻塞的（ps/lsof/TCP），丢到阻塞线程池
    let v = tokio::task::spawn_blocking(move || snapshot(fresh))
        .await
        .unwrap_or_else(|_| json!({"ok": false, "models": []}));
    Json(v).into_response()
}

// 保留：`_touch` 用的类型别名在未启用时不会告警
#[allow(dead_code)]
type Unused = (Instant, HashMap<u16, i64>);
