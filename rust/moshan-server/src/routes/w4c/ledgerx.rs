// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`/api/ledger/upload`、`/api/ledger/image`、`/api/ledger/open`
//       —— 记账三件与 multipart 有关的路由，连同 `ledger.ingest` /
//       `ledger.replace_image` / `parse_fields` 的移植。
// 不做：不改 ledger.rs 里已迁好的读/改/删。识别与裁图仍交给
//       `~/Documents/cuntu/bin/ledger_shot`（Swift + Vision），照抄调用方式。

#![allow(dead_code)]

use super::py;
use crate::ledger;
use crate::paths::Ctx;
use crate::routes::common::{http_err, peek, CtxState};
use axum::extract::{Multipart, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::Path;

const DIR_NAME: &str = "12-记账";
const UPLOAD_LIMIT: usize = 64 * 1024 * 1024;

fn shot_tool() -> std::path::PathBuf {
    py::home().join("Documents/cuntu/bin/ledger_shot")
}

fn ocr_tool() -> std::path::PathBuf {
    py::home().join("Documents/cuntu/bin/ocrvision")
}

// ---------------------------------------------------------------- 解析

const PLATFORMS: &[(&str, &[&str])] = &[
    ("京东", &["京东", "JD.COM", "京东商城"]),
    ("天猫", &["天猫", "tmall"]),
    ("淘宝", &["淘宝", "taobao"]),
    ("拼多多", &["拼多多", "pinduoduo"]),
    ("闲鱼", &["闲鱼", "咸鱼"]),
    ("得物", &["得物", "poizon"]),
    ("抖音", &["抖音", "抖店"]),
    ("小红书", &["小红书"]),
    ("苏宁", &["苏宁"]),
    ("唯品会", &["唯品会"]),
    ("Apple", &["Apple Store", "apple.com.cn", "苹果官网"]),
    ("小米", &["小米商城", "小米有品", "mi.com"]),
    ("华为", &["华为商城", "vmall"]),
    ("亚马逊", &["亚马逊", "amazon"]),
];

const PRICE_WORDS: &[&str] = &[
    "实付款", "实付", "应付", "合计", "总价", "订单金额", "付款金额", "成交价", "商品总价",
];
const BUY_WORDS: &[&str] = &[
    "下单时间", "购买时间", "创建时间", "成交时间", "付款时间", "支付时间", "订单时间",
];
const NOISE: &[&str] = &[
    "订单详情", "订单", "等待收货", "待收货", "待发货", "已完成", "交易成功", "退款",
    "查看物流", "申请退款", "联系客服", "再次购买", "评价", "确认收货", "全部", "首页",
    "我的", "购物车", "实付款", "实付", "合计", "优惠", "运费", "店铺", "订单编号",
    "支付方式", "运单号", "收货地址", "备注", "发票", "客服",
];

const CAT_WORDS: &[(&str, &[&str])] = &[
    ("数码", &[
        "键盘", "鼠标", "耳机", "相机", "镜头", "手机", "电脑", "显示器", "平板", "硬盘",
        "固态", "路由", "音箱", "充电", "线材", "iPad", "iPhone", "Mac", "显卡", "内存",
        "手表", "手柄", "投影", "录音",
    ]),
    ("家电", &[
        "冰箱", "洗衣机", "空调", "电饭", "微波炉", "吸尘", "扫地", "风扇", "取暖", "净化",
        "热水壶", "电磁炉", "烤箱",
    ]),
    ("家居", &[
        "椅子", "书桌", "桌子", "床垫", "台灯", "衣柜", "收纳", "被子", "枕头", "炒锅",
        "餐具", "地毯", "窗帘",
    ]),
    ("日用", &["牙刷", "牙膏", "纸巾", "洗衣", "清洁", "洗发", "沐浴", "厨房", "垃圾袋", "保鲜"]),
    ("服饰", &[
        "外套", "羽绒", "T恤", "衬衫", "长裤", "短裤", "运动鞋", "跑鞋", "帽子", "袜子",
        "卫衣", "毛衣",
    ]),
    ("书籍", &["全集", "文集", "译本", "出版社", "画集", "小说"]),
    ("食品", &["零食", "咖啡豆", "挂耳", "牛奶", "坚果", "巧克力", "面包", "大米", "茶叶"]),
    ("出行", &["机票", "火车票", "高铁", "酒店", "打车", "门票", "签证"]),
];

fn clean(s: &str) -> String {
    let re = regex::Regex::new(r"\s+").unwrap();
    re.replace_all(s.trim(), " ").into_owned()
}

fn money(s: &str) -> Option<f64> {
    let re = regex::Regex::new(r"(?:[¥￥]\s*)?(\d{1,3}(?:,\d{3})*(?:\.\d{1,2})?|\d+(?:\.\d{1,2})?)").unwrap();
    let stripped = s.replace(' ', "");
    let m = re.captures(&stripped)?;
    let raw = m.get(1)?.as_str().replace(',', "");
    let v: f64 = raw.parse().ok()?;
    if v > 0.0 && v < 10_000_000.0 { Some(v) } else { None }
}

fn fmt_date(m: &regex::Captures) -> String {
    let y = m.get(1).map(|x| x.as_str()).unwrap_or("");
    let mo: i64 = m.get(2).and_then(|x| x.as_str().parse().ok()).unwrap_or(0);
    let d: i64 = m.get(3).and_then(|x| x.as_str().parse().ok()).unwrap_or(0);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return String::new();
    }
    format!("{y}-{mo:02}-{d:02}")
}

