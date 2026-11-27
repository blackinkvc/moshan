// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`/api/pack/*` —— `.moshanplug` 插件包（pluginpack.py 的逐条移植）。
// 不做：不引 zip crate（Cargo.toml 不是本泳道能改的），zip 的中央目录自己解析，
//       deflate 走系统 libz 的 FFI —— 与 Python zipfile/zlib 同源。

#![allow(dead_code)]

use super::util::{self, expanduser, py_str, s_of, trunc};
use crate::paths::Ctx;
use crate::routes::common::{body_obj, peek, CtxState};
use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const MANIFEST: &str = "plugin.json";
const PACK_EXT: &[&str] = &[".moshanplug", ".zip"];
const PLUG_DIR: &str = "_插件";
const HISTORY_DIR: &str = ".history";

const MAX_FILES: usize = 20000;
const MAX_TOTAL: u64 = 512 * 1024 * 1024;
const MAX_ONE: u64 = 200 * 1024 * 1024;

const SKIP_DIRS: &[&str] = &["__MACOSX", ".git", ".svn", "node_modules", "__pycache__"];
const SKIP_NAMES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

const ID_RE: &str = r"^[a-z][a-z0-9_-]{0,31}$";
const USER_AGENT: &str = "Moshan-PluginPack/1.0";

#[derive(Debug)]
pub struct PackError(pub String);

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, PackError> {
    Err(PackError(msg.into()))
}

fn sv(s: &str) -> Value {
    Value::String(s.to_string())
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

fn plugins_dir(cfg: &Value) -> PathBuf {
    util::data_root(cfg).join(PLUG_DIR)
}

fn plugins_json(cfg: &Value) -> PathBuf {
    crate::paths::workbench_dir_for(cfg).join("plugins.json")
}

// ---------------------------------------------------------------- zip 读

// 解压 zip 里 method 8（deflate）的条目。
//
// **这里刻意不用 `#[link(name = "z")]`**：那是 macOS / Linux 的系统 zlib，
// Windows 上没有名为 z 的库，链接阶段直接
// `LNK1181: cannot open input file 'z.lib'` —— Windows 包就是因为这一行发不出去。
//
// 解压是确定性的（同一条 deflate 流只有一种展开结果），用 flate2 的纯 Rust 后端
// 与系统 zlib 逐字节一致。只有**压缩**才需要锁死具体实现（见 `routes/u/zipw.rs`，
// 那份限 `target_os = "macos"`，因为要和 Python 的 zlib 输出对齐）。
fn inflate_raw(input: &[u8], expected: usize) -> Option<Vec<u8>> {
    use flate2::{Decompress, FlushDecompress, Status};

    let mut d = Decompress::new(false); // false = raw deflate（windowBits -15）
    let mut out: Vec<u8> = Vec::with_capacity(expected.min(64 * 1024 * 1024));
    let mut buf = vec![0u8; 65536];
    loop {
        let (in0, out0) = (d.total_in(), d.total_out());
        let rest = input.get(in0 as usize..).unwrap_or(&[]);
        let st = d.decompress(rest, &mut buf, FlushDecompress::Finish).ok()?;
        let produced = (d.total_out() - out0) as usize;
        if produced > 0 {
            out.extend_from_slice(&buf[..produced]);
        }
        match st {
            Status::StreamEnd => return Some(out),
            _ => {
                // 一轮既没吃进输入也没产出 = 流截断或损坏，退出，别死循环。
                if produced == 0 && d.total_in() == in0 {
                    return None;
                }
            }
        }
    }
}

struct ZipEntry {
    name: String,
    flags: u16,
    method: u16,
    csize: u64,
    usize: u64,
    local_off: u64,
    extattr: u32,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn read_zip_entries(raw: &[u8]) -> Result<Vec<ZipEntry>, PackError> {
    // EOCD：从尾部往前找签名（注释最长 65535）。
    let n = raw.len();
    if n < 22 {
        return err("不是有效的 zip：File is not a zip file");
    }
    let start = n.saturating_sub(65535 + 22);
    let mut eocd = None;
    let mut i = n.saturating_sub(22);
    loop {
        if i >= start && u32le(raw, i) == 0x0605_4b50 {
            eocd = Some(i);
            break;
        }
        if i == 0 || i <= start {
            break;
        }
        i -= 1;
    }
    let Some(eocd) = eocd else {
        return err("不是有效的 zip：File is not a zip file");
    };
    if eocd + 22 > n {
        return err("不是有效的 zip：File is not a zip file");
    }
    let count = u16le(raw, eocd + 10) as usize;
    let cd_off = u32le(raw, eocd + 16) as u64;
    // zip64：EOCD 的值是哨兵时，暂时不支持（夹具与正常插件包远小于 4GB/65535 个）。
    if count == 0xFFFF || cd_off == 0xFFFF_FFFF {
        return err("不是有效的 zip：zip64 暂不支持");
    }
    let mut out: Vec<ZipEntry> = Vec::new();
    let mut p = cd_off as usize;
    for _ in 0..count {
        if p + 46 > n || u32le(raw, p) != 0x0201_4b50 {
            return err("不是有效的 zip：Bad central directory");
        }
        let flags = u16le(raw, p + 8);
        let method = u16le(raw, p + 10);
        let csize = u32le(raw, p + 20) as u64;
        let usize_ = u32le(raw, p + 24) as u64;
        let fnlen = u16le(raw, p + 28) as usize;
        let extralen = u16le(raw, p + 30) as usize;
        let commentlen = u16le(raw, p + 32) as usize;
        let extattr = u32le(raw, p + 38);
        let local_off = u32le(raw, p + 42) as u64;
        if p + 46 + fnlen > n {
            return err("不是有效的 zip：Bad central directory");
        }
        let name_bytes = &raw[p + 46..p + 46 + fnlen];
        let name = if flags & 0x800 != 0 {
            String::from_utf8_lossy(name_bytes).into_owned()
        } else {
            match std::str::from_utf8(name_bytes) {
                Ok(s) => s.to_string(),
                Err(_) => name_bytes.iter().map(|b| *b as char).collect(),
            }
        };
        // zip64 extra（0x0001）里可能补 8 字节字段；正常包用不到。
        if usize_ == 0xFFFF_FFFF || csize == 0xFFFF_FFFF {
            return err("不是有效的 zip：zip64 暂不支持");
        }
        out.push(ZipEntry { name, flags, method, csize, usize: usize_, local_off, extattr });
        p += 46 + fnlen + extralen + commentlen;
    }
    Ok(out)
}

impl ZipEntry {
    fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }

    fn is_symlink(&self) -> bool {
        let mode = (self.extattr >> 16) & 0xFFFF;
        mode & 0xF000 == 0xA000
    }
}

fn entry_data(raw: &[u8], e: &ZipEntry) -> Result<Vec<u8>, PackError> {
    let off = e.local_off as usize;
    if off + 30 > raw.len() || u32le(raw, off) != 0x0403_4b50 {
        return err(format!("解包 {} 失败：Bad local header", e.name));
    }
    let fnlen = u16le(raw, off + 26) as usize;
    let extralen = u16le(raw, off + 28) as usize;
    let data_start = off + 30 + fnlen + extralen;
    let end = data_start.saturating_add(e.csize as usize);
    if end > raw.len() {
        return err(format!("解包 {} 失败：truncated", e.name));
    }
    let body = &raw[data_start..end];
    match e.method {
        0 => Ok(body.to_vec()),
        8 => inflate_raw(body, e.usize as usize)
            .ok_or_else(|| PackError(format!("解包 {} 失败：bad deflate stream", e.name))),
        m => err(format!("解包 {} 失败：unsupported compression {m}", e.name)),
    }
}

fn is_zip_bytes(raw: &[u8]) -> bool {
    if raw.len() < 22 {
        return false;
    }
    let start = raw.len().saturating_sub(65535 + 22);
    let mut i = raw.len() - 22;
    loop {
        if u32le(raw, i) == 0x0605_4b50 {
            return true;
        }
        if i == 0 || i <= start {
            return false;
        }
        i -= 1;
    }
}

// ---------------------------------------------------------------- 路径与清单

fn safe_rel(name: &str) -> Result<String, PackError> {
    let mut n = name.replace('\\', "/");
    n = n.trim().to_string();
    while n.starts_with("./") {
        n = n[2..].to_string();
    }
    if n.is_empty() || n.ends_with('/') {
        return Ok(String::new());
    }
    if n.starts_with('/') {
        return err(format!("包里有个绝对路径，拒绝：{name}"));
    }
    let drive = regex::Regex::new(r"^[A-Za-z]:").unwrap();
    if drive.is_match(&n) {
        return err(format!("包里有个绝对路径，拒绝：{name}"));
    }
    let parts: Vec<&str> = n.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    if parts.iter().any(|p| *p == "..") {
        return err(format!("包里有个往上跳的路径，拒绝：{name}"));
    }
    Ok(parts.join("/"))
}

fn skip_member(name: &str) -> bool {
    let parts: Vec<&str> = name.split('/').collect();
    if parts.iter().any(|p| SKIP_DIRS.contains(p)) {
        return true;
    }
    let leaf = parts.last().copied().unwrap_or("");
    SKIP_NAMES.contains(&leaf) || leaf.starts_with("._")
}

fn read_manifest(p: &Path) -> Result<Value, PackError> {
    let text = match std::fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) => return err(format!("plugin.json 读不出来：{e}")),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => {
            if !v.is_object() {
                return err("plugin.json 要是一个对象");
            }
            Ok(v)
        }
        Err(e) => {
            let msg = if text.trim().is_empty() {
                "Expecting value: line 1 column 1 (char 0)".to_string()
            } else {
                e.to_string()
            };
            err(format!("plugin.json 不是合法 JSON：{msg}"))
        }
    }
}

