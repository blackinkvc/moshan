// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：文件层的路径语义与目录/回收站/分享数据，逐条对齐 `moshan/storage.py`。
// 不做：不碰 HTTP、不碰 UI、不写日志。上层只负责把 `PathError` 映射成响应。
//
// 这一层是全项目最容易出错的地方，原因不是代码复杂，而是**符号链接**：
// 数据根下几乎每个目录都是链接（`07-收件箱 -> /Volumes/AI仓库/资料库/07-收件箱`）。
// 判「路径是否在根内」用的是**字面路径**（不解析链接），返回给前端、拼面包屑、
// 下一次请求回传也都用字面路径。一旦这里改用 resolve 结果，手机端点进任何
// 文件夹都会 403（2026-11-26 修过一次，`151c3f5`）。

use crate::paths::Ctx;
use chrono::{Local, TimeZone};
use serde_json::{json, Map, Value};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct PathError {
    pub code: u16,
    pub msg: String,
}

impl PathError {
    pub fn new(msg: &str, code: u16) -> Self {
        Self { code, msg: msg.to_string() }
    }
}

pub type PResult<T> = Result<T, PathError>;

fn err<T>(msg: &str, code: u16) -> PResult<T> {
    Err(PathError::new(msg, code))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `Path.expanduser()`：只处理 `~` 与 `~/`，`~user` 在实际配置里没出现过。
pub fn expanduser(raw: &str) -> PathBuf {
    if raw == "~" {
        return home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home().join(rest);
    }
    PathBuf::from(raw)
}

/// `os.path.abspath()`：相对路径接 cwd，然后**纯字面**折叠 `.` 与 `..`。
/// 故意不解析符号链接 —— 这一项专门用于「字面上在不在根里」的判定。
pub fn abspath(p: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `Path.resolve(strict=False)`：解析已存在的部分，不存在的尾部按字面拼回去。
/// 与 canonicalize 的差别就在这里 —— 上传目标、待创建目录都还不存在。
pub fn py_resolve(p: &Path) -> PathBuf {
    let abs = abspath(p);
    let comps: Vec<Component> = abs.components().collect();
    for take in (1..=comps.len()).rev() {
        let mut probe = PathBuf::new();
        for c in &comps[..take] {
            probe.push(c.as_os_str());
        }
        if let Ok(real) = std::fs::canonicalize(&probe) {
            let mut out = real;
            for c in &comps[take..] {
                out.push(c.as_os_str());
            }
            return out;
        }
    }
    abs
}

/// 只允许 allowed_roots 以内的路径，挡住 `../` 与越界。返回**字面路径**。
pub fn safe_path(ctx: &Ctx, raw: &str, must_exist: bool) -> PResult<PathBuf> {
    if raw.trim().is_empty() {
        return err("路径为空", 400);
    }
    let expanded = expanduser(raw);
    let p = py_resolve(&expanded);
    let lex = abspath(&expanded);
    for root in ctx.allowed_roots() {
        let r = py_resolve(&root);
        let r0 = abspath(&root);
        if p == r || p.starts_with(&r) || lex == r0 || lex.starts_with(&r0) {
            if must_exist && !lex.exists() {
                return err("文件不存在", 404);
            }
            return Ok(lex);
        }
    }
    err("这个路径不在允许范围内", 403)
}

/// 当前路径属于哪个允许根（按归属判断，不写死 ROOTS[0]）。
pub fn root_for(ctx: &Ctx, path: &Path) -> PathBuf {
    let rp = py_resolve(path);
    for r in ctx.allowed_roots() {
        let rr = py_resolve(&r);
        if rp == rr || rp.starts_with(&rr) {
            return rr;
        }
    }
    ctx.allowed_roots()
        .first()
        .map(|r| py_resolve(r))
        .unwrap_or(rp)
}

#[allow(dead_code)] // W3 写族使用
pub fn is_trash(ctx: &Ctx, p: &Path) -> bool {
    let t = py_resolve(&ctx.trash_dir());
    let rp = py_resolve(p);
    rp == t || rp.starts_with(&t)
}

/// 禁止对根目录或回收站本身做破坏性操作。
#[allow(dead_code)] // W3 写族使用
pub fn guard_target(ctx: &Ctx, p: &Path) -> PResult<()> {
    let rp = py_resolve(p);
    for root in ctx.allowed_roots() {
        if rp == py_resolve(&root) {
            return err("不能操作根目录", 403);
        }
    }
    if is_trash(ctx, &rp) {
        return err("回收站内的文件请用「恢复」或「清空」", 403);
    }
    Ok(())
}

/// 与 `storage.human()` 同一套分档；`{:.0}` / `{:.1}` 的舍入口径与 Python 一致
/// （都是对二进制精确值做 round-half-even）。
pub fn human(n: f64) -> String {
    if n < 1024.0 {
        return format!("{} B", n as i64);
    }
    if n < 1048576.0 {
        return format!("{:.0} KB", n / 1024.0);
    }
    if n < 1073741824.0 {
        return format!("{:.1} MB", n / 1048576.0);
    }
    format!("{:.2} GB", n / 1073741824.0)
}

const CATEGORY_OF_EXT: &[(&str, &str)] = &[
    ("pdf", "文档"), ("doc", "文档"), ("docx", "文档"), ("txt", "文档"),
    ("md", "文档"), ("markdown", "文档"), ("rtf", "文档"), ("pages", "文档"),
    ("odt", "文档"), ("epub", "文档"), ("mobi", "文档"), ("azw3", "文档"),
    ("xls", "表格"), ("xlsx", "表格"), ("csv", "表格"), ("tsv", "表格"),
    ("numbers", "表格"),
    ("ppt", "演示"), ("pptx", "演示"), ("key", "演示"), ("keynote", "演示"),
    ("jpg", "图片"), ("jpeg", "图片"), ("png", "图片"), ("gif", "图片"),
    ("webp", "图片"), ("heic", "图片"), ("heif", "图片"), ("bmp", "图片"),
    ("tif", "图片"), ("tiff", "图片"), ("svg", "图片"), ("ico", "图片"),
    ("mp3", "音视频"), ("wav", "音视频"), ("m4a", "音视频"), ("aac", "音视频"),
    ("flac", "音视频"), ("ogg", "音视频"), ("opus", "音视频"), ("mp4", "音视频"),
    ("mov", "音视频"), ("mkv", "音视频"), ("avi", "音视频"), ("webm", "音视频"),
    ("flv", "音视频"), ("m4v", "音视频"),
    ("zip", "压缩包"), ("rar", "压缩包"), ("7z", "压缩包"), ("tar", "压缩包"),
    ("gz", "压缩包"), ("tgz", "压缩包"), ("bz2", "压缩包"), ("xz", "压缩包"),
    ("dmg", "压缩包"), ("iso", "压缩包"),
    ("py", "代码"), ("js", "代码"), ("ts", "代码"), ("jsx", "代码"),
    ("tsx", "代码"), ("json", "代码"), ("yaml", "代码"), ("yml", "代码"),
    ("toml", "代码"), ("ini", "代码"), ("html", "代码"), ("css", "代码"),
    ("scss", "代码"), ("sh", "代码"), ("sql", "代码"),
];

const PREVIEW_IMAGE: &[&str] =
    &["jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp", "svg", "ico"];
const PREVIEW_VIDEO: &[&str] = &["mp4", "mov", "m4v", "webm", "mkv", "avi"];
const PREVIEW_AUDIO: &[&str] = &["mp3", "m4a", "wav", "aac", "flac", "ogg", "opus"];
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "json", "csv", "tsv", "log", "yaml", "yml", "toml", "ini",
    "py", "js", "ts", "jsx", "tsx", "html", "css", "scss", "sh", "sql", "xml", "conf",
    "gitignore", "env",
];

/// `Path(name).suffix.lower().lstrip(".")` —— 注意 `.gitignore` 的后缀是空串，
/// 不是 `gitignore`（Python 的 Path 同样如此）。
fn suffix(name: &str) -> String {
    match Path::new(name).extension() {
        Some(e) => e.to_string_lossy().to_lowercase(),
        None => String::new(),
    }
}

pub fn category(name: &str) -> String {
    let ext = suffix(name);
    CATEGORY_OF_EXT
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, c)| c.to_string())
        .unwrap_or_else(|| "其他".to_string())
}