fn guess_category(name: &str) -> String {
    let low = name.to_lowercase();
    for (cat, words) in CAT_WORDS {
        if words.iter().any(|w| low.contains(&w.to_lowercase())) {
            return cat.to_string();
        }
    }
    String::new()
}

/// `ledger.parse_fields`：从 OCR 文本里猜字段。规则优先，模型不参与 ——
/// 订单页的字段是固定词 + 数字，正则比 4B 稳，也不会算错日期。
pub fn parse_fields(text: &str, lines: &[Value]) -> Value {
    let raw = text;
    let date_re = regex::Regex::new(r"(20\d{2})\s*[-/.年]\s*(\d{1,2})\s*[-/.月]\s*(\d{1,2})").unwrap();
    let order_re = regex::Regex::new(r"订单编号\D{0,4}(\d{10,})").unwrap();

    // seq: (文本, y, h)；lines 为空时退回按行切
    let mut seq: Vec<(String, f64, f64)> = Vec::new();
    for l in lines {
        let t = clean(&py::s_of(l.get("t")));
        if !t.is_empty() {
            seq.push((t, py::f64_of(l.get("y")), py::f64_of(l.get("h"))));
        }
    }
    if seq.is_empty() {
        for x in raw.split('\n') {
            let t = clean(x);
            if !t.is_empty() {
                seq.push((t, 0.0, 0.0));
            }
        }
    }
    let rows: Vec<String> = seq.iter().map(|s| s.0.clone()).collect();

    let mut name = String::new();
    let mut platform = String::new();
    let mut bought_at = String::new();
    let mut price: Option<f64> = None;
    let mut order_no = String::new();
    let mut shop = String::new();
    let mut conf: Vec<String> = Vec::new();

    let low = raw.to_lowercase();
    for (pname, keys) in PLATFORMS {
        if keys.iter().any(|k| low.contains(&k.to_lowercase())) {
            platform = pname.to_string();
            break;
        }
    }

    for r in &rows {
        if PRICE_WORDS.iter().any(|w| r.contains(w)) {
            if let Some(v) = money(r) {
                price = Some(v);
                break;
            }
        }
    }
    if price.is_none() {
        let mut nums: Vec<f64> = Vec::new();
        for r in &rows {
            if r.contains('¥') || r.contains('￥') {
                if let Some(v) = money(r) {
                    nums.push(v);
                }
            }
        }
        if !nums.is_empty() {
            price = Some(nums.iter().cloned().fold(f64::MIN, f64::max));
        }
    }
    if price.is_none() {
        conf.push("price".to_string());
    }

    for r in &rows {
        if BUY_WORDS.iter().any(|w| r.contains(w)) {
            if let Some(m) = date_re.captures(r) {
                bought_at = fmt_date(&m);
                break;
            }
        }
    }
    if bought_at.is_empty() {
        if let Some(m) = date_re.captures(raw) {
            bought_at = fmt_date(&m);
        }
    }
    if bought_at.is_empty() {
        conf.push("bought_at".to_string());
    }

    if let Some(m) = order_re.captures(&raw.replace(' ', "")) {
        order_no = m.get(1).map(|x| x.as_str()).unwrap_or("").to_string();
    }
    for (idx, r) in rows.iter().enumerate() {
        if r.contains("店铺") {
            let seg: Vec<&str> = regex::Regex::new(r"店铺[：:]?")
                .unwrap()
                .splitn(r, 2)
                .collect();
            let mut tail = if seg.len() > 1 { clean(seg[1]) } else { String::new() };
            if tail.is_empty() && idx + 1 < rows.len() {
                let re2 = regex::Regex::new(r"运单号|订单号|备注").unwrap();
                tail = re2.split(&rows[idx + 1]).next().unwrap_or("").to_string();
            }
            if !tail.is_empty() {
                shop = py::trunc(&clean(&tail), 40);
            }
            break;
        }
    }

    let mut cand: Vec<String> = Vec::new();
    for r in &rows {
        let ln = r.chars().count();
        if ln < 6 || ln > 120 {
            continue;
        }
        if NOISE.iter().any(|w| r.contains(w)) {
            continue;
        }
        if date_re.is_match(r) || r.contains('¥') || r.contains('￥') {
            continue;
        }
        if regex::Regex::new(r"^[\d\s.,:：%\-]+$").unwrap().is_match(r) {
            continue;
        }
        if !regex::Regex::new(r"[\u{4e00}-\u{9fa5}A-Za-z]").unwrap().is_match(r) {
            continue;
        }
        cand.push(r.clone());
    }
    if !cand.is_empty() {
        // `max(cand, key=len)`：等长取第一个
        let mut best = &cand[0];
        for c in &cand[1..] {
            if c.chars().count() > best.chars().count() {
                best = c;
            }
        }
        let mut nm = best.clone();
        let idx = rows.iter().position(|r| r == best).unwrap_or(0);
        if idx + 1 < rows.len() {
            let nxt = &seq[idx + 1].0;
            let cur = &seq[idx];
            let gap = if cur.2 != 0.0 { seq[idx + 1].1 - (cur.1 + cur.2) } else { 0.0 };
            let tight = if cur.2 != 0.0 { gap <= f64::max(10.0, cur.2 * 0.9) } else { true };
            let nlen = nxt.chars().count();
            let ok_next = tight
                && (4..=40).contains(&nlen)
                && !NOISE.iter().any(|w| nxt.contains(w))
                && !date_re.is_match(nxt)
                && !nxt.contains('¥')
                && !nxt.contains('￥');
            if ok_next && nm.chars().count() + nlen + 1 <= 60 {
                nm = format!("{nm} {nxt}");
            }
        }
        name = py::trunc(&nm, 60);
    } else {
        conf.push("name".to_string());
    }

    let category = guess_category(&name);
    if category.is_empty() {
        conf.push("category".to_string());
    }

    let mut out = Map::new();
    out.insert("name".into(), json!(name));
    out.insert("platform".into(), json!(platform));
    out.insert("bought_at".into(), json!(bought_at));
    out.insert(
        "price".into(),
        match price {
            Some(p) => json!(p),
            None => Value::Null,
        },
    );
    out.insert("category".into(), json!(category));
    out.insert("order_no".into(), json!(order_no));
    out.insert("shop".into(), json!(shop));
    out.insert("conf".into(), json!(conf));
    Value::Object(out)
}

