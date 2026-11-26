// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`storage.make_zip()` 的字节级复刻（zipfile.ZIP_DEFLATED / allowZip64）。
// 不做：不支持 >4GB 的 zip64 扩展（夹具与真实下载都远小于这个量级）。
//
// 为什么自己写：Cargo.toml 里没有 zip/flate2，而判定标准是「与 Python 逐字节一致」。
// macOS 自带 libz，Python 的 zlib 也是它（1.2.12），所以直接用 FFI 调 deflateInit2
// 就能拿到同一段 deflate 流 —— 自己实现压缩算法反而更难对齐。

#![allow(dead_code)]

use std::ffi::CString;
use std::path::{Path, PathBuf};

#[repr(C)]
struct ZStream {
    next_in: *const u8,
    avail_in: u32,
    total_in: u64,
    next_out: *mut u8,
    avail_out: u32,
    total_out: u64,
    msg: *const std::os::raw::c_char,
    state: *mut std::os::raw::c_void,
    zalloc: *const std::os::raw::c_void,
    zfree: *const std::os::raw::c_void,
    opaque: *mut std::os::raw::c_void,
    data_type: i32,
    adler: u64,
    reserved: u64,
}

#[link(name = "z")]
extern "C" {
    fn deflateInit2_(
        strm: *mut ZStream,
        level: i32,
        method: i32,
        window_bits: i32,
        mem_level: i32,
        strategy: i32,
        version: *const std::os::raw::c_char,
        stream_size: i32,
    ) -> i32;
    fn deflate(strm: *mut ZStream, flush: i32) -> i32;
    fn deflateEnd(strm: *mut ZStream) -> i32;
}

const Z_OK: i32 = 0;
const Z_STREAM_END: i32 = 1;
const Z_BUF_ERROR: i32 = -5;
const Z_FINISH: i32 = 4;

/// raw deflate（windowBits=-15），level=6/默认策略 —— 与
/// `zlib.compressobj(None, zlib.DEFLATED, -15)` 完全同参数。
fn deflate_raw(input: &[u8]) -> Vec<u8> {
    let version = CString::new("1.2.12").unwrap();
    let mut z: ZStream = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        deflateInit2_(
            &mut z,
            6,
            8,
            -15,
            8,
            0,
            version.as_ptr(),
            std::mem::size_of::<ZStream>() as i32,
        )
    };
    if rc != Z_OK {
        return Vec::new();
    }
    z.next_in = input.as_ptr();
    z.avail_in = input.len() as u32;
    let mut out: Vec<u8> = Vec::with_capacity(input.len() / 2 + 64);
    let mut buf = vec![0u8; 65536];
    loop {
        z.next_out = buf.as_mut_ptr();
        z.avail_out = buf.len() as u32;
        let r = unsafe { deflate(&mut z, Z_FINISH) };
        let produced = buf.len() - z.avail_out as usize;
        out.extend_from_slice(&buf[..produced]);
        if r == Z_STREAM_END {
            break;
        }
        if r != Z_OK && r != Z_BUF_ERROR {
            unsafe { deflateEnd(&mut z) };
            return Vec::new();
        }
        if produced == 0 && r == Z_BUF_ERROR {
            break;
        }
    }
    unsafe { deflateEnd(&mut z) };
    out
}

fn crc32(data: &[u8]) -> u32 {
    static mut TABLE: [u32; 256] = [0; 256];
    static INIT: std::sync::Once = std::sync::Once::new();
    unsafe {
        INIT.call_once(|| {
            for i in 0..256u32 {
                let mut c = i;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xedb88320 ^ (c >> 1) } else { c >> 1 };
                }
                TABLE[i as usize] = c;
            }
        });
        let mut c: u32 = 0xffffffff;
        for &b in data {
            c = TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
        }
        c ^ 0xffffffff
    }
}

struct Entry {
    name: String,
    crc: u32,
    csize: u32,
    usize: u32,
    offset: u32,
    dostime: u16,
    dosdate: u16,
    extattr: u32,
    flags: u16,
}