fn preview_kind(p: &Path, is_dir: bool) -> &'static str {
    let ext = suffix(&p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
    let e = ext.as_str();
    if PREVIEW_IMAGE.contains(&e) {
        return "image";
    }
    if PREVIEW_VIDEO.contains(&e) {
        return "video";
    }
    if PREVIEW_AUDIO.contains(&e) {
        return "audio";
    }
    if e == "pdf" {
        return "pdf";
    }
    if TEXT_EXTS.contains(&e) && !is_dir {
        if let Ok(md) = p.metadata() {
            if md.len() <= 2 * 1024 * 1024 {
                return "text";
            }
        }
    }
    "none"
}

fn mtime_secs(md: &std::fs::Metadata) -> i64 {
    match md.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        },
        Err(_) => 0,
    }
}

pub fn entry(p: &Path, root: Option<&Path>) -> Value {
    let md = std::fs::metadata(p).ok();
    let is_dir = md.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let size = if is_dir { 0 } else { md.as_ref().map(|m| m.len()).unwrap_or(0) };
    let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();

    let mut item = Map::new();
    item.insert("name".into(), json!(name));
    item.insert("path".into(), json!(p.to_string_lossy().to_string()));
    item.insert("is_dir".into(), json!(is_dir));
    item.insert("bytes".into(), json!(size));
    item.insert("size".into(), json!(if is_dir { String::new() } else { human(size as f64) }));
    item.insert("mtime".into(), json!(md.as_ref().map(mtime_secs).unwrap_or(0)));
    item.insert(
        "kind".into(),
        json!(if is_dir { "folder" } else { preview_kind(p, false) }),
    );
    item.insert(
        "category".into(),
        json!(if is_dir { "文件夹".to_string() } else { category(&name) }),
    );
    if let Some(r) = root {
        let rel = p
            .strip_prefix(r)
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_else(|_| name.clone());
        item.insert("rel".into(), json!(rel));
    }
    if is_dir {
        let mut n: u32 = 0;
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                if !e.file_name().to_string_lossy().starts_with('.') {
                    n += 1;
                }
                if n >= 999 {
                    break;
                }
            }
        }
        item.insert("children".into(), json!(n));
    }
    Value::Object(item)
}

