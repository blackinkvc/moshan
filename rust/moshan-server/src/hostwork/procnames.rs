// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：进程名的归组与分类（helper 合并、按 .app 认主人、图标归类）。对齐 hostwork.py。
// 不做：不去转 icns（那是 sips 的事）；这里只回答「这个进程属于谁、画什么图标」。
#![allow(dead_code)]

/// `_HELPER`：`^(.*?)\s+Helper(\s*\(.*\))?$`
fn helper_base(name: &str) -> Option<&str> {
    let mut from = 0;
    while let Some(p) = name[from..].find("Helper") {
        let pos = from + p;
        let head = &name[..pos];
        let tail = &name[pos + 6..];
        let head_ok = head.chars().last().map(|c| c.is_whitespace()).unwrap_or(false);
        let t = tail.trim_start();
        let tail_ok = t.is_empty() || (t.starts_with('(') && t.ends_with(')'));
        if head_ok && tail_ok {
            let trimmed = head.trim_end();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
        from = pos + 6;
    }
    None
}

const GENERIC: &[&str] = &[
    "electron", "chromium", "chrome", "node", "deno", "python", "python3", "python3.11",
    "python3.12", "python3.13", "java", "ruby", "php",
];

/// `_app_from_path`：`/([^/]+)\.app/` 里的第一段。
pub fn app_from_path(path: &str) -> String {
    let mut from = 0;
    while let Some(p) = path[from..].find(".app/") {
        let end = from + p; // ".app" 的起点
        // 往前找这一段的开头（上一个 '/'）
        let head = &path[..end];
        if let Some(slash) = head.rfind('/') {
            let seg = &path[slash + 1..end];
            if !seg.is_empty() && !seg.contains('/') {
                return seg.to_string();
            }
        }
        from = end + 5;
    }
    String::new()
}

/// `_bundle_of`：`^(/.+?\.app)/`
pub fn bundle_of(path: &str) -> String {
    if !path.starts_with('/') {
        return String::new();
    }
    match path.find(".app/") {
        Some(p) => path[..p + 4].to_string(),
        None => String::new(),
    }
}

/// `_group_name(name, path)`：Helper 归并到主应用；运行时要按 .app 归主人。
pub fn group_name(name: &str, path: &str) -> String {
    let base = match helper_base(name) {
        Some(b) => b.to_string(),
        None => name.to_string(),
    };
    if GENERIC.contains(&base.to_lowercase().as_str()) {
        let app = app_from_path(path);
        if !app.is_empty() {
            return app;
        }
    }
    base
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Workbuddy,
    Trae,
    Pocket,
}

impl Owner {
    pub fn id(self) -> &'static str {
        match self {
            Owner::Workbuddy => "workbuddy",
            Owner::Trae => "trae",
            Owner::Pocket => "pocket",
        }
    }
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    hay.to_lowercase().contains(needle)
}

/// `_OWNER_PAT`：靠可执行文件路径认是哪个软件的进程。
pub fn owners_of(path: &str, name: &str) -> Vec<&'static str> {
    let hay = format!("{} {}", path, name);
    let low = hay.to_lowercase();
    let mut out = Vec::new();
    if low.contains("workbuddy") {
        out.push("workbuddy");
    }
    if low.contains("trae cn") || low.contains("trae.app") || low.contains("/trae") || low.contains("\\trae") {
        out.push("trae");
    }
    let _ = contains_ci(&hay, "");
    if low.contains("local-ai-workspace/pocket")
        || low.contains("local-ai-workspace\\pocket")
        || low.contains("pocket.log")
        || low.contains("pocket/journal")
        || low.contains("pocket\\journal")
        || low.contains("/pocket/server.py")
        || low.contains("/moshan/")
    {
        out.push("pocket");
    }
    out
}