// ---------------------------------------------------------------- 工具链路

fn ocr_only(src: &Path) -> String {
    let tool = ocr_tool();
    if !tool.exists() {
        return String::new();
    }
    let argv = vec![tool.to_string_lossy().to_string(), src.to_string_lossy().to_string()];
    let Some((_c, out, _e)) = py::run_cmd(&argv, 45, None) else {
        return String::new();
    };
    let line = out.trim().to_string();
    match line.split_once('\t') {
        Some((_, t)) => t.to_string(),
        None => line,
    }
}

fn run_shot(src: &Path, dst: &Path) -> Value {
    let tool = shot_tool();
    if !tool.exists() {
        return json!({"ok": false, "text": ocr_only(src), "lines": [], "crop_src": "none"});
    }
    let argv = vec![
        tool.to_string_lossy().to_string(),
        src.to_string_lossy().to_string(),
        dst.to_string_lossy().to_string(),
    ];
    let Some((_c, out, _e)) = py::run_cmd(&argv, 60, None) else {
        return json!({"ok": false, "text": ocr_only(src), "lines": [], "crop_src": "none"});
    };
    let parsed: Value = match serde_json::from_str(out.trim()) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            return json!({"ok": false, "text": ocr_only(src), "lines": [], "crop_src": "none"})
        }
    };
    let mut data = parsed;
    let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let text = py::s_of(data.get("text"));
    if !ok && text.is_empty() {
        data["text"] = json!(ocr_only(src));
    }
    data
}