fn manifest_of(root: &Path) -> Result<(PathBuf, Value, PathBuf), PackError> {
    let direct = root.join(MANIFEST);
    if direct.is_file() {
        let m = read_manifest(&direct)?;
        return Ok((root.to_path_buf(), m, direct));
    }
    let mut tops: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for ent in rd.flatten() {
            let p = ent.path();
            let name = util::name_of(&p);
            if p.is_dir() && !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') && !name.starts_with('_') {
                tops.push(p);
            }
        }
    }
    if tops.len() == 1 {
        let cand = tops[0].join(MANIFEST);
        if cand.is_file() {
            let m = read_manifest(&cand)?;
            return Ok((tops[0].clone(), m, cand));
        }
        let mut subs: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&tops[0]) {
            for ent in rd.flatten() {
                let p = ent.path();
                let name = util::name_of(&p);
                if p.is_dir() && !name.starts_with('.') && !name.starts_with('_') {
                    subs.push(p);
                }
            }
        }
        if subs.len() == 1 && subs[0].join(MANIFEST).is_file() {
            let cand = subs[0].join(MANIFEST);
            let m = read_manifest(&cand)?;
            return Ok((subs[0].clone(), m, cand));
        }
    }
    // 深度两层：先 */plugin.json，再 */*/plugin.json；各自按路径排序。
    let mut deep: Vec<PathBuf> = Vec::new();
    let mut one: Vec<PathBuf> = Vec::new();
    let mut two: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for ent in rd.flatten() {
            let d1 = ent.path();
            if !d1.is_dir() {
                continue;
            }
            if d1.join(MANIFEST).is_file() {
                one.push(d1.join(MANIFEST));
            }
            if let Ok(rd2) = std::fs::read_dir(&d1) {
                for e2 in rd2.flatten() {
                    let d2 = e2.path();
                    if d2.is_dir() && d2.join(MANIFEST).is_file() {
                        two.push(d2.join(MANIFEST));
                    }
                }
            }
        }
    }
    one.sort();
    two.sort();
    deep.extend(one);
    deep.extend(two);
    if let Some(p) = deep.first() {
        let m = read_manifest(p)?;
        return Ok((p.parent().unwrap_or(root).to_path_buf(), m, p.clone()));
    }
    err("包里没有 plugin.json —— 这不像一个墨山插件包")
}

