// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：机器指标（hostwork._disk / _extra_disks / _net / _power / _gpu / _mem /
//       _uptime_h）与机器历史（snapshots / daily_history）。
// 不做：不采样、不落盘（那是 metrics.py 的调度）；这里只读。
//
// plist 不引第三方库：`ioreg -a` / `diskutil info -plist` 的输出交给系统自带的
// `plutil -convert json` 转一遍再解析 —— 与 Python plistlib 读到的键值同源。

use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::platform;
use super::util::{fnum, py_float, round1, round2};

const GB: f64 = 1073741824.0;

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

fn sys_dir(library: &Path) -> PathBuf {
    library.join("10-系统记录")
}

/// `_disk()`：数据根在 10-系统记录，不在就看 HOME。
pub fn disk(library: &Path) -> Value {
    let sys = sys_dir(library);
    let target = if sys.exists() { sys } else { home() };
    let Some((total, free)) = platform::disk_usage(&target) else {
        return json!({"disk_pct": 0.0, "free_gb": 0.0, "total_gb": 0.0, "used_gb": 0.0});
    };
    let used = total as f64 - free as f64;
    let mut m = Map::new();
    m.insert(
        "disk_pct".into(),
        fnum(round1(if total != 0 { used * 100.0 / total as f64 } else { 0.0 })),
    );
    m.insert("free_gb".into(), fnum(round1(free as f64 / GB)));
    m.insert("total_gb".into(), fnum(round1(total as f64 / GB)));
    m.insert("used_gb".into(), fnum(round1(used / GB)));
    Value::Object(m)
}

static EXTRA_DISK: Mutex<Option<(f64, Vec<Value>)>> = Mutex::new(None);

fn plist_json(stdout: &str) -> Option<Value> {
    serde_json::from_str(stdout.trim()).ok()
}

/// `_extra_disks(force)`：/Volumes 下 Ejectable 的外挂盘，缓存 8 秒。
pub fn extra_disks(force: bool) -> Vec<Value> {
    let now = now_secs();
    {
        let g = EXTRA_DISK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, list)) = g.as_ref() {
            if !force && now - *at < 8.0 {
                return list.clone();
            }
        }
    }
    let root = Path::new("/Volumes");
    let mut mounts: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() && !name.starts_with('.') {
                mounts.push(p);
            }
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for p in mounts {
        let raw = platform::run_cmd(
            &["/usr/sbin/diskutil", "info", "-plist", &p.to_string_lossy()],
            Duration::from_secs(3),
        );
        let info = match plist_to_json(&raw) {
            Some(Value::Object(m)) => m,
            Some(Value::Array(a)) => match a.into_iter().next() {
                Some(Value::Object(m)) => m,
                _ => continue,
            },
            _ => continue,
        };
        if !info.get("Ejectable").map(super::util::truthy).unwrap_or(false) {
            continue;
        }
        let Some((total, free)) = platform::disk_usage(&p) else {
            continue;
        };
        if total == 0 {
            continue;
        }
        let used = total as f64 - free as f64;
        let name = {
            let v = super::util::str_or(info.get("VolumeName"));
            if v.is_empty() { p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default() } else { v }
        };
        let mount = {
            let v = super::util::str_or(info.get("MountPoint"));
            if v.is_empty() { p.to_string_lossy().to_string() } else { v }
        };
        let mut m = Map::new();
        m.insert("name".into(), Value::String(name));
        m.insert("mount".into(), Value::String(mount));
        m.insert("device".into(), Value::String(super::util::str_or(info.get("DeviceNode"))));
        m.insert("disk_pct".into(), fnum(round1(used * 100.0 / total as f64)));
        m.insert("total_gb".into(), fnum(round1(total as f64 / GB)));
        m.insert("free_gb".into(), fnum(round1(free as f64 / GB)));
        m.insert("used_gb".into(), fnum(round1(used / GB)));
        out.push(Value::Object(m));
    }
    out.sort_by(|a, b| {
        let pa = py_float(a.get("disk_pct").unwrap_or(&Value::Null)).unwrap_or(0.0);
        let pb = py_float(b.get("disk_pct").unwrap_or(&Value::Null)).unwrap_or(0.0);
        pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut g = EXTRA_DISK.lock().unwrap_or_else(|e| e.into_inner());
    *g = Some((now, out.clone()));
    out
}

/// 把 plist 文本转成 JSON（系统 plutil，不引 plist 依赖）。
fn plist_to_json(text: &str) -> Option<Value> {
    plist_to_json_bytes(text.as_bytes())
}

fn plist_to_json_bytes(raw: &[u8]) -> Option<Value> {
    if raw.is_empty() {
        return None;
    }
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut ch = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut si) = ch.stdin.take() {
        let _ = si.write_all(raw);
    }
    let out = ch.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

static NET_LAST: Mutex<(f64, f64, f64)> = Mutex::new((0.0, 0.0, 0.0));

/// `_net()`：netstat -ib 累计字节做差分（B/s）。
pub fn net() -> Value {
    let now = now_secs();
    let out = platform::run_cmd(&["/usr/sbin/netstat", "-ib"], Duration::from_secs(3));
    if out.is_empty() {
        return json!({"in_bps": 0.0, "out_bps": 0.0, "in_total": 0.0, "out_total": 0.0});
    }
    let mut bin = 0.0f64;
    let mut bout = 0.0f64;
    for line in out.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || !f[2].contains("<Link#") || f[0].starts_with("lo") {
            continue;
        }
        if let (Ok(a), Ok(b)) = (f[6].parse::<f64>(), f[9].parse::<f64>()) {
            bin += a;
            bout += b;
        }
    }
    let mut g = NET_LAST.lock().unwrap_or_else(|e| e.into_inner());
    let (at, lin, lout) = *g;
    let dt = now - at;
    let (r_in, r_out) = if dt >= 1.0 && at != 0.0 {
        (((bin - lin) / dt).max(0.0), ((bout - lout) / dt).max(0.0))
    } else {
        (0.0, 0.0)
    };
    if dt >= 1.0 {
        *g = (now, bin, bout);
    }
    json!({
        "in_bps": fnum(round1(r_in)),
        "out_bps": fnum(round1(r_out)),
        "in_total": fnum(round1(bin)),
        "out_total": fnum(round1(bout)),
    })
}