// ---------------------------------------------------------------- ingest

fn ledger_dir(ctx: &Ctx) -> std::path::PathBuf {
    ledger::ledger_dir(ctx)
}

fn ensure_dir(ctx: &Ctx) -> std::path::PathBuf {
    let d = ledger_dir(ctx);
    for sub in ["图", "原图", "_回收站"] {
        let _ = std::fs::create_dir_all(d.join(sub));
    }
    d
}

/// `ledger.ingest`：存原图 → OCR → 裁产品图 → 落一条记录。
pub fn ingest(ctx: &Ctx, raw: &[u8], filename: &str) -> Value {
    if raw.is_empty() {
        return json!({"ok": false, "error": "空文件"});
    }
    if raw.len() > 30 * 1024 * 1024 {
        return json!({"ok": false, "error": "截图超过 30MB"});
    }
    let d = ensure_dir(ctx);
    let day = py::local_str("%Y%m%d");
    let iid = format!("p-{day}-{}", py::rand_hex(4));
    let mut ext = Path::new(filename)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if ![".png", ".jpg", ".jpeg", ".heic", ".webp"].contains(&ext.as_str()) {
        ext = ".png".to_string();
    }
    let src = d.join("原图").join(format!("{iid}{ext}"));
    if std::fs::write(&src, raw).is_err() {
        return json!({"ok": false, "error": "写不进原图"});
    }
    let crop_path = d.join("图").join(format!("{iid}.jpg"));
    let shot = run_shot(&src, &crop_path);
    let fields = parse_fields(
        &py::s_of(shot.get("text")),
        shot.get("lines").and_then(|v| v.as_array()).map(|a| a.as_slice()).unwrap_or(&[]),
    );
    let crop_src = py::s_of(shot.get("crop_src"));
    let has_crop = !(crop_src.is_empty() || crop_src == "none") && crop_path.exists();
    let now = py::local_str("%Y-%m-%dT%H:%M:%S");
    let name = {
        let n = py::s_of(fields.get("name"));
        if n.is_empty() { "（待命名）".to_string() } else { n }
    };
    let bought = {
        let b = py::s_of(fields.get("bought_at"));
        if b.is_empty() { py::local_str("%Y-%m-%d") } else { b }
    };
    let price_v = match fields.get("price") {
        Some(Value::Number(n)) => Value::Number(n.clone()),
        _ => json!(0),
    };
    let category = {
        let c = py::s_of(fields.get("category"));
        if c.is_empty() { "其他".to_string() } else { c }
    };
    let conf = fields.get("conf").cloned().unwrap_or(json!([]));
    let mut item = Map::new();
    item.insert("id".into(), json!(iid));
    item.insert("name".into(), json!(name));
    item.insert("platform".into(), json!(py::s_of(fields.get("platform"))));
    item.insert("bought_at".into(), json!(bought));
    item.insert("price".into(), price_v);
    item.insert("category".into(), json!(category));
    item.insert("shop".into(), json!(py::s_of(fields.get("shop"))));
    item.insert("order_no".into(), json!(py::s_of(fields.get("order_no"))));
    item.insert("note".into(), json!(""));
    item.insert("status".into(), json!("在用"));
    item.insert("sold_at".into(), json!(""));
    item.insert("sold_price".into(), Value::Null);
    item.insert("sold_to".into(), json!(""));
    item.insert(
        "image".into(),
        json!(if has_crop { format!("{DIR_NAME}/图/{iid}.jpg") } else { String::new() }),
    );
    item.insert(
        "raw".into(),
        json!(format!(
            "{DIR_NAME}/原图/{}",
            src.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        )),
    );
    item.insert(
        "crop_src".into(),
        json!(if crop_src.is_empty() { "none".to_string() } else { crop_src.clone() }),
    );
    item.insert("ocr".into(), json!(py::trunc(&py::s_of(shot.get("text")), 4000)));
    item.insert("conf".into(), conf.clone());
    item.insert("created_at".into(), json!(now));
    item.insert("updated_at".into(), json!(now));
    let item = Value::Object(item);

    let (mut items, _) = ledger::load(ctx);
    items.insert(0, item.clone());
    ledger::save(ctx, &items);

    let mut guess = Map::new();
    for k in ["name", "platform", "bought_at", "price", "category"] {
        guess.insert(k.to_string(), fields.get(k).cloned().unwrap_or(Value::Null));
    }
    json!({
        "ok": true,
        "item": item,
        "guess": Value::Object(guess),
        "conf": conf,
        "crop_src": py::s_of(item.get("crop_src")),
    })
}