/// 面包屑：从 root 到当前目录。
pub fn crumbs(path: &Path, root: &Path) -> Vec<Value> {
    let root_name = root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let mut out = vec![json!({"name": root_name, "path": root.to_string_lossy()})];
    let rel = match path.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return out,
    };
    let mut acc = root.to_path_buf();
    for part in rel.components() {
        acc.push(part.as_os_str());
        let n = part.as_os_str().to_string_lossy().to_string();
        out.push(json!({"name": n, "path": acc.to_string_lossy()}));
    }
    out
}

pub fn list_dir(ctx: &Ctx, path: &Path, q: &str, sort: &str) -> PResult<Value> {
    if !path.exists() {
        return err("目录不存在", 404);
    }
    if !path.is_dir() {
        return err("这不是一个目录", 400);
    }
    let root = root_for(ctx, path);
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return err("没有读取权限", 403)
        }
        Err(_) => return err("目录不存在", 404),
    };

    let mut dirs: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "index.jsonl" {
            continue;
        }
        let item = entry(&PathBuf::from(e.path()), Some(&root));
        if item.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false) {
            dirs.push(item);
        } else {
            files.push(item);
        }
    }

    if !q.is_empty() {
        let ql = q.to_lowercase();
        files.retain(|f| f.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase().contains(&ql));
        dirs.retain(|d| d.get("name").and_then(|v| v.as_str()).unwrap_or("").to_lowercase().contains(&ql));
        if files.is_empty() && dirs.is_empty() {
            files = deep_search(path, &ql, &root, 200);
        }
    }

    let key = |v: &Value| -> SortKey {
        match sort {
            "mtime" => SortKey::Num(-v.get("mtime").and_then(|x| x.as_i64()).unwrap_or(0)),
            "size" => SortKey::Num(-v.get("bytes").and_then(|x| x.as_i64()).unwrap_or(0)),
            _ => SortKey::Text(v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_lowercase()),
        }
    };
    // 稳定排序：键相同时保留 readdir 原序，与 Python 的 sorted 行为一致。
    dirs.sort_by(|a, b| cmp_key(&key(a), &key(b)));
    files.sort_by(|a, b| cmp_key(&key(a), &key(b)));

    let count = dirs.len() + files.len();
    let parent = if path != root {
        json!(path.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default())
    } else {
        Value::Null
    };

    Ok(json!({
        "path": path.to_string_lossy(),
        "crumbs": crumbs(path, &root),
        "parent": parent,
        "dirs": dirs,
        "files": files,
        "count": count,
        "usage": usage(path),
    }))
}

