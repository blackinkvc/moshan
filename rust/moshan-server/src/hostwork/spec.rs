// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：这台是什么机器（specs）/ 装了哪些软件（software）/ 装了哪些常驻服务（units）。
// 不做：不在每次请求里去 fork system_profiler / brew —— 三段各有缓存（900 / 300 / 60 秒）。
#![allow(dead_code)]

use super::platform;
use super::util::{fnum, load_json, round_n};
use crate::paths::Ctx;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// `SYS_DIR if exists else HOME`
fn disk_target(ctx: &Ctx) -> PathBuf {
    let sys = ctx.runtime_dir.join("10-系统记录");
    if sys.exists() {
        sys
    } else {
        home()
    }
}

fn sysctl(key: &str) -> String {
    platform::run_cmd(&["/usr/sbin/sysctl", "-n", key], Duration::from_secs(8))
        .trim()
        .to_string()
}

/// `int(re.sub(r"\D", "", sysctl(k)) or 0)`
fn sysctl_num(key: &str) -> i64 {
    let s = sysctl(key);
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(0)
}

/// 跑一条命令并完整读走 stdout（`Command::output` 会并发排空管道，不会卡死）。
fn run_out(argv: &[&str]) -> String {
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

fn run_out_env(argv: &[&str], extra: &[(&str, &str)]) -> String {
    let mut cmd = std::process::Command::new(argv[0]);
    cmd.args(&argv[1..]);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ 芯片表

const SOC: &[(&str, f64, i64, i64)] = &[
    ("m4 max", 546.0, 3, 16),
    ("m4 pro", 273.0, 3, 16),
    ("m4", 120.0, 3, 16),
    ("m3 max", 400.0, 3, 16),
    ("m3 pro", 150.0, 3, 16),
    ("m3", 100.0, 3, 16),
    ("m2 ultra", 800.0, 5, 32),
    ("m2 max", 400.0, 5, 16),
    ("m2 pro", 200.0, 5, 16),
    ("m2", 100.0, 5, 16),
    ("m1 ultra", 800.0, 5, 32),
    ("m1 max", 400.0, 5, 16),
    ("m1 pro", 200.0, 5, 16),
    ("m1", 68.0, 5, 16),
];

fn soc_of(chip: &str) -> Vec<(&'static str, Value)> {
    let k = chip.to_lowercase();
    for (name, bw, nm, npu) in SOC {
        if k.contains(name) {
            return vec![
                ("bw_gbs", json!(*bw as i64)),
                ("nm", json!(*nm)),
                ("npu_cores", json!(*npu)),
            ];
        }
    }
    Vec::new()
}

// ------------------------------------------------------------------ system_profiler

fn profiler() -> Map<String, Value> {
    let raw = run_out(&[
        "/usr/sbin/system_profiler",
        "SPHardwareDataType",
        "SPDisplaysDataType",
        "SPNVMeDataType",
    ]);
    let mut out = Map::new();
    for (key, pat) in [
        ("model_name", r"Model Name:\s*(.+)"),
        ("model_number", r"Model Number:\s*(.+)"),
        ("fw", r"System Firmware Version:\s*(.+)"),
        ("metal", r"Metal Support:\s*(.+)"),
        ("cores_txt", r"Total Number of Cores:\s*(\d+[^\n]*)"),
    ] {
        if let Some(m) = regex_capture(&raw, pat, 1) {
            out.insert(key.into(), Value::String(m.trim().to_string()));
        }
    }
    if let Some(m) = regex_capture(&raw, r"Type:\s*GPU[\s\S]{0,400}?Total Number of Cores:\s*(\d+)", 1) {
        if let Ok(n) = m.trim().parse::<i64>() {
            out.insert("gpu_cores".into(), json!(n));
        }
    }
    // 行首只有 `Model:` 的那一行（SPNVMe）
    for ln in raw.lines() {
        let trimmed = ln.trim_start();
        if trimmed.starts_with("Model:") && ln.len() - trimmed.len() > 0 {
            let v = trimmed["Model:".len()..].trim();
            if !v.is_empty() {
                out.insert("ssd".into(), Value::String(v.to_string()));
                break;
            }
        }
    }

    // 显示器
    let mut displays: Vec<Value> = Vec::new();
    let mut cur: Option<Map<String, Value>> = None;
    let mut inside = false;
    for ln in raw.lines() {
        let s = ln.trim();
        if s == "Displays:" {
            inside = true;
            cur = None;
            continue;
        }
        if !inside || s.is_empty() {
            continue;
        }
        let ind = ln.chars().take_while(|c| *c == ' ').count();
        if ind <= 6 {
            inside = false;
            continue;
        }
        if ind == 8 && s.ends_with(':') && !s.starts_with("Resolution") && !s.starts_with("UI Looks") {
            if let Some(c) = cur.take() {
                displays.push(Value::Object(c));
            }
            let mut m = Map::new();
            m.insert("name".into(), Value::String(s[..s.len() - 1].to_string()));
            cur = Some(m);
        } else if let Some(c) = cur.as_mut() {
            if let Some(rest) = s.strip_prefix("Resolution:") {
                let v = rest.trim().split(" (").next().unwrap_or("").to_string();
                c.insert("res".into(), Value::String(v));
            } else if let Some(rest) = s.strip_prefix("UI Looks like:") {
                c.insert("ui".into(), Value::String(rest.trim().to_string()));
            } else if let Some(rest) = s.strip_prefix("Main Display:") {
                c.insert(
                    "main".into(),
                    json!(rest.trim().to_lowercase() == "yes"),
                );
            }
        }
    }
    if let Some(c) = cur.take() {
        displays.push(Value::Object(c));
    }
    out.insert("displays".into(), Value::Array(displays));
    out
}

/// 简易正则：只支持 `\s*` / `\s+` / `(.+)` / `(\d+...)` / `[\s\S]{0,400}?` 这种固定模式，
/// 这里用 regex crate 的等价手写太啰嗦，直接用一次全局编译的正则。
fn regex_capture(text: &str, pat: &str, group: usize) -> Option<String> {
    let re = regex::Regex::new(pat).ok()?;
    re.captures(text)
        .and_then(|c| c.get(group).map(|m| m.as_str().to_string()))
}

// ------------------------------------------------------------------ specs

static SPEC_CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
const SPEC_TTL: Duration = Duration::from_secs(900);

fn round_or_zero(x: f64) -> Value {
    let v = round_n(x, 0);
    if v == 0.0 {
        Value::Null
    } else {
        json!(v as i64)
    }
}

fn compute_specs(ctx: &Ctx) -> Value {
    let now = now_secs();
    let prof = profiler();
    let chip = {
        let s = sysctl("machdep.cpu.brand_string");
        if s.is_empty() { "—".to_string() } else { s }
    };
    let arch = {
        let s = sysctl("hw.machine");
        if s.is_empty() { "—".to_string() } else { s }
    };
    let kv = sysctl("kern.version");
    let soc_code = regex_capture(&kv, r"RELEASE_ARM64_(\w+)", 1).unwrap_or_default();
    let mem_b = sysctl_num("hw.memsize");
    let disk = super::disk::metrics_disk(&disk_target(ctx));

    let mut os = Map::new();
    os.insert("name".into(), Value::String("macOS".into()));
    os.insert("ver".into(), Value::String(sysctl("kern.osproductversion")));
    os.insert("build".into(), Value::String(sysctl("kern.osversion")));
    os.insert("kernel".into(), Value::String(sysctl("kern.osrelease")));

    let mut model = Map::new();
    model.insert("id".into(), Value::String(sysctl("hw.model")));
    model.insert("name".into(), Value::String(str_of(&prof, "model_name")));
    model.insert("number".into(), Value::String(str_of(&prof, "model_number")));
    model.insert("board".into(), Value::String(sysctl("hw.targettype")));
    model.insert("soc".into(), Value::String(soc_code));
    model.insert("fw".into(), Value::String(str_of(&prof, "fw")));

    let mut chipm = Map::new();
    chipm.insert("name".into(), Value::String(chip.clone()));
    chipm.insert("arch".into(), Value::String(arch.clone()));
    chipm.insert(
        "isa".into(),
        Value::String(if arch == "arm64" { "ARM64".into() } else { arch.to_uppercase() }),
    );
    for (k, v) in soc_of(&chip) {
        chipm.insert(k.into(), v);
    }

    let mut cpu = Map::new();
    cpu.insert("cores".into(), json!(sysctl_num("hw.ncpu")));
    cpu.insert("perf".into(), json!(sysctl_num("hw.perflevel0.physicalcpu")));
    cpu.insert("eff".into(), json!(sysctl_num("hw.perflevel1.physicalcpu")));
    cpu.insert("l1_kb".into(), round_or_zero(sysctl_num("hw.l1icachesize") as f64 / 1024.0));
    cpu.insert("l1d_kb".into(), round_or_zero(sysctl_num("hw.l1dcachesize") as f64 / 1024.0));
    cpu.insert("l2_mb".into(), round_or_zero(sysctl_num("hw.l2cachesize") as f64 / 1048576.0));
    cpu.insert("page_kb".into(), round_or_zero(sysctl_num("hw.pagesize") as f64 / 1024.0));
    cpu.insert(
        "cacheline".into(),
        match sysctl_num("hw.cachelinesize") {
            0 => Value::Null,
            n => json!(n),
        },
    );

    let mut gpu = Map::new();
    gpu.insert(
        "cores".into(),
        match prof.get("gpu_cores") {
            Some(v) => v.clone(),
            None => Value::Null,
        },
    );
    gpu.insert("metal".into(), Value::String(str_of(&prof, "metal")));

    let mut mem = Map::new();
    mem.insert(
        "total_gb".into(),
        if mem_b != 0 {
            fnum(round_n(mem_b as f64 / 1073741824.0, 1))
        } else {
            json!(0)
        },
    );
    mem.insert(
        "type".into(),
        Value::String(if arch == "arm64" { "统一内存".into() } else { "内存".into() }),
    );

    let mut dsk = Map::new();
    dsk.insert("total_gb".into(), disk.get("total_gb").cloned().unwrap_or(Value::Null));
    dsk.insert("free_gb".into(), disk.get("free_gb").cloned().unwrap_or(Value::Null));
    dsk.insert("ssd".into(), Value::String(str_of(&prof, "ssd")));

    let mut o = Map::new();
    o.insert("at".into(), fnum(round_n(now, 1)));
    o.insert("os".into(), Value::Object(os));
    o.insert("model".into(), Value::Object(model));
    o.insert("chip".into(), Value::Object(chipm));
    o.insert("cpu".into(), Value::Object(cpu));
    o.insert("gpu".into(), Value::Object(gpu));
    o.insert("mem".into(), Value::Object(mem));
    o.insert("disk".into(), Value::Object(dsk));
    o.insert(
        "displays".into(),
        prof.get("displays").cloned().unwrap_or(json!([])),
    );
    Value::Object(o)
}

fn str_of(m: &Map<String, Value>, k: &str) -> String {
    m.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

pub fn specs(fresh: bool, ctx: &Ctx) -> Value {
    if !fresh {
        if let Ok(g) = SPEC_CACHE.lock() {
            if let Some((at, v)) = g.as_ref() {
                if at.elapsed() < SPEC_TTL {
                    return v.clone();
                }
            }
        }
    }
    let v = compute_specs(ctx);
    if let Ok(mut g) = SPEC_CACHE.lock() {
        *g = Some((Instant::now(), v.clone()));
    }
    v
}

// ------------------------------------------------------------------ software

static SW_CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
const SW_TTL: Duration = Duration::from_secs(300);

fn app_version(bundle: &Path) -> String {
    let p = bundle.join("Contents").join("Info.plist");
    let txt = platform::run_cmd(
        &["plutil", "-convert", "json", "-o", "-", &p.to_string_lossy()],
        Duration::from_secs(8),
    );
    let v: Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    for k in ["CFBundleShortVersionString", "CFBundleVersion"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    String::new()
}

fn brew_bin() -> String {
    for p in ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"] {
        if Path::new(p).exists() {
            return p.to_string();
        }
    }
    String::new()
}

fn compute_software() -> Value {
    let now = now_secs();
    let env = [
        ("HOMEBREW_NO_AUTO_UPDATE", "1"),
        ("HOMEBREW_NO_ANALYTICS", "1"),
        ("HOMEBREW_NO_ENV_HINTS", "1"),
    ];
    let home = home();
    let mut apps: Vec<Value> = Vec::new();
    for d in [PathBuf::from("/Applications"), home.join("Applications")] {
        let mut names: Vec<String> = match std::fs::read_dir(&d) {
            Ok(rd) => rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(_) => continue,
        };
        names.sort();
        for n in names {
            if !n.ends_with(".app") {
                continue;
            }
            let b = d.join(&n);
            let mut m = Map::new();
            m.insert("name".into(), Value::String(n[..n.len() - 4].to_string()));
            m.insert("ver".into(), Value::String(app_version(&b)));
            m.insert("user".into(), json!(d.to_string_lossy().starts_with(&*home.to_string_lossy())));
            apps.push(Value::Object(m));
        }
    }
    let brew = brew_bin();
    let (mut formula, mut leaves, mut cask) = (Vec::new(), Vec::new(), Vec::new());
    if !brew.is_empty() {
        formula = lines_nonempty(&run_out_env(&[&brew, "list", "--formula"], &env));
        leaves = lines_nonempty(&run_out_env(&[&brew, "leaves"], &env));
        cask = lines_nonempty(&run_out_env(&[&brew, "list", "--cask"], &env));
    }
    formula.sort();
    leaves.sort();
    cask.sort();

    let mut o = Map::new();
    o.insert("at".into(), fnum(round_n(now, 1)));
    o.insert("apps".into(), Value::Array(apps));
    o.insert("formula".into(), json!(formula));
    o.insert("leaves".into(), json!(leaves));
    o.insert("cask".into(), json!(cask));
    o.insert("brew".into(), Value::String(brew));
    Value::Object(o)
}

fn lines_nonempty(s: &str) -> Vec<String> {
    s.lines()
        .filter(|x| !x.trim().is_empty())
        .map(|x| x.to_string())
        .collect()
}

pub fn software(fresh: bool) -> Value {
    if !fresh {
        if let Ok(g) = SW_CACHE.lock() {
            if let Some((at, v)) = g.as_ref() {
                if at.elapsed() < SW_TTL {
                    return v.clone();
                }
            }
        }
    }
    let v = compute_software();
    if let Ok(mut g) = SW_CACHE.lock() {
        *g = Some((Instant::now(), v.clone()));
    }
    v
}

// ------------------------------------------------------------------ units

static UNITS_CACHE: Mutex<Option<(Instant, Value)>> = Mutex::new(None);
const UNITS_TTL: Duration = Duration::from_secs(60);

fn launch_pids(domain: &str) -> Vec<(String, i64)> {
    let raw = platform::run_cmd(&["/bin/launchctl", "print", domain], Duration::from_secs(6));
    let mut out: Vec<(String, i64)> = Vec::new();
    for ln in raw.lines() {
        let t = ln.trim();
        let parts: Vec<&str> = t.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        let pid = match parts[0].parse::<i64>() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let label = parts[2];
        if !label.contains('.') {
            continue;
        }
        if label.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
            if !out.iter().any(|(l, _)| l == label) {
                out.push((label.to_string(), pid));
            }
        }
    }
    out
}

fn plists(dir: &Path) -> Vec<(String, String, String)> {
    let mut names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".plist"))
            .collect(),
        Err(_) => return Vec::new(),
    };
    names.sort();
    let mut out = Vec::new();
    for name in names {
        let p = dir.join(&name);
        let txt = platform::run_cmd(
            &["plutil", "-convert", "json", "-o", "-", &p.to_string_lossy()],
            Duration::from_secs(8),
        );
        let v: Value = match serde_json::from_str(&txt) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !v.is_object() {
            continue;
        }
        let label = v
            .get("Label")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| name.trim_end_matches(".plist").to_string());
        let prog = v
            .get("Program")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                v.get("ProgramArguments")
                    .and_then(|x| x.as_array())
                    .and_then(|a| a.first())
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        let prog_name = Path::new(&prog)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push((label, name, prog_name));
    }
    out
}