fn norm_manifest(m: &Value) -> Result<Value, PackError> {
    let mut pid = s_of(m.get("id")).trim().to_lowercase();
    let name = {
        let n = s_of(m.get("name"));
        if n.trim().is_empty() { s_of(m.get("displayName")) } else { n }
    }
    .trim()
    .to_string();
    if pid.is_empty() {
        let re = regex::Regex::new(r"[^a-z0-9]+").unwrap();
        let s = re.replace_all(&name.to_lowercase(), "-").to_string();
        pid = trunc(s.trim_matches('-'), 24);
    }
    let id_re = regex::Regex::new(ID_RE).unwrap();
    if pid.is_empty() || !id_re.is_match(&pid) {
        return err("plugin.json 里的 id 要么没写，要么不合规（小写字母开头，只能小写字母/数字/-/_，32 位以内）");
    }
    if name.is_empty() {
        return err("plugin.json 里没写 name（顶栏上要显示的名字）");
    }
    let mut ver = {
        let v = s_of(m.get("version"));
        if v.trim().is_empty() { "0.0.0".to_string() } else { v }
    }
    .trim()
    .to_string();
    ver = ver.trim_start_matches(['v', 'V']).to_string();
    if ver.is_empty() {
        ver = "0.0.0".to_string();
    }

    let str_or = |keys: &[&str], d: &str| -> String {
        for k in keys {
            let v = s_of(m.get(*k));
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
        d.to_string()
    };
    let note = str_or(&["note", "summary"], "");
    let glyph = {
        let g = str_or(&["glyph"], "plugin");
        if g.is_empty() { "plugin".to_string() } else { g }
    };
    let group = {
        let g = s_of(m.get("group")).trim().to_lowercase();
        if g.is_empty() { "other".to_string() } else { g }
    };
    let args: Vec<Value> = match m.get("args") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| sv(&py_str(x)))
            .filter(|v| !s_of(Some(v)).trim().is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let platforms: Vec<Value> = match m.get("platforms") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| sv(&py_str(x).to_lowercase()))
            .filter(|v| !s_of(Some(v)).trim().is_empty())
            .collect(),
        _ => Vec::new(),
    };
    let requires = match m.get("requires") {
        Some(v) if v.is_object() => v.clone(),
        _ => obj(vec![]),
    };
    let update = match m.get("update") {
        Some(v) if v.is_object() => v.clone(),
        _ => obj(vec![]),
    };

    let mut out = Map::new();
    out.insert("id".into(), sv(&pid));
    out.insert("name".into(), sv(&trunc(&name, 24)));
    out.insert("version".into(), sv(&ver));
    out.insert("note".into(), sv(&trunc(&note, 60)));
    out.insert("glyph".into(), sv(&glyph));
    out.insert("group".into(), sv(&group));
    out.insert("port".into(), m.get("port").cloned().unwrap_or(Value::Null));
    out.insert("url".into(), sv(&s_of(m.get("url")).trim()));
    out.insert("entry".into(), sv(&s_of(m.get("entry")).trim()));
    out.insert("args".into(), Value::Array(args));
    out.insert("ping".into(), sv(&s_of(m.get("ping")).trim()));
    out.insert("data_env".into(), sv(&s_of(m.get("data_env")).trim()));
    out.insert("data_default".into(), sv(&s_of(m.get("data_default")).trim()));
    out.insert("repo".into(), sv(&s_of(m.get("repo")).trim()));
    out.insert("platforms".into(), Value::Array(platforms));
    out.insert("icon".into(), sv(&s_of(m.get("icon")).trim()));
    out.insert("requires".into(), requires);
    out.insert("update".into(), update);

    // port：None / "" / 0 / "0" → null；其余尽力转 int，转不了 null。
    let port = out.get("port").cloned().unwrap_or(Value::Null);
    let pv: Option<i64> = match &port {
        Value::Null => None,
        Value::String(s) if s.is_empty() || s == "0" => None,
        Value::Number(n) if n.as_i64() == Some(0) => None,
        Value::Bool(_) => Some(util::i64_of(Some(&port))),
        Value::Number(_) | Value::String(_) => Some(util::i64_of(Some(&port))),
        _ => None,
    };
    out.insert("port".into(), pv.map(|x| json!(x)).unwrap_or(Value::Null));

    let g = s_of(out.get("group"));
    if !matches!(g.as_str(), "tool" | "plugin" | "other") {
        out.insert("group".into(), sv("other"));
    }
    Ok(Value::Object(out))
}

fn platform_ok(m: &Value) -> (bool, String) {
    let plats: Vec<String> = match m.get("platforms") {
        Some(Value::Array(a)) => a.iter().map(|x| s_of(Some(x))).collect(),
        _ => Vec::new(),
    };
    if plats.is_empty() {
        return (true, String::new());
    }
    let me = if cfg!(windows) { "windows" } else if cfg!(target_os = "macos") { "macos" } else { "linux" };
    if plats.iter().any(|p| p == me) {
        (true, String::new())
    } else {
        (false, format!("这个包只给 {} 用，当前是 {me}", plats.join("、")))
    }
}

fn run_script(m: &Value) -> (String, String) {
    let entry = s_of(m.get("entry"));
    if entry.is_empty() {
        return (String::new(), String::new());
    }
    let args: Vec<String> = match m.get("args") {
        Some(Value::Array(a)) => a.iter().map(|x| s_of(Some(x))).collect(),
        _ => Vec::new(),
    };
    if cfg!(windows) {
        let data_env = s_of(m.get("data_env"));
        let mut body: Vec<String> = vec!["@echo off".into(), "cd /d \"%~dp0\"".into()];
        if !data_env.is_empty() {
            body.push(format!("set \"{data_env}=%{data_env}%\""));
        }
        body.push(format!("python \"{entry}\" {}", args.join(" ")));
        body.retain(|x| !x.is_empty());
        return ("run.bat".into(), format!("{}\r\n", body.join("\r\n")));
    }
    let quoted: Vec<String> = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
        .collect();
    let args_s = quoted.join(" ");
    let mut lines = vec![
        "#!/bin/bash".to_string(),
        "# 墨山中台自动生成：双击就能起这个插件。".to_string(),
        "cd \"$(dirname \"$0\")\" || exit 1".to_string(),
    ];
    let data_env = s_of(m.get("data_env"));
    let data_default = s_of(m.get("data_default"));
    if !data_env.is_empty() && !data_default.is_empty() {
        let mut dd = data_default.replace('\\', "\\\\").replace('"', "\\\"");
        if dd.starts_with('~') {
            dd = format!("$HOME{}", &dd[1..]);
        }
        lines.push(format!("export {data_env}=\"${{{data_env}:-{dd}}}\""));
    }
    lines.push("PY=\"$(command -v python3 || command -v python)\"".into());
    lines.push("[ -n \"$PY\" ] || { echo \"需要 python3\"; exit 1; }".into());
    lines.push(format!("exec \"$PY\" \"{entry}\" {args_s}").trim_end().to_string());
    ("run.command".into(), format!("{}\n", lines.join("\n")))
}

fn find_launcher(root: &Path) -> String {
    for name in ["run.command", "start.command", "run.sh", "run.bat", "start.bat"] {
        if root.join(name).is_file() {
            return name.to_string();
        }
    }
    String::new()
}

fn chmod_x(p: &Path) {
    crate::hostwork::platform::add_exec_bits(p);
}

// ---------------------------------------------------------------- plugins.json

fn read_rows(cfg: &Value) -> Vec<Value> {
    let p = plugins_json(cfg);
    if !p.exists() {
        return Vec::new();
    }
    let d = util::read_json(&p).unwrap_or(Value::Null);
    let rows = match &d {
        Value::Object(o) => o.get("plugins").cloned(),
        Value::Array(_) => Some(d.clone()),
        _ => None,
    };
    rows.and_then(|v| v.as_array().cloned())
        .map(|a| a.into_iter().filter(|x| x.is_object()).collect())
        .unwrap_or_default()
}

fn write_rows(cfg: &Value, rows: &[Value]) -> std::io::Result<()> {
    let p = plugins_json(cfg);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let old = if p.exists() { util::read_json(&p).unwrap_or(Value::Null) } else { Value::Null };
    let head = old.get("_说明").cloned().unwrap_or(Value::Null);
    let mut payload = Map::new();
    if !util::is_blank(&head) {
        payload.insert("_说明".into(), head);
    }
    payload.insert("plugins".into(), Value::Array(rows.to_vec()));
    payload.insert("updated_at".into(), sv(&util::now_iso()));
    let tmp = util::with_json_tmp(&p);
    std::fs::write(&tmp, crate::jsonio::dumps(&Value::Object(payload), 1))?;
    std::fs::rename(&tmp, &p)
}

