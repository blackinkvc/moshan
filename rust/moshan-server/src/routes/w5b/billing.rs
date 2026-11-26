// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：计费账号（`moshan/billing.py` 的 Rust 版）—— 5 条路由：
//       GET  /api/billing           整份：账号清单 + 合计
//       GET  /api/billing/providers 供应商登记表（表单字段顺序就是契约）
//       POST /api/billing/save      新建/改一个账号
//       POST /api/billing/del       删一个账号
//       POST /api/billing/pull      真去供应商那边要一次账（联网）
// 不做：本地按 token 的估算（`_billing_est`）—— 它读 Cherry Studio 的 sqlite，
//       而 Cargo.toml 里没有 sqlite 依赖。所以 `est`/`gap` 两个字段不会出现，
//       详见汇报里的「已知分叉」。
//
// 落盘格式必须与 Python 逐字节一致：`json.dumps(..., ensure_ascii=False, indent=1)`，
// 权限 600（里面有 AccessKey）。写盘走 `util::write_private`。

use super::util::{
    clip, fnum, mask, num2, now_hms, now_ym, now_ymd_hms, py_str, str_or, truthy,
};
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

const UA: &str = "cuntu-billing/1.0";
const TIMEOUT_SECS: u64 = 12;

fn store_path(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("_本机工作").join("计费账号.json")
}

/// 供应商登记表。**顺序即表单顺序**，`fields` 的键序也是契约。
fn providers() -> Value {
    json!({
        "bailian": {
            "name": "阿里百炼", "short": "百炼", "vendor": "阿里云",
            "bill": true,
            "fields": [
                {"k": "ak", "label": "AccessKey ID", "ph": "LTAI5t…", "hint": "阿里云控制台 → 右上头像 → AccessKey 管理"},
                {"k": "sk", "label": "AccessKey Secret", "ph": "（只在本机保存）", "secret": 1},
                {"k": "key", "label": "百炼 API Key（可选）", "ph": "sk-…", "secret": 1, "opt": 1,
                 "hint": "填了也读不了账，只用来验这个号是不是百炼的"}
            ],
            "note": "AK 需要 AliyunBSSReadOnlyAccess 权限；账单按产品拆，自动挑出「百炼 / 灵积」那一行。"
        },
        "silicon": {
            "name": "硅基流动", "short": "硅基", "vendor": "硅基流动",
            "bill": false,
            "fields": [
                {"k": "key", "label": "API Key", "ph": "sk-…", "secret": 1,
                 "hint": "siliconflow.cn → API 密钥"}
            ],
            "note": "官方只给余额（总 / 充值 / 赠送三档），没有按月账单；这里存每次拉取的余额，用余额差看消耗。"
        },
        "deepseek": {
            "name": "DeepSeek", "short": "DS", "vendor": "深度求索",
            "bill": false,
            "fields": [{"k": "key", "label": "API Key", "ph": "sk-…", "secret": 1}],
            "note": "只给余额（总 / 赠送 / 充值），没有账单接口。"
        },
        "moonshot": {
            "name": "月之暗面 Kimi", "short": "Kimi", "vendor": "月之暗面",
            "bill": false,
            "fields": [{"k": "key", "label": "API Key", "ph": "sk-…", "secret": 1}],
            "note": "余额分可用 / 代金券 / 现金三档。"
        },
        "openrouter": {
            "name": "OpenRouter", "short": "OR", "vendor": "OpenRouter",
            "bill": false, "usd": true,
            "fields": [{"k": "key", "label": "API Key", "ph": "sk-or-…", "secret": 1}],
            "note": "美元计。给的是「额度 − 已用」，没有按月账单。"
        },
        "manual": {
            "name": "手动录入", "short": "手录", "vendor": "自定义",
            "bill": true, "offline": true,
            "fields": [
                {"k": "balance", "label": "余额", "ph": "0.00", "num": 1},
                {"k": "month", "label": "本月账单", "ph": "0.00", "num": 1},
                {"k": "cur", "label": "币种", "ph": "CNY", "opt": 1}
            ],
            "note": "没接口的供应商（智谱 / 火山 / 腾讯混元…）用这档：账单页上抄两个数进来，一样能对账。"
        }
    })
}

fn spec_of(prov: &str) -> Option<Value> {
    let all = providers();
    all.get(prov).cloned()
}

