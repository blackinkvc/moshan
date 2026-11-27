// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：工作台顶栏那套「插件 / 服务 / 应用入口」的现算，加上 /api/status 与
//       /api/plugincards 两条路由。对应 moshan/dash.py 的 _all_specs / _services /
//       apps_payload / _auto_view_apps / read_plugins，以及 server.py 的 status。
// 不做：不写 plugins.json、不起进程 —— 那些写族路由不在本泳道。

use super::platform;
use super::util::{clip, load_json, now_hms, str_or, truthy};
use crate::paths::Ctx;
use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// `dash._root(cfg)`：allowed_roots[0] 的**原始字符串**，没有就 inbox_dir 的父目录。
///
/// 不能直接拿 `ctx.library_dir`：那条走 `canonicalize`，macOS 上 MOSHAN_HOME 若在
/// `/var/folders/...` 底下就会被改写成 `/private/var/...`，而这个字符串会原样出现在
/// 接口的 `why` / `root` 里 —— 与 Python 差一个前缀，逐字节对拍立刻红。
pub fn root(ctx: &Ctx) -> PathBuf {
    if let Some(Value::Array(rs)) = ctx.cfg.get("allowed_roots") {
        if let Some(Value::String(s)) = rs.first() {
            return PathBuf::from(s);
        }
    }
    if let Some(Value::String(s)) = ctx.cfg.get("inbox_dir") {
        if let Some(p) = Path::new(s).parent() {
            return p.to_path_buf();
        }
    }
    ctx.library_dir.clone()
}

pub fn workbench(ctx: &Ctx) -> PathBuf {
    root(ctx).join("_工作台")
}

// ------------------------------------------------------------------ 内置清单

pub struct App {
    pub id: &'static str,
    pub name: &'static str,
    pub scope: &'static str,
    pub port: Option<i64>,
    pub svc_port: Option<i64>,
    pub svc_name: Option<&'static str>,
    pub selfish: bool,
    pub url: &'static str,
    pub note: &'static str,
    pub cmd: Option<(&'static str, &'static str)>,
    pub glyph: &'static str,
    pub group: &'static str,
}

pub const BUILTIN_APPS: &[App] = &[
    App { id: "pocket", name: "口袋", scope: "service", port: None, svc_port: Some(8321),
          svc_name: Some("口袋（传输/抓取）"), selfish: true, url: "/",
          note: "抓取 / 传输 / 工作台数据源", cmd: None, glyph: "pocket", group: "plugin" },
    App { id: "gallery", name: "看图", scope: "service", port: Some(8771), svc_port: None,
          svc_name: None, selfish: false, url: "http://127.0.0.1:8771/",
          note: "存图库", cmd: Some(("04-存图", "_看图.command")), glyph: "gallery", group: "tool" },
    App { id: "reader", name: "阅读", scope: "service", port: Some(8772), svc_port: None,
          svc_name: None, selfish: false, url: "http://127.0.0.1:8772/",
          note: "长文阅读 / 标注", cmd: Some(("08-阅读", "_阅读.command")), glyph: "reader", group: "tool" },
    App { id: "media", name: "影音", scope: "service", port: Some(8773), svc_port: None,
          svc_name: Some("媒体墙"), selfish: false, url: "http://127.0.0.1:8773/",
          note: "媒体墙 / 播放", cmd: Some(("02-媒体", "_媒体墙.command")), glyph: "media", group: "tool" },
    App { id: "inspect", name: "巡检", scope: "service", port: Some(8774), svc_port: None,
          svc_name: Some("系统巡检"), selfish: false, url: "/dash?v=ops",
          note: "系统巡检", cmd: Some(("_巡检", "_系统巡检.command")), glyph: "inspect", group: "plugin" },
    App { id: "hostwork", name: "Agent 监控", scope: "view", port: None, svc_port: None,
          svc_name: None, selfish: false, url: "/dash?v=host",
          note: "进程 / 磁盘内存 / token 折算", cmd: None, glyph: "hostwork", group: "plugin" },
    App { id: "llmcost", name: "Token 价格追踪", scope: "service", port: Some(8776), svc_port: None,
          svc_name: Some("Token 价格追踪"), selfish: false, url: "http://127.0.0.1:8776/",
          note: "模型 API / 套餐 / 本地部署成本", cmd: Some(("13-算价", "_算价.command")),
          glyph: "llmcost", group: "other" },
    App { id: "follow", name: "关注", scope: "service", port: Some(8780), svc_port: None,
          svc_name: Some("关注追踪"), selfish: false, url: "http://127.0.0.1:8780/",
          note: "微博/B站/小红书/抖音更新", cmd: Some(("11-关注追踪", "_关注追踪.command")),
          glyph: "follow", group: "plugin" },
    App { id: "review", name: "界面评审", scope: "service", port: Some(8785), svc_port: None,
          svc_name: Some("界面评审"), selfish: false, url: "http://127.0.0.1:8785/",
          note: "UI 体检 / 待决定 / 已落地", cmd: Some(("18-界面评审", "_界面评审.command")),
          glyph: "review", group: "other" },
];