fn find_row(cfg: &Value, pid: &str) -> Option<Value> {
    read_rows(cfg).into_iter().find(|r| s_of(r.get("id")) == pid)
}

fn val_present(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        _ => true,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn upsert_row(cfg: &Value, row: &Value) -> std::io::Result<Value> {
    let pid = s_of(row.get("id"));
    let mut rows = read_rows(cfg);
    for i in 0..rows.len() {
        if s_of(rows[i].get("id")) != pid {
            continue;
        }
        let mut merged = rows[i].as_object().cloned().unwrap_or_default();
        for k in ["name", "url", "port", "cmd", "svc_name"] {
            if val_present(row.get(k)) {
                merged.insert(k.to_string(), row.get(k).cloned().unwrap_or(Value::Null));
            }
        }
        for k in ["glyph", "note", "group"] {
            if truthy(row.get(k)) && !truthy(merged.get(k)) {
                merged.insert(k.to_string(), row.get(k).cloned().unwrap_or(Value::Null));
            }
        }
        for k in ["version", "repo", "pack", "update", "data_env", "data_default", "entry", "ping", "platforms"] {
            if row.get(k).is_some() {
                merged.insert(k.to_string(), row.get(k).cloned().unwrap_or(Value::Null));
            }
        }
        let v = Value::Object(merged);
        rows[i] = v.clone();
        write_rows(cfg, &rows)?;
        return Ok(v);
    }
    rows.push(row.clone());
    write_rows(cfg, &rows)?;
    Ok(row.clone())
}

fn drop_row(cfg: &Value, pid: &str) -> bool {
    let rows = read_rows(cfg);
    let keep: Vec<Value> = rows.iter().filter(|r| s_of(r.get("id")) != pid).cloned().collect();
    if keep.len() == rows.len() {
        return false;
    }
    let _ = write_rows(cfg, &keep);
    true
}

// ---------------------------------------------------------------- 解包

fn unpack_zip(src: &Path, dest: &Path) -> Result<(PathBuf, Value, usize, u64), PackError> {
    let raw = match std::fs::read(src) {
        Ok(r) => r,
        Err(e) => return err(format!("不是有效的 zip：{e}")),
    };
    let entries = read_zip_entries(&raw)?;
    if entries.len() > MAX_FILES {
        return err(format!("包里 {} 个文件，超过上限 {MAX_FILES}", entries.len()));
    }
    let declared: u64 = entries.iter().filter(|e| !e.is_dir()).map(|e| e.usize).sum();
    if declared > MAX_TOTAL {
        return err(format!(
            "解压后约 {:.0} MB，超过上限 {} MB",
            declared as f64 / 1048576.0,
            MAX_TOTAL / 1048576
        ));
    }
    std::fs::create_dir_all(dest).map_err(|e| PackError(format!("不是有效的 zip：{e}")))?;
    let base = util::py_resolve(dest);
    let mut n_files = 0usize;
    let mut n_bytes = 0u64;
    for e in &entries {
        if e.is_dir() {
            continue;
        }
        let rel = safe_rel(&e.name)?;
        if rel.is_empty() || skip_member(&rel) {
            continue;
        }
        if e.is_symlink() {
            return err(format!("包里有个符号链接，拒绝：{}", e.name));
        }
        if e.usize > MAX_ONE {
            return err(format!("{rel} 单个 {:.0} MB，太大了", e.usize as f64 / 1048576.0));
        }
        let target = base.join(&rel);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|x| PackError(format!("解包 {rel} 失败：{x}")))?;
        }
        let resolved_parent = target.parent().map(util::py_resolve).unwrap_or_else(|| base.clone());
        let resolved = resolved_parent.join(target.file_name().unwrap_or_default());
        let base_s = format!("{}/", base.to_string_lossy());
        if !resolved.to_string_lossy().starts_with(&base_s) && resolved != base {
            return err(format!("包里的路径会写到外面去，拒绝：{}", e.name));
        }
        let data = entry_data(&raw, e)?;
        std::fs::write(&target, &data).map_err(|x| PackError(format!("解包 {rel} 失败：{x}")))?;
        n_files += 1;
        n_bytes += std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
        if n_bytes > MAX_TOTAL {
            return err("解出来的体积超过上限，中止");
        }
    }
    let (root, m, _mp) = manifest_of(dest)?;
    Ok((root, m, n_files, n_bytes))
}

fn collect_files(pairs: &[(String, Vec<u8>)], dest: &Path) -> Result<(PathBuf, Value, usize, u64), PackError> {
    std::fs::create_dir_all(dest).map_err(|e| PackError(e.to_string()))?;
    let base = util::py_resolve(dest);
    let mut n = 0usize;
    let mut total = 0u64;
    for (rel, raw) in pairs {
        let name = safe_rel(rel)?;
        if name.is_empty() || skip_member(&name) {
            continue;
        }
        if raw.len() as u64 > MAX_ONE {
            return err(format!("{name} 单个 {:.0} MB，太大了", raw.len() as f64 / 1048576.0));
        }
        let target = base.join(&name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PackError(e.to_string()))?;
        }
        let resolved = util::py_resolve(&target);
        let base_s = format!("{}/", base.to_string_lossy());
        if !resolved.to_string_lossy().starts_with(&base_s) {
            return err(format!("路径会写到外面去，拒绝：{rel}"));
        }
        std::fs::write(&target, raw).map_err(|e| PackError(e.to_string()))?;
        n += 1;
        total += raw.len() as u64;
        if total > MAX_TOTAL {
            return err("内容超过上限，中止");
        }
    }
    if n > MAX_FILES {
        return err(format!("{n} 个文件，超过上限 {MAX_FILES}"));
    }
    let (root, m, _) = manifest_of(dest)?;
    Ok((root, m, n, total))
}

fn tree_preview(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    walk_files(root, root, &mut files);
    files.sort();
    let mut out: Vec<String> = Vec::new();
    for f in files {
        if f.split('/').any(|p| SKIP_DIRS.contains(&p)) {
            continue;
        }
        out.push(f);
        if out.len() >= 40 {
            out.push("…".into());
            break;
        }
    }
    out
}