fn dos_datetime(md: &std::fs::Metadata) -> (u16, u16) {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let secs = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let dt = match Local.timestamp_opt(secs, 0).single() {
        Some(d) => d,
        None => return (0, 0x21),
    };
    let mut year = dt.year();
    if year < 1980 {
        year = 1980;
    }
    if year > 2107 {
        year = 2107;
    }
    let date = (((year - 1980) as u16) << 9) | ((dt.month() as u16) << 5) | (dt.day() as u16);
    let time = ((dt.hour() as u16) << 11) | ((dt.minute() as u16) << 5) | ((dt.second() as u16) / 2);
    (time, date)
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// 把一组文件/目录打成一个 zip，返回 zip 文件路径。
/// `tmp_dir` 由调用方建好，清理也由调用方负责（Python 用 BackgroundTask）。
pub fn make_zip(targets: &[PathBuf], tmp_dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(tmp_dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let rnd = short_hex(4);
    let out = tmp_dir.join(format!("{name}-{stamp}-{rnd}.zip"));

    let mut body: Vec<u8> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();

    for p in targets {
        if p.is_dir() {
            let base = p.parent().map(|x| x.to_path_buf()).unwrap_or_else(|| PathBuf::from("/"));
            walk_zip(p, &base, &mut body, &mut entries);
        } else if p.is_file() {
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            add_file(p, &name, &mut body, &mut entries);
        }
    }

    let cd_start = body.len() as u32;
    for e in &entries {
        push_u32(&mut body, 0x02014b50);
        push_u16(&mut body, 0x0314); // version made by: unix, 2.0
        push_u16(&mut body, 20);
        push_u16(&mut body, e.flags);
        push_u16(&mut body, 8); // deflate
        push_u16(&mut body, e.dostime);
        push_u16(&mut body, e.dosdate);
        push_u32(&mut body, e.crc);
        push_u32(&mut body, e.csize);
        push_u32(&mut body, e.usize);
        push_u16(&mut body, e.name.as_bytes().len() as u16);
        push_u16(&mut body, 0); // extra len
        push_u16(&mut body, 0); // comment len
        push_u16(&mut body, 0); // disk start
        push_u16(&mut body, 0); // internal attr
        push_u32(&mut body, e.extattr);
        push_u32(&mut body, e.offset);
        body.extend_from_slice(e.name.as_bytes());
    }
    let cd_size = body.len() as u32 - cd_start;

    push_u32(&mut body, 0x06054b50);
    push_u16(&mut body, 0);
    push_u16(&mut body, 0);
    push_u16(&mut body, entries.len() as u16);
    push_u16(&mut body, entries.len() as u16);
    push_u32(&mut body, cd_size);
    push_u32(&mut body, cd_start);
    push_u16(&mut body, 0);

    std::fs::write(&out, &body)?;
    Ok(out)
}

fn add_file(p: &Path, arcname: &str, body: &mut Vec<u8>, entries: &mut Vec<Entry>) {
    let Ok(raw) = std::fs::read(p) else { return };
    let md = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(_) => return,
    };
    let comp = deflate_raw(&raw);
    let (dostime, dosdate) = dos_datetime(&md);
    let flags: u16 = if arcname.is_ascii() { 0 } else { 0x0800 };
    let name_bytes = arcname.as_bytes();
    let offset = body.len() as u32;

    push_u32(body, 0x04034b50);
    push_u16(body, 20); // version needed
    push_u16(body, flags);
    push_u16(body, 8); // deflate
    push_u16(body, dostime);
    push_u16(body, dosdate);
    push_u32(body, crc32(&raw));
    push_u32(body, comp.len() as u32);
    push_u32(body, raw.len() as u32);
    push_u16(body, name_bytes.len() as u16);
    push_u16(body, 0);
    body.extend_from_slice(name_bytes);
    body.extend_from_slice(&comp);

    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        md.mode() & 0xFFFF
    };
    #[cfg(not(unix))]
    let mode: u32 = 0o100644;

    entries.push(Entry {
        name: arcname.to_string(),
        crc: crc32(&raw),
        csize: comp.len() as u32,
        usize: raw.len() as u32,
        offset,
        dostime,
        dosdate,
        extattr: mode << 16,
        flags,
    });
}

/// `os.walk` 同序：目录自身先产出（这里只写文件），子目录按 readdir 原序深度优先。
fn walk_zip(dir: &Path, base: &Path, body: &mut Vec<u8>, entries: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let p = PathBuf::from(e.path());
        let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        let md = match std::fs::metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_dir() {
            // os.walk(followlinks=False)：符号链接目录列出来但不进去。
            if !is_link {
                subdirs.push(p);
            }
            continue;
        }
        if md.is_file() {
            let rel = p
                .strip_prefix(base)
                .map(|x| x.to_string_lossy().to_string())
                .unwrap_or_else(|_| name.clone());
            add_file(&p, &rel, body, entries);
        }
    }
    for d in subdirs {
        walk_zip(&d, base, body, entries);
    }
}

fn short_hex(n: usize) -> String {
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