fn spec_fields(spec: &Value) -> Vec<Value> {
    spec.get("fields")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

/// `_read()`：任何形态不对都退化成 `{"accounts": []}`；其余键原样保留。
fn read_store(ctx: &Ctx) -> Value {
    let p = store_path(ctx);
    let mut d = match jsonio::read_json(&p) {
        Some(Value::Object(m)) => Value::Object(m),
        _ => json!({ "accounts": [] }),
    };
    let is_list = d.get("accounts").map(|v| v.is_array()).unwrap_or(false);
    if !is_list {
        if let Value::Object(m) = &mut d {
            m.insert("accounts".into(), json!([]));
        }
    }
    d
}

fn public_of(acc: &Value) -> Value {
    let pid = acc.get("provider").and_then(|v| v.as_str()).unwrap_or("");
    let spec = spec_of(pid).unwrap_or(Value::Object(Map::new()));
    let fields_obj = acc.get("fields").cloned().unwrap_or(json!({}));

    let mut f = Map::new();
    let mut filled = Map::new();
    for spec_item in spec_fields(&spec) {
        let k = spec_item.get("k").and_then(|v| v.as_str()).unwrap_or("");
        let v = fields_obj.get(k).cloned().unwrap_or(Value::Null);
        let secret = truthy(&spec_item.get("secret").cloned().unwrap_or(Value::Null));
        let cell = if secret {
            if truthy(&v) {
                Value::String(mask(&py_str(&v)))
            } else {
                Value::String(String::new())
            }
        } else if v.is_null() {
            Value::String(String::new())
        } else {
            v.clone()
        };
        f.insert(k.to_string(), cell);
        filled.insert(k.to_string(), Value::Bool(truthy(&v)));
    }

    let prov_name = spec
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| acc.get("provider").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string()))
        .unwrap_or_else(|| "?".to_string());
    let prov_short = spec
        .get("short")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| clip(&prov_name, 1));

    let hist: Vec<Value> = acc
        .get("hist")
        .and_then(|v| v.as_array())
        .map(|a| {
            let start = a.len().saturating_sub(60);
            a[start..].to_vec()
        })
        .unwrap_or_default();

    json!({
        "id": acc.get("id").map(|v| if truthy(v) { v.clone() } else { json!("") }).unwrap_or(json!("")),
        "provider": acc.get("provider").map(|v| if truthy(v) { v.clone() } else { json!("") }).unwrap_or(json!("")),
        "provider_name": prov_name,
        "provider_short": prov_short,
        "name": acc.get("name").map(|v| if truthy(v) { v.clone() } else { json!("") }).unwrap_or(json!("")),
        "note": acc.get("note").map(|v| if truthy(v) { v.clone() } else { json!("") }).unwrap_or(json!("")),
        "fields": Value::Object(f),
        "filled": Value::Object(filled),
        "last": acc.get("last").map(|v| if truthy(v) { v.clone() } else { json!({}) }).unwrap_or(json!({})),
        "hist": hist,
        "can_pull": spec.is_object() && spec.as_object().map(|m| !m.is_empty()).unwrap_or(false),
        "bill": truthy(&spec.get("bill").cloned().unwrap_or(Value::Null)),
        "usd": truthy(&spec.get("usd").cloned().unwrap_or(Value::Null)),
    })
}

/// 数字的「是不是浮点」—— Python 里 int - int 还是 int，`round(0, 2)` 输出 `0`。
fn numf(v: Option<&Value>) -> (f64, bool) {
    match v {
        Some(Value::Number(n)) => {
            let f = n.as_f64().unwrap_or(0.0);
            (f, n.is_f64())
        }
        Some(x) if truthy(x) => match super::util::py_float(x) {
            Some(f) => (f, true),
            None => (0.0, false),
        },
        _ => (0.0, false),
    }
}

fn sub_out(a: (f64, bool), b: (f64, bool)) -> Value {
    if a.1 || b.1 {
        fnum(super::util::round2(a.0 - b.0))
    } else {
        json!(a.0 as i64 - b.0 as i64)
    }
}

fn delta_of(acc: &Value) -> Value {
    let hist: Vec<Value> = acc
        .get("hist")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Map::new();
    if hist.len() >= 2 {
        let last = &hist[hist.len() - 1];
        let prev = &hist[hist.len() - 2];
        out.insert(
            "d_prev".into(),
            sub_out(numf(last.get("b")), numf(prev.get("b"))),
        );
        out.insert("t_prev".into(), prev.get("t").cloned().unwrap_or(Value::Null));
    }
    if !hist.is_empty() {
        let ym = now_ym();
        let mut base: Option<&Value> = None;
        for h in &hist {
            let t = h.get("t").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let secs = t as i64;
            let here = chrono::TimeZone::timestamp_opt(&chrono::Local, secs, 0)
                .single()
                .map(|d| d.format("%Y-%m").to_string())
                .unwrap_or_default();
            if here == ym {
                base = Some(h);
                break;
            }
        }
        let base = base.unwrap_or(&hist[0]);
        let last = &hist[hist.len() - 1];
        out.insert(
            "d_month".into(),
            sub_out(numf(last.get("b")), numf(base.get("b"))),
        );
        out.insert("t_month".into(), base.get("t").cloned().unwrap_or(Value::Null));
        out.insert("n".into(), json!(hist.len()));
    }
    Value::Object(out)
}

// ---------------------------------------------------------------- 路由

pub async fn billing(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    Json(view(&ctx)).into_response()
}

