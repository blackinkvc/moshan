// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：虫巢的两把令牌（fleet_token / view_token）与「本机免令牌」放行，
//       逐行对齐 `moshan/fleet.py` 的 `agent_auth` / `view_auth` / `_is_local`。
// 不做：这里**没有**按设备 access 档（control/read/self）做的拦截 ——
//       源码里那一档只落库不校验（`set_access` 注释写明「接口校验在 P2 接」），
//       自己加一层就等于把判定链改了。
//
// 为什么单独一把：agent 令牌要落到每台被控机的硬盘上，它只能 report/poll/result；
// 查看令牌只在主人手里。两把互通一次，被控机就能读全部设备数据 —— 这是曾经出过
// 事故的地方，所以这里连「先放行后补判」的写法都不留。

use crate::paths::Ctx;
use crate::routes::common::{http_err, q};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;

/// 常量时间比较（与 `auth.rs` 同款）：普通 `==` 会泄露首个不同字节的位置。
fn eq_ct(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// 每次现读 config.json：Python 的 `cfg` 是活的（`ensure_token` 会往内存里塞新键），
/// 迁移期两边共用一份配置文件，缓存住就会「Python 换了令牌，Rust 还认旧的」。
pub fn cfg(ctx: &Ctx) -> serde_json::Value {
    std::fs::read_to_string(crate::paths::config_file())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| ctx.cfg.clone())
}

fn tok_of(cfg: &serde_json::Value, key: &str) -> String {
    cfg.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// `agent_auth`：`x_agent_token or k or ""`，只认 fleet_token。
pub fn agent_ok(cfg: &serde_json::Value, headers: &HeaderMap, uri: &Uri) -> bool {
    let supplied = {
        let h = header(headers, "x-agent-token");
        if !h.is_empty() {
            h
        } else {
            q(uri, "k")
        }
    };
    let want = tok_of(cfg, "fleet_token");
    if want.is_empty() {
        return false;
    }
    eq_ct(&want, &supplied)
}

pub fn agent_gate(cfg: &serde_json::Value, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if agent_ok(cfg, headers, uri) {
        None
    } else {
        Some(http_err(401, "agent 令牌无效"))
    }
}

/// `_is_local`：源地址必须是本机 **且** Host 必须是本机写法。
///
/// 源地址这一半拿不到（main.rs 的 `axum::serve` 没挂 ConnectInfo，且只绑
/// 127.0.0.1），所以这里按「对端永远是回环」处理 —— 与实际部署一致。
/// Host 这一半是防 DNS rebinding 的关键，照抄不动。
pub fn is_local(headers: &HeaderMap) -> bool {
    let host = header(headers, "host").trim().to_ascii_lowercase();
    host == "127.0.0.1"
        || host == "localhost"
        || host == "[::1]"
        || host.starts_with("127.0.0.1:")
        || host.starts_with("localhost:")
        || host.starts_with("[::1]:")
}

/// `view_auth`：本机直接放行（**不挡主人**）；否则 `k or x_view_token or ""`。
pub fn view_gate(cfg: &serde_json::Value, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    if is_local(headers) {
        return None;
    }
    let supplied = {
        let k = q(uri, "k");
        if !k.is_empty() {
            k
        } else {
            header(headers, "x-view-token")
        }
    };
    let want = tok_of(cfg, "view_token");
    if want.is_empty() || !eq_ct(&want, &supplied) {
        return Some(http_err(401, "查看令牌无效"));
    }
    None
}

/// `view_auth` 的返回值：本机是 `local`，远端是 `remote:{host}`。
/// 远端那一段拿不到对端地址（没挂 ConnectInfo），只能写成 `remote:` ——
/// 这个值只进 `commands.issued_from` 这一列，不参与任何判定。
pub fn actor(headers: &HeaderMap) -> String {
    if is_local(headers) {
        "local".to_string()
    } else {
        "remote:".to_string()
    }
}

/// `/api/fleet/report` 的体积闸门（1MB）：在解析 body **之前**按 Content-Length 拦。
pub const MAX_REPORT_BYTES: u64 = 1024 * 1024;

pub fn content_length(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(axum::http::header::CONTENT_LENGTH)?.to_str().ok()?;
    if raw.is_empty() || !raw.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    raw.parse::<u64>().ok()
}