pub const LAZY_SERVICES: &[&str] =
    &["gallery", "reader", "media", "llmcost", "ghrepos", "devices", "games", "review"];

pub const INFRA_SERVICES: &[(&str, &str, i64)] = &[
    ("gateway", "微信助手 gateway", 18789),
    ("ollama", "本机模型 Ollama", 11434),
    ("qdrant", "向量库 Qdrant", 6333),
];

fn posix_ok(aid: &str) -> bool {
    // os.name == "posix"；Windows 分支在 Rust 侧按非 posix 处理
    let _ = aid;
    !cfg!(windows)
}

// ------------------------------------------------------------------ 用户插件清单

fn norm_plugin(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let name = clip(str_or(raw.get("name")).trim(), 24);
    if name.is_empty() {
        return None;
    }
    let soon = truthy(&raw.get("soon").cloned().unwrap_or(Value::Null));
    let ext = str_or(raw.get("ext")).trim().to_string();
    let mut port: Option<i64> = match raw.get("port") {
        Some(Value::Number(n)) => n.as_i64().filter(|p| *p != 0),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok().filter(|p| *p != 0),
        _ => None,
    };
    if port == Some(0) {
        port = None;
    }
    let mut url = str_or(raw.get("url")).trim().to_string();
    if url.is_empty() {
        if let Some(p) = port {
            if ext.is_empty() {
                url = format!("http://127.0.0.1:{p}/");
            }
        }
    }
    if url.is_empty() && !soon && ext.is_empty() {
        return None;
    }
    let mut pid = str_or(raw.get("id")).trim().to_lowercase();
    if !valid_pid(&pid) {
        pid = slug(str_or(raw.get("id")).as_str());
        if pid.is_empty() {
            pid = slug(&name);
        }
        if pid.is_empty() {
            if let Some(p) = port {
                pid = format!("p{p}");
            }
        }
        if !valid_pid(&pid) {
            pid = format!("p{}", random_hex(6));
        }
    }
    let cmd_t: Option<(String, String)> = match raw.get("cmd") {
        Some(Value::Array(a)) if a.len() == 2 => {
            let x = str_or(Some(&a[0])).trim().to_string();
            let y = str_or(Some(&a[1])).trim().to_string();
            if !x.is_empty() && !y.is_empty() { Some((x, y)) } else { None }
        }
        Some(Value::String(s)) if s.contains('/') => {
            let (d, f) = s.rsplit_once('/').unwrap();
            if !d.trim().is_empty() && !f.trim().is_empty() {
                Some((d.trim().to_string(), f.trim().to_string()))
            } else {
                None
            }
        }
        _ => None,
    };
    let mut group = str_or(raw.get("group")).trim().to_lowercase();
    if !["tool", "plugin", "other"].contains(&group.as_str()) {
        group = if port.is_some() || cmd_t.is_some() { "plugin" } else { "tool" }.to_string();
    }
    let glyph = {
        let g = str_or(raw.get("glyph")).trim().to_string();
        if g.is_empty() { "plugin".to_string() } else { g }
    };
    let svc_name = {
        let s = str_or(raw.get("svc_name")).trim().to_string();
        if s.is_empty() { name.clone() } else { s }
    };
    let scope = if !ext.is_empty() {
        "ext"
    } else if soon {
        "soon"
    } else if port.is_some() || cmd_t.is_some() {
        "service"
    } else {
        "view"
    };
    let mut m = Map::new();
    m.insert("id".into(), json!(pid));
    m.insert("name".into(), json!(name));
    m.insert("glyph".into(), json!(glyph));
    m.insert("note".into(), json!(clip(str_or(raw.get("note")).trim(), 60)));
    m.insert("url".into(), json!(url));
    m.insert("port".into(), match port { Some(p) => json!(p), None => Value::Null });
    m.insert("host".into(), json!(clip(str_or(raw.get("host")).trim(), 64)));
    m.insert("scope".into(), json!(scope));
    m.insert("cmd".into(), match cmd_t { Some((a, b)) => json!([a, b]), None => Value::Null });
    m.insert("svc_name".into(), json!(svc_name));
    m.insert("source".into(), json!("plugin"));
    m.insert("group".into(), json!(group));
    m.insert("soon".into(), json!(soon));
    m.insert("ext".into(), json!(ext));
    Some(Value::Object(m))
}