fn walk_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            walk_files(root, &p, out);
        } else if p.is_file() {
            if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

// ---------------------------------------------------------------- 安装

fn ensure_plugins_dir(cfg: &Value) -> PathBuf {
    let d = plugins_dir(cfg);
    let _ = std::fs::create_dir_all(&d);
    d
}

fn mtime(p: &Path) -> f64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn prune_history(root: &Path, pid: &str, keep: usize) {
    let hist = root.join(HISTORY_DIR);
    if !hist.is_dir() {
        return;
    }
    let mut items: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&hist) {
        for ent in rd.flatten() {
            let p = ent.path();
            if util::name_of(&p).starts_with(&format!("{pid}-")) {
                items.push(p);
            }
        }
    }
    items.sort_by(|a, b| mtime(b).partial_cmp(&mtime(a)).unwrap_or(std::cmp::Ordering::Equal));
    for p in items.into_iter().skip(keep) {
        util::rmtree(&p);
    }
}

fn install_tree(
    cfg: &Value,
    pack_root: &Path,
    m: &Value,
    source: &str,
    files: usize,
    size: u64,
) -> Result<Value, PackError> {
    let pid = s_of(m.get("id"));
    let (ok, why) = platform_ok(m);
    if !ok {
        return err(why);
    }
    let dest_root = ensure_plugins_dir(cfg);
    let dest = dest_root.join(&pid);
    let mut warnings: Vec<String> = Vec::new();

    let mut old_ver = String::new();
    let mut keep_git = false;
    if dest.exists() {
        old_ver = util::read_json(&dest.join(MANIFEST))
            .map(|v| s_of(v.get("version")))
            .unwrap_or_default();
        keep_git = dest.join(".git").is_dir();
        let hist = dest_root.join(HISTORY_DIR);
        let _ = std::fs::create_dir_all(&hist);
        let stamp = util::local_fmt("%Y%m%d-%H%M%S");
        let old_name = if old_ver.is_empty() { "old" } else { old_ver.as_str() };
        let mut target = hist.join(format!("{pid}-{old_name}-{stamp}"));
        // shutil.move 的脾气：目标已存在（同一秒里连升两次就会撞上）时，它把源
        // 整个挪进目标目录里，而不是报错。照抄这一条，否则同秒升级一边成功一边失败。
        if target.is_dir() {
            target = target.join(&pid);
        }
        if let Err(e) = std::fs::rename(&dest, &target) {
            return err(format!("旧版挪不开（插件正在运行？）：{e}"));
        }
        prune_history(&dest_root, &pid, 3);
    }

    let _ = std::fs::create_dir_all(&dest);
    if let Ok(rd) = std::fs::read_dir(pack_root) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            if SKIP_DIRS.contains(&name.as_str()) || SKIP_NAMES.contains(&name.as_str()) || name.starts_with("._") {
                continue;
            }
            let from = ent.path();
            let to = dest.join(&name);
            if from.is_dir() {
                util::copy_tree(&from, &to);
            } else if from.is_file() {
                let _ = std::fs::copy(&from, &to);
            }
        }
    }

    let mut launcher = find_launcher(&dest);
    if launcher.is_empty() {
        let (fname, body) = run_script(m);
        if !fname.is_empty() && !body.is_empty() {
            let _ = std::fs::write(dest.join(&fname), body);
            launcher = fname;
            warnings.push("包里没带启动脚本，按 entry 生成了一个".into());
        }
    }
    if !launcher.is_empty() {
        chmod_x(&dest.join(&launcher));
    }

    if keep_git && !dest.join(".git").exists() {
        let hist = dest_root.join(HISTORY_DIR);
        let mut donors: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&hist) {
            for ent in rd.flatten() {
                let p = ent.path();
                if util::name_of(&p).starts_with(&format!("{pid}-")) {
                    donors.push(p);
                }
            }
        }
        donors.sort_by(|a, b| mtime(b).partial_cmp(&mtime(a)).unwrap_or(std::cmp::Ordering::Equal));
        for d in donors {
            let src = d.join(".git");
            if src.is_dir() {
                if std::fs::rename(&src, dest.join(".git")).is_ok() {
                    warnings.push("保留了原来的 .git，升级后还能 git pull".into());
                }
                break;
            }
        }
    }

    if !old_ver.is_empty() {
        let hist = dest_root.join(HISTORY_DIR);
        let mut donors: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&hist) {
            for ent in rd.flatten() {
                let p = ent.path();
                if util::name_of(&p).starts_with(&format!("{pid}-")) {
                    donors.push(p);
                }
            }
        }
        donors.sort_by(|a, b| mtime(b).partial_cmp(&mtime(a)).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(first) = donors.first() {
            if let Ok(rd) = std::fs::read_dir(first) {
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') || name == ".git" || name == ".DS_Store" {
                        continue;
                    }
                    let target = dest.join(&name);
                    if target.exists() {
                        continue;
                    }
                    let _ = std::fs::rename(ent.path(), &target);
                }
            }
        }
    }

    // manifest 回写：{**m, installed_at, source} + "\n"
    let mut mm = m.as_object().cloned().unwrap_or_default();
    mm.insert("installed_at".into(), sv(&util::now_iso()));
    mm.insert("source".into(), sv(source));
    let _ = std::fs::write(
        dest.join(MANIFEST),
        format!("{}\n", crate::jsonio::dumps(&Value::Object(mm), 1)),
    );

    let mut data_dir = String::new();
    if !s_of(m.get("data_default")).is_empty() {
        let raw = s_of(m.get("data_default")).replace('~', &util::home().to_string_lossy());
        let mut dd = PathBuf::from(&raw);
        if !dd.is_absolute() {
            dd = util::data_root(cfg).join(&raw);
        }
        match std::fs::create_dir_all(&dd) {
            Ok(_) => data_dir = dd.to_string_lossy().to_string(),
            Err(e) => warnings.push(format!("数据目录建不出来：{e}")),
        }
    }

    if !s_of(m.get("icon")).is_empty() {
        let icon = s_of(m.get("icon"));
        let src = dest.join(&icon);
        let suffix = util::suffix_of(&src).to_lowercase();
        if src.is_file() && [".png", ".jpg", ".jpeg", ".svg"].contains(&suffix.as_str()) {
            let ic = crate::paths::workbench_dir_for(cfg).join("_图标");
            let _ = std::fs::create_dir_all(&ic);
            let _ = std::fs::copy(&src, ic.join(format!("{pid}{suffix}")));
        } else {
            warnings.push(format!("图标 {icon} 在包里找不到，用默认图标"));
        }
    }

    let port = util::i64_of(m.get("port"));
    if port != 0 && util::port_open(port, 300) {
        warnings.push(format!("端口 {port} 现在有东西在跑 —— 如果就是它自己，正常；不是的话换一个"));
    }

    let mut row = Map::new();
    row.insert("id".into(), sv(&pid));
    row.insert("name".into(), m.get("name").cloned().unwrap_or(Value::Null));
    row.insert("version".into(), m.get("version").cloned().unwrap_or(Value::Null));
    let url = {
        let u = s_of(m.get("url"));
        if u.is_empty() && port != 0 {
            format!("http://127.0.0.1:{port}/")
        } else {
            u
        }
    };
    row.insert("url".into(), sv(&url));
    row.insert("port".into(), if port != 0 { json!(port) } else { Value::Null });
    row.insert(
        "cmd".into(),
        sv(&if launcher.is_empty() { String::new() } else { format!("{PLUG_DIR}/{pid}/{launcher}") }),
    );
    row.insert("glyph".into(), {
        let g = s_of(m.get("glyph"));
        sv(if g.is_empty() { "plugin" } else { &g })
    });
    row.insert("note".into(), m.get("note").cloned().unwrap_or_else(|| sv("")));
    row.insert("group".into(), {
        let g = s_of(m.get("group"));
        sv(if g.is_empty() { "other" } else { &g })
    });
    row.insert("svc_name".into(), m.get("name").cloned().unwrap_or(Value::Null));
    row.insert("pack".into(), json!(true));
    row.insert("repo".into(), m.get("repo").cloned().unwrap_or_else(|| sv("")));
    row.insert("data_env".into(), m.get("data_env").cloned().unwrap_or_else(|| sv("")));
    row.insert("data_default".into(), m.get("data_default").cloned().unwrap_or_else(|| sv("")));
    row.insert("entry".into(), m.get("entry").cloned().unwrap_or_else(|| sv("")));
    row.insert("ping".into(), m.get("ping").cloned().unwrap_or_else(|| sv("")));
    row.insert("platforms".into(), m.get("platforms").cloned().unwrap_or_else(|| json!([])));
    row.insert("update".into(), m.get("update").cloned().unwrap_or_else(|| obj(vec![])));
    row.insert("installed_at".into(), sv(&util::now_iso()));
    if url.is_empty() && port == 0 {
        warnings.push("包里没写 port / url —— 装上了，但顶栏点它没有页面".into());
    }
    let row_v = Value::Object(row.into_iter().filter(|(_, v)| !util::is_blank(v)).collect());
    match upsert_row(cfg, &row_v) {
        Ok(_) => {}
        Err(e) => return err(format!("插件文件装好了，但 plugins.json 写不进去：{e}")),
    }

    Ok(obj(vec![
        ("ok", json!(true)),
        ("plugin", row_v),
        ("upgraded", json!(!old_ver.is_empty())),
        ("old_version", sv(&old_ver)),
        ("files", json!(files)),
        ("size", json!(size)),
        ("dir", sv(&dest.to_string_lossy())),
        ("data_dir", sv(&data_dir)),
        ("launcher", sv(&launcher)),
        ("warnings", Value::Array(warnings.into_iter().map(|w| sv(&w)).collect())),
    ]))
}

