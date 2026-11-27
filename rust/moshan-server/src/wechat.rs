// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：给微信发消息 / 发文件（对齐 `moshan/wechat_push.py`）。
//       走网关的 WebSocket RPC：connect → conversations.list → conversations.send，
//       全程不经过模型。
// 不做：不建网关、不装插件、不管渠道在微信侧怎么配 —— 那些是 openclaw 的事。
//
// 调用方：`brief.push`（简报逐段）、`pending`（问句与回执）、
//        `pair`（把接入方案发到手机）、`transfer`（文件传输助手回推）。
//
// 对外是**阻塞**接口：调用方都在 spawn_blocking / std::thread 里，不占 tokio
// 工作线程。内部各自建一个 current_thread runtime，绝不在 async 上下文里
// block_on（误调时换一条干净线程跑，见 `run`）。
//
// 协议三处实测坑（与 Python 文件头同一份结论）：
//   · `to` 不能带 "user:" 前缀，否则渠道拿整串去 getuploadurl，微信回 ret=-1；
//   · mediaUrl 只认网关白名单目录，所以媒体走 `buffer`（base64），不走路径；
//   · conversations.send 只吃 message，媒体必须走 `send`。

use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use futures_util::SinkExt;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;

pub const CHANNEL: &str = "openclaw-weixin";

const DEFAULT_URL: &str = "ws://127.0.0.1:18789";
const PROTO: u32 = 4;
const REF_TTL: f64 = 600.0;
const MEDIA_MAX: u64 = 16 * 1024 * 1024;

// 超时：首帧 5s、握手 8s、普通调用 25s、发媒体 120s（与 Python 逐项一致）。
const T_OPEN: Duration = Duration::from_secs(10);
const T_FIRST: Duration = Duration::from_secs(5);
const T_HELLO: Duration = Duration::from_secs(8);
const T_CALL: Duration = Duration::from_secs(25);
const T_MEDIA: Duration = Duration::from_secs(120);

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------- 状态

fn last_err_slot() -> &'static Mutex<String> {
    static S: OnceLock<Mutex<String>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(String::new()))
}

fn set_last_error(msg: &str) {
    let cut: String = msg.chars().take(200).collect();
    if let Ok(mut g) = last_err_slot().lock() {
        *g = cut;
    }
}

/// 最近一次失败的原话。上层记账要能写出「为什么没发出去」，
/// 只回 false 的话界面上就只剩一句废话。
pub fn last_error() -> String {
    last_err_slot().lock().map(|g| g.clone()).unwrap_or_default()
}

fn ref_cache() -> &'static Mutex<HashMap<String, (String, f64)>> {
    static S: OnceLock<Mutex<HashMap<String, (String, f64)>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------- 配置

/// `Path.home() / ".openclaw" / "openclaw.json"`。
///
/// 测试开关（只影响本模块，默认零改动）：
///   `MOSHAN_WECHAT_CONFIG` / `OPENCLAW_CONFIG` 指到别的配置文件；
///   `MOSHAN_WECHAT_URL` 指到假网关（真网关在跑时用它避开真机）。
fn config_path() -> std::path::PathBuf {
    for key in ["MOSHAN_WECHAT_CONFIG", "OPENCLAW_CONFIG"] {
        if let Some(v) = std::env::var_os(key) {
            if !v.is_empty() {
                return std::path::PathBuf::from(v);
            }
        }
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".openclaw").join("openclaw.json")
}