fn valid_pid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 32 {
        return false;
    }
    if !b[0].is_ascii_lowercase() {
        return false;
    }
    b[1..]
        .iter()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

fn slug(text: &str) -> String {
    let low = text.to_lowercase();
    let mut out = String::new();
    let mut prev_dash = false;
    for c in low.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    clip(&out, 24)
}

fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n.div_ceil(2)];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf));
    buf.iter().map(|b| format!("{b:02x}")).collect::<String>()[..n].to_string()
}

pub fn read_plugins(ctx: &Ctx) -> Vec<Value> {
    let d = load_json(&workbench(ctx).join("plugins.json")).unwrap_or(Value::Null);
    let rows: Vec<Value> = match &d {
        Value::Object(m) => m.get("plugins").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
        Value::Array(a) => a.clone(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in rows {
        if let Some(p) = norm_plugin(&raw) {
            if seen.insert(str_or(p.get("id"))) {
                out.push(p);
            }
        }
    }
    out
}

// ------------------------------------------------------------------ 卡片自报

fn auto_view_apps(ctx: &Ctx) -> Vec<Value> {
    let html = match std::fs::read_to_string(root(ctx).join("_工作台").join("dash.html")) {
        Ok(h) => h,
        Err(_) => return Vec::new(),
    };
    let tag_re = regex::Regex::new(r#"<[a-zA-Z][^>]*\bdata-card="([A-Za-z0-9_-]+)"[^>]*>"#).unwrap();
    let attr_re = regex::Regex::new(r#"data-tool(-glyph|-note|-group)?="([^"]*)""#).unwrap();
    let mut out = Vec::new();
    for m in tag_re.captures_iter(&html) {
        let whole = m.get(0).map(|x| x.as_str()).unwrap_or("");
        let cid = m.get(1).map(|x| x.as_str()).unwrap_or("");
        let mut attrs: HashMap<String, String> = HashMap::new();
        for a in attr_re.captures_iter(whole) {
            let k = a.get(1).map(|x| x.as_str()).unwrap_or("").to_string();
            let v = a.get(2).map(|x| x.as_str()).unwrap_or("").to_string();
            attrs.insert(k, v);
        }
        let name = attrs.get("").map(|s| s.trim().to_string()).unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let glyph = attrs
            .get("-glyph")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "pocket".to_string());
        let note = attrs.get("-note").map(|s| s.trim().to_string()).unwrap_or_default();
        let group = if attrs.get("-group").map(|s| s.trim().to_lowercase()).unwrap_or_default() == "other" {
            "other"
        } else {
            "tool"
        };
        out.push(json!({
            "id": cid,
            "name": name,
            "kind": "builtin",
            "glyph": glyph,
            "note": note,
            "url": format!("/dash?v=more#card={cid}"),
            "port": null,
            "scope": "view",
            "up": true,
            "group": group,
        }));
    }
    out
}

/// `_all_specs()`：内置 + plugins.json（同 id 覆盖）+ 卡片自报。
pub fn all_specs(ctx: &Ctx) -> Vec<Value> {
    let mut specs: Vec<Value> = BUILTIN_APPS
        .iter()
        .map(|a| {
            let mut m = Map::new();
            m.insert("id".into(), json!(a.id));
            m.insert("name".into(), json!(a.name));
            m.insert("scope".into(), json!(a.scope));
            m.insert("port".into(), match a.port { Some(p) => json!(p), None => Value::Null });
            m.insert("svc_port".into(), match a.svc_port { Some(p) => json!(p), None => Value::Null });
            if let Some(s) = a.svc_name {
                m.insert("svc_name".into(), json!(s));
            }
            if a.selfish {
                m.insert("self".into(), json!(true));
            }
            m.insert("url".into(), json!(a.url));
            m.insert("note".into(), json!(a.note));
            if let Some((d, f)) = a.cmd {
                m.insert("cmd".into(), json!([d, f]));
            }
            m.insert("glyph".into(), json!(a.glyph));
            m.insert("group".into(), json!(a.group));
            Value::Object(m)
        })
        .collect();
    for p in read_plugins(ctx) {
        let pid = str_or(p.get("id"));
        let mut hit = None;
        for (i, b) in specs.iter().enumerate() {
            if str_or(b.get("id")) == pid {
                hit = Some(i);
                break;
            }
        }
        if let Some(i) = hit {
            let mut merged = specs[i].as_object().cloned().unwrap_or_default();
            if let Value::Object(pm) = &p {
                for (k, v) in pm {
                    if v.is_null() {
                        continue;
                    }
                    if let Value::String(s) = v {
                        if s.is_empty() {
                            continue;
                        }
                    }
                    merged.insert(k.clone(), v.clone());
                }
            }
            specs[i] = Value::Object(merged);
        } else {
            specs.push(p);
        }
    }
    let have: HashSet<String> = specs.iter().map(|a| str_or(a.get("id"))).collect();
    for a in auto_view_apps(ctx) {
        if !have.contains(&str_or(a.get("id"))) {
            specs.push(a);
        }
    }
    specs.into_iter().filter(|a| posix_ok(&str_or(a.get("id")))).collect()
}

// ------------------------------------------------------------------ 探活

type ProbeMap = HashMap<(String, i64), (bool, Instant)>;
static SVCPROBE: Mutex<Option<ProbeMap>> = Mutex::new(None);

fn probe_port(port: i64, host: &str) -> bool {
    let host = if host.is_empty() { "127.0.0.1".to_string() } else { host.to_string() };
    let key = (host.clone(), port);
    {
        let g = SVCPROBE.lock().unwrap();
        if let Some(map) = g.as_ref() {
            if let Some((up, at)) = map.get(&key) {
                if at.elapsed() < Duration::from_secs(5) {
                    return *up;
                }
            }
        }
    }
    let up = platform::port_open(port as u16, &host, Duration::from_millis(350));
    if let Ok(mut g) = SVCPROBE.lock() {
        if g.is_none() {
            *g = Some(HashMap::new());
        }
        if let Some(map) = g.as_mut() {
            map.insert(key, (up, Instant::now()));
        }
    }
    up
}

fn which(bin: &str) -> bool {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':').chain(["/opt/homebrew/bin", "/usr/local/bin"]) {
        if !dir.is_empty() && Path::new(dir).join(bin).exists() {
            return true;
        }
    }
    false
}

fn launchd_has(aid: &str) -> bool {
    let dir = dirs_home().join("Library").join("LaunchAgents");
    let a = aid.to_lowercase();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_lowercase();
            if n.ends_with(".plist") && n.contains(&a) {
                return true;
            }
        }
    }
    false
}

