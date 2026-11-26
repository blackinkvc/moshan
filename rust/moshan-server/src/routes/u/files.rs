// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：文件/媒体类 10 条路由（file/raw/thumb/preview/icon/zip、外观三张图、分享下载）。
// 不做：不碰写族（mkdir/rename/move/delete/upload），那些在 w3。

use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{http_err, peek, q, query_map};
use crate::routes::u::appearance;
use crate::routes::u::util::{file_response, html, mime_of, preview_kind, range_header};
use crate::routes::u::zipw;
use crate::storage;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

type CtxState = State<Arc<Ctx>>;

// ---------------------------------------------------------------- 422 形状
//
// FastAPI 在进函数体之前就把所有查询参数校验完：必填缺失 → `missing`，
// 声明成 int 但解析不了 → `int_parsing`。多条错误按签名里的参数顺序堆在一起。
// 这几条路由都是「不带参数直接打」一眼可见的边界，形状抄全才有意义。

fn q_opt(uri: &Uri, key: &str) -> Option<String> {
    query_map(uri)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

fn missing_query(field: &str) -> Value {
    json!({
        "type": "missing",
        "loc": ["query", field],
        "msg": "Field required",
        "input": null,
        "url": "https://errors.pydantic.dev/2.14/v/missing",
    })
}

fn int_parsing(field: &str, input: &str) -> Value {
    json!({
        "type": "int_parsing",
        "loc": ["query", field],
        "msg": "Input should be a valid integer, unable to parse string as an integer",
        "input": input,
        "url": "https://errors.pydantic.dev/2.14/v/int_parsing",
    })
}

fn unprocessable(errors: Vec<Value>) -> Response {
    (
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({ "detail": errors })),
    )
        .into_response()
}

/// 必填 `path` 的解析结果：要么拿到字符串，要么已经该回 422 了。
fn need_path(uri: &Uri) -> Result<String, Response> {
    q_opt(uri, "path").ok_or_else(|| unprocessable(vec![missing_query("path")]))
}

/// Python `int(str)` 的口径：两侧空白可省、允许正负号、位数不限。
/// 位数超出 i64 时饱和 —— 调用点（`w` clamp、`dl` 真假）都不在乎绝对值。
fn parse_py_int(s: &str) -> Option<i64> {
    let t = s.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match t.parse::<i64>() {
        Ok(v) => Some(v),
        Err(_) => Some(if neg { i64::MIN } else { i64::MAX }),
    }
}

// ---------------------------------------------------------------- 下载 / 直出