enum SortKey {
    Text(String),
    Num(i64),
}

fn cmp_key(a: &SortKey, b: &SortKey) -> std::cmp::Ordering {
    match (a, b) {
        (SortKey::Text(x), SortKey::Text(y)) => x.cmp(y),
        (SortKey::Num(x), SortKey::Num(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
}

/// 当前层没命中时往下搜四层（NAS 的「全盘搜索」体验）。
fn deep_search(root: &Path, ql: &str, allowed_root: &Path, limit: usize) -> Vec<Value> {
    let mut hits: Vec<Value> = Vec::new();
    let base_depth = root.components().count();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.components().count() - base_depth >= 4 {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let p = PathBuf::from(e.path());
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if name == "index.jsonl" || !name.to_lowercase().contains(ql) {
                continue;
            }
            hits.push(entry(&p, Some(allowed_root)));
            if hits.len() >= limit {
                return hits;
            }
        }
    }
    hits
}

/// `shutil.disk_usage()`：statvfs 的三个量，口径必须一致（free 用 bavail 不是 bfree）。
pub fn usage(path: &Path) -> Value {
    match disk_usage(path) {
        Some((total, used, free)) => {
            let pct = if total > 0 {
                py_round1(used as f64 / total as f64 * 100.0)
            } else {
                0.0
            };
            json!({
                "total": total,
                "used": used,
                "free": free,
                "total_h": human(total as f64),
                "free_h": human(free as f64),
                "used_pct": pct,
            })
        }
        None => json!({}),
    }
}

fn py_round1(x: f64) -> f64 {
    format!("{:.1}", x).parse().unwrap_or(x)
}

#[cfg(unix)]
fn disk_usage(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = if st.f_frsize > 0 { st.f_frsize } else { st.f_bsize } as u64;
    let total = st.f_blocks as u64 * frsize;
    let free = st.f_bavail as u64 * frsize;
    let used = (st.f_blocks as u64).saturating_sub(st.f_bfree as u64) * frsize;
    Some((total, used, free))
}

#[cfg(windows)]
fn disk_usage(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            dir: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let (mut free_caller, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe {
        GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_caller, &mut total, &mut total_free)
    };
    if ok == 0 {
        return None;
    }
    Some((total, total - total_free, free_caller))
}

// ---------------------------------------------------------------- 回收站

fn trash_index(ctx: &Ctx) -> PathBuf {
    ctx.trash_dir().join("index.jsonl")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn dir_size(p: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = PathBuf::from(e.path());
            // os.walk(followlinks=False)：符号链接目录列出来但不进去，尺寸也不计。
            let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            match std::fs::metadata(&path) {
                Ok(md) if md.is_dir() => {
                    if !is_link {
                        stack.push(path);
                    }
                }
                Ok(md) => total += md.len(),
                Err(_) => {}
            }
        }
    }
    total
}

pub fn list_trash(ctx: &Ctx) -> Vec<Value> {
    let idx = trash_index(ctx);
    let Ok(raw) = std::fs::read_to_string(&idx) else { return Vec::new() };
    let trash = ctx.trash_dir();
    let mut rows: Vec<Value> = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(Value::Object(mut r)) = serde_json::from_str::<Value>(line) else { continue };
        let Some(name) = r.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
            continue;
        };
        let p = trash.join(&name);
        if !p.exists() {
            continue;
        }
        let bytes = if p.is_dir() { dir_size(&p) } else {
            std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0)
        };
        let ts = r.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        r.insert("bytes".into(), json!(bytes));
        r.insert("size".into(), json!(human(bytes as f64)));
        r.insert("age".into(), json!(age(ts)));
        rows.push(Value::Object(r));
    }
    // 稳定降序：时间相同保持索引文件里的先后。
    rows.sort_by(|a, b| {
        let x = a.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        let y = b.get("ts").and_then(|v| v.as_i64()).unwrap_or(0);
        y.cmp(&x)
    });
    rows
}