pub fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

type InstMap = HashMap<(String, String), (Instant, bool, String)>;
static INSTALLED: Mutex<Option<InstMap>> = Mutex::new(None);

fn svc_installed(ctx: &Ctx, spec: &Value) -> (bool, String) {
    let aid = str_or(spec.get("id"));
    let root_s = root(ctx).to_string_lossy().to_string();
    let key = (aid.clone(), root_s.clone());
    {
        let g = INSTALLED.lock().unwrap();
        if let Some(map) = g.as_ref() {
            if let Some((at, ok, why)) = map.get(&key) {
                if at.elapsed() < Duration::from_secs(60) {
                    return (*ok, why.clone());
                }
            }
        }
    }
    let mut ok = true;
    let mut why = String::new();
    if truthy(&spec.get("self").cloned().unwrap_or(Value::Null))
        || truthy(&spec.get("ext").cloned().unwrap_or(Value::Null))
    {
        ok = true;
    } else if ["gateway", "ollama", "qdrant"].contains(&aid.as_str()) {
        let bins: &[&str] = match aid.as_str() {
            "gateway" => &["openclaw"],
            "ollama" => &["ollama"],
            _ => &["qdrant"],
        };
        let apps: &[&str] = match aid.as_str() {
            "gateway" => &["OpenClaw.app"],
            "ollama" => &["Ollama.app"],
            _ => &[],
        };
        let found = bins.iter().any(|b| which(b))
            || apps.iter().any(|a| Path::new("/Applications").join(a).exists());
        ok = found;
        if !ok {
            let name = {
                let n = str_or(spec.get("name"));
                if n.is_empty() { aid.clone() } else { n }
            };
            why = format!("这台机器上没装 {name}");
        }
    } else if let Some(Value::Array(a)) = spec.get("cmd") {
        if a.len() == 2 {
            let p = Path::new(&root_s)
                .join(str_or(Some(&a[0])))
                .join(str_or(Some(&a[1])));
            ok = (!root_s.is_empty() && p.exists()) || launchd_has(&aid);
            if !ok {
                why = format!("这台机器上没有它的启动脚本（{}）", p.display());
            }
        }
    }
    if let Ok(mut g) = INSTALLED.lock() {
        if g.is_none() {
            *g = Some(HashMap::new());
        }
        if let Some(map) = g.as_mut() {
            map.insert(key, (Instant::now(), ok, why.clone()));
        }
    }
    (ok, why)
}