pub async fn api_file(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let raw = match need_path(&uri) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let p = match storage::safe_path(&ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    if !p.is_file() {
        return http_err(404, "文件不存在");
    }
    let name = p
        .file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    let mime = mime_of(&p);
    let range = range_header(&headers);
    file_response(&p, &mime, None, Some(&name), range.as_deref())
}

pub async fn api_raw(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let raw = match need_path(&uri) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let p = match storage::safe_path(&ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    if !p.is_file() {
        return http_err(404, "文件不存在");
    }
    let mime = mime_of(&p);
    let range = range_header(&headers);
    file_response(&p, &mime, Some("private, max-age=60"), None, range.as_deref())
}

pub async fn api_thumb(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    // 参数校验全在进函数体之前：`path` 缺失、`w` 不是整数会同时报出来，
    // 顺序按 Python 签名 `api_thumb(path: str, w: int = 360)`。
    let mut errs: Vec<Value> = Vec::new();
    let raw = q_opt(&uri, "path");
    if raw.is_none() {
        errs.push(missing_query("path"));
    }
    let mut w = 360i64;
    match q_opt(&uri, "w") {
        None => {}
        Some(s) => match parse_py_int(&s) {
            Some(v) => w = v,
            None => errs.push(int_parsing("w", &s)),
        },
    }
    if !errs.is_empty() {
        return unprocessable(errs);
    }
    let raw = raw.unwrap_or_default();
    // Python 先 clamp 再查路径：`w = max(80, min(int(w), 1200))`。
    let w = w.clamp(80, 1200) as u32;
    let p = match storage::safe_path(&ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    if !p.is_file() {
        return http_err(404, "文件不存在");
    }
    let pp = p.clone();
    let made = tokio::task::spawn_blocking(move || storage::make_thumb(&pp, w))
        .await
        .ok()
        .flatten();
    match made {
        Some(t) => file_response(&t, "image/jpeg", Some("private, max-age=2592000"), None, None),
        None => {
            let mime = mime_of(&p);
            file_response(&p, &mime, Some("private, max-age=60"), None, None)
        }
    }
}

pub async fn api_preview(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let raw = match need_path(&uri) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let p = match storage::safe_path(&ctx, &raw, false) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    if !p.exists() {
        return http_err(404, "文件不存在");
    }
    let name = p
        .file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    if p.is_dir() {
        return Json(json!({
            "kind": "folder",
            "name": name,
            "path": p.to_string_lossy(),
        }))
        .into_response();
    }
    let kind = preview_kind(&p);
    let md = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(_) => return http_err(404, "文件不存在"),
    };
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("name".into(), json!(name));
    out.insert("path".into(), json!(p.to_string_lossy()));
    out.insert("size".into(), json!(storage::human(md.len() as f64)));
    out.insert("bytes".into(), json!(md.len()));
    out.insert("mtime".into(), json!(mtime));
    out.insert("category".into(), json!(storage::category(&name)));
    if kind == "text" {
        match std::fs::read(&p) {
            Ok(raw) => {
                let text = String::from_utf8_lossy(&raw);
                let head: String = text.chars().take(20000).collect();
                out.insert("text".into(), json!(head));
            }
            Err(e) => {
                out.insert("kind".into(), json!("none"));
                out.insert("error".into(), json!(truncate120(&e.to_string())));
            }
        }
    }
    Json(Value::Object(out)).into_response()
}

fn truncate120(s: &str) -> String {
    s.chars().take(120).collect()
}

// ---------------------------------------------------------------- 图标

pub async fn api_icon(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    // 这条是公开的（Python 侧没有 dependencies=[Depends(auth)]）。
    let _ = (&ctx, &headers);
    let f = q(&uri, "f");
    // re.fullmatch(r"[0-9a-f]{6,32}\.png", f)
    let ok = {
        let body = f.strip_suffix(".png").unwrap_or("");
        f.ends_with(".png")
            && (6..=32).contains(&body.len())
            && body.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if !ok {
        return http_err(400, "文件名不对");
    }
    let fp = ctx.library_dir.join("_本机工作").join("图标").join(&f);
    if !fp.exists() {
        return http_err(404, "没有这个图标");
    }
    file_response(&fp, "image/png", Some("public, max-age=604800"), None, None)
}

// ---------------------------------------------------------------- 打包

pub async fn api_zip(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let paths_raw = q(&uri, "paths");
    let mut targets: Vec<PathBuf> = Vec::new();
    if !paths_raw.trim().is_empty() {
        for x in paths_raw.split('|') {
            if x.trim().is_empty() {
                continue;
            }
            match storage::safe_path(&ctx, x, false) {
                Ok(p) => targets.push(p),
                Err(e) => return http_err(e.code, &e.msg),
            }
        }
    }
    let single = q(&uri, "path");
    if targets.is_empty() && !single.trim().is_empty() {
        match storage::safe_path(&ctx, &single, false) {
            Ok(p) => targets.push(p),
            Err(e) => return http_err(e.code, &e.msg),
        }
    }
    if targets.is_empty() {
        return http_err(400, "没有指定要打包的内容");
    }
    let name = if targets.len() == 1 {
        targets[0]
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_else(|| "口袋打包".to_string())
    } else {
        "口袋打包".to_string()
    };

    let tmp = match mkdtemp("pocket-zip-") {
        Ok(p) => p,
        Err(_) => return http_err(500, "打包失败：无法建临时目录"),
    };
    let n_targets = targets.len();
    let tmp2 = tmp.clone();
    let built = tokio::task::spawn_blocking(move || zipw::make_zip(&targets, &tmp2, &name))
        .await
        .ok()
        .and_then(|r| r.ok());
    let z = match built {
        Some(z) => z,
        None => {
            let _ = std::fs::remove_dir_all(&tmp);
            return http_err(500, "打包失败：写入失败");
        }
    };
    let size = std::fs::metadata(&z).map(|m| m.len()).unwrap_or(0);
    storage::log(
        &ctx,
        json!({"event": "zip", "n": n_targets, "bytes": size}),
    );

    let zname = z
        .file_name()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    let res = file_response(&z, "application/zip", None, Some(&zname), None);
    let _ = std::fs::remove_dir_all(&tmp);
    res
}

fn mkdtemp(prefix: &str) -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    for _ in 0..64 {
        let cand = base.join(format!("{prefix}{}", rand_hex(10)));
        match std::fs::create_dir(&cand) {
            Ok(_) => return Ok(cand),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "tmp"))
}

fn rand_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((seed >> ((i % 16) * 8)) & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------------------------------------------------------------- 外观

pub async fn appearance_css(State(ctx): CtxState) -> Response {
    let text = appearance::sync(&ctx.cfg);
    let mut res = text.into_response();
    res.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/css; charset=utf-8"),
    );
    res.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store, max-age=0"),
    );
    res
}

pub async fn appearance_bg(State(ctx): CtxState) -> Response {
    let key = appearance::effective_bg(&ctx.cfg);
    let p = if key.is_empty() {
        appearance::bg_path(&ctx.cfg)
    } else {
        appearance::wall_path(&ctx.cfg, &key)
    };
    if !p.is_file() {
        return http_err(404, "没有壁纸");
    }
    file_response(&p, "image/jpeg", Some("public, max-age=86400"), None, None)
}

pub async fn appearance_wall(State(ctx): CtxState, Path(name): Path<String>) -> Response {
    // Python 的路由是 `/__wall/{name}.jpg`；axum 的段参数不吃后缀，这里自己剥。
    let Some(key) = name.strip_suffix(".jpg") else {
        return http_err(404, "Not Found");
    };
    if key.contains('/') {
        return http_err(404, "Not Found");
    }
    let p = appearance::wall_path(&ctx.cfg, key);
    if !p.is_file() {
        return http_err(404, "没有这张壁纸");
    }
    file_response(&p, "image/jpeg", Some("public, max-age=86400"), None, None)
}

// ---------------------------------------------------------------- 分享下载

fn read_shares(ctx: &Ctx) -> Map<String, Value> {
    std::fs::read_to_string(ctx.shares_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// `storage._save_shares()`：indent=2 落盘 + 0600。
fn save_shares(ctx: &Ctx, d: &Map<String, Value>) {
    let path = ctx.shares_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = jsonio::dumps(&Value::Object(d.clone()), 2);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `storage.share_get(sid)`：过期的顺手删掉；有效则 hits+1、last 更新后落盘。
fn share_get(ctx: &Ctx, sid: &str) -> Option<Value> {
    let mut d = read_shares(ctx);
    let rec = d.get(sid)?.clone();
    let expires = rec.get("expires").and_then(|v| v.as_i64()).unwrap_or(0);
    if expires != 0 && expires < now_secs() {
        d.shift_remove(sid);
        save_shares(ctx, &d);
        return None;
    }
    let mut rec = rec;
    if let Some(o) = rec.as_object_mut() {
        let hits = o.get("hits").and_then(|v| v.as_i64()).unwrap_or(0) + 1;
        o.insert("hits".into(), json!(hits));
        o.insert("last".into(), json!(now_secs()));
    }
    d.insert(sid.to_string(), rec.clone());
    save_shares(ctx, &d);
    Some(rec)
}

pub async fn public_share(
    State(ctx): CtxState,
    Path((sid, name)): Path<(String, String)>,
    uri: Uri,
) -> Response {
    // `dl: int = 0` 也是进函数体前校验：非法值连 share_get（会 +1 计数）都不该跑。
    let dl = match q_opt(&uri, "dl") {
        None => 0,
        Some(s) => match parse_py_int(&s) {
            Some(v) => v,
            None => return unprocessable(vec![int_parsing("dl", &s)]),
        },
    };
    // 公开下载：只认分享 ID。
    let rec = match share_get(&ctx, &sid) {
        Some(r) => r,
        None => return html(410, "<h2>链接已失效或已过期</h2>", &[]),
    };
    let _ = name;
    let path = rec.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let p = PathBuf::from(&path);
    if !p.exists() {
        return html(404, "<h2>文件已被移走</h2>", &[]);
    }
    storage::log(
        &ctx,
        json!({"event": "share_hit", "sid": sid, "hits": rec.get("hits").cloned().unwrap_or(json!(0))}),
    );
    if p.is_dir() {
        let name = p
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_else(|| "口袋打包".to_string());
        let tmp = match mkdtemp("pocket-share-") {
            Ok(t) => t,
            Err(_) => return html(500, "<h2>打包失败</h2>", &[]),
        };
        let tmp2 = tmp.clone();
        let targets = vec![p.clone()];
        let n2 = name.clone();
        let built = tokio::task::spawn_blocking(move || zipw::make_zip(&targets, &tmp2, &n2))
            .await
            .ok()
            .and_then(|r| r.ok());
        let z = match built {
            Some(z) => z,
            None => {
                let _ = std::fs::remove_dir_all(&tmp);
                return html(500, "<h2>打包失败</h2>", &[]);
            }
        };
        let zname = z
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let res = file_response(&z, "application/zip", None, Some(&zname), None);
        let _ = std::fs::remove_dir_all(&tmp);
        return res;
    }
    let mime = mime_of(&p);
    let fname = if dl != 0 {
        p.file_name().map(|x| x.to_string_lossy().to_string())
    } else {
        None
    };
    file_response(&p, &mime, None, fname.as_deref(), None)
}

/// `/s/{sid}/`（name 为空）也能打开：Python 的 path 转换器允许空串。
pub async fn public_share_empty(
    State(ctx): CtxState,
    Path(sid): Path<String>,
    uri: Uri,
) -> Response {
    public_share(State(ctx), Path((sid, String::new())), uri).await
}

