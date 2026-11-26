// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/naming.py` 的移植 —— 读图上的字（Vision）→ 本机 4B 概括 → 改目录名。
// 不做：不联网抓网页、不碰 HTTP 契约。
//
// 这里的「不能硬凑」是产品口径：图里没字就不改名。名字规则（clean_name）必须
// 与 Python 逐字一致，因为对拍时 diff 的是**两边的目录列表**。

#![allow(dead_code)]

use super::py;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const IMG_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "heic", "heif", "webp", "gif", "tif", "tiff", "bmp",
];
pub const MAX_IMAGES: usize = 8;
pub const MAX_CHARS: usize = 1600;
pub const OLLAMA_ADDR: &str = "127.0.0.1:11434";
pub const OLLAMA_PATH: &str = "/api/generate";
pub const DEFAULT_MODEL: &str = "qwen3.5-4b-gguf:q4km";

const PROMPT: &str = "下面是手机上传的一组图片上的文字（OCR 结果，可能有错别字和噪声）。请概括这一组的共同主题，起一个中文名字。

要求：
- 2 到 10 个字
- 不要标点、引号、书名号、序号、表情
- 说清楚「这是什么」，不要出现「截图」「图片」「一组」「文件」这类词
- 只输出名字本身，不要解释，不要换行

文字内容：
{text}

名字：";

pub fn ocr_bin() -> Option<PathBuf> {
    let bin = py::home().join("Documents/cuntu/bin/pocket_ocr");
    if bin.is_file() {
        return Some(bin);
    }
    let src = py::home().join("Documents/cuntu/bin/ocr.swift");
    if !src.is_file() {
        return None;
    }
    let argv = vec![
        "swiftc".to_string(),
        "-O".to_string(),
        "-o".to_string(),
        bin.to_string_lossy().to_string(),
        src.to_string_lossy().to_string(),
    ];
    let _ = py::run_cmd(&argv, 240, None);
    if bin.is_file() { Some(bin) } else { None }
}

/// 一次调用读完所有图（模型只加载一次）。
pub fn ocr_images(paths: &[PathBuf], timeout: u64) -> Vec<(String, String)> {
    let Some(exe) = ocr_bin() else { return Vec::new() };
    if paths.is_empty() {
        return Vec::new();
    }
    let mut argv = vec![exe.to_string_lossy().to_string()];
    argv.extend(paths.iter().map(|p| p.to_string_lossy().to_string()));
    let Some((_code, out, _err)) = py::run_cmd(&argv, timeout, None) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for line in out.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((p, t)) = line.split_once('\t') {
            if !p.is_empty() {
                rows.push((p.to_string(), t.trim().to_string()));
            }
        } else if !line.is_empty() {
            // 没有制表符时 Python 得到 path=line, text=""（dict[path]=""）
            // 但这只会在工具输出异常时发生；保持一致地记一个空文本。
            rows.push((line.to_string(), String::new()));
        }
    }
    rows
}

/// `group_images`：只取图片，按体积降序（大图信息多），最多 8 张。
pub fn group_images(d: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(d) else { return Vec::new() };
    let mut files: Vec<(u64, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !IMG_EXT.contains(&ext.as_str()) {
            continue;
        }
        let sz = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        files.push((sz, p));
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    files.into_iter().take(MAX_IMAGES).map(|(_, p)| p).collect()
}

/// 返回（拼好的文字, 读了几张, 有多少字）。
pub fn collect_text(d: &Path) -> (String, usize, usize) {
    let files = group_images(d);
    if files.is_empty() {
        return (String::new(), 0, 0);
    }
    let got = ocr_images(&files, 90);
    let chunks: Vec<String> = got
        .into_iter()
        .map(|(_, t)| t)
        .filter(|t| t.chars().count() >= 6)
        .collect();
    let mut text = chunks.join("\n");
    if text.chars().count() > MAX_CHARS {
        text = format!("{}…", text.chars().take(MAX_CHARS).collect::<String>());
    }
    let n = text.chars().count();
    (text, files.len(), n)
}

/// 本机 ollama 的极简 HTTP/1.1 POST。
///
/// 不走代理是刻意的（Python 那边也是 `ProxyHandler({})`）：本机地址被
/// `HTTP_PROXY` 劫走时，报出来的错会指向代理而不是「4B 没起」。
fn ollama_post(body: &[u8], timeout: u64) -> Result<Vec<u8>, String> {
    let addrs: Vec<std::net::SocketAddr> = vec![];
    let _ = addrs;
    let mut stream = TcpStream::connect(OLLAMA_ADDR).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(timeout)))
        .ok();
    stream
        .set_write_timeout(Some(Duration::from_secs(timeout)))
        .ok();
    let head = format!(
        "POST {OLLAMA_PATH} HTTP/1.1\r\nHost: {OLLAMA_ADDR}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(body).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let start = Instant::now();
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if start.elapsed() > Duration::from_secs(timeout) {
                    return Err("timed out".into());
                }
            }
            Err(e) => {
                if buf.is_empty() {
                    return Err(e.to_string());
                }
                break;
            }
        }
    }
    // 去 chunked 编码并切头
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some((h, b)) => (h.to_string(), b.to_string()),
        None => return Err("bad response".into()),
    };
    let chunked = head.to_lowercase().contains("transfer-encoding: chunked");
    let payload = if chunked { dechunk(&body) } else { body };
    Ok(payload.into_bytes())
}

fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    loop {
        let Some((len_line, tail)) = rest.split_once("\r\n") else { break };
        let n = usize::from_str_radix(len_line.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        if tail.len() < n {
            out.push_str(tail);
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].strip_prefix("\r\n").unwrap_or(&tail[n..]);
    }
    out
}

pub fn ask_4b(prompt: &str, model: &str, timeout: u64) -> Result<String, String> {
    let body = json!({
        "model": model,
        "prompt": prompt,
        "stream": false,
        "think": false,
        "options": {"temperature": 0.2, "num_predict": 32, "top_p": 0.9},
    });
    let raw = ollama_post(serde_json::to_string(&body).unwrap_or_default().as_bytes(), timeout)?;
    let data: Value = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    Ok(py::s_of(data.get("response")).trim().to_string())
}

/// `clean_name`：把模型输出收拾成能当目录名的短句。
pub fn clean_name(raw: &str) -> String {
    let mut line = String::new();
    for ln in raw.split('\n') {
        let t = ln.trim();
        if !t.is_empty() {
            line = t.to_string();
            break;
        }
    }
    line = strip_wrap(&line);
    if line.contains('：') || line.contains(':') {
        let i1 = line.find('：');
        let i2 = line.find(':');
        let cut = match (i1, i2) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => line.len(),
        };
        line = line[..cut].to_string();
    }
    let re_bad = regex::Regex::new(r#"[\\/:*?"<>|\n\r\t]+"#).unwrap();
    line = re_bad.replace_all(&line, " ").into_owned();
    let re_sp = regex::Regex::new(r"\s+").unwrap();
    line = re_sp.replace_all(&line, " ").into_owned();
    line = line
        .trim_matches(|c: char| " .。、,，;；".contains(c))
        .to_string();
    for pre in ["名字是", "名字", "答案", "标题"] {
        if let Some(rest) = line.strip_prefix(pre) {
            line = rest.trim_start_matches(['：', ':', ' ', '是']).to_string();
        }
    }
    if line.chars().count() > 12 {
        let cut: String = line.chars().take(12).collect();
        line = cut.trim_end_matches([' ', '的', '之', '与', '和']).to_string();
    }
    line
}

fn strip_wrap(s: &str) -> String {
    let set = " \t\r\n\u{0b}\u{0c}「」『』“”\"'‘’《》【】[]()（）";
    s.trim_matches(|c: char| set.contains(c)).to_string()
}

/// `naming._uniq`：`名` → `名 (2)` … `名 (39)` → `名 (秒)`。
pub fn uniq(directory: &Path, name: &str) -> PathBuf {
    let target = directory.join(name);
    if !target.exists() {
        return target;
    }
    for i in 2..40 {
        let cand = directory.join(format!("{name} ({i})"));
        if !cand.exists() {
            return cand;
        }
    }
    directory.join(format!("{name} ({})", py::epoch_secs()))
}

/// `naming.name_folder`：读图 → 起名 → （可选）改目录名。永不抛异常。
pub fn name_folder(d: &Path, model: Option<&str>, force: bool) -> Value {
    let t0 = Instant::now();
    let mut res = json!({
        "ok": false,
        "renamed": false,
        "name": d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "path": d.to_string_lossy(),
        "images": 0,
        "chars": 0,
        "ms": 0,
        "reason": "",
    });
    let ms = |t0: Instant| -> i64 { (t0.elapsed().as_secs_f64() * 1000.0) as i64 };

    if !d.is_dir() {
        res["reason"] = json!("目录不在了");
        return res;
    }
    let base = d.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    if !force && !base.starts_with("组-") {
        res["reason"] = json!("这组已经有名字了");
        return res;
    }
    let (text, n_img, n_chars) = collect_text(d);
    res["images"] = json!(n_img);
    res["chars"] = json!(n_chars);
    if n_img == 0 {
        res["reason"] = json!("这组里没有图片");
        res["ms"] = json!(ms(t0));
        return res;
    }
    if n_chars < 12 {
        res["reason"] = json!("图里没读到字（照片或视频截图），留着原名");
        res["ms"] = json!(ms(t0));
        return res;
    }
    let prompt = PROMPT.replace("{text}", &text);
    let raw = match ask_4b(&prompt, model.unwrap_or(DEFAULT_MODEL), 90) {
        Ok(r) => r,
        Err(e) => {
            res["reason"] = json!(format!("4B 没应答：{}", py::trunc(&e, 60)));
            res["ms"] = json!(ms(t0));
            return res;
        }
    };
    let name = clean_name(&raw);
    if name.chars().count() < 2 {
        res["reason"] = json!(format!("4B 没给出像样的名字（原文：{}）", py::trunc(&raw, 24)));
        res["ms"] = json!(ms(t0));
        return res;
    }
    res["name"] = json!(name);
    res["ok"] = json!(true);
    let target = uniq(d.parent().unwrap_or_else(|| Path::new(".")), &name);
    match std::fs::rename(d, &target) {
        Ok(_) => {
            res["renamed"] = json!(true);
            res["path"] = json!(target.to_string_lossy());
        }
        Err(e) => {
            res["reason"] = json!(format!("改名失败：{}", py::trunc(&e.to_string(), 60)));
        }
    }
    res["ms"] = json!(ms(t0));
    res
}
