// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/pushupd.py` 的移植 —— 主机推送更新的落盘、校验、切换、回滚、
//       状态查询（`/api/update/push|status|rollback`）。
// 不做：不执行重启脚本。Python 的 `_commit` 会写 restart.sh 然后 detached 拉起，
//       那个脚本 3 秒后 `kill <pid>` 再 `exec python server.py`；在迁移期真跑
//       等于把正在对拍的服务杀掉。这里**写同样的脚本、回同样的形状**，但不 Popen。
//       汇报里把这条列为已知分叉。
//
// zip 解包用 `zip` crate（Cargo 已有）：系统 unzip 在 Windows 上不存在。
// 白名单、`..` 拒绝、大小上限全部由本文件自己判定，与 `pushupd._safe_name` 同口径。

#![allow(dead_code)]

use crate::paths::Ctx;
use base64::Engine;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAX_PKG_BYTES: usize = 64 * 1048576;
const MAX_UNPACK_BYTES: u64 = 512 * 1048576;

const ALLOW_SUFFIX: &[&str] = &[
    "py", "html", "css", "js", "json", "txt", "md", "pem", "crt", "key", "svg", "ico", "png", "jpg",
];
const SKIP_DIRS: &[&str] = &["__pycache__", ".git", ".idea", ".vscode"];

pub enum PushError {
    TooBig(String),
    Other(String),
}

fn root(ctx: &Ctx) -> PathBuf {
    ctx.state_dir.join("updates")
}

fn ensure(ctx: &Ctx) -> PathBuf {
    let d = root(ctx);
    let _ = std::fs::create_dir_all(d.join("pkgs"));
    d
}

fn read_json(p: &Path) -> Value {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_json(p: &Path, obj: &Value) {
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = crate::jsonio::atomic_write(p, &crate::jsonio::dumps(obj, 1));
}

fn safe_name(name: &str) -> Option<String> {
    let name = name.replace('\\', "/");
    let name = name.trim().to_string();
    if name.is_empty() || name.ends_with('/') {
        return None;
    }
    let parts: Vec<&str> = name.split('/').filter(|x| !x.is_empty() && *x != ".").collect();
    if parts.is_empty() || parts.iter().any(|x| *x == "..") {
        return None;
    }
    if parts.iter().any(|x| SKIP_DIRS.contains(x)) {
        return None;
    }
    let last = parts[parts.len() - 1];
    if last == "VERSION" {
        return Some(parts.join("/"));
    }
    let ext = Path::new(last)
        .extension()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !ALLOW_SUFFIX.contains(&ext.as_str()) {
        return None;
    }
    Some(parts.join("/"))
}

struct Unpacked {
    files: i64,
    bytes: u64,
    skipped: Vec<String>,
    skipped_n: usize,
}

/// 打开 zip：错误文案与 Python 的 `zipfile.BadZipFile` 对齐，调用方按原文案回给前端。
fn open_zip(path: &Path) -> Result<zip::ZipArchive<std::fs::File>, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("File is not a zip file（{e}）"))?;
    zip::ZipArchive::new(f).map_err(|_| "File is not a zip file".to_string())
}

/// 读 ZIP 中央目录里的条目名（顺序与 `zipfile.namelist()` 一致）。
///
/// 为什么不用系统 `unzip -Z1`：Info-ZIP 6.0 在 macOS 上会把 UTF-8 名字按本地
/// 字符集转码，中文目录名出来是 `?`；Windows 上更是根本没有 `/usr/bin/unzip`。
/// 用 `zip` crate 直接读中央目录，三平台同一份结果。
fn zip_names(ar: &mut zip::ZipArchive<std::fs::File>) -> Result<Vec<String>, String> {
    let mut names = Vec::with_capacity(ar.len());
    for i in 0..ar.len() {
        let f = ar.by_index_raw(i).map_err(|_| "File is not a zip file".to_string())?;
        names.push(f.name().to_string());
    }
    Ok(names)
}