fn compute_units() -> Value {
    let now = now_secs();
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0; // Windows 没有 launchd；下面 launch_pids 会直接回空
    let gui = launch_pids(&format!("gui/{}", uid));
    let mut brew_svc: Vec<(String, String)> = Vec::new();
    let brew = brew_bin();
    if !brew.is_empty() {
        let raw = run_out_env(
            &[&brew, "services", "list"],
            &[("HOMEBREW_NO_AUTO_UPDATE", "1"), ("HOMEBREW_NO_ANALYTICS", "1")],
        );
        for ln in raw.lines().skip(1) {
            let parts: Vec<&str> = ln.split_whitespace().collect();
            if parts.len() >= 2 {
                brew_svc.push((parts[0].to_string(), parts[1].to_string()));
            }
        }
    }

    let pack = |dir: &Path, scope: &str, with_state: bool| -> Vec<Value> {
        let mut rows = Vec::new();
        for (label, file, prog) in plists(dir) {
            let pid = if with_state {
                gui.iter().find(|(l, _)| *l == label).map(|(_, p)| *p).unwrap_or(0)
            } else {
                0
            };
            let brew_st = brew_svc
                .iter()
                .find(|(k, _)| label.ends_with(k.as_str()))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            let short = ["com.local-ai.", "homebrew.mxcl.", "sh.brew."]
                .iter()
                .find(|p| label.starts_with(*p))
                .map(|p| label[p.len()..].to_string())
                .unwrap_or_else(|| label.clone());
            let mut m = Map::new();
            m.insert("label".into(), Value::String(label));
            m.insert("file".into(), Value::String(file));
            m.insert("prog".into(), Value::String(prog));
            m.insert("scope".into(), Value::String(scope.to_string()));
            m.insert("pid".into(), json!(pid));
            m.insert("running".into(), json!(pid > 0));
            m.insert("brew".into(), Value::String(brew_st));
            m.insert("short".into(), Value::String(short));
            rows.push(Value::Object(m));
        }
        rows
    };

    let home = home();
    let mut bs: Vec<Value> = brew_svc
        .iter()
        .map(|(k, v)| json!({"name": k, "status": v}))
        .collect();
    bs.sort_by(|a, b| {
        a.get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .cmp(b.get("name").and_then(|x| x.as_str()).unwrap_or(""))
    });

    let mut o = Map::new();
    o.insert("at".into(), fnum(round_n(now, 1)));
    o.insert("user".into(), Value::Array(pack(&home.join("Library").join("LaunchAgents"), "user", true)));
    o.insert("global".into(), Value::Array(pack(Path::new("/Library/LaunchAgents"), "global", true)));
    o.insert("daemon".into(), Value::Array(pack(Path::new("/Library/LaunchDaemons"), "daemon", false)));
    o.insert("brew_services".into(), Value::Array(bs));
    Value::Object(o)
}

pub fn units(fresh: bool) -> Value {
    if !fresh {
        if let Ok(g) = UNITS_CACHE.lock() {
            if let Some((at, v)) = g.as_ref() {
                if at.elapsed() < UNITS_TTL {
                    return v.clone();
                }
            }
        }
    }
    let v = compute_units();
    if let Ok(mut g) = UNITS_CACHE.lock() {
        *g = Some((Instant::now(), v.clone()));
    }
    v
}

/// 让 `load_json` 别被当成未用（spec 之外也会用到，保留这一处引用）。
#[allow(dead_code)]
fn _keep(p: &Path) -> Option<Value> {
    load_json(p)
}