// ---------------------------------------------------------------- 对外接口

pub async fn list(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    Json(json!({ "ok": true, "plugins": installed_packs(&ctx.cfg) })).into_response()
}

fn installed_packs(cfg: &Value) -> Vec<Value> {
    let d = plugins_dir(cfg);
    if !d.is_dir() {
        return Vec::new();
    }
    let rows: Vec<Value> = read_rows(cfg);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&d) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.is_dir() && !util::name_of(&p).starts_with('.') {
                dirs.push(p);
            }
        }
    }
    dirs.sort();
    let mut out = Vec::new();
    for p in dirs {
        let mf = p.join(MANIFEST);
        if !mf.is_file() {
            continue;
        }
        let Ok(raw) = read_manifest(&mf) else { continue };
        let Ok(m) = norm_manifest(&raw) else { continue };
        let id = s_of(m.get("id"));
        let row = rows.iter().find(|r| s_of(r.get("id")) == id).cloned().unwrap_or_else(|| obj(vec![]));
        let repo = {
            let a = s_of(m.get("repo"));
            if a.is_empty() { s_of(row.get("repo")) } else { a }
        };
        let note = {
            let a = s_of(m.get("note"));
            if a.is_empty() { s_of(row.get("note")) } else { a }
        };
        let data_default = {
            let a = s_of(m.get("data_default"));
            if a.is_empty() { s_of(row.get("data_default")) } else { a }
        };
        let port = match m.get("port") {
            Some(Value::Null) | None => row.get("port").cloned().unwrap_or(Value::Null),
            Some(v) if util::i64_of(Some(v)) == 0 => row.get("port").cloned().unwrap_or(Value::Null),
            Some(v) => v.clone(),
        };
        out.push(obj(vec![
            ("id", sv(&id)),
            ("name", m.get("name").cloned().unwrap_or(Value::Null)),
            ("version", m.get("version").cloned().unwrap_or(Value::Null)),
            ("repo", sv(&repo)),
            ("note", sv(&note)),
            ("dir", sv(&p.to_string_lossy())),
            ("registered", json!(rows.iter().any(|r| s_of(r.get("id")) == id))),
            ("data_default", sv(&data_default)),
            ("port", port),
        ]));
    }
    out
}

/// `_read_upload` 的 Rust 等价：整个读进内存，但有上限。
fn read_upload_part(data: &[u8], limit: u64, what: &str) -> Result<Vec<u8>, String> {
    if data.len() as u64 > limit {
        return Err(format!("{what}超过 {}MB 上限", limit / 1048576));
    }
    Ok(data.to_vec())
}

pub async fn preview(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let mut file: Option<(String, Vec<u8>)> = None;
    let mut mp = mp;
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let fname = field.file_name().unwrap_or("").to_string();
        let data = match field.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => return Json(obj(vec![
                ("ok", json!(false)),
                ("error", sv(&format!("读不了这个文件：{e}"))),
            ]))
            .into_response(),
        };
        if name == "file" {
            file = Some((fname, data));
        }
    }
    let Some((fname, data)) = file else {
        return util::validation_missing("file");
    };
    let tmp = util::mkdtemp("mpk-prev-");
    let p = tmp.join(if fname.is_empty() { "pack.moshanplug".to_string() } else { fname });
    let res = (|| -> Result<Value, PackError> {
        let raw = read_upload_part(&data, 256 * 1048576, "插件包")
            .map_err(PackError)?;
        std::fs::write(&p, raw).map_err(|e| PackError(format!("读不了这个文件：{e}")))?;
        inspect_zip(&p)
    })();
    util::rmtree(&tmp);
    match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(obj(vec![("ok", json!(false)), ("error", sv(&e.0))])).into_response(),
    }
}

fn inspect_zip(path: &Path) -> Result<Value, PackError> {
    let tmp = util::mkdtemp("mpk-");
    let r = (|| -> Result<Value, PackError> {
        let (root, m, n, size) = unpack_zip(path, &tmp.join("pack"))?;
        let nm = norm_manifest(&m)?;
        let (ok, why) = platform_ok(&nm);
        Ok(obj(vec![
            ("ok", json!(true)),
            ("manifest", nm),
            ("files", json!(n)),
            ("size", json!(size)),
            ("platform_ok", json!(ok)),
            ("platform_note", sv(&why)),
            ("has_launcher", json!(!find_launcher(&root).is_empty())),
            ("preview", Value::Array(tree_preview(&root).into_iter().map(|s| sv(&s)).collect())),
        ]))
    })();
    util::rmtree(&tmp);
    r
}