/// `_services()`：端口探活。
pub fn services(ctx: &Ctx) -> Vec<Value> {
    let specs = all_specs(ctx);
    let mut items: Vec<(String, String, i64, String, bool, String)> = Vec::new();
    let mut spec_by_id: HashMap<String, Value> = HashMap::new();
    for a in &specs {
        if !posix_ok(&str_or(a.get("id"))) {
            continue;
        }
        let port = a
            .get("svc_port")
            .and_then(|v| v.as_i64())
            .or_else(|| a.get("port").and_then(|v| v.as_i64()));
        let Some(port) = port else { continue };
        let sack = truthy(&a.get("self").cloned().unwrap_or(Value::Null));
        let href = if sack { String::new() } else { str_or(a.get("url")) };
        let name = {
            let s = str_or(a.get("svc_name"));
            if s.is_empty() { str_or(a.get("name")) } else { s }
        };
        spec_by_id.insert(str_or(a.get("id")), a.clone());
        items.push((str_or(a.get("id")), name, port, href, sack, str_or(a.get("host"))));
    }
    for (sid, name, port) in INFRA_SERVICES {
        if !posix_ok(sid) {
            continue;
        }
        items.push((sid.to_string(), name.to_string(), *port, String::new(), false, String::new()));
    }
    // 并发探活（Python 用 8 线程池），每批 8 条
    let mut probe_results: Vec<bool> = Vec::with_capacity(items.len());
    for chunk in items.chunks(8) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|(_, _, port, _, always, host)| {
                let port = *port;
                let always = *always;
                let host = host.clone();
                std::thread::spawn(move || {
                    if always { true } else { probe_port(port, &host) }
                })
            })
            .collect();
        for h in handles {
            probe_results.push(h.join().unwrap_or(false));
        }
    }
    let mut out = Vec::new();
    for ((sid, name, port, href, _always, _host), up) in items.iter().zip(probe_results) {
        let spec = spec_by_id.get(sid).cloned().unwrap_or_else(|| json!({"id": sid, "name": name}));
        let (inst, why) = svc_installed(ctx, &spec);
        out.push(json!({
            "id": sid,
            "name": name,
            "port": port,
            "href": href,
            "up": up,
            "lazy": LAZY_SERVICES.contains(&sid.as_str()),
            "installed": up || inst,
            "why": if up { String::new() } else { why },
        }));
    }
    out
}

// ------------------------------------------------------------------ apps_payload