/// `_power()`：AppleSmartBattery.PowerTelemetryData（毫瓦）。
pub fn power() -> Value {
    let mut out = json!({"watts": 0.0, "battery_pct": null, "charging": null, "has_battery": false});
    let raw = platform::run_cmd(
        &["/usr/sbin/ioreg", "-a", "-r", "-c", "AppleSmartBattery", "-d", "1"],
        Duration::from_secs(3),
    );
    let Some(v) = plist_to_json(&raw) else { return out };
    let o = match v {
        Value::Array(a) => a.into_iter().next().unwrap_or(json!({})),
        other => other,
    };
    let tel = o.get("PowerTelemetryData").cloned().unwrap_or(json!({}));
    let mw = tel
        .get("SystemPowerIn")
        .or_else(|| tel.get("SystemLoad"))
        .cloned()
        .unwrap_or(json!(0));
    let watts = round1(py_float(&mw).unwrap_or(0.0) / 1000.0);
    out["watts"] = fnum(watts);
    if o.get("BatteryInstalled").map(super::util::truthy).unwrap_or(false) {
        let mx = py_float(o.get("MaxCapacity").unwrap_or(&Value::Null)).unwrap_or(0.0);
        let cur = py_float(o.get("CurrentCapacity").unwrap_or(&Value::Null)).unwrap_or(0.0);
        out["has_battery"] = json!(true);
        out["battery_pct"] = if mx != 0.0 { fnum(round1(cur * 100.0 / mx)) } else { Value::Null };
        out["charging"] = json!(o.get("IsCharging").map(super::util::truthy).unwrap_or(false));
    }
    out
}