fn url() -> String {
    std::env::var("MOSHAN_WECHAT_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// `gateway.auth.token`；读不出来就是空串（网关会自己拒绝）。
fn token() -> String {
    let Ok(text) = std::fs::read_to_string(config_path()) else {
        return String::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return String::new();
    };
    v.get("gateway")
        .and_then(|g| g.get("auth"))
        .and_then(|a| a.get("token"))
        .map(|t| match t {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 小工具

fn run<F, T>(fut: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>> + Send + 'static,
    T: Send + 'static,
{
    let make = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("RuntimeError: {e}"))
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        // 误在 async 上下文里调用：换一条干净线程，别在别人运行时上 block_on。
        let h = std::thread::spawn(move || make()?.block_on(fut));
        match h.join() {
            Ok(r) => r,
            Err(_) => Err("RuntimeError: 推送线程崩溃".to_string()),
        }
    } else {
        make()?.block_on(fut)
    }
}

fn hex(n: usize) -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let c = CTR.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seed = nanos ^ c.rotate_left(17) ^ ((std::process::id() as u64) << 32);
    let mut s = format!("{seed:016x}{c:016x}");
    s.truncate(n);
    while s.len() < n {
        s.push('0');
    }
    s
}

fn operation_id() -> String {
    format!("convop_{}", hex(24))
}

fn idempotency_key() -> String {
    format!("tfmedia_{}", hex(20))
}

/// `str(v or "")` 的轻量版。
fn s_or(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other if other.is_boolean() && !other.as_bool().unwrap_or(false) => String::new(),
        other if other.as_f64() == Some(0.0) => String::new(),
        other => other.to_string(),
    }
}

fn status_text(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `mimetypes.guess_type(name)[0]` 的常用子集；认不出的落回 octet-stream。
fn mime_for(name: &str) -> String {
    let ext = name
        .rsplit('.')
        .next()
        .filter(|e| !e.is_empty() && e.len() <= 8)
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let m = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" | "jpe" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "svg" => "image/svg+xml",
        "ico" => "image/vnd.microsoft.icon",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "mov" | "qt" => "video/quicktime",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/x-wav",
        "aac" => "audio/aac",
        "amr" => "audio/AMR",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "md" | "markdown" => "text/markdown",
        "html" | "htm" => "text/html",
        "rtf" => "application/rtf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    };
    m.to_string()
}

// ---------------------------------------------------------------- 帧

async fn send_frame(ws: &mut Ws, v: &Value) -> Result<(), String> {
    let text = serde_json::to_string(v).map_err(|e| e.to_string())?;
    ws.send(Message::Text(text.into()))
        .await
        .map_err(|e| format!("WebSocketException: {e}"))
}

/// 读一帧 JSON。超时/心跳/事件都回 `None`，真出错才 `Err`。
async fn recv_json(ws: &mut Ws, dur: Duration) -> Result<Option<Value>, String> {
    use futures_util::StreamExt;
    match timeout(dur, ws.next()).await {
        Err(_) => Ok(None),
        Ok(None) => Err("ConnectionClosed: 网关断开了".to_string()),
        Ok(Some(Err(e))) => Err(format!("WebSocketException: {e}")),
        Ok(Some(Ok(Message::Text(t)))) => {
            serde_json::from_str::<Value>(&t)
                .map(Some)
                .map_err(|e| format!("JSONDecodeError: {e}"))
        }
        Ok(Some(Ok(Message::Binary(b)))) => serde_json::from_slice::<Value>(&b)
            .map(Some)
            .map_err(|e| format!("JSONDecodeError: {e}")),
        Ok(Some(Ok(Message::Close(_)))) => Err("ConnectionClosed: 网关断开了".to_string()),
        // Ping/Pong/Frame：跳过
        Ok(Some(Ok(_))) => Ok(None),
    }
}

fn frame_id_matches(v: &Value, want: &str) -> bool {
    match v.get("id") {
        Some(Value::String(s)) => s == want,
        Some(Value::Number(n)) => n.to_string() == want,
        _ => false,
    }
}

async fn open() -> Result<Ws, String> {
    let u = url();
    let (ws, _) = timeout(T_OPEN, tokio_tungstenite::connect_async(&u))
        .await
        .map_err(|_| "TimeoutError: 连接网关超时".to_string())?
        .map_err(|e| format!("ConnectionError: {e}"))?;
    Ok(ws)
}

/// 走完握手，返回 hello-ok 的 payload。
async fn hello(ws: &mut Ws) -> Result<Value, String> {
    // 老网关可能没有 challenge：首帧读不到/不是 challenge 都照常 connect。
    let _ = recv_json(ws, T_FIRST).await;

    let params = json!({
        "minProtocol": PROTO,
        "maxProtocol": PROTO,
        "client": {"id": "cli", "version": "1.0.0", "platform": "macos", "mode": "cli"},
        "role": "operator",
        "scopes": ["operator.read", "operator.write", "operator.admin"],
        "caps": [],
        "commands": [],
        "permissions": {},
        "auth": {"token": token()},
        "locale": "zh-CN",
        "userAgent": "pocket-intake/1.0",
    });
    send_frame(
        ws,
        &json!({"type": "req", "id": "c1", "method": "connect", "params": params}),
    )
    .await?;
    loop {
        let Some(msg) = recv_json(ws, T_HELLO).await? else {
            return Err("TimeoutError: 握手超时".to_string());
        };
        if !frame_id_matches(&msg, "c1") {
            continue;
        }
        if msg.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(msg.get("payload").cloned().unwrap_or(json!({})));
        }
        let msg_text = msg
            .get("error")
            .and_then(|e| e.get("message"))
            .map(s_or)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "握手失败".to_string());
        return Err(msg_text);
    }
}

async fn call(ws: &mut Ws, mid: &str, method: &str, params: Value, dur: Duration) -> Result<Value, String> {
    send_frame(
        ws,
        &json!({"type": "req", "id": mid, "method": method, "params": params}),
    )
    .await?;
    loop {
        let Some(msg) = recv_json(ws, dur).await? else {
            return Err("TimeoutError: 等待网关响应超时".to_string());
        };
        if !frame_id_matches(&msg, mid) {
            continue;
        }
        if msg.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(msg.get("payload").cloned().unwrap_or(json!({})));
        }
        let msg_text = msg
            .get("error")
            .and_then(|e| e.get("message"))
            .map(s_or)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "调用失败".to_string());
        return Err(msg_text);
    }
}

