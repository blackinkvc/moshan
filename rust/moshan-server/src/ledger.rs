// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：记账账本（`moshan/ledger.py`）的读取、汇总、改、删。逐条对齐字段与判定。
// 不做：不做 OCR 与裁图（那要起 Swift 工具 `ledger_shot`，属于上传那两条，
//       等 multipart 落地再一起迁）。这里只处理「账本.json 已经存在」之后的事。

use crate::jsonio;
use crate::paths::Ctx;
use chrono::{Local, NaiveDate};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub const DIR_NAME: &str = "12-记账";

pub fn root(ctx: &Ctx) -> PathBuf {
    ctx.allowed_roots()
        .first()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn ledger_dir(ctx: &Ctx) -> PathBuf {
    root(ctx).join(DIR_NAME)
}

fn ensure(ctx: &Ctx) -> PathBuf {
    let d = ledger_dir(ctx);
    for sub in ["图", "原图", "_回收站"] {
        let _ = std::fs::create_dir_all(d.join(sub));
    }
    d
}

pub fn book_path(ctx: &Ctx) -> PathBuf {
    ledger_dir(ctx).join("账本.json")
}

/// `load()`：只认 items（且必须有 id）与 updated_at，其余字段一律丢掉 ——
/// 账本里混进别的东西时，前端不该看见。
pub fn load(ctx: &Ctx) -> (Vec<Value>, String) {
    let raw = jsonio::read_json(&book_path(ctx)).unwrap_or(Value::Null);
    let items: Vec<Value> = raw
        .get("items")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter(|i| i.is_object() && truthy(i.get("id")))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let updated_at = raw.get("updated_at").and_then(|v| v.as_str()).unwrap_or("").to_string();
    (items, updated_at)
}

pub fn save(ctx: &Ctx, items: &[Value]) {
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    let mut book = Map::new();
    book.insert("items".into(), Value::Array(items.to_vec()));
    book.insert("updated_at".into(), json!(now));
    let _ = jsonio::atomic_write(&book_path(ctx), &jsonio::dumps(&Value::Object(book), 1));
}

pub fn get_item(ctx: &Ctx, iid: &str) -> Option<Value> {
    load(ctx).0.into_iter().find(|i| i.get("id").and_then(|v| v.as_str()) == Some(iid))
}

fn round2(x: f64) -> f64 {
    format!("{:.2}", x).parse().unwrap_or(x)
}

fn num_or_zero(v: Option<&Value>) -> f64 {
    v.and_then(|x| x.as_f64()).unwrap_or(0.0)
}

/// `summary()`：花掉/收回/在持/盈亏。字段顺序与 Python 完全一致。
pub fn summary(items: &[Value]) -> Value {
    let mut spent = 0.0;
    let mut recovered = 0.0;
    let mut owned_value = 0.0;
    let (mut owned, mut sold) = (0, 0);
    let mut pnl = 0.0;
    for i in items {
        let price = num_or_zero(i.get("price"));
        spent += price;
        if i.get("status").and_then(|v| v.as_str()) == Some("已卖") {
            sold += 1;
            let sp = num_or_zero(i.get("sold_price"));
            recovered += sp;
            pnl += sp - price;
        } else {
            owned += 1;
            owned_value += price;
        }
    }
    json!({
        "total": items.len(),
        "owned": owned,
        "sold": sold,
        "spent": round2(spent),
        "recovered": round2(recovered),
        "net": round2(spent - recovered),
        "owned_value": round2(owned_value),
        "pnl": round2(pnl),
    })
}

fn parse_date(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()
}

fn days(i: &Value) -> i64 {
    let Some(a) = parse_date(i.get("bought_at").and_then(|v| v.as_str()).unwrap_or("")) else {
        return 0;
    };
    let b = if i.get("status").and_then(|v| v.as_str()) == Some("已卖") {
        match parse_date(i.get("sold_at").and_then(|v| v.as_str()).unwrap_or("")) {
            Some(d) => d,
            None => return 0,
        }
    } else {
        Local::now().date_naive()
    };
    (b - a).num_days().max(0)
}

fn pnl(i: &Value) -> Value {
    if i.get("status").and_then(|v| v.as_str()) != Some("已卖") {
        return Value::Null;
    }
    json!(round2(num_or_zero(i.get("sold_price")) - num_or_zero(i.get("price"))))
}

fn str_or(v: Option<&Value>, d: &str) -> String {
    match v.and_then(|x| x.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => d.to_string(),
    }
}

/// `payload()`：卡片用，不带 OCR 全文。
pub fn payload(ctx: &Ctx) -> Value {
    let (items, updated_at) = load(ctx);
    let cards: Vec<Value> = items
        .iter()
        .map(|i| {
            json!({
                "id": i.get("id").cloned().unwrap_or(Value::Null),
                "name": str_or(i.get("name"), ""),
                "platform": str_or(i.get("platform"), ""),
                "bought_at": str_or(i.get("bought_at"), ""),
                "price": pick_num(i.get("price")),
                "category": str_or(i.get("category"), "其他"),
                "status": str_or(i.get("status"), "在用"),
                "sold_at": str_or(i.get("sold_at"), ""),
                "sold_price": i.get("sold_price").cloned().unwrap_or(Value::Null),
                "sold_to": str_or(i.get("sold_to"), ""),
                "image": str_or(i.get("image"), ""),
                "crop_src": str_or(i.get("crop_src"), "none"),
                // note 只给卡片一部分，避免一次响应几 MB
                "note": trunc_chars(&str_or(i.get("note"), ""), 400),
                "conf": i.get("conf").cloned().filter(|v| v.is_array()).unwrap_or(json!([])),
                "days": days(i),
                "pnl": pnl(i),
            })
        })
        .collect();
    json!({
        "summary": summary(&items),
        "items": cards,
        "dir": ledger_dir(ctx).to_string_lossy(),
        "updated_at": updated_at,
    })
}

/// `i.get("price") or 0`：0/缺失/null 都给 0，其余原样透传（保住 12.5 这种小数形态）。
fn pick_num(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Number(n)) if n.as_f64().unwrap_or(0.0) != 0.0 => Value::Number(n.clone()),
        _ => json!(0),
    }
}