/// `proc_kind(name, path)`：kernel / app / script / sys / bin。
pub fn proc_kind(name: &str, path: &str) -> &'static str {
    let low = name.to_lowercase();
    if low.starts_with("kernel") {
        return "kernel";
    }
    if !bundle_of(path).is_empty() {
        return "app";
    }
    let hay = format!("{} {}", path, low).to_lowercase();
    for k in [
        "python", "node", "ruby", "perl", "deno", "bun", ".workbuddy/", "local-ai-workspace",
    ] {
        if hay.contains(k) {
            return "script";
        }
    }
    if path.starts_with("/System/")
        || path.starts_with("/usr/")
        || path.starts_with("/sbin/")
        || path.starts_with("/Library/Apple/")
    {
        return "sys";
    }
    "bin"
}

/// 图标缓存：key → (png 文件名, 生成时刻)。照 Python 的 `_ICON_DONE`：
/// 生成过的空结果留 900 秒，免得每轮采样都去 fork 一次 sips。
fn icon_done() -> &'static std::sync::Mutex<std::collections::HashMap<String, (String, f64)>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (String, f64)>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn icon_skip() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(audio|doc|html|image|movie|video|text|pdf|font|folder|sound|generic|badge|template)",
        )
        .unwrap()
    })
}

fn icon_pref() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)^(appicon|applicationicon|app|icon)[^/]*\.icns$").unwrap())
}

fn md5_hex(s: &str) -> String {
    // 系统自带 md5，比手写一份 MD5 稳；只在真的要生成图标时调一次（结果落盘缓存）
    let out = std::process::Command::new("/sbin/md5").arg("-q").arg("-s").arg(s).output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => String::new(),
    }
}

/// `icon_name(key, path)`：已缓存就返 png 文件名；没有就现转一张（sips），
/// 转不出来返回空串（前端走首字母/齿轮兜底）。
pub fn icon_name(icon_dir: &std::path::Path, key: &str, path: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    if let Ok(g) = icon_done().lock() {
        if let Some((f, at)) = g.get(key) {
            if !f.is_empty() || now - *at < 900.0 {
                return f.clone();
            }
        }
    }
    let got = make_icon(icon_dir, path);
    if let Ok(mut g) = icon_done().lock() {
        g.insert(key.to_string(), (got.clone(), now));
    }
    got
}

/// `_make_icon(key, path)`：找 .app 里最像图标的那份 icns，转成 72×72 png。
fn make_icon(icon_dir: &std::path::Path, path: &str) -> String {
    let bundle = bundle_of(path);
    if bundle.is_empty() {
        return String::new();
    }
    let res = std::path::Path::new(&bundle).join("Contents").join("Resources");
    let Ok(rd) = std::fs::read_dir(&res) else { return String::new() };
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let ext = p.extension().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if ext == "icns" {
            files.push(p);
        }
    }
    if files.is_empty() {
        return String::new();
    }
    let good: Vec<std::path::PathBuf> = {
        let f: Vec<std::path::PathBuf> = files
            .iter()
            .filter(|p| {
                let n = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                !icon_skip().is_match(&n)
            })
            .cloned()
            .collect();
        if f.is_empty() { files.clone() } else { f }
    };
    let pref: Vec<std::path::PathBuf> = good
        .iter()
        .filter(|p| {
            let n = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            icon_pref().is_match(&n)
        })
        .cloned()
        .collect();
    // Python `max(..., key=st_size)`：并列时取**先出现**的那个
    let pool = if pref.is_empty() { good } else { pref };
    let mut best: Option<(std::path::PathBuf, u64)> = None;
    for p in pool {
        let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        match &best {
            Some((_, b)) if sz <= *b => {}
            _ => best = Some((p, sz)),
        }
    }
    let Some((icns, _)) = best else { return String::new() };
    let mt = std::fs::metadata(&icns)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stem = icns.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let h = md5_hex(&format!("{}|{}|{}", bundle, stem, mt));
    if h.is_empty() {
        return String::new();
    }
    let name = format!("{}.png", &h[..h.len().min(12)]);
    let dst = icon_dir.join(&name);
    if dst.exists() {
        return name;
    }
    let _ = std::fs::create_dir_all(icon_dir);
    let _ = std::process::Command::new("/usr/bin/sips")
        .args(["-s", "format", "png", "--resampleHeightWidth", "72", "72"])
        .arg(&icns)
        .arg("--out")
        .arg(&dst)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if dst.exists() { name } else { String::new() }
}