/// `ledger.replace_image`：手动换产品图（不再裁）。
pub fn replace_image(ctx: &Ctx, iid: &str, raw: &[u8], filename: &str) -> Value {
    let Some(_item) = ledger::get_item(ctx, iid) else {
        return json!({"ok": false, "error": "没有这条记录"});
    };
    if raw.is_empty() {
        return json!({"ok": false, "error": "空文件"});
    }
    let d = ensure_dir(ctx);
    let mut ext = Path::new(filename)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if ext.is_empty() {
        ext = ".png".to_string();
    }
    if ![".png", ".jpg", ".jpeg", ".webp", ".heic"].contains(&ext.as_str()) {
        ext = ".png".to_string();
    }
    let tmp = d.join("原图").join(format!("{iid}-换图{ext}"));
    if std::fs::write(&tmp, raw).is_err() {
        return json!({"ok": false, "error": "写不进原图"});
    }
    let dest = d.join("图").join(format!("{iid}.jpg"));
    let mut ok = false;
    for argv in [
        vec![
            "sips".to_string(), "-Z".to_string(), "1000".to_string(),
            "-s".to_string(), "format".to_string(), "jpeg".to_string(),
            tmp.to_string_lossy().to_string(), "--out".to_string(),
            dest.to_string_lossy().to_string(),
        ],
        vec![
            "sips".to_string(), "-Z".to_string(), "1000".to_string(),
            tmp.to_string_lossy().to_string(), "--out".to_string(),
            dest.to_string_lossy().to_string(),
        ],
    ] {
        if let Some((code, _o, _e)) = py::run_cmd(&argv, 30, None) {
            if code == 0 && dest.exists() {
                ok = true;
                break;
            }
        }
    }
    if !ok {
        let _ = std::fs::copy(&tmp, &dest);
    }
    ledger::update(
        ctx,
        iid,
        &json!({
            "image": format!("{DIR_NAME}/图/{iid}.jpg"),
            "raw": format!("{DIR_NAME}/原图/{}", tmp.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
            "crop_src": "manual",
        }),
    )
}

// ---------------------------------------------------------------- multipart 小工具

fn missing_body(field: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "missing",
            "loc": ["body", field],
            "msg": "Field required",
            "input": null,
            "url": "https://errors.pydantic.dev/2.14/v/missing",
        }]})),
    )
        .into_response()
}