/// `_gpu()`：IOGPU.PerformanceStatistics（统一内存）。
pub fn gpu() -> Value {
    let mut out = json!({"used_gb": 0.0, "alloc_gb": 0.0, "util_pct": 0.0});
    // ioreg -a 吐的是**二进制 plist**，必须按字节接；当字符串收会被 UTF-8
    // 有损替换掉，解析出来永远是空表（GPU 三个数一直是 0）。
    let raw = platform::run_cmd_raw(
        &["/usr/sbin/ioreg", "-a", "-r", "-c", "IOGPU", "-d", "1"],
        Duration::from_secs(3),
    );
    // ioreg 的 plist 里有 `<data>` 节点，`plutil -convert json` 直接拒收
    // （Invalid object in plist for JSON format），所以只从 XML 里抠这三个数。
    let text = String::from_utf8_lossy(&raw);
    let grab = |key: &str| -> f64 {
        let pat = format!(
            r"(?s)<key>{}</key>\s*<(?:integer|real)>([^<]+)</(?:integer|real)>",
            regex::escape(key)
        );
        regex::Regex::new(&pat)
            .ok()
            .and_then(|r| r.captures(&text))
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().trim().parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let u = grab("In use system memory");
    let a = grab("Alloc system memory");
    let util = grab("Device Utilization %");
    out["used_gb"] = fnum(super::util::round_n(u / GB, 2));
    out["alloc_gb"] = fnum(super::util::round_n(a / GB, 2));
    out["util_pct"] = fnum(util);
    out
}

/// `_mem()`：active + wired + compressor（口径同 metrics.py / syscheck.py）。
pub fn mem() -> Value {
    let out = platform::run_cmd(&["/usr/bin/vm_stat"], Duration::from_secs(4));
    let mut page = 4096i64;
    if let Some(c) = regex::Regex::new(r"page size of (\d+) bytes").ok().and_then(|r| r.captures(&out)) {
        page = c.get(1).unwrap().as_str().parse().unwrap_or(4096);
    }
    let mut pages: HashMap<String, i64> = HashMap::new();
    let re_line = regex::Regex::new(r"(.+?):\s+(\d+)\.").unwrap();
    for line in out.lines() {
        if let Some(c) = re_line.captures(line) {
            pages.insert(c.get(1).unwrap().as_str().trim().to_string(), c.get(2).unwrap().as_str().parse().unwrap_or(0));
        }
    }
    let used = (pages.get("Pages active").copied().unwrap_or(0)
        + pages.get("Pages wired down").copied().unwrap_or(0)
        + pages.get("Pages occupied by compressor").copied().unwrap_or(0)) as f64
        * page as f64;
    let mt = platform::run_cmd(&["/usr/sbin/sysctl", "-n", "hw.memsize"], Duration::from_secs(4));
    let total: i64 = mt.trim().parse().unwrap_or(0);
    json!({
        "mem_pct": fnum(round1(if total != 0 { used * 100.0 / total as f64 } else { 0.0 })),
        "mem_gb": fnum(round1(used / GB)),
        "mem_total_gb": fnum(round1(if total != 0 { total as f64 / GB } else { 0.0 })),
    })
}

/// `_uptime_h()`
pub fn uptime_h() -> f64 {
    let bt = platform::run_cmd(&["/usr/sbin/sysctl", "-n", "kern.boottime"], Duration::from_secs(4));
    let re_bt = regex::Regex::new(r"sec = (\d+)").unwrap();
    match re_bt.captures(&bt) {
        Some(c) => {
            let sec: f64 = c.get(1).unwrap().as_str().parse().unwrap_or(0.0);
            round1((now_secs() - sec).max(0.0) / 3600.0)
        }
        None => 0.0,
    }
}

/// `_load()`：os.getloadavg()。
pub fn load() -> Vec<Value> {
    match platform::load_avg() {
        Some(v) => v.iter().map(|x| fnum(round2(*x))).collect(),
        None => vec![fnum(0.0), fnum(0.0), fnum(0.0)],
    }
}

/// `snapshots(days)`
pub fn snapshots(library: &Path, days: i64) -> Value {
    let path = sys_dir(library).join("机器").join("快照.jsonl");
    if !path.exists() {
        return json!({"disk": [], "mem": [], "load": [], "cpu": []});
    }
    let since = (chrono::Local::now() - chrono::Duration::days(days))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let mut rows: Vec<Value> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for ln in text.lines() {
            if !ln.contains("\"at\"") {
                continue;
            }
            if let Ok(d) = serde_json::from_str::<Value>(ln) {
                let at = super::util::str_or(d.get("at"));
                if at.as_str() >= since.as_str() {
                    rows.push(d);
                }
            }
        }
    }
    // ceil(len/160)
    let step = std::cmp::max(1, ((rows.len() as i64 + 159) / 160) as usize);
    let mut picked: Vec<Value> = rows.iter().step_by(step).cloned().collect();
    if !rows.is_empty() {
        if let Some(last) = rows.last() {
            let same = picked
                .last()
                .map(|p| p.get("at") == last.get("at") && p.get("disk_pct") == last.get("disk_pct"))
                .unwrap_or(false);
            if !same {
                picked.push(last.clone());
            }
        }
    }
    let disk: Vec<Value> = picked
        .iter()
        .map(|r| json!([at_slice(r), r.get("disk_pct").cloned().unwrap_or(Value::Null)]))
        .collect();
    let mem: Vec<Value> = picked
        .iter()
        .map(|r| json!([at_slice(r), r.get("mem_pct").cloned().unwrap_or(Value::Null)]))
        .collect();
    let load: Vec<Value> = picked
        .iter()
        .map(|r| json!([at_slice(r), r.get("load1").cloned().unwrap_or(Value::Null)]))
        .collect();
    let cpus = rows.last().and_then(|r| r.get("cpus").cloned()).unwrap_or(Value::Null);
    json!({"disk": disk, "mem": mem, "load": load, "cpus": cpus})
}

fn at_slice(r: &Value) -> Value {
    let at = super::util::str_or(r.get("at"));
    let chars: Vec<char> = at.chars().collect();
    if chars.len() >= 16 {
        Value::String(chars[5..16].iter().collect())
    } else {
        Value::String(at)
    }
}

/// `daily_history(limit)`
pub fn daily_history(library: &Path, limit: usize) -> Vec<Value> {
    let path = sys_dir(library)
        .join("使用报告")
        .join("_数据")
        .join("日聚合.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    let mut out: Vec<Value> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for ln in text.lines() {
            if !ln.starts_with("{\"date\"") {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(ln) {
                out.push(v);
            }
        }
    }
    if out.len() > limit {
        out.split_off(out.len() - limit)
    } else {
        out
    }
}