fn age(ts: i64) -> String {
    if ts == 0 {
        return String::new();
    }
    let d = (now_secs() - ts).max(0);
    if d < 3600 {
        format!("{} 分钟前", d / 60)
    } else if d < 86400 {
        format!("{} 小时前", d / 3600)
    } else {
        format!("{} 天前", d / 86400)
    }
}

// ---------------------------------------------------------------- 分享

fn read_shares(ctx: &Ctx) -> Map<String, Value> {
    std::fs::read_to_string(ctx.shares_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

/// 列出生效中的分享：过期的不返回（也不顺手清理——清理要写盘，
/// 而这里只是读接口，写盘留给下一次真正需要写的时候）。
pub fn share_list(ctx: &Ctx) -> Vec<Value> {
    let d = read_shares(ctx);
    let now = now_secs();
    let mut live: Vec<Value> = Vec::new();
    for rec in d.values() {
        let expires = rec.get("expires").and_then(|v| v.as_i64()).unwrap_or(0);
        if expires != 0 && expires < now {
            continue;
        }
        let mut rec = match rec.as_object() {
            Some(o) => o.clone(),
            None => continue,
        };
        let path = rec.get("path").and_then(|v| v.as_str()).unwrap_or("");
        rec.insert("exists".into(), json!(Path::new(path).exists()));
        let expires_h = if expires != 0 {
            Local
                .timestamp_opt(expires, 0)
                .single()
                .map(|t| t.format("%m-%d %H:%M").to_string())
                .unwrap_or_else(|| "永久".to_string())
        } else {
            "永久".to_string()
        };
        rec.insert("expires_h".into(), json!(expires_h));
        live.push(Value::Object(rec));
    }
    live.sort_by(|a, b| {
        let x = a.get("created").and_then(|v| v.as_i64()).unwrap_or(0);
        let y = b.get("created").and_then(|v| v.as_i64()).unwrap_or(0);
        y.cmp(&x)
    });
    live
}

// ---------------------------------------------------------------- 缩略图

/// `~/.cache/pocket-thumbs`（Windows 退到 `%LOCALAPPDATA%`）。
fn thumb_dir() -> PathBuf {
    if cfg!(target_os = "macos") {
        return home().join(".cache").join("pocket-thumbs");
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Local"))
        .join("pocket-thumbs")
}

fn mtime_nanos(md: &std::fs::Metadata) -> u128 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// 缓存键与 Python 完全一致（`路径|mtime_ns|size|宽`），两边才能共用同一份磁盘缓存：
/// 对拍时 Python 先生成的缩略图 Rust 直接命中，不必各跑一次 sips。
fn thumb_target(p: &Path, md: &std::fs::Metadata, w: u32) -> PathBuf {
    use sha1::{Digest, Sha1};
    let key = format!("{}|{}|{}|{}", p.to_string_lossy(), mtime_nanos(md), md.len(), w);
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    let mut hex = String::with_capacity(40);
    for b in h.finalize() {
        hex.push_str(&format!("{:02x}", b));
    }
    thumb_dir().join(format!("{hex}.jpg"))
}

/// 用系统自带的图像工具缩图并落盘缓存。失败返回 None，前端退回显示原图。
///
/// macOS 走 `sips`（两条命令是有原因的：某些格式不吃 `-s format jpeg`，
/// 要退回纯 `-Z`）；其它平台交给 `crate::imaging`（探测 ImageMagick / ffmpeg），
/// 一个都没有也返回 None —— 与 Python 在那些平台上的表现一致（那边只有 sips）。
pub fn make_thumb(p: &Path, w: u32) -> Option<PathBuf> {
    let md = std::fs::metadata(p).ok()?;
    let out = thumb_target(p, &md, w);
    if std::fs::metadata(&out).map(|m| m.len() > 0).unwrap_or(false) {
        return Some(out);
    }
    std::fs::create_dir_all(thumb_dir()).ok()?;
    // 注意：Rust 的 target_os 是 `macos`，Python 的 sys.platform 才叫 `darwin`。
    // 写成 "darwin" 编译能过（cfg! 只当布尔），但永远为假 —— 缩略图会静默失效。
    if !cfg!(target_os = "macos") {
        return if crate::imaging::make_thumb_q(p, &out, w, 60) {
            Some(out)
        } else {
            None
        };
    }
    let stem = out.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = thumb_dir().join(format!("{stem}.{}.tmp", std::process::id()));
    let ws = w.to_string();
    let variants: [Vec<&str>; 2] = [
        vec!["-Z", &ws, "-s", "format", "jpeg", "-s", "formatOptions", "60"],
        vec!["-Z", &ws],
    ];
    for v in variants.iter() {
        let ok = std::process::Command::new("sips")
            .args(v)
            .arg(p)
            .arg("--out")
            .arg(&tmp)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok && std::fs::metadata(&tmp).map(|m| m.len() > 0).unwrap_or(false) {
            if std::fs::rename(&tmp, &out).is_ok() {
                return Some(out);
            }
        }
        let _ = std::fs::remove_file(&tmp);
    }
    None
}

// ---------------------------------------------------------------- 写族
//
// 这一段的每个函数都对应 `storage.py` 里的同名函数，判定顺序也照搬 ——
// 「先查同名再查越界」还是反过来，直接决定返回 409 还是 403，前端按状态码分支。

/// 目录里取一个不冲突的名字（`a.txt` → `a-2.txt` → …）。
pub fn unique_path(directory: &Path, name: &str) -> PathBuf {
    let base = Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().trim().to_string())
        .unwrap_or_default();
    let safe = if base.is_empty() || base == "." || base == ".." { "未命名".to_string() } else { base };
    let p = directory.join(&safe);
    if !p.exists() {
        return p;
    }
    let stem = Path::new(&safe).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let suf = Path::new(&safe).extension().map(|s| format!(".{}", s.to_string_lossy())).unwrap_or_default();
    for i in 2..1000 {
        let cand = directory.join(format!("{stem}-{i}{suf}"));
        if !cand.exists() {
            return cand;
        }
    }
    directory.join(format!("{stem}-{}{suf}", now_secs()))
}

pub fn mkdir(ctx: &Ctx, parent: &Path, name: &str) -> PResult<Value> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        return err("目录名不合法", 400);
    }
    if !parent.is_dir() {
        return err("上级不是目录", 400);
    }
    let dest = parent.join(name);
    if dest.exists() {
        return err("同名目录已存在", 409);
    }
    std::fs::create_dir(&dest).map_err(|e| PathError::new(&e.to_string(), 500))?;
    let _ = ctx;
    Ok(entry(&dest, None))
}