pub async fn install(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let mut file: Option<(String, Vec<u8>)> = None;
    let mut mp = mp;
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let fname = field.file_name().unwrap_or("").to_string();
        let data = match field.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return Json(obj(vec![("ok", json!(false)), ("error", sv(&format!("写不进去：{e}")))]))
                    .into_response()
            }
        };
        if name == "file" {
            file = Some((fname, data));
        }
    }
    let Some((fname, data)) = file else {
        return util::validation_missing("file");
    };
    let res = (|| -> Result<Value, PackError> {
        let raw = read_upload_part(&data, 256 * 1048576, "插件包").map_err(PackError)?;
        install_raw(&ctx.cfg, &raw, &fname)
    })();
    match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(obj(vec![("ok", json!(false)), ("error", sv(&e.0))])).into_response(),
    }
}

fn install_zip(cfg: &Value, src: &Path, source: &str) -> Result<Value, PackError> {
    let tmp = util::mkdtemp("mpk-");
    let r = (|| -> Result<Value, PackError> {
        let (root, m, n, size) = unpack_zip(src, &tmp.join("pack"))?;
        let nm = norm_manifest(&m)?;
        install_tree(cfg, &root, &nm, source, n, size)
    })();
    util::rmtree(&tmp);
    r
}

fn install_raw(cfg: &Value, raw: &[u8], name: &str) -> Result<Value, PackError> {
    let lower = name.to_lowercase();
    let by_ext = PACK_EXT.iter().any(|e| lower.ends_with(e));
    if !by_ext && !is_zip_bytes(raw) {
        return err("这个文件不是插件包（.moshanplug / .zip）");
    }
    let tmp = util::mkdtemp("mpk-");
    let fname = if name.is_empty() { "pack.moshanplug" } else { name };
    let f = tmp.join(fname);
    let r = (|| -> Result<Value, PackError> {
        std::fs::write(&f, raw).map_err(|e| PackError(e.to_string()))?;
        let (root, m, n, size) = unpack_zip(&f, &tmp.join("pack"))?;
        let nm = norm_manifest(&m)?;
        install_tree(cfg, &root, &nm, "upload", n, size)
    })();
    util::rmtree(&tmp);
    r
}

fn install_files(cfg: &Value, pairs: &[(String, Vec<u8>)], source: &str) -> Result<Value, PackError> {
    let tmp = util::mkdtemp("mpk-");
    let r = (|| -> Result<Value, PackError> {
        let (root, m, n, size) = collect_files(pairs, &tmp.join("pack"))?;
        let nm = norm_manifest(&m)?;
        install_tree(cfg, &root, &nm, source, n, size)
    })();
    util::rmtree(&tmp);
    r
}

fn install_dir(cfg: &Value, path: &Path) -> Result<Value, PackError> {
    let p = expanduser(&path.to_string_lossy());
    if !p.exists() {
        return err(format!("这个路径不存在：{}", p.to_string_lossy()));
    }
    if p.is_file() {
        return install_zip(cfg, &p, "path");
    }
    let (root, _m, _) = manifest_of(&p)?;
    let mut files: Vec<PathBuf> = Vec::new();
    walk_all(&root, &mut files);
    files.sort();
    let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
    for f in files {
        if !f.is_file() {
            continue;
        }
        let rel = f.strip_prefix(&root).map(|x| x.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        if skip_member(&rel) {
            continue;
        }
        if let Ok(raw) = std::fs::read(&f) {
            pairs.push((rel, raw));
        }
    }
    install_files(cfg, &pairs, "path")
}

fn walk_all(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            walk_all(&p, out);
        } else {
            out.push(p);
        }
    }
}

pub async fn install_url_api(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    let url = s_of(b.get("url"));
    match install_url(&ctx.cfg, &url).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(obj(vec![("ok", json!(false)), ("error", sv(&e.0))])).into_response(),
    }
}

/// urllib 的报错形状：`<urlopen error [Errno 61] Connection refused>`。
/// reqwest 只会说「error sending request for url(…)」，这里把底层 io 错误挖出来
/// 还原成 Python 那句话 —— 前端/用户看到的文案得对得上。
fn url_err_text(e: &reqwest::Error) -> String {
    use std::error::Error as StdError;
    if e.is_timeout() {
        return "<urlopen error timed out>".to_string();
    }
    let mut src: Option<&(dyn StdError + 'static)> = StdError::source(e);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            if let Some(code) = io.raw_os_error() {
                let msg = match code {
                    61 => "Connection refused".to_string(),
                    60 => "Operation timed out".to_string(),
                    8 => "nodename nor servname provided, or not known".to_string(),
                    51 => "Network is unreachable".to_string(),
                    54 => "Connection reset by peer".to_string(),
                    _ => io.to_string(),
                };
                return format!("<urlopen error [Errno {code}] {msg}>");
            }
            return format!("<urlopen error {io}>");
        }
        src = StdError::source(s);
    }
    format!("<urlopen error {e}>")
}

async fn install_url(cfg: &Value, url: &str) -> Result<Value, PackError> {
    let u = url.trim();
    let re = regex::Regex::new(r"(?i)^https?://").unwrap();
    if !re.is_match(u) {
        return err("要给一个 http(s) 地址");
    }
    let rel = regex::Regex::new(r"github\.com/[^/]+/[^/]+/releases/latest/?$").unwrap();
    if rel.is_match(u) {
        return err("这是 Release 页面地址，不是包地址。用 …/releases/latest/download/<包名>.moshanplug");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| PackError(format!("下载失败：{e}")))?;
    let resp = client
        .get(u)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .map_err(|e| PackError(format!("下载失败：{}", url_err_text(&e))))?;
    if !resp.status().is_success() {
        return err(format!("下载失败：HTTP {}", resp.status().as_u16()));
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let mut raw: Vec<u8> = Vec::new();
    let mut resp = resp;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                raw.extend_from_slice(&chunk);
                if raw.len() as u64 > MAX_TOTAL {
                    return err("下载超过上限，中止");
                }
            }
            Ok(None) => break,
            Err(e) => return err(format!("下载失败：{}", url_err_text(&e))),
        }
    }
    if ctype.contains("text/html") && !is_zip_bytes(&raw) {
        return err("这个地址返回的是网页，不是插件包（是不是给了项目主页而不是文件地址？）");
    }
    let path = u.split('?').next().unwrap_or(u).split('#').next().unwrap_or(u);
    let name = path.rsplit('/').next().unwrap_or("");
    let name = if name.is_empty() { "pack.moshanplug" } else { name };
    install_raw(cfg, &raw, name)
}