fn read_launchers(ctx: &Ctx) -> Vec<Value> {
    let d = load_json(&workbench(ctx).join("launchers.json")).unwrap_or(Value::Null);
    let rows = match d {
        Value::Object(m) => m.get("items").cloned().unwrap_or(json!([])),
        other => other,
    };
    match rows {
        Value::Array(a) => a
            .into_iter()
            .filter(|x| x.is_object() && !str_or(x.get("id")).is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

fn app_exists(p: &str) -> bool {
    !p.is_empty() && Path::new(p).exists()
}

pub fn apps_payload(ctx: &Ctx) -> Value {
    let mut builtin = Vec::new();
    for a in all_specs(ctx) {
        let id = str_or(a.get("id"));
        let src = {
            let s = str_or(a.get("source"));
            if s.is_empty() {
                if BUILTIN_APPS.iter().any(|b| b.id == id) { "builtin".to_string() } else { "card".to_string() }
            } else {
                s
            }
        };
        let ext_id = str_or(a.get("ext"));
        let rec = if ext_id.is_empty() { None } else { ext_recipe(ctx, &ext_id) };
        let soon = truthy(&a.get("soon").cloned().unwrap_or(Value::Null));
        let port = a.get("port").and_then(|v| v.as_i64());
        let up = if soon {
            false
        } else if let Some(r) = &rec {
            ext_running(r)
        } else if port.is_none() {
            true
        } else {
            probe_port(port.unwrap(), &str_or(a.get("host")))
        };
        let scope = {
            let s = str_or(a.get("scope"));
            if s.is_empty() { "service".to_string() } else { s }
        };
        let group = {
            let g = str_or(a.get("group"));
            if g.is_empty() { "plugin".to_string() } else { g }
        };
        builtin.push(json!({
            "id": id,
            "name": str_or(a.get("name")),
            "kind": "builtin",
            "glyph": str_or(a.get("glyph")),
            "note": str_or(a.get("note")),
            "url": str_or(a.get("url")),
            "port": match port { Some(p) => json!(p), None => Value::Null },
            "host": str_or(a.get("host")),
            "scope": scope,
            "group": group,
            "soon": soon,
            "ext": ext_id,
            "installed": rec.as_ref().map(ext_installed).unwrap_or(false),
            "source": src,
            "editable": src == "plugin",
            "up": up,
        }));
    }
    let mut custom = Vec::new();
    for it in read_launchers(ctx) {
        let kind = str_or(it.get("kind"));
        let alive = ["url", "bundle", "appname"].contains(&kind.as_str())
            || app_exists(&str_or(it.get("target")));
        let mut m = it.as_object().cloned().unwrap_or_default();
        m.insert("alive".into(), json!(alive));
        custom.push(Value::Object(m));
    }
    json!({"builtin": builtin, "custom": custom})
}

// ------------------------------------------------------------------ 第三方插件配方（只读）

fn ext_recipe(ctx: &Ctx, rid: &str) -> Option<Value> {
    let d = load_json(&workbench(ctx).join("第三方插件.json"))?;
    let rows = match &d {
        Value::Object(m) => m.get("recipes")?.as_array()?.clone(),
        Value::Array(a) => a.clone(),
        _ => return None,
    };
    rows.into_iter()
        .find(|r| r.is_object() && str_or(r.get("id")) == rid && !str_or(r.get("name")).is_empty())
}

fn ext_running(r: &Value) -> bool {
    let exe = {
        let app = str_or(r.get("app"));
        if app.is_empty() {
            str_or(r.get("exe"))
        } else {
            Path::new(&app).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()
        }
    };
    if !exe.is_empty() {
        let (code, out, _) = platform::run_cmd_full(&["pgrep", "-x", &exe], Duration::from_millis(1500));
        if code == 0 && !out.trim().is_empty() {
            return true;
        }
    }
    if let Some(p) = r.get("port").and_then(|v| v.as_i64()) {
        return platform::port_open(p as u16, "127.0.0.1", Duration::from_millis(350));
    }
    false
}

fn ext_installed(r: &Value) -> bool {
    let app = str_or(r.get("app"));
    if !app.is_empty() && Path::new(&app).is_dir() {
        return true;
    }
    let cli = str_or(r.get("cli"));
    !cli.is_empty() && which(&cli)
}

// ------------------------------------------------------------------ /api/plugincards

/// pgkit.CARDS 的逐字副本（w/h 必须与页面上的 data-w/data-h 一致）。
pub fn pg_cards(id: &str) -> Vec<(&'static str, &'static str, i64, i64)> {
    match id {
        "llmcost" => vec![
            ("verdict", "本轮结论", 2, 6),
            ("snapshot", "价格快照", 2, 6),
            ("stats", "数据规模", 4, 3),
            ("ladder", "模型天梯榜", 4, 10),
            ("value", "价格与实力", 4, 11),
            ("trend", "价格走势", 4, 14),
            ("level", "全盘水位", 4, 11),
            ("moves", "调价日志", 2, 5),
            ("matrix", "月度价格矩阵", 2, 9),
            ("apitable", "API 价目", 4, 10),
            ("apichart", "API 输出价排名", 4, 12),
            ("panotable", "模型 × 平台", 2, 11),
            ("platrank", "平台排行", 2, 11),
            ("plans", "订阅套餐", 4, 7),
            ("local", "本地部署", 4, 9),
            ("tasks", "任务换算", 4, 7),
            ("verify", "数据校验", 4, 9),
            ("timeline", "调价事件", 4, 8),
        ],
        "ghrepos" => vec![("repos", "仓库列表", 4, 12)],
        "devices" => vec![
            ("space", "设备空间", 4, 11),
            ("ledger", "设备台账", 2, 9),
            ("connect", "接入设备", 2, 12),
            ("pair", "手机接入", 4, 7),
        ],
        "follow" => vec![("feed", "关注流", 4, 12)],
        _ => Vec::new(),
    }
}

static PCARD_CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);

pub async fn plugincards(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let fresh_raw = crate::routes::common::q(&uri, "fresh");
    let fresh = fresh_raw.trim().parse::<i64>().map(|v| v != 0).unwrap_or(false);
    if !fresh {
        if let Ok(g) = PCARD_CACHE.lock() {
            if let Some((at, v)) = g.as_ref() {
                if at.elapsed() < Duration::from_secs(30) {
                    return Json(v.clone()).into_response();
                }
            }
        }
    }
    let c = ctx.clone();
    let data = tokio::task::spawn_blocking(move || {
        let apps = apps_payload(&c);
        let mut rows = Vec::new();
        let mut all: Vec<Value> = Vec::new();
        if let Some(b) = apps.get("builtin").and_then(|v| v.as_array()) {
            all.extend(b.clone());
        }
        if let Some(b) = apps.get("plugins").and_then(|v| v.as_array()) {
            all.extend(b.clone());
        }
        for a in all {
            let pid = str_or(a.get("id"));
            let cards = pg_cards(&pid);
            if cards.is_empty() {
                continue;
            }
            let nm = {
                let n = str_or(a.get("name"));
                if n.is_empty() { pid.clone() } else { n }
            };
            rows.push(json!({
                "id": pid,
                "name": nm,
                "port": a.get("port").cloned().unwrap_or(Value::Null),
                "up": truthy(&a.get("up").cloned().unwrap_or(Value::Null)),
                "url": str_or(a.get("url")),
                "cards": cards.iter().map(|(i, n, w, h)| json!({"id": i, "name": n, "w": w, "h": h})).collect::<Vec<_>>(),
            }));
        }
        json!({"ok": true, "items": rows, "at": now_hms()})
    })
    .await
    .unwrap_or_else(|_| json!({"ok": false, "error": "读不到插件清单", "items": []}));
    if let Ok(mut g) = PCARD_CACHE.lock() {
        *g = Some((Instant::now(), data.clone()));
    }
    Json(data).into_response()
}

// ------------------------------------------------------------------ /api/status

pub fn walk_inbox(ctx: &Ctx, limit: usize) -> Vec<(PathBuf, f64)> {
    let inbox = ctx.inbox_dir();
    if !inbox.exists() {
        return Vec::new();
    }
    let mut out: Vec<(PathBuf, f64)> = Vec::new();
    for p in super::util::rglob_all(&inbox, limit * 8) {
        if out.len() >= limit {
            break;
        }
        let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let is_file = std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false);
        if is_file && !name.starts_with('.') && name != "index.jsonl" {
            out.push((p.clone(), super::util::mtime(&p)));
        }
    }
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// 本机地址 / Tailscale / 主机名这套探测已经收进 `crate::tls`
// （`alt_hosts/lan_ip/tailscale_ip/tailscale_name`）—— 证书与 `/api/status`
// 必须用同一份口径，各留一份迟早会对不上。原来的六个函数已删除。

/// GW.tools()：POST /mcp tools/list；失败就当空表（网关不在不影响 /api/status）。
async fn gw_tool_names(ctx: &Ctx) -> Vec<String> {
    let base_raw = str_or(ctx.cfg.get("gateway_base"));
    let base = if base_raw.trim().is_empty() {
        "http://127.0.0.1:8080".to_string()
    } else {
        base_raw.trim().trim_end_matches('/').to_string()
    };
    let key = str_or(ctx.cfg.get("gateway_api_key"));
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(8)).build() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let resp = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("x-api-key", key.clone())
        .header("Authorization", format!("Bearer {}", if key.is_empty() { "none".into() } else { key.clone() }))
        .json(&body)
        .send()
        .await;
    let Ok(resp) = resp else { return Vec::new() };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(v) = resp.json::<Value>().await else { return Vec::new() };
    v.get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.get("name").and_then(|x| x.as_str()))
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub async fn status(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let tool_names = gw_tool_names(&ctx).await;
    let c = ctx.clone();
    let tools_len = tool_names.len();
    let v = tokio::task::spawn_blocking(move || {
        let files = walk_inbox(&c, 800);
        let inbox = c.inbox_dir();
        let roots = c.allowed_roots();
        let cert_dir = {
            let raw = str_or(c.cfg.get("tls_dir"));
            if raw.trim().is_empty() {
                c.state_dir.join("certs")
            } else {
                PathBuf::from(expand_tilde(&raw))
            }
        };
        let dav_root = {
            let raw = str_or(c.cfg.get("dav_root"));
            if raw.trim().is_empty() {
                c.library_dir.clone()
            } else {
                PathBuf::from(expand_tilde(&raw))
            }
        };
        json!({
            "gateway_ok": true,
            "model": str_or(c.cfg.get("model")),
            "tools": tools_len,
            "tool_names": tool_names,
            "inbox": inbox.to_string_lossy(),
            "inbox_count": files.len(),
            "home": roots.first().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            "dav": if truthy(&c.cfg.get("dav_enabled").cloned().unwrap_or(Value::Null)) { json!("/dav") } else { Value::Null },
            "dav_root": dav_root.to_string_lossy(),
            "tls": c.cfg.get("tls").map(truthy).unwrap_or(true),
            // 证书信息只能有一个来源：`crate::tls::describe`（它和签发/复用是同一套判定），
            // 在这儿再写一遍「哪个文件、覆不覆盖哪个名字」迟早会和实际加载的证书对不上。
            "cert": crate::tls::describe(&cert_dir),
            "private_only": c.cfg.get("allow_private_only").map(truthy).unwrap_or(true),
            "trash_count": crate::storage::list_trash(&c).len(),
            "usage": crate::storage::usage(&inbox),
            "max_upload_mb": c.cfg.get("max_upload_mb").cloned().unwrap_or(json!(500)),
        })
    })
    .await
    .unwrap_or_else(|_| json!({"detail": "采集线程崩了"}));
    Json(v).into_response()
}

pub fn expand_tilde(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        return dirs_home().join(rest).to_string_lossy().to_string();
    }
    s.to_string()
}

// ------------------------------------------------------------------ notes（dash 里要用）

pub fn notes_path(ctx: &Ctx) -> PathBuf {
    root(ctx).join("09-个人工作台").join("notes.json")
}

pub fn read_notes(ctx: &Ctx) -> Value {
    let d = load_json(&notes_path(ctx)).unwrap_or(Value::Null);
    let m = d.as_object().cloned().unwrap_or_default();
    let list = |k: &str| -> Value {
        match m.get(k) {
            Some(Value::Array(a)) => Value::Array(a.clone()),
            _ => json!([]),
        }
    };
    let diary = match m.get("diary") {
        Some(Value::Object(o)) => Value::Object(o.clone()),
        _ => json!({}),
    };
    json!({
        "todos": list("todos"),
        "events": list("events"),
        "memo": list("memo"),
        "diary": diary,
        "updated_at": str_or(m.get("updated_at")),
    })
}