async fn conversation_ref(ws: &mut Ws, agent: &str, channel: &str) -> Result<String, String> {
    if let Ok(g) = ref_cache().lock() {
        if let Some((r, ts)) = g.get(channel) {
            if now_ts() - *ts < REF_TTL {
                return Ok(r.clone());
            }
        }
    }
    let payload = call(
        ws,
        "c2",
        "conversations.list",
        json!({"agentId": agent, "limit": 20}),
        T_CALL,
    )
    .await?;
    if let Some(list) = payload.get("conversations").and_then(|v| v.as_array()) {
        for c in list {
            if s_or(c.get("channel").unwrap_or(&Value::Null)) != channel {
                continue;
            }
            let r = s_or(c.get("conversationRef").unwrap_or(&Value::Null));
            if !r.is_empty() {
                if let Ok(mut g) = ref_cache().lock() {
                    g.insert(channel.to_string(), (r.clone(), now_ts()));
                }
                return Ok(r);
            }
        }
    }
    Err(format!("找不到 {channel} 的会话"))
}

// ---------------------------------------------------------------- 对外：文本

async fn do_send(text: String, agent: String, channel: String) -> Result<Value, String> {
    let mut ws = open().await?;
    hello(&mut ws).await?;
    let r = conversation_ref(&mut ws, &agent, &channel).await?;
    let payload = call(
        &mut ws,
        "s1",
        "conversations.send",
        json!({
            "agentId": agent,
            "conversationRef": r,
            "message": text,
            "operationId": operation_id(),
        }),
        T_CALL,
    )
    .await?;
    Ok(payload)
}

/// 发一条纯文本。成功 `true`，失败 `false`（永不 panic、不抛错 —— 发不出去
/// 不该把调用方的业务带失败，与 Python 的 `send()` 同语义）。
pub fn send(text: &str) -> bool {
    send_to(text, "main", CHANNEL)
}

pub fn send_to(text: &str, agent: &str, channel: &str) -> bool {
    let body = text.trim().to_string();
    if body.is_empty() {
        set_last_error("空文本");
        return false;
    }
    let cut: String = body.chars().take(1500).collect();
    let agent = if agent.trim().is_empty() { "main".to_string() } else { agent.to_string() };
    let channel = if channel.trim().is_empty() { CHANNEL.to_string() } else { channel.to_string() };
    match run(do_send(cut, agent, channel)) {
        Ok(r) => {
            let status = s_or(r.get("status").unwrap_or(&Value::Null));
            let ok = matches!(status.as_str(), "sent" | "queued" | "suppressed");
            if ok {
                set_last_error("");
            } else {
                set_last_error(&format!(
                    "状态 {}",
                    status_text(r.get("status").unwrap_or(&Value::Null))
                ));
            }
            ok
        }
        Err(e) => {
            set_last_error(&e);
            false
        }
    }
}

// ---------------------------------------------------------------- 对外：媒体

async fn do_send_media(
    raw: Vec<u8>,
    filename: String,
    mime: String,
    text: String,
    agent: String,
    channel: String,
) -> Result<Value, String> {
    let mut ws = open().await?;
    hello(&mut ws).await?;
    let payload = call(
        &mut ws,
        "c2",
        "conversations.list",
        json!({"agentId": agent, "limit": 20}),
        T_CALL,
    )
    .await?;
    let conv = payload
        .get("conversations")
        .and_then(|v| v.as_array())
        .and_then(|a| {
            a.iter()
                .find(|c| s_or(c.get("channel").unwrap_or(&Value::Null)) == channel)
        })
        .cloned()
        .ok_or_else(|| format!("RuntimeError: 找不到 {channel} 的会话"))?;
    let mut to = s_or(conv.get("target").unwrap_or(&Value::Null));
    if let Some(rest) = to.strip_prefix("user:") {
        // 上传接口不认前缀，见文件头注释。
        to = rest.to_string();
    }
    let note: String = text.chars().take(500).collect();
    let mut params = json!({
        "to": to,
        "channel": channel,
        "message": note,
        "buffer": base64::engine::general_purpose::STANDARD.encode(&raw),
        "filename": filename,
        "contentType": mime,
        "idempotencyKey": idempotency_key(),
    });
    if let Some(acc) = conv.get("accountId") {
        if !acc.is_null() && s_or(acc) != "" {
            if let Some(o) = params.as_object_mut() {
                o.insert("accountId".to_string(), acc.clone());
            }
        }
    }
    call(&mut ws, "m1", "send", params, T_MEDIA).await
}

/// 发一个本地文件（图片/视频/其它）。超过 16MB 或文件不在都不发。
pub fn send_media(path: &Path, text: &str) -> bool {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if !path.is_file() {
        set_last_error("文件不在了");
        return false;
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size == 0 || size > MEDIA_MAX {
        set_last_error(&format!("大小不合适（{size} 字节，上限 {MEDIA_MAX}）"));
        return false;
    }
    let raw = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            set_last_error(&format!("{e}"));
            return false;
        }
    };
    let mime = mime_for(&name);
    match run(do_send_media(
        raw,
        name,
        mime,
        text.to_string(),
        "main".to_string(),
        CHANNEL.to_string(),
    )) {
        Ok(r) => {
            let mid = s_or(r.get("messageId").unwrap_or(&Value::Null));
            if mid.is_empty() {
                set_last_error("没有 messageId");
                false
            } else {
                set_last_error("");
                true
            }
        }
        Err(e) => {
            set_last_error(&e);
            false
        }
    }
}