pub async fn install_files_api(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    mp: Multipart,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let mut rel: Vec<Option<String>> = Vec::new();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total = 0u64;
    let mut mp = mp;
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "paths" {
            let text = field.text().await.unwrap_or_default();
            if text.trim_start().starts_with('[') {
                if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&text) {
                    rel = a
                        .iter()
                        .map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
            }
            continue;
        }
        if name != "files" {
            continue;
        }
        let fname = field.file_name().unwrap_or("").to_string();
        let data = match field.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                return Json(obj(vec![("ok", json!(false)), ("error", sv(&format!("读 {fname} 失败：{e}")))]))
                    .into_response()
            }
        };
        files.push((fname, data));
    }
    let mut pairs: Vec<(String, Vec<u8>)> = Vec::new();
    for (i, (fname, data)) in files.into_iter().enumerate() {
        let name = match rel.get(i).and_then(|x| x.clone()) {
            Some(s) => s,
            None => fname.clone(),
        };
        let got = match read_upload_part(&data, 256 * 1048576, &format!("文件 {name}")) {
            Ok(g) => g,
            Err(msg) => return Json(obj(vec![("ok", json!(false)), ("error", sv(&msg))])).into_response(),
        };
        total += got.len() as u64;
        if total > 256 * 1048576 {
            return Json(obj(vec![
                ("ok", json!(false)),
                ("error", sv("这一批文件超过插件包上限")),
            ]))
            .into_response();
        }
        pairs.push((name, got));
    }
    if pairs.is_empty() {
        return util::validation_missing("files");
    }
    match install_files(&ctx.cfg, &pairs, "folder") {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(obj(vec![("ok", json!(false)), ("error", sv(&e.0))])).into_response(),
    }
}

pub async fn install_path_api(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    let raw = s_of(b.get("path")).trim().to_string();
    if raw.is_empty() {
        return Json(obj(vec![("ok", json!(false)), ("error", sv("要给一个路径"))])).into_response();
    }
    let p = expanduser(&raw);
    let p = util::py_resolve(&p);
    let roots: Vec<PathBuf> = ctx
        .cfg
        .get("allowed_roots")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| util::py_resolve(&expanduser(s))).collect())
        .unwrap_or_default();
    let inside = |p: &Path, r: &Path| p.starts_with(r);
    if !roots.is_empty() && !roots.iter().any(|r| inside(&p, r)) {
        return Json(obj(vec![
            ("ok", json!(false)),
            ("error", sv("只允许装数据根以内的目录")),
        ]))
        .into_response();
    }
    match install_dir(&ctx.cfg, &p) {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(obj(vec![("ok", json!(false)), ("error", sv(&e.0))])).into_response(),
    }
}

pub async fn uninstall(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    match do_uninstall(&ctx.cfg, &s_of(b.get("id"))) {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(obj(vec![("ok", json!(false)), ("error", sv(&e.0))])).into_response(),
    }
}

fn do_uninstall(cfg: &Value, pid: &str) -> Result<Value, PackError> {
    let id_re = regex::Regex::new(ID_RE).unwrap();
    if !id_re.is_match(pid) {
        return err("id 不合规");
    }
    let dest = plugins_dir(cfg).join(pid);
    let had_row = drop_row(cfg, pid);
    let mut moved = String::new();
    if dest.exists() {
        let hist = ensure_plugins_dir(cfg).join(HISTORY_DIR);
        let _ = std::fs::create_dir_all(&hist);
        let stamp = util::local_fmt("%Y%m%d-%H%M%S");
        let target = hist.join(format!("{pid}-uninstalled-{stamp}"));
        if let Err(e) = std::fs::rename(&dest, &target) {
            return err(format!("删不掉（可能正在运行）：{e}"));
        }
        moved = target.to_string_lossy().to_string();
    }
    if !had_row && moved.is_empty() {
        return err("清单里没有这个插件，目录里也没有");
    }
    // 注意：Python 在 drop_row 之后才 find_row，所以 data_kept 永远是空串。
    // 这是上游的既有行为，照抄。
    let row = find_row(cfg, pid).unwrap_or_else(|| obj(vec![]));
    let data_kept = s_of(row.get("data_default"));
    Ok(obj(vec![
        ("ok", json!(true)),
        ("moved_to", sv(&moved)),
        ("data_kept", sv(&data_kept)),
        ("note", sv(if data_kept.is_empty() { "" } else { "程序已移走，数据目录没动" })),
    ]))
}

pub async fn upgrade(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    match do_upgrade(&ctx, &s_of(b.get("id"))).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => Json(obj(vec![("ok", json!(false)), ("error", sv(&e.0))])).into_response(),
    }
}

fn load_cache(ctx: &Ctx) -> Value {
    util::read_json(&ctx.state_dir.join("plugin-updates.json")).unwrap_or(Value::Null)
}

async fn do_upgrade(ctx: &Ctx, pid: &str) -> Result<Value, PackError> {
    let cfg = &ctx.cfg;
    let Some(row) = find_row(cfg, pid) else {
        return err("清单里没有这个插件");
    };
    let upd = match row.get("update") {
        Some(v) if v.is_object() => v.clone(),
        _ => obj(vec![]),
    };
    let mut pack_url = {
        let a = s_of(upd.get("pack"));
        if a.trim().is_empty() { s_of(upd.get("zip")) } else { a }
    }
    .trim()
    .to_string();

    if pack_url.is_empty() {
        let mut man = s_of(upd.get("manifest")).trim().to_string();
        if man.is_empty() {
            let cache = load_cache(ctx);
            man = s_of(cache.get(pid).and_then(|v| v.get("url"))).trim().to_string();
        }
        let json_re = regex::Regex::new(r"/[^/]+\.json$").unwrap();
        if !man.is_empty() && json_re.is_match(&man) {
            pack_url = format!("{}/{pid}.moshanplug", man.rsplit_once('/').map(|x| x.0).unwrap_or(""));
        }
    }
    if pack_url.is_empty() {
        let repo_raw = {
            let a = s_of(upd.get("repo"));
            if a.is_empty() { s_of(row.get("repo")) } else { a }
        };
        let repo = regex::Regex::new(r"^https?://github\.com/")
            .unwrap()
            .replace(&repo_raw, "")
            .trim_matches('/')
            .to_string();
        if !repo.is_empty() {
            pack_url = format!("https://github.com/{repo}/releases/latest/download/{pid}.moshanplug");
        }
    }
    if pack_url.is_empty() {
        return err("这个版本里没写更新地址，自己下不了新版 —— 把新包拖进中台就行");
    }
    let mut res = match install_url(cfg, &pack_url).await {
        Ok(v) => v,
        Err(e) => return err(format!("{}（下载地址：{pack_url}）", e.0)),
    };
    if let Some(o) = res.as_object_mut() {
        o.insert("upgraded".into(), json!(true));
    }
    Ok(res)
}