fn trunc_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python 的 `str(v)`：布尔是 True/False、null 是 None、数字按最小形态。
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => if *b { "True" } else { "False" }.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

pub const EDITABLE: &[&str] = &[
    "name", "platform", "bought_at", "price", "category", "shop", "order_no",
    "note", "status", "sold_at", "sold_price", "sold_to",
    "image", "raw", "crop_src",
];

pub fn update(ctx: &Ctx, iid: &str, patch: &Value) -> Value {
    let (mut items, _) = load(ctx);
    let Some(idx) = items
        .iter()
        .position(|i| i.get("id").and_then(|v| v.as_str()) == Some(iid))
    else {
        return json!({"ok": false, "error": "没有这条记录"});
    };

    // 按 EDITABLE 的固定顺序处理（不是按 patch 的键序）：账本里字段顺序要稳定，
    // 用户在编辑器里打开时不该因为改了个字段就整块重排。
    for k in EDITABLE {
        let Some(v) = patch.get(*k) else { continue };
        let item = items[idx].as_object_mut().unwrap();
        match *k {
            "price" | "sold_price" => {
                let empty = v.is_null() || v.as_str().map(|s| s.is_empty()).unwrap_or(false);
                if empty {
                    item.insert((*k).into(), if *k == "sold_price" { Value::Null } else { json!(0) });
                } else {
                    let s = py_str(v).replace('¥', "").replace(',', "");
                    match s.trim().parse::<f64>() {
                        Ok(n) => {
                            item.insert((*k).into(), json!(round2(n)));
                        }
                        Err(_) => return json!({"ok": false, "error": format!("{k} 不是数字")}),
                    }
                }
            }
            "status" => {
                let s = py_str(v);
                let sold = matches!(s.as_str(), "已卖" | "sold" | "1" | "true");
                item.insert("status".into(), json!(if sold { "已卖" } else { "在用" }));
            }
            _ => {
                let limit = if *k == "note" { 4000 } else { 200 };
                item.insert((*k).into(), json!(trunc_chars(&py_str(v), limit)));
            }
        }
    }

    // 状态与卖出信息对齐：免得「已卖」但没价钱/没日期的记录混进统计。
    {
        let item = items[idx].as_object_mut().unwrap();
        if item.get("status").and_then(|v| v.as_str()) == Some("已卖") {
            if !truthy(item.get("sold_at")) {
                item.insert("sold_at".into(), json!(Local::now().format("%Y-%m-%d").to_string()));
            }
            if item.get("sold_price").map(|v| v.is_null()).unwrap_or(true) {
                item.insert("sold_price".into(), json!(0));
            }
        }
        item.insert(
            "updated_at".into(),
            json!(Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()),
        );
    }

    let out = items[idx].clone();
    save(ctx, &items);
    json!({"ok": true, "item": out})
}

pub fn remove(ctx: &Ctx, iid: &str) -> Value {
    let (mut items, _) = load(ctx);
    let Some(idx) = items
        .iter()
        .position(|i| i.get("id").and_then(|v| v.as_str()) == Some(iid))
    else {
        return json!({"ok": false, "error": "没有这条记录"});
    };
    let item = items[idx].clone();
    let d = ensure(ctx);
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let bin = d.join("_回收站").join(format!("{stamp}-{iid}"));
    let _ = std::fs::create_dir_all(&bin);

    // 图和原图一起挪走（还能捡回来），挪不动不算失败 —— 记录该删还是要删。
    for key in ["image", "raw"] {
        let rel = item.get(key).and_then(|v| v.as_str()).unwrap_or("");
        if rel.is_empty() {
            continue;
        }
        let p = root(ctx).join(rel);
        if p.exists() {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let _ = std::fs::rename(&p, bin.join(name));
        }
    }
    let _ = jsonio::atomic_write(&bin.join("item.json"), &jsonio::dumps(&item, 1));

    items.remove(idx);
    save(ctx, &items);
    json!({"ok": true, "trash": bin.to_string_lossy()})
}

pub fn full_item(ctx: &Ctx, iid: &str) -> Value {
    match get_item(ctx, iid) {
        Some(i) => {
            let ocr = i.get("ocr").and_then(|v| v.as_str()).unwrap_or("").to_string();
            json!({"ok": true, "item": i, "ocr": ocr})
        }
        None => json!({"ok": false, "error": "没有这条记录"}),
    }
}

/// 按 id 找图。账本里存的是**相对路径**（挪数据目录不至于全断），
/// 这里拼回绝对路径；不直接把相对路径交给通用文件接口，避免按服务端 CWD 解析越界。
pub fn image_file(ctx: &Ctx, iid: &str, raw: bool) -> Option<PathBuf> {
    let i = get_item(ctx, iid)?;
    let rel = if raw { i.get("raw") } else { i.get("image") }.and_then(|v| v.as_str()).unwrap_or("");
    if rel.is_empty() {
        return None;
    }
    let p = root(ctx).join(rel);
    p.is_file().then_some(p)
}

pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

pub fn mime_of(p: &Path) -> &'static str {
    match p
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}