struct Part {
    name: String,
    data: Vec<u8>,
    over: bool,
}

/// `_read_upload`：整读进内存，但有上限；超限报 413（形状是 HTTPException）。
async fn read_field(field: &mut axum::extract::multipart::Field<'_>, what: &str) -> Result<Vec<u8>, Response> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match field.chunk().await {
            Ok(Some(c)) => {
                buf.extend_from_slice(&c);
                if buf.len() > UPLOAD_LIMIT {
                    return Err(http_err(
                        413,
                        &format!("{what}超过 {}MB 上限", UPLOAD_LIMIT / 1048576),
                    ));
                }
            }
            Ok(None) => break,
            Err(e) => return Err(http_err(400, &format!("表单解析失败：{e}"))),
        }
    }
    Ok(buf)
}

/// `POST /api/ledger/upload`：一次可传多张订单截图。
pub async fn upload(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    mut mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let mut parts: Vec<Part> = Vec::new();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return http_err(400, &format!("表单解析失败：{e}")),
        };
        let name = field.name().unwrap_or("").to_string();
        if name != "files" {
            continue;
        }
        let fname = field.file_name().map(|s| s.to_string()).unwrap_or_default();
        let mut field = field;
        match read_field(&mut field, "这个文件").await {
            Ok(data) => parts.push(Part { name: fname, data, over: false }),
            Err(_) => parts.push(Part { name: fname, data: Vec::new(), over: true }),
        }
    }
    if parts.is_empty() {
        return missing_body("files");
    }
    let mut ok: Vec<Value> = Vec::new();
    let mut bad: Vec<Value> = Vec::new();
    for p in parts {
        if p.over {
            bad.push(json!({"name": p.name, "error": "这个文件超过 64MB 上限"}));
            continue;
        }
        let r = ingest(&ctx, &p.data, &p.name);
        if r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            ok.push(r);
        } else {
            bad.push(json!({"name": p.name, "error": r.get("error").cloned().unwrap_or(Value::Null)}));
        }
    }
    Json(json!({"ok": !ok.is_empty(), "added": ok, "failed": bad})).into_response()
}

/// `POST /api/ledger/image?id=...`：换产品图。
pub async fn image(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    Query(q): Query<HashMap<String, String>>,
    mut mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let id = q.get("id").cloned().unwrap_or_default();
    let mut found: Option<(String, Vec<u8>)> = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return http_err(400, &format!("表单解析失败：{e}")),
        };
        if field.name().unwrap_or("") != "file" {
            continue;
        }
        let fname = field.file_name().map(|s| s.to_string()).unwrap_or_default();
        let mut field = field;
        match read_field(&mut field, "这张图").await {
            Ok(data) => found = Some((fname, data)),
            Err(r) => return r,
        }
    }
    let Some((fname, data)) = found else {
        return missing_body("file");
    };
    Json(replace_image(&ctx, &id, &data, &fname)).into_response()
}

/// `POST /api/ledger/open`：在 Finder 里打开记账目录。
pub async fn open(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let d = ledger_dir(&ctx);
    if std::fs::create_dir_all(&d).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Internal Server Error",
        )
            .into_response();
    }
    match std::process::Command::new("open")
        .arg(d.to_string_lossy().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => Json(json!({"ok": true, "dir": d.to_string_lossy()})).into_response(),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})).into_response(),
    }
}