/// `_unpack`：逐条校验名字后解到 dest。dest 必须已存在。
///
/// 解压走 `zip` crate（Cargo 里已有），不再 fork `/usr/bin/unzip -p`：
/// 后者在 Windows 上不存在，Linux 上还依赖发行版是否装了 unzip。
fn unpack(raw_path: &Path, dest: &Path) -> Result<Unpacked, PushError> {
    use std::io::Read;
    let mut ar = open_zip(raw_path).map_err(PushError::Other)?;
    let names = zip_names(&mut ar).map_err(PushError::Other)?;
    let mut files = 0i64;
    let mut written: u64 = 0;
    let mut skipped: Vec<String> = Vec::new();
    let mut skipped_n = 0usize;
    for (i, name) in names.iter().enumerate() {
        let name = name.as_str();
        if name.is_empty() || name.ends_with('/') {
            continue;
        }
        let Some(rel) = safe_name(name) else {
            skipped_n += 1;
            if skipped.len() < 5 {
                skipped.push(name.to_string());
            }
            continue;
        };
        let mut data = Vec::new();
        {
            let mut zf = ar.by_index(i).map_err(|_| PushError::Other("解压条目失败".into()))?;
            zf.read_to_end(&mut data)
                .map_err(|_| PushError::Other("解压条目失败".into()))?;
        }
        let out_path = dest.join(&rel);
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| PushError::Other(format!("{e}")))?;
        }
        std::fs::write(&out_path, &data).map_err(|e| PushError::Other(format!("{e}")))?;
        written += data.len() as u64;
        files += 1;
        if written > MAX_UNPACK_BYTES {
            return Err(PushError::TooBig(format!(
                "解压后超过 {}MB，疑似压缩炸弹，已中止",
                MAX_UNPACK_BYTES / 1048576
            )));
        }
    }
    Ok(Unpacked { files, bytes: written, skipped, skipped_n })
}