pub async fn providers_api(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let items: Vec<Value> = providers()
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| {
                    json!({
                        "id": k,
                        "name": v.get("name").cloned().unwrap_or(Value::Null),
                        "vendor": v.get("vendor").cloned().unwrap_or(Value::Null),
                        "bill": truthy(&v.get("bill").cloned().unwrap_or(Value::Null)),
                        "offline": truthy(&v.get("offline").cloned().unwrap_or(Value::Null)),
                        "usd": truthy(&v.get("usd").cloned().unwrap_or(Value::Null)),
                        "fields": v.get("fields").cloned().unwrap_or(json!([])),
                        "note": v.get("note").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Json(json!({ "ok": true, "items": items })).into_response()
}

pub async fn save_api(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    // Python: `body: dict | None = None` —— 空体当 {}，坏 JSON 仍是 422
    let body = match super::util::optional_body(&body) {
        Ok(v) => v,
        Err((code, body)) => return (axum::http::StatusCode::from_u16(code).unwrap(), Json(body)).into_response(),
    };
    Json(save_account(&ctx, &body)).into_response()
}

pub async fn del_api(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match super::util::optional_body(&body) {
        Ok(v) => v,
        Err((code, b)) => return (axum::http::StatusCode::from_u16(code).unwrap(), Json(b)).into_response(),
    };
    let id = str_or(body.get("id"));
    Json(del_account(&ctx, &id)).into_response()
}

pub async fn pull_api(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: axum::body::Bytes,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match super::util::optional_body(&body) {
        Ok(v) => v,
        Err((code, b)) => return (axum::http::StatusCode::from_u16(code).unwrap(), Json(b)).into_response(),
    };
    let aid = str_or(body.get("id"));
    let ids = body.get("ids").cloned().unwrap_or(Value::Null);
    // `(body or {}).get("ids") or None`：空表/空串都当「没指定」
    let want: Vec<String> = if truthy(&ids) {
        match &ids {
            Value::Array(a) => a.iter().filter(|x| truthy(x)).map(py_str).collect(),
            Value::String(s) => s.chars().map(|c| c.to_string()).collect(),
            Value::Object(m) => m.keys().cloned().collect(),
            other => vec![py_str(other)],
        }
    } else {
        Vec::new()
    };
    Json(pull(&ctx, &aid, &want).await).into_response()
}

// ---------------------------------------------------------------- 写

pub fn save_account(ctx: &Ctx, body: &Value) -> Value {
    let mut d = read_store(ctx);
    let mut accs: Vec<Value> = d
        .get("accounts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let pid = str_or(body.get("id")).trim().to_string();
    let prov = str_or(body.get("provider")).trim().to_string();
    let Some(spec) = spec_of(&prov) else {
        return json!({ "ok": false, "error": "供应商不认识" });
    };

    let mut idx = accs
        .iter()
        .position(|a| a.get("id").and_then(|v| v.as_str()).map(|s| s == pid).unwrap_or(false));

    if idx.is_none() {
        let mut name = str_or(body.get("name")).trim().to_string();
        if name.is_empty() {
            name = spec
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
        let dup = accs.iter().any(|a| {
            a.get("name").and_then(|v| v.as_str()) == Some(name.as_str())
                && a.get("provider").and_then(|v| v.as_str()) == Some(prov.as_str())
        });
        if dup {
            name.push_str(" 2");
        }
        accs.push(json!({
            "id": super::util::rand_hex(8),
            "provider": prov.clone(),
            "name": name,
            "note": "",
            "fields": {},
            "last": {},
            "hist": [],
        }));
        idx = Some(accs.len() - 1);
    }
    let i = idx.unwrap();

    let cur_prov = accs[i]
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if cur_prov != prov {
        if let Value::Object(m) = &mut accs[i] {
            m.insert("provider".into(), json!(prov));
            m.insert("last".into(), json!({}));
        }
    }
    let name_raw = str_or(body.get("name")).trim().to_string();
    if !name_raw.is_empty() {
        if let Value::Object(m) = &mut accs[i] {
            m.insert("name".into(), json!(clip(&name_raw, 40)));
        }
    }
    if body.get("note").is_some() {
        let note = clip(str_or(body.get("note")).trim(), 120);
        if let Value::Object(m) = &mut accs[i] {
            m.insert("note".into(), json!(note));
        }
    }

    let specs = spec_fields(&spec);
    let mut nf: Map<String, Value> = match accs[i].get("fields") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    if let Some(Value::Object(fields)) = body.get("fields") {
        for (k, v) in fields {
            let Some(sp) = specs
                .iter()
                .find(|s| s.get("k").and_then(|x| x.as_str()) == Some(k.as_str()))
            else {
                continue;
            };
            let vv = str_or(Some(v)).trim().to_string();
            if truthy(&sp.get("secret").cloned().unwrap_or(Value::Null))
                && (vv.contains('…') || vv.contains('•'))
            {
                continue; // 打码串 = 没改
            }
            nf.insert(k.clone(), json!(vv));
        }
    }
    if let Value::Object(m) = &mut accs[i] {
        m.insert("fields".into(), Value::Object(nf.clone()));
    }

    let mut miss: Vec<String> = Vec::new();
    for sp in &specs {
        let k = sp.get("k").and_then(|v| v.as_str()).unwrap_or("");
        let opt = truthy(&sp.get("opt").cloned().unwrap_or(Value::Null));
        let numeric = truthy(&sp.get("num").cloned().unwrap_or(Value::Null));
        if !opt && !numeric && !truthy(nf.get(k).unwrap_or(&Value::Null)) {
            miss.push(
                sp.get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            );
        }
    }
    if !miss.is_empty() {
        return json!({ "ok": false, "error": format!("还缺 {}", miss.join("、")) });
    }

    let account = public_of(&accs[i]);
    if let Value::Object(m) = &mut d {
        m.insert("accounts".into(), Value::Array(accs));
    }
    let _ = util_write(ctx, &d);
    json!({ "ok": true, "account": account })
}

fn util_write(ctx: &Ctx, d: &Value) -> std::io::Result<()> {
    super::util::write_private(&store_path(ctx), d)
}

pub fn del_account(ctx: &Ctx, aid: &str) -> Value {
    let mut d = read_store(ctx);
    let before = d
        .get("accounts")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let kept: Vec<Value> = d
        .get("accounts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|x| x.get("id").and_then(|v| v.as_str()) != Some(aid))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if let Value::Object(m) = &mut d {
        m.insert("accounts".into(), Value::Array(kept.clone()));
    }
    let _ = util_write(ctx, &d);
    json!({ "ok": true, "removed": before - kept.len() })
}

pub fn view(ctx: &Ctx) -> Value {
    let d = read_store(ctx);
    let accs = d
        .get("accounts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut rows: Vec<Value> = Vec::new();
    for a in &accs {
        let mut p = public_of(a);
        if let Value::Object(m) = &mut p {
            m.insert("delta".into(), delta_of(a));
        }
        rows.push(p);
    }
    let cny: Vec<&Value> = rows
        .iter()
        .filter(|r| !truthy(r.get("usd").unwrap_or(&Value::Null)))
        .collect();

    let mut bal = 0.0f64;
    let mut bal_float = false;
    let mut month = 0.0f64;
    let mut month_n = 0usize;
    for r in &cny {
        let last = r.get("last").cloned().unwrap_or(json!({}));
        if !truthy(last.get("ok").unwrap_or(&Value::Null)) {
            continue;
        }
        match last.get("balance") {
            Some(x) if truthy(x) => {
                if let Some(f) = super::util::py_float(x) {
                    bal += f;
                    bal_float = true;
                }
            }
            _ => {
                bal += 0.0;
            }
        }
        month += super::util::py_float(last.get("month").unwrap_or(&Value::Null)).unwrap_or(0.0);
        month_n += 1;
    }
    let total_balance = if bal_float {
        fnum(super::util::round2(bal))
    } else {
        json!(0)
    };
    // `sum(...)` 空序列是 int 0 —— Python 那边会输出 `0` 而不是 `0.0`
    let total_month = if month_n == 0 {
        json!(0)
    } else {
        fnum(super::util::round2(month))
    };

    json!({
        "ok": true,
        "accounts": rows,
        "store": store_path(ctx).to_string_lossy(),
        "n": rows.len(),
        "total_balance": total_balance,
        "total_month": total_month,
        "at": now_hms(),
    })
}

// ---------------------------------------------------------------- 联网

fn urlerr(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "URLError: <urlopen error timed out>".to_string();
    }
    if e.is_connect() {
        let text = format!("{e}");
        if text.contains("refused") || text.contains("61") {
            return "URLError: <urlopen error [Errno 61] Connection refused>".to_string();
        }
        if text.contains("dns") || text.contains("name") {
            return "URLError: <urlopen error [Errno 8] nodename nor servname provided, or not known>"
                .to_string();
        }
        return format!("URLError: <urlopen error {text}>");
    }
    format!("URLError: <urlopen error {e}>")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// `_http()`：返回 (状态码, 文本)；连不上回 (0, "Type: msg")。
async fn http(url: &str, headers: &[(String, String)], data: Option<Vec<u8>>) -> (i64, String) {
    let mut req = client().get(url).header("User-Agent", UA);
    for (k, v) in headers {
        req = req.header(k.as_str(), v.as_str());
    }
    if let Some(d) = data {
        req = client().post(url).header("User-Agent", UA).body(d);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
    }
    match req.send().await {
        Ok(resp) => {
            let code = resp.status().as_u16() as i64;
            match resp.text().await {
                Ok(t) => {
                    let limit = if code >= 400 { 20000 } else { 200000 };
                    (code, clip_bytes(&t, limit))
                }
                Err(e) => (0, urlerr(&e)),
            }
        }
        Err(e) => (0, urlerr(&e)),
    }
}

/// 按**字节**截断再解码（Python 是 `read(N).decode("utf-8","replace")`）。
fn clip_bytes(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    let mut end = n;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

async fn jget(url: &str, key: &str) -> (Option<Value>, String) {
    let headers = if key.is_empty() {
        vec![]
    } else {
        vec![("Authorization".to_string(), format!("Bearer {key}"))]
    };
    let (code, txt) = http(url, &headers, None).await;
    if code == 0 {
        return (None, txt);
    }
    match serde_json::from_str::<Value>(&txt) {
        Ok(v) => (Some(v), String::new()),
        Err(_) => (None, format!("HTTP {}：{}", code, clip(&txt, 120))),
    }
}

fn err_of(obj: &Value) -> String {
    if !truthy(obj) {
        return String::new();
    }
    if let Some(e) = obj.get("error") {
        match e {
            Value::Object(m) => {
                for k in ["message", "code"] {
                    if let Some(v) = m.get(k) {
                        if truthy(v) {
                            return py_str(v);
                        }
                    }
                }
                return String::new();
            }
            Value::String(s) => return s.clone(),
            _ => {}
        }
    }
    for k in ["message", "msg", "Message", "detail", "code"] {
        if let Some(v) = obj.get(k) {
            if truthy(v) {
                return py_str(v);
            }
        }
    }
    String::new()
}

/// `urllib.parse.quote(s, safe="")` + 三个替换（阿里云签名要求）。
fn pe(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        let c = *b as char;
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let mut h = Sha1::new();
        h.update(key);
        let d = h.finalize();
        k[..20].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha1::new();
    inner.update(ipad);
    inner.update(msg);
    let id = inner.finalize();
    let mut outer = Sha1::new();
    outer.update(opad);
    outer.update(id);
    outer.finalize().to_vec()
}

async fn ali(ak: &str, sk: &str, action: &str, extra: &[(String, String)]) -> (Option<Value>, String) {
    let mut p: Vec<(String, String)> = vec![
        ("Action".into(), action.into()),
        ("Format".into(), "JSON".into()),
        ("Version".into(), "2017-12-14".into()),
        ("AccessKeyId".into(), ak.into()),
        ("SignatureMethod".into(), "HMAC-SHA1".into()),
        ("SignatureVersion".into(), "1.0".into()),
        ("SignatureNonce".into(), super::util::rand_hex(32)),
        (
            "Timestamp".into(),
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        ),
    ];
    for (k, v) in extra {
        p.push((k.clone(), v.clone()));
    }
    p.sort_by(|a, b| a.0.cmp(&b.0));
    let q = p
        .iter()
        .map(|(k, v)| format!("{}={}", pe(k), pe(v)))
        .collect::<Vec<_>>()
        .join("&");
    let sts = format!("GET&%2F&{}", pe(&q));
    let sig = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(hmac_sha1(format!("{sk}&").as_bytes(), sts.as_bytes()))
    };
    let url = format!("https://business.aliyuncs.com/?{}&Signature={}", q, pe(&sig));
    let (obj, err) = jget(&url, "").await;
    if !err.is_empty() {
        return (None, err);
    }
    if let Some(o) = &obj {
        if let Some(code) = o.get("Code") {
            if truthy(code) {
                return (
                    None,
                    format!(
                        "{}：{}",
                        py_str(code),
                        o.get("Message").map(py_str).unwrap_or_default()
                    ),
                );
            }
        }
    }
    (obj, String::new())
}

fn f_of(v: Option<&Value>) -> f64 {
    super::util::py_float(v.unwrap_or(&Value::Null)).unwrap_or(0.0)
}

async fn bill_month(ak: &str, sk: &str, ym: &str) -> (Option<Value>, String) {
    let extra: Vec<(String, String)> = vec![
        ("BillingCycle".into(), ym.to_string()),
        ("IsGroupByProduct".into(), "true".into()),
        ("Granularity".into(), "MONTHLY".into()),
        ("PageNum".into(), "1".into()),
        ("PageSize".into(), "300".into()),
    ];
    let (bill, berr) = ali(ak, sk, "QueryAccountBill", &extra).await;
    if !berr.is_empty() {
        return (None, berr);
    }
    let bd = bill
        .as_ref()
        .and_then(|b| b.get("Data").cloned())
        .unwrap_or(json!({}));
    let bd = if truthy(&bd) { bd } else { json!({}) };
    let mut items = bd
        .get("Items")
        .and_then(|i| i.get("Item").cloned())
        .unwrap_or(json!([]));
    if items.is_object() {
        items = json!([items]);
    }
    let mut net = 0.0;
    let mut rows: Vec<(f64, Value)> = Vec::new();
    for it in items.as_array().cloned().unwrap_or_default() {
        let g = f_of(it.get("PretaxGrossAmount"));
        let d_ = f_of(it.get("DeductedByCoupons"))
            + f_of(it.get("InvoiceDiscount"))
            + f_of(it.get("DeductedByPrepaidCard"))
            + f_of(it.get("DeductedByCashCoupons"));
        let paid = f_of(it.get("PaymentAmount"));
        let n = if paid != 0.0 { paid } else { (g - d_).max(0.0) };
        let nm = {
            let a = it.get("ProductName").map(py_str).unwrap_or_default();
            let b = it.get("ProductCode").map(py_str).unwrap_or_default();
            if !a.is_empty() {
                a
            } else if !b.is_empty() {
                b
            } else {
                "?".to_string()
            }
        };
        net += n;
        rows.push((
            n,
            json!({
                "name": nm,
                "gross": fnum(super::util::round2(g)),
                "disc": fnum(super::util::round2(d_)),
                "net": fnum(super::util::round2(n)),
                "code": it.get("ProductCode").cloned().unwrap_or(json!("")),
            }),
        ));
    }
    // `rows.sort(key=lambda r: -r["net"])` —— 稳定排序，同额保持原序
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let rows: Vec<Value> = rows.into_iter().map(|(_, v)| v).collect();
    let hit: Vec<Value> = rows
        .iter()
        .filter(|r| {
            let nm = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
            ["百炼", "灵积", "DashScope", "dashscope"]
                .iter()
                .any(|w| nm.contains(w))
        })
        .cloned()
        .collect();
    let scoped = if hit.is_empty() { rows.clone() } else { hit.clone() };
    let sum = |key: &str| -> f64 {
        scoped
            .iter()
            .map(|r| r.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0))
            .sum()
    };
    let ym_out = bd.get("BillingCycle").map(py_str).unwrap_or_default();
    let ym_out = if ym_out.is_empty() || ym_out == "None" {
        ym.to_string()
    } else {
        ym_out
    };
    (
        Some(json!({
            "ym": ym_out,
            "net": fnum(super::util::round2(sum("net"))),
            "gross": fnum(super::util::round2(sum("gross"))),
            "disc": fnum(super::util::round2(sum("disc"))),
            "acct_net": fnum(super::util::round2(net)),
            "scoped": if hit.is_empty() { "全账号" } else { "百炼" },
            "items": rows.into_iter().take(8).collect::<Vec<_>>(),
        })),
        String::new(),
    )
}

async fn pull_bailian(a: &Value) -> Value {
    let f = a.get("fields").cloned().unwrap_or(json!({}));
    let ak = str_or(f.get("ak")).trim().to_string();
    let sk = str_or(f.get("sk")).trim().to_string();
    if ak.is_empty() || sk.is_empty() {
        return json!({"ok": false, "err": "要 AccessKey（AK + SK）。只用百炼 API Key 读不了账 —— 那个 key 只能调模型。"});
    }
    let (bal, err) = ali(&ak, &sk, "QueryAccountBalance", &[]).await;
    if !err.is_empty() {
        return json!({ "ok": false, "err": err });
    }
    let data = bal
        .as_ref()
        .and_then(|b| b.get("Data").cloned())
        .unwrap_or(json!({}));
    let data = if truthy(&data) { data } else { json!({}) };
    let mut out = json!({
        "ok": true,
        "balance": num2(data.get("AvailableAmount")).map(fnum).unwrap_or(Value::Null),
        "cash": num2(data.get("AvailableCashAmount")).map(fnum).unwrap_or(Value::Null),
        "gift": Value::Null,
        "credit": num2(data.get("CreditAmount")).map(fnum).unwrap_or(Value::Null),
        "currency": data.get("Currency").map(|v| if truthy(v) { v.clone() } else { json!("CNY") }).unwrap_or(json!("CNY")),
        "items": [],
    });

    // 最近四个账期
    let now = chrono::Local::now();
    let mut yms: Vec<String> = Vec::new();
    let mut cur = now.date_naive();
    for _ in 0..4 {
        yms.push(cur.format("%Y-%m").to_string());
        let first = chrono::NaiveDate::from_ymd_opt(
            chrono::Datelike::year(&cur),
            chrono::Datelike::month(&cur),
            1,
        )
        .unwrap_or(cur);
        cur = first - chrono::Duration::days(1);
    }
    let mut got: Vec<Value> = Vec::new();
    let mut first_err = String::new();
    for ym in &yms {
        let (r, berr) = bill_month(&ak, &sk, ym).await;
        if !berr.is_empty() {
            if first_err.is_empty() {
                first_err = berr;
            }
            continue;
        }
        if let Some(v) = r {
            if truthy(&v) {
                got.push(v);
            }
        }
    }
    let o = out.as_object_mut().unwrap();
    if !first_err.is_empty() && got.is_empty() {
        o.insert("bill_err".into(), json!(first_err));
        return out;
    }
    let find = |k: &str, v: &str| -> Option<Value> {
        got.iter()
            .find(|r| r.get(k).and_then(|x| x.as_str()) == Some(v))
            .cloned()
    };
    let cur_m = find("ym", &yms[0]);
    let paid = got
        .iter()
        .find(|r| r.get("net").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.0)
        .cloned();
    o.insert("months".into(), Value::Array(got.clone()));
    let g = |k: &str| -> Value {
        cur_m
            .as_ref()
            .and_then(|m| m.get(k).cloned())
            .unwrap_or(Value::Null)
    };
    o.insert("month".into(), g("net"));
    o.insert(
        "month_ym".into(),
        match g("ym") {
            Value::Null => json!(yms[0]),
            v if truthy(&v) => v,
            _ => json!(yms[0]),
        },
    );
    o.insert("month_gross".into(), g("gross"));
    o.insert("month_disc".into(), g("disc"));
    o.insert("acct_net".into(), g("acct_net"));
    o.insert(
        "scoped".into(),
        match g("scoped") {
            Value::Null => Value::Null,
            v => v,
        },
    );
    let items = match g("items") {
        Value::Array(a) if !a.is_empty() => Value::Array(a),
        _ => json!([]),
    };
    o.insert("items".into(), items);
    if let Some(p) = &paid {
        let pym = p.get("ym").and_then(|v| v.as_str()).unwrap_or("");
        if pym != yms[0] {
            o.insert("paid".into(), p.clone());
        }
    }
    if !first_err.is_empty() {
        o.insert("bill_err".into(), json!(first_err));
    }
    out
}

fn key_of(a: &Value) -> String {
    str_or(a.get("fields").and_then(|f| f.get("key"))).trim().to_string()
}

async fn pull_silicon(a: &Value) -> Value {
    let key = key_of(a);
    if key.is_empty() {
        return json!({"ok": false, "err": "缺 API Key"});
    }
    let (obj, err) = jget("https://api.siliconflow.cn/v1/user/info", &key).await;
    if !err.is_empty() {
        return json!({"ok": false, "err": err});
    }
    let o = obj.clone().unwrap_or(Value::Null);
    let bad = !truthy(&o)
        || {
            let c = o.get("code").cloned().unwrap_or(Value::Null);
            !(c.is_null()
                || c.as_i64() == Some(20000)
                || c.as_i64() == Some(0))
        }
        || o.get("status").map(|v| *v == Value::Bool(false)).unwrap_or(false);
    if bad {
        let e = err_of(&o);
        return json!({"ok": false, "err": if e.is_empty() { "接口没认这个 key".to_string() } else { e }});
    }
    let d = o.get("data").cloned().unwrap_or(json!({}));
    let d = if truthy(&d) { d } else { json!({}) };
    json!({
        "ok": true, "currency": "CNY",
        "balance": num2(d.get("totalBalance")).map(fnum).unwrap_or(Value::Null),
        "gift": num2(d.get("balance")).map(fnum).unwrap_or(Value::Null),
        "cash": num2(d.get("chargeBalance")).map(fnum).unwrap_or(Value::Null),
        "who": d.get("name").map(|v| if truthy(v) { v.clone() } else { json!("") }).unwrap_or(json!("")),
        "items": [],
    })
}

async fn pull_deepseek(a: &Value) -> Value {
    let key = key_of(a);
    if key.is_empty() {
        return json!({"ok": false, "err": "缺 API Key"});
    }
    let (obj, err) = jget("https://api.deepseek.com/user/balance", &key).await;
    if !err.is_empty() {
        return json!({"ok": false, "err": err});
    }
    let o = obj.clone().unwrap_or(Value::Null);
    if !truthy(&o) || o.get("error").map(truthy).unwrap_or(false) {
        let e = err_of(&o);
        return json!({"ok": false, "err": if e.is_empty() { "接口没认这个 key".to_string() } else { e }});
    }
    let bi = o
        .get("balance_infos")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_else(|| vec![json!({})]);
    let d = bi.first().cloned().unwrap_or(json!({}));
    json!({
        "ok": true,
        "currency": d.get("currency").map(|v| if truthy(v) { v.clone() } else { json!("CNY") }).unwrap_or(json!("CNY")),
        "balance": num2(d.get("total_balance")).map(fnum).unwrap_or(Value::Null),
        "gift": num2(d.get("granted_balance")).map(fnum).unwrap_or(Value::Null),
        "cash": num2(d.get("topped_up_balance")).map(fnum).unwrap_or(Value::Null),
        "alive": truthy(&o.get("is_available").cloned().unwrap_or(Value::Null)),
        "items": [],
    })
}

async fn pull_moonshot(a: &Value) -> Value {
    let key = key_of(a);
    if key.is_empty() {
        return json!({"ok": false, "err": "缺 API Key"});
    }
    let (obj, err) = jget("https://api.moonshot.cn/v1/users/me/balance", &key).await;
    if !err.is_empty() {
        return json!({"ok": false, "err": err});
    }
    let o = obj.clone().unwrap_or(Value::Null);
    if !truthy(&o) || o.get("error").map(truthy).unwrap_or(false) {
        let e = err_of(&o);
        return json!({"ok": false, "err": if e.is_empty() { "接口没认这个 key".to_string() } else { e }});
    }
    let d = o.get("data").cloned().unwrap_or(json!({}));
    let d = if truthy(&d) { d } else { json!({}) };
    json!({
        "ok": true, "currency": "CNY",
        "balance": num2(d.get("available_balance")).map(fnum).unwrap_or(Value::Null),
        "gift": num2(d.get("voucher_balance")).map(fnum).unwrap_or(Value::Null),
        "cash": num2(d.get("cash_balance")).map(fnum).unwrap_or(Value::Null),
        "items": [],
    })
}

async fn pull_openrouter(a: &Value) -> Value {
    let key = key_of(a);
    if key.is_empty() {
        return json!({"ok": false, "err": "缺 API Key"});
    }
    let (obj, err) = jget("https://openrouter.ai/api/v1/credits", &key).await;
    if !err.is_empty() {
        return json!({"ok": false, "err": err});
    }
    let o = obj.clone().unwrap_or(Value::Null);
    if !truthy(&o) || o.get("error").map(truthy).unwrap_or(false) {
        let e = err_of(&o);
        return json!({"ok": false, "err": if e.is_empty() { "接口没认这个 key".to_string() } else { e }});
    }
    let d = o.get("data").cloned().unwrap_or(json!({}));
    let d = if truthy(&d) { d } else { json!({}) };
    let cr = num2(d.get("total_credits"));
    let us = num2(d.get("total_usage"));
    json!({
        "ok": true, "currency": "USD", "usd": true,
        "balance": fnum(super::util::round2(cr.unwrap_or(0.0) - us.unwrap_or(0.0))),
        "spent": us.map(fnum).unwrap_or(Value::Null),
        "credit": cr.map(fnum).unwrap_or(Value::Null),
        "items": [],
    })
}

fn pull_manual(a: &Value) -> Value {
    let f = a.get("fields").cloned().unwrap_or(json!({}));
    let cur = {
        let raw = str_or(f.get("cur"));
        let raw = if raw.is_empty() { "CNY".to_string() } else { raw };
        clip(&raw.trim().to_uppercase(), 4)
    };
    json!({
        "ok": true, "currency": cur, "manual": true,
        "balance": num2(f.get("balance")).map(fnum).unwrap_or(Value::Null),
        "month": num2(f.get("month")).map(fnum).unwrap_or(Value::Null),
        "month_ym": now_ym(),
        "items": [],
    })
}

async fn pull_one(prov: &str, a: &Value) -> Value {
    match prov {
        "bailian" => pull_bailian(a).await,
        "silicon" => pull_silicon(a).await,
        "deepseek" => pull_deepseek(a).await,
        "moonshot" => pull_moonshot(a).await,
        "openrouter" => pull_openrouter(a).await,
        "manual" => pull_manual(a),
        _ => json!({ "ok": false, "err": "这个供应商还没有取数实现" }),
    }
}

pub async fn pull(ctx: &Ctx, aid: &str, want: &[String]) -> Value {
    let t0 = std::time::Instant::now();
    let mut d = read_store(ctx);
    let accs = d
        .get("accounts")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut todo: Vec<usize> = Vec::new();
    for (i, a) in accs.iter().enumerate() {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if (aid.is_empty() && want.is_empty()) || id == aid || want.iter().any(|w| w == id) {
            todo.push(i);
        }
    }
    if (!aid.is_empty() || !want.is_empty()) && todo.is_empty() {
        return json!({ "ok": false, "error": "没这个账号" });
    }

    let mut done: Vec<Value> = Vec::new();
    for i in todo {
        let a = accs[i].clone();
        let prov = a
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let t1 = std::time::Instant::now();
        let mut res = pull_one(&prov, &a).await;
        let ms = (t1.elapsed().as_secs_f64() * 1000.0) as i64;
        if let Value::Object(m) = &mut res {
            m.insert("at".into(), json!(now_ymd_hms()));
            m.insert("ms".into(), json!(ms));
            m.insert("provider".into(), json!(prov));
        }
        // 失败也记一笔（只是不记余额）
        if let Value::Object(m) = &mut d["accounts"][i] {
            m.insert("last".into(), res.clone());
            let ok = truthy(res.get("ok").unwrap_or(&Value::Null));
            let bal = res.get("balance").cloned().unwrap_or(Value::Null);
            if ok && !bal.is_null() {
                let mut hist = a
                    .get("hist")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                hist.push(json!({
                    "t": chrono::Local::now().timestamp(),
                    "b": bal,
                    "m": res.get("month").cloned().unwrap_or(Value::Null),
                }));
                let keep = hist.len().saturating_sub(120);
                m.insert("hist".into(), Value::Array(hist[keep..].to_vec()));
            }
        }
        let mut item = Map::new();
        item.insert(
            "id".into(),
            a.get("id").cloned().unwrap_or(Value::Null),
        );
        if let Value::Object(m) = &res {
            for (k, v) in m {
                item.insert(k.clone(), v.clone());
            }
        }
        done.push(Value::Object(item));
    }
    let _ = util_write(ctx, &d);
    let _ = t0;
    json!({ "ok": true, "items": done })
}