pub fn rename(ctx: &Ctx, p: &Path, new_name: &str) -> PResult<Value> {
    guard_target(ctx, p)?;
    let new_name = new_name.trim();
    if new_name.is_empty() || new_name.contains('/') || new_name == "." || new_name == ".." {
        return err("名字不合法", 400);
    }
    let dest = p.parent().unwrap_or_else(|| Path::new("/")).join(new_name);
    if dest == p {
        return Ok(entry(p, None));
    }
    if dest.exists() {
        return err("这个名字已被占用", 409);
    }
    std::fs::rename(p, &dest).map_err(|e| PathError::new(&e.to_string(), 500))?;
    Ok(entry(&dest, None))
}

fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)?.flatten() {
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// 移动/复制。逐项返回结果，整体不因单项失败而中断 —— 与 Python 一致。
pub fn move_paths(ctx: &Ctx, paths: &[PathBuf], dest_dir: &Path, copy: bool) -> PResult<Vec<Value>> {
    if !dest_dir.is_dir() {
        return err("目标不是目录", 400);
    }
    guard_target(ctx, dest_dir)?;
    let mut out = Vec::new();
    for p in paths {
        // Python 在这里直接 `_guard_target(p)` 抛出，整单失败 —— 不是"这一项失败"。
        // 上层把它翻成 403，前端按状态码提示"不能操作根目录"。
        guard_target(ctx, p)?;
        if !copy && p.parent() == Some(dest_dir) {
            out.push(json!({"path": p.to_string_lossy(), "ok": true, "note": "已在目标目录"}));
            continue;
        }
        if p.is_dir() && (dest_dir == p || dest_dir.starts_with(p)) {
            out.push(json!({"path": p.to_string_lossy(), "ok": false, "error": "不能移进自己"}));
            continue;
        }
        let target = unique_path(dest_dir, &p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
        let res = if copy {
            copy_recursive(p, &target)
        } else {
            std::fs::rename(p, &target)
        };
        match res {
            Ok(_) => out.push(json!({"path": p.to_string_lossy(), "ok": true, "to": target.to_string_lossy()})),
            Err(e) => out.push(json!({
                "path": p.to_string_lossy(), "ok": false,
                "error": e.to_string().chars().take(120).collect::<String>(),
            })),
        }
    }
    Ok(out)
}

fn stamp_compact() -> String {
    Local::now().format("%Y%m%d-%H%M%S").to_string()
}

fn append_trash_index(ctx: &Ctx, rec: &Value) {
    let dir = ctx.trash_dir();
    let _ = std::fs::create_dir_all(&dir);
    let line = serde_json::to_string(rec).unwrap_or_default();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("index.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

pub fn delete_to_trash(ctx: &Ctx, paths: &[PathBuf]) -> PResult<Vec<Value>> {
    let trash = ctx.trash_dir();
    let _ = std::fs::create_dir_all(&trash);
    let mut out = Vec::new();
    for p in paths {
        // 同上：guard 失败整单 403（Python 的 `_guard_target` 是抛出而非记账）。
        guard_target(ctx, p)?;
        if !p.exists() {
            out.push(json!({"path": p.to_string_lossy(), "ok": false, "error": "不存在"}));
            continue;
        }
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let dest = unique_path(&trash, &format!("{}_{}", stamp_compact(), name));
        let size = if p.is_file() {
            std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
        } else {
            dir_size(p)
        };
        let is_dir = p.is_dir();
        match std::fs::rename(p, &dest) {
            Ok(_) => {
                let dest_name = dest.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                append_trash_index(ctx, &json!({
                    "name": dest_name,
                    "orig": p.to_string_lossy(),
                    "ts": now_secs(),
                    "bytes": size,
                    "is_dir": is_dir,
                }));
                out.push(json!({"path": p.to_string_lossy(), "ok": true, "trash": dest_name}));
            }
            Err(e) => out.push(json!({
                "path": p.to_string_lossy(), "ok": false,
                "error": e.to_string().chars().take(120).collect::<String>(),
            })),
        }
    }
    Ok(out)
}

fn rewrite_trash_index(ctx: &Ctx, rows: &[Value]) {
    let dir = ctx.trash_dir();
    let _ = std::fs::create_dir_all(&dir);
    let tmp = dir.join("index.tmp");
    let mut body = String::new();
    for r in rows {
        body.push_str(&serde_json::to_string(r).unwrap_or_default());
        body.push('\n');
    }
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("index.jsonl"));
    }
}

pub fn restore(ctx: &Ctx, names: &[String]) -> Vec<Value> {
    let trash = ctx.trash_dir();
    let rows = list_trash(ctx);
    let mut out = Vec::new();
    let mut keep: Vec<Value> = Vec::new();
    for n in names {
        let rec = rows.iter().find(|r| r.get("name").and_then(|v| v.as_str()) == Some(n.as_str()));
        let Some(rec) = rec else {
            out.push(json!({"name": n, "ok": false, "error": "回收站里没有"}));
            continue;
        };
        let orig = rec.get("orig").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let orig_path = PathBuf::from(&orig);
        let parent = orig_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("/"));
        let target = unique_path(&parent, &orig_path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default());
        let res = std::fs::create_dir_all(&parent).and_then(|_| {
            std::fs::rename(trash.join(n), &target)
        });
        match res {
            Ok(_) => out.push(json!({"name": n, "ok": true, "to": parent.to_string_lossy()})),
            Err(e) => {
                keep.push(rec.clone());
                out.push(json!({
                    "name": n, "ok": false,
                    "error": e.to_string().chars().take(120).collect::<String>(),
                }));
            }
        }
    }
    rewrite_trash_index(ctx, &keep);
    out
}

pub fn purge(ctx: &Ctx, names: Option<&[String]>) -> usize {
    let trash = ctx.trash_dir();
    let rows = list_trash(ctx);
    let mut n = 0usize;
    let mut rest: Vec<Value> = Vec::new();
    for r in rows.iter() {
        let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let hit = match names {
            None => true,
            Some(list) => list.iter().any(|x| x == name),
        };
        if !hit {
            rest.push(r.clone());
            continue;
        }
        let p = trash.join(name);
        let ok = if p.is_dir() {
            std::fs::remove_dir_all(&p)
        } else {
            std::fs::remove_file(&p)
        };
        if ok.is_ok() {
            n += 1;
        }
        // 删失败的那条仍然要从索引里去掉吗？Python 是先删后统一按 rest 重写，
        // 失败的条目也不在 rest 里 —— 照搬，别在这里"修 bug"，否则两边分叉。
    }
    // 注意：Python 把**带 size/age 的富行**写回索引，这里必须一致，
    // 否则下一次 list_trash 出来的键序会不同。
    rewrite_trash_index(ctx, &rest);
    n
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        // 没有 /dev/urandom 时不追求密码学强度：sid 只用于本机分享链接。
        let seed = format!("{}-{}-{:?}", now_secs(), std::process::id(), SystemTime::now());
        let mut h: u64 = 1469598103934665603;
        for b in seed.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((h >> ((i % 8) * 8)) & 0xff) as u8;
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

fn write_shares(ctx: &Ctx, d: &Map<String, Value>) {
    let path = ctx.shares_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("tmp");
    // Python 用 `indent=2`；这里必须同格式，否则两边写出来的 shares.json 不一致。
    let body = serde_json::to_string_pretty(d).unwrap_or_default();
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

pub fn share_create(ctx: &Ctx, p: &Path, hours: f64) -> PResult<Value> {
    if p.is_dir() {
        guard_target(ctx, p)?;
    }
    if !p.exists() {
        return err("文件不存在", 404);
    }
    let sid = &random_hex(16)[..10];
    let now = now_secs();
    let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let expires = if hours > 0.0 { now + (hours * 3600.0) as i64 } else { 0 };
    let rec = json!({
        "sid": sid,
        "path": p.to_string_lossy(),
        "name": name,
        "is_dir": p.is_dir(),
        "created": now,
        "expires": expires,
        "hits": 0,
    });
    let mut d = read_shares(ctx);
    d.insert(sid.to_string(), rec.clone());
    write_shares(ctx, &d);
    Ok(rec)
}

pub fn share_revoke(ctx: &Ctx, sid: &str) -> bool {
    let mut d = read_shares(ctx);
    if d.shift_remove(sid).is_some() {
        write_shares(ctx, &d);
        return true;
    }
    false
}

// ---------------------------------------------------------------- 运行账本

/// `server.py:log()` —— 追加一行到运行账本。位置跟 Python 一样：
/// 源码态写到代码目录，打包后写状态目录（写进 .app 会让签名失效）。
pub fn log(ctx: &Ctx, entry: Value) {
    let dir = if ctx.frozen { ctx.state_dir.clone() } else { ctx.app_dir.clone() };
    let path = dir.join("pocket.jsonl");
    let mut obj = Map::new();
    obj.insert("ts".into(), json!(Local::now().format("%Y-%m-%dT%H:%M:%S%z").to_string()));
    if let Some(o) = entry.as_object() {
        for (k, v) in o {
            obj.insert(k.clone(), v.clone());
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", serde_json::to_string(&Value::Object(obj)).unwrap_or_default());
    }
}