fn copy_tree(src: &Path, dst: &Path, skip_logs: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let name = ent.file_name().to_string_lossy().to_string();
        let from = ent.path();
        let to = dst.join(&name);
        if from.is_dir() {
            if name == "__pycache__" {
                continue;
            }
            copy_tree(&from, &to, skip_logs)?;
        } else {
            if name.ends_with(".pyc") || (skip_logs && name.ends_with(".log")) {
                continue;
            }
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

fn app_version(app: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(app.join("version.py")) else {
        return "unknown".into();
    };
    let re = regex::Regex::new(r#"VERSION\s*=\s*"([^"]+)""#).unwrap();
    re.captures(&text)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn restart_script(ctx: &Ctx, new_dir: &Path) -> Value {
    let d = ensure(ctx);
    let pid = std::process::id();
    let py = "python3";
    let entry = new_dir.join("server.py");
    let sh = d.join("restart.sh");
    let text = format!(
        "#!/bin/sh\nsleep 3\nkill {pid} 2>/dev/null\nsleep 2\ncd '{}' && exec '{py}' '{}' >>/dev/null 2>&1 &\n",
        new_dir.to_string_lossy(),
        entry.to_string_lossy()
    );
    let _ = std::fs::write(&sh, text);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755));
    }
    // 不真拉起：见文件头。响应形状与 Python 的 POSIX 分支一致。
    json!({"ok": true, "how": "sh", "script": sh.to_string_lossy(), "pid": pid})
}

fn commit(
    ctx: &Ctx,
    version: &str,
    dest: &Path,
    source: &str,
    restart: bool,
    backup: &str,
) -> Value {
    let d = ensure(ctx);
    let prev = read_json(&d.join("current.json"));
    let prev_ver = prev.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut cur = Map::new();
    cur.insert("version".into(), json!(version));
    cur.insert("dir".into(), json!(dest.to_string_lossy()));
    cur.insert("at".into(), json!(at));
    cur.insert("source".into(), json!(source));
    cur.insert("backup".into(), json!(backup));
    cur.insert("prev".into(), json!(prev_ver));
    write_json(&d.join("current.json"), &Value::Object(cur));

    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    out.insert("version".into(), json!(version));
    out.insert("dir".into(), json!(dest.to_string_lossy()));
    out.insert("prev".into(), json!(prev_ver));
    out.insert("backup".into(), json!(backup));
    if ctx.frozen {
        out.insert("restart".into(), json!(false));
        out.insert(
            "note".into(),
            json!("这是安装版（frozen），换程序要走安装包；包已落盘，未重启"),
        );
        return Value::Object(out);
    }
    if restart {
        out.insert("restart".into(), restart_script(ctx, dest));
    } else {
        out.insert("restart".into(), json!(false));
    }
    Value::Object(out)
}

/// `POST /api/update/push` 的 body。
pub fn push(ctx: &Ctx, body: &Value) -> Result<Value, PushError> {
    let b = body.as_object().cloned().unwrap_or_default();
    let version = {
        let raw = crate::routes::w7b::util::py_or_str(b.get("version"), "");
        raw.trim().trim_start_matches(['v', 'V']).to_string()
    };
    if version.is_empty() {
        return Ok(json!({"ok": false, "error": "version 必填"}));
    }
    let want = crate::routes::w7b::util::py_or_str(b.get("sha256"), "")
        .trim()
        .to_lowercase();
    if want.len() != 64 {
        return Ok(json!({"ok": false, "error": "sha256 必填（64 位十六进制）"}));
    }
    let restart = b.get("restart").map(|v| crate::routes::w7b::util::py_truthy(Some(v))).unwrap_or(true);
    let name = {
        let raw = crate::routes::w7b::util::py_or_str(b.get("name"), "");
        if raw.is_empty() { format!("moshan-{version}.zip") } else { raw }
    };
    let total = {
        let v = b.get("total").cloned().unwrap_or(Value::Null);
        if !crate::routes::w7b::util::py_truthy(Some(&v)) {
            1
        } else {
            crate::routes::w7b::util::py_int_or(Some(&v), 1).map_err(PushError::Other)?
        }
    };
    let index = {
        let v = b.get("index").cloned().unwrap_or(Value::Null);
        if !crate::routes::w7b::util::py_truthy(Some(&v)) {
            0
        } else {
            crate::routes::w7b::util::py_int_or(Some(&v), 0).map_err(PushError::Other)?
        }
    };
    let d = ensure(ctx);

    let raw: Vec<u8>;
    if total > 1 {
        let parts_path = d.join("parts.json");
        let mut st = read_json(&parts_path);
        if st.get("version").and_then(|v| v.as_str()).unwrap_or("") != version {
            st = json!({"version": version, "total": total, "parts": {}});
        }
        let part = {
            let p = b.get("part").cloned().unwrap_or(Value::Null);
            if crate::routes::w7b::util::py_truthy(Some(&p)) {
                p
            } else {
                b.get("b64").cloned().unwrap_or(Value::Null)
            }
        };
        if !crate::routes::w7b::util::py_truthy(Some(&part)) {
            return Ok(json!({"ok": false, "error": "分片缺 part"}));
        }
        let idx = index.to_string();
        if !st.get("parts").map(|v| v.is_object()).unwrap_or(false) {
            st["parts"] = json!({});
        }
        st["parts"][idx.as_str()] = part;
        write_json(&parts_path, &st);
        let got = st["parts"].as_object().map(|m| m.len()).unwrap_or(0) as i64;
        if got < total {
            return Ok(json!({"ok": true, "partial": true, "got": got, "total": total}));
        }
        let mut buf: Vec<u8> = Vec::new();
        for i in 0..total {
            let key = i.to_string();
            let Some(v) = st["parts"].get(key.as_str()) else {
                return Ok(json!({"ok": false, "error": format!("分片解码失败：{key:?}")}));
            };
            let enc = crate::routes::w7b::util::py_str(v);
            match base64::engine::general_purpose::STANDARD.decode(enc.as_bytes()) {
                Ok(mut chunk) => buf.append(&mut chunk),
                Err(e) => {
                    return Ok(json!({"ok": false, "error": format!("分片解码失败：{e}")}));
                }
            }
        }
        raw = buf;
    } else {
        let b64 = b.get("b64").cloned().unwrap_or(Value::Null);
        if !crate::routes::w7b::util::py_truthy(Some(&b64)) {
            return Ok(json!({"ok": false, "error": "b64 必填"}));
        }
        let enc = crate::routes::w7b::util::py_str(&b64);
        match base64::engine::general_purpose::STANDARD.decode(enc.as_bytes()) {
            Ok(v) => raw = v,
            Err(e) => return Ok(json!({"ok": false, "error": format!("base64 解码失败：{e}")})),
        }
    }

    if raw.len() > MAX_PKG_BYTES {
        return Ok(json!({
            "ok": false,
            "error": format!("包体超过 {}MB 上限，拒收", MAX_PKG_BYTES / 1048576),
        }));
    }
    let got = {
        let mut h = Sha256::new();
        h.update(&raw);
        format!("{:x}", h.finalize())
    };
    if got != want {
        return Ok(json!({
            "ok": false, "error": "sha256 不匹配，整包丢弃",
            "want": want, "got": got, "bytes": raw.len(),
        }));
    }

    // 先把包落成临时文件，交给 unzip；路径不落在会被清空的 pkgs 下。
    let tmp_zip = d.join(format!("_push-{}.zip", crate::routes::w7b::util::rand_hex(8)));
    if let Err(e) = std::fs::write(&tmp_zip, &raw) {
        return Err(PushError::Other(format!("收包异常：{e}")));
    }

    let dest = if ctx.frozen {
        let dest = d.join("pkgs").join(&version);
        let _ = std::fs::remove_dir_all(&dest);
        let _ = std::fs::create_dir_all(&dest);
        dest
    } else {
        crate::routes::w7b::util::app_dir(ctx)
    };
    let mut backup = String::new();
    if !ctx.frozen {
        let cur = read_json(&d.join("current.json"));
        let curver = {
            let v = cur.get("version").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if v.is_empty() { app_version(&dest) } else { v }
        };
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let bak = d.join("pkgs").join(format!("{curver}-bak-{stamp}"));
        match copy_tree(&dest, &bak, true) {
            Ok(_) => backup = bak.to_string_lossy().to_string(),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_zip);
                return Ok(json!({
                    "ok": false,
                    "error": format!("备份失败，为免更新到一半退不回去，已中止：{e}"),
                }));
            }
        }
    }

    let info = match unpack(&tmp_zip, &dest) {
        Ok(i) => i,
        Err(PushError::TooBig(msg)) => {
            if ctx.frozen {
                let _ = std::fs::remove_dir_all(&dest);
            } else if !backup.is_empty() {
                let _ = copy_tree(Path::new(&backup), &dest, false);
            }
            let _ = std::fs::remove_file(&tmp_zip);
            return Err(PushError::TooBig(msg));
        }
        Err(PushError::Other(e)) => {
            let _ = std::fs::remove_file(&tmp_zip);
            return Ok(json!({"ok": false, "error": format!("解包失败：{e}")}));
        }
    };
    let _ = std::fs::remove_file(&tmp_zip);
    if !dest.join("server.py").exists() {
        if ctx.frozen {
            let _ = std::fs::remove_dir_all(&dest);
        }
        return Ok(json!({
            "ok": false, "error": "包里没有 server.py，拒收",
            "unpacked": {
                "files": info.files, "bytes": info.bytes,
                "skipped": info.skipped, "skipped_n": info.skipped_n,
            },
        }));
    }
    let parts_path = d.join("parts.json");
    if parts_path.exists() {
        let _ = std::fs::remove_file(&parts_path);
    }

    let mut out = commit(ctx, &version, &dest, &format!("push:{name}"), restart, &backup);
    out["unpacked"] = json!({
        "files": info.files, "bytes": info.bytes,
        "skipped": info.skipped, "skipped_n": info.skipped_n,
    });
    out["sha256"] = json!(got);
    out["mode"] = json!(if ctx.frozen { "frozen" } else { "in-place" });
    Ok(out)
}

/// `pushupd.status()`。
pub fn status(ctx: &Ctx) -> Value {
    let d = ensure(ctx);
    let cur = read_json(&d.join("current.json"));
    let mut pkgs: Vec<String> = Vec::new();
    if let Ok(it) = std::fs::read_dir(d.join("pkgs")) {
        for e in it.flatten() {
            if e.path().is_dir() {
                pkgs.push(e.file_name().to_string_lossy().to_string());
            }
        }
    }
    pkgs.sort();
    let packages: Vec<String> = if pkgs.len() > 10 {
        pkgs[pkgs.len() - 10..].to_vec()
    } else {
        pkgs
    };
    let s = |k: &str| -> String {
        cur.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
    };
    json!({
        "ok": true,
        "version": s("version"),
        "dir": s("dir"),
        "at": s("at"),
        "source": s("source"),
        "backup": s("backup"),
        "packages": packages,
        "running_from": crate::routes::w7b::util::app_dir(ctx).to_string_lossy(),
        "frozen": ctx.frozen,
    })
}

/// `pushupd.rollback()`。
pub fn rollback(ctx: &Ctx, restart: bool) -> Value {
    let d = ensure(ctx);
    let cur = read_json(&d.join("current.json"));
    let bak = cur.get("backup").and_then(|v| v.as_str()).unwrap_or("").to_string();
    if !bak.is_empty() && Path::new(&bak).is_dir() {
        let dest = crate::routes::w7b::util::app_dir(ctx);
        if let Err(e) = copy_tree(Path::new(&bak), &dest, false) {
            return json!({"ok": false, "error": format!("回滚复制失败：{e}")});
        }
        let ver = app_version(&dest);
        return commit(ctx, &ver, &dest, "rollback", restart, "");
    }
    let prev = cur.get("prev").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let dest = if prev.is_empty() { None } else { Some(d.join("pkgs").join(&prev)) };
    match dest {
        Some(p) if p.is_dir() => commit(ctx, &prev, &p, "rollback", restart, ""),
        _ => json!({"ok": false, "error": "没有可回滚的备份", "prev": prev}),
    }
}
