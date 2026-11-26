// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：进程采样器（10 秒一轮，最近 2 小时的六维走势）+ 逐进程网络（nettop，60 秒）。
//       对齐 hostwork 的 sample_procs / _HIST / _NETP，给 overview 的 apps / app_series 供数。
// 不做：不落盘 进程.jsonl（Python 侧才记），也不建图标（见 procnames::icon_name）。
//
// 为什么自己起线程：Python 在 import 时就起了一条采样线程，`_PROC` 里一直有账；
// 这里改成**首次用到时懒启动**，之后同样每 10 秒一轮。
#![allow(dead_code)]

use super::platform;
use super::procnames::{group_name, owners_of};
use super::util::{fnum, round_n, str_or};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

pub const TICK_SECS: i64 = 10;
const HIST_MAX: usize = 720;
const NETP_EVERY: i64 = 60;

#[derive(Clone, Default)]
struct PState {
    at: f64,
    apps: Vec<Value>,
    cpu_pct: f64,
    cpu_true: f64,
    cpu_ready: bool,
    total_rss: f64,
}

static STATE: Mutex<Option<PState>> = Mutex::new(None);
static HIST: Mutex<Option<VecDeque<Value>>> = Mutex::new(None);
static PREV_CPU: Mutex<Option<HashMap<String, f64>>> = Mutex::new(None);
static PREV_RU: Mutex<Option<HashMap<String, (f64, f64, f64)>>> = Mutex::new(None);
static PID_GROUP: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
static CPU_LAST: Mutex<Option<[u64; 4]>> = Mutex::new(None);
static CPU_READY: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
static TICK: AtomicU64 = AtomicU64::new(0);

struct NetP {
    at: f64,
    cum: HashMap<String, (f64, f64)>,
    rate: HashMap<String, (f64, f64)>,
    ok: Option<bool>,
    err: String,
}

static NETP: Mutex<Option<NetP>> = Mutex::new(None);

fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn cpu_count() -> i64 {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 {
        n
    } else {
        1
    }
}

/// 整机 CPU 使用率：user + system + nice 的增量 ÷ 总增量。
fn cpu_true() -> f64 {
    let (user, nice, system, idle) = match platform::cpu_ticks() {
        Some(x) => x,
        None => return 0.0,
    };
    let cur = [user, nice, system, idle];
    let prev = {
        let mut g = CPU_LAST.lock().unwrap();
        let p = *g;
        *g = Some(cur);
        p
    };
    let prev = match prev {
        Some(p) => p,
        None => return 0.0,
    };
    // `cpu_ticks()` 已经是 Python `[user, system, idle, nice]` 的顺序，
    // 再按索引重排一次会把 idle 换成 nice —— 算出来永远是 100%。
    let c = cur;
    let p = prev;
    let mut total: i64 = 0;
    let mut d = [0i64; 4];
    for i in 0..4 {
        d[i] = c[i] as i64 - p[i] as i64;
        total += d[i];
    }
    if total <= 0 {
        return 0.0;
    }
    CPU_READY.store(true, Ordering::Relaxed);
    round_n((1.0 - d[2] as f64 / total as f64) * 100.0, 1)
}

fn needs_cwd(name: &str) -> bool {
    const KEYS: [&str; 9] = ["python", "node", "ruby", "perl", "deno", "bun", "bash", "zsh", "uvicorn"];
    let low = name.to_lowercase();
    KEYS.iter().any(|k| low.contains(k)) || low.contains("gunicorn")
}

fn pid_group_get(pid: &str) -> Option<String> {
    PID_GROUP.lock().ok()?.as_ref()?.get(pid).cloned()
}

fn netp_sample() {
    let out = std::process::Command::new("/usr/bin/nettop")
        .args(["-P", "-x", "-L", "1", "-J", "bytes_in,bytes_out"])
        .output();
    let raw = match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(e) => {
            let mut g = NETP.lock().unwrap();
            *g = Some(NetP {
                at: 0.0,
                cum: HashMap::new(),
                rate: HashMap::new(),
                ok: Some(false),
                err: format!("{}: {}", std::any::type_name_of_val(&e), e).chars().take(120).collect(),
            });
            return;
        }
    };
    let mut cum: HashMap<String, (f64, f64)> = HashMap::new();
    for line in raw.lines() {
        let t = line.trim().trim_end_matches(',');
        let f: Vec<&str> = t.split(',').collect();
        if f.len() != 3 || f[0].is_empty() {
            continue;
        }
        let bi: f64 = match f[1].trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let bo: f64 = match f[2].trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let (name, pid) = match f[0].rsplit_once('.') {
            Some((n, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (n.to_string(), p.to_string()),
            _ => (f[0].to_string(), String::new()),
        };
        let key = if pid.is_empty() { name } else { pid_group_get(&pid).unwrap_or(name) };
        let e = cum.entry(key).or_insert((0.0, 0.0));
        e.0 += bi;
        e.1 += bo;
    }
    let now = now_secs();
    let mut g = NETP.lock().unwrap();
    let (prev_at, prev) = match g.as_ref() {
        Some(p) => (p.at, p.cum.clone()),
        None => (0.0, HashMap::new()),
    };
    let dt = now - if prev_at == 0.0 { now } else { prev_at };
    let mut rate: HashMap<String, (f64, f64)> = HashMap::new();
    if !prev.is_empty() && dt >= 5.0 {
        for (k, cur) in &cum {
            if let Some(p) = prev.get(k) {
                rate.insert(
                    k.clone(),
                    (((cur.0 - p.0) / dt).max(0.0), ((cur.1 - p.1) / dt).max(0.0)),
                );
            }
        }
    }
    *g = Some(NetP { at: now, cum, rate, ok: Some(true), err: String::new() });
}

fn netp_loop() {
    std::thread::sleep(Duration::from_secs(8));
    loop {
        netp_sample();
        std::thread::sleep(Duration::from_secs(NETP_EVERY as u64));
    }
}

/// 采一次全量进程，按名字合并。
pub fn sample_once() -> Value {
    let now = now_secs();
    let pids = platform::list_pids();
    let cpus = cpu_count() as f64;

    let mut cpu_now: HashMap<String, f64> = HashMap::new();
    let mut rss: HashMap<String, f64> = HashMap::new();
    let mut paths: HashMap<String, String> = HashMap::new();
    let mut owners: HashMap<String, Vec<&'static str>> = HashMap::new();
    let mut starts: HashMap<String, f64> = HashMap::new();
    let mut pids_by: HashMap<String, i64> = HashMap::new();
    let mut io_r: HashMap<String, f64> = HashMap::new();
    let mut io_w: HashMap<String, f64> = HashMap::new();
    let mut nj: HashMap<String, f64> = HashMap::new();
    let mut pid_group: HashMap<String, String> = HashMap::new();

    for pid in &pids {
        let (ppid, started, name) = match platform::proc_bsdinfo(*pid) {
            Some(x) => (x.0, x.2, x.3),
            None => continue,
        };
        let _ = ppid;
        if name.is_empty() {
            continue;
        }
        let path = platform::proc_path(*pid).unwrap_or_default();
        let (rss_b, cpu_us) = platform::proc_taskinfo(*pid).unwrap_or((0, 0));

        let key = group_name(&name, &path);
        *cpu_now.entry(key.clone()).or_insert(0.0) += cpu_us as f64 / 1e9;
        *rss.entry(key.clone()).or_insert(0.0) += rss_b as f64;
        *pids_by.entry(key.clone()).or_insert(0) += 1;
        pid_group.insert(pid.to_string(), key.clone());

        if let Some((r, w, e)) = platform::proc_rusage(*pid) {
            *io_r.entry(key.clone()).or_insert(0.0) += r as f64;
            *io_w.entry(key.clone()).or_insert(0.0) += w as f64;
            *nj.entry(key.clone()).or_insert(0.0) += e as f64;
        }
        if !path.is_empty() && !paths.contains_key(&key) {
            paths.insert(key.clone(), path.clone());
        }
        // 解释器只认可执行文件路径认不出主人：`python` 打开的脚本/cwd 才是线索。
        // 代价是给每个解释器进程多两次 proc_pidinfo，Python 侧也是这么干的。
        let files = if needs_cwd(&name) {
            platform::proc_fd_paths(*pid, 24)
        } else {
            String::new()
        };
        let hit = owners.entry(key.clone()).or_default();
        for o in owners_of(&format!("{} {}", path, files), &name) {
            if !hit.contains(&o) {
                hit.push(o);
            }
        }
        if started != 0 {
            let cur = starts.get(&key).copied().unwrap_or(0.0);
            starts.insert(key.clone(), if cur == 0.0 { started as f64 } else { cur.min(started as f64) });
        }
    }

    let prev_cpu = {
        let mut g = PREV_CPU.lock().unwrap();
        let p = g.clone().unwrap_or_default();
        *g = Some(cpu_now.clone());
        p
    };
    let prev_at = STATE.lock().unwrap().as_ref().map(|s| s.at).unwrap_or(0.0);
    let dt = now - if prev_at == 0.0 { now } else { prev_at };
    let netrate = NETP
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| p.rate.clone())
        .unwrap_or_default();

    let mut ru_now: HashMap<String, (f64, f64, f64)> = HashMap::new();
    let mut apps: Vec<Value> = Vec::new();
    for (key, tot) in &cpu_now {
        let pct = match prev_cpu.get(key) {
            None => 0.0,
            Some(p) => {
                if dt <= 0.0 {
                    0.0
                } else {
                    ((tot - p) / dt * 100.0).max(0.0)
                }
            }
        };
        let cur = (
            io_r.get(key).copied().unwrap_or(0.0),
            io_w.get(key).copied().unwrap_or(0.0),
            nj.get(key).copied().unwrap_or(0.0),
        );
        ru_now.insert(key.clone(), cur);
        let prev_ru = {
            let g = PREV_RU.lock().unwrap();
            g.as_ref().and_then(|m| m.get(key).copied())
        };
        let (dr, dw, pw) = match prev_ru {
            Some(p) if dt > 0.0 => (
                ((cur.0 - p.0) / dt).max(0.0),
                ((cur.1 - p.1) / dt).max(0.0),
                ((cur.2 - p.2) / dt / 1e9).max(0.0),
            ),
            _ => (0.0, 0.0, 0.0),
        };
        let nr = netrate.get(key);
        let mut m = Map::new();
        m.insert("name".into(), Value::String(key.clone()));
        m.insert("cpu".into(), fnum(round_n(pct, 1)));
        m.insert("cpu_s".into(), fnum(round_n(*tot, 1)));
        m.insert("cores".into(), fnum(round_n(pct / 100.0, 2)));
        m.insert("mem".into(), fnum(round_n(rss.get(key).copied().unwrap_or(0.0) / 1048576.0, 1)));
        m.insert("dread".into(), fnum(round_n(dr, 1)));
        m.insert("dwrite".into(), fnum(round_n(dw, 1)));
        m.insert(
            "net".into(),
            fnum(round_n(nr.map(|x| x.0 + x.1).unwrap_or(0.0), 1)),
        );
        m.insert(
            "net_in".into(),
            match nr { Some(x) => fnum(round_n(x.0, 1)), None => fnum(0.0) },
        );
        m.insert(
            "net_out".into(),
            match nr { Some(x) => fnum(round_n(x.1, 1)), None => fnum(0.0) },
        );
        m.insert("pwr".into(), fnum(round_n(pw, 2)));
        m.insert("pids".into(), json!(pids_by.get(key).copied().unwrap_or(0)));
        m.insert(
            "age".into(),
            match starts.get(key) {
                Some(s) if *s != 0.0 => json!(round_n(now - s, 0) as i64),
                _ => Value::Null,
            },
        );
        m.insert("path".into(), Value::String(paths.get(key).cloned().unwrap_or_default()));
        let mut ow = owners.get(key).cloned().unwrap_or_default();
        ow.sort();
        m.insert("owners".into(), json!(ow));
        apps.push(Value::Object(m));
    }
    {
        let mut g = PREV_RU.lock().unwrap();
        *g = Some(ru_now);
    }
    {
        let mut g = PID_GROUP.lock().unwrap();
        *g = Some(pid_group);
    }

    apps.sort_by(|a, b| {
        let ca = a.get("cpu").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let cb = b.get("cpu").and_then(|v| v.as_f64()).unwrap_or(0.0);
        cb.partial_cmp(&ca)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let ma = a.get("mem").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let mb = b.get("mem").and_then(|v| v.as_f64()).unwrap_or(0.0);
                mb.partial_cmp(&ma).unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let total_pct = round_n(
        apps.iter().filter_map(|a| a.get("cpu").and_then(|v| v.as_f64())).sum::<f64>() / cpus,
        1,
    );
    let total_rss = round_n(
        apps.iter().filter_map(|a| a.get("mem").and_then(|v| v.as_f64())).sum::<f64>(),
        1,
    );
    let true_pct = cpu_true();
    let rec = PState {
        at: round_n(now, 1),
        apps: apps.clone(),
        cpu_pct: total_pct,
        cpu_true: true_pct,
        cpu_ready: CPU_READY.load(Ordering::Relaxed),
        total_rss,
    };
    {
        let mut g = STATE.lock().unwrap();
        *g = Some(rec.clone());
    }
    {
        let mut g = HIST.lock().unwrap();
        let q = g.get_or_insert_with(VecDeque::new);
        if q.len() >= HIST_MAX {
            q.pop_front();
        }
        let mut am = Map::new();
        for a in apps.iter().take(40) {
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let pick = |k: &str| a.get(k).cloned().unwrap_or(json!(0.0));
            am.insert(
                name,
                json!([
                    pick("cpu"),
                    pick("mem"),
                    pick("dwrite"),
                    pick("dread"),
                    pick("net"),
                    pick("pwr")
                ]),
            );
        }
        q.push_back(json!({"at": rec.at, "cpu": rec.cpu_true, "apps": am}));
    }
    let tick = TICK.fetch_add(1, Ordering::Relaxed) + 1;
    if tick % 12 == 0 {
        dump_procs(&rec);
    }

    let mut out = Map::new();
    out.insert("at".into(), fnum(rec.at));
    out.insert("apps".into(), Value::Array(rec.apps.clone()));
    out.insert("cpu_pct".into(), fnum(rec.cpu_pct));
    out.insert("cpu_true".into(), fnum(rec.cpu_true));
    out.insert("cpu_ready".into(), json!(rec.cpu_ready));
    out.insert("total_rss".into(), fnum(rec.total_rss));
    Value::Object(out)
}

/// 首次调用时起两条采样线程（进程 10 秒、网络 60 秒），幂等。
/// 数据根下的 `_本机工作`（进程.jsonl 落在那儿，跟 Python 共用一份）。
static DATA_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
/// 上一次落盘的内容指纹与时刻（内容和上一条一样就不重复记账，一小时留一条）
static LAST_DUMP: Mutex<Option<(String, f64)>> = Mutex::new(None);

pub fn start_at(dir: std::path::PathBuf) {
    let _ = DATA_DIR.set(dir);
    start();
}

fn proc_path() -> Option<std::path::PathBuf> {
    DATA_DIR.get().map(|d| d.join("进程.jsonl"))
}

/// `load_proc_history()`：重启后别让「近两小时」空着。
/// 粒度是落盘节奏（2 分钟），比内存里的 10 秒粗，但图接得上。
fn load_proc_history() {
    let Some(p) = proc_path() else { return };
    let Ok(b) = std::fs::read(&p) else { return };
    let since = now_secs() - 2.0 * 3600.0;
    let text = String::from_utf8_lossy(&b);
    let mut rows: Vec<Value> = Vec::new();
    for ln in text.lines() {
        if !ln.starts_with('{') {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(ln) else { continue };
        let ts = match chrono::NaiveDateTime::parse_from_str(
            str_or(d.get("at")).get(..19).unwrap_or(""),
            "%Y-%m-%d %H:%M:%S",
        ) {
            Ok(x) => x.and_local_timezone(chrono::Local).single().map(|t| t.timestamp() as f64),
            Err(_) => None,
        };
        let Some(ts) = ts else { continue };
        if ts < since {
            continue;
        }
        let mut am = Map::new();
        if let Some(Value::Array(top)) = d.get("top") {
            for x in top {
                let Some(xs) = x.as_array() else { continue };
                if xs.len() < 2 {
                    continue;
                }
                let nm = str_or(xs.first());
                let cpu = xs.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
                let mem = if xs.len() > 2 { xs.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0) } else { 0.0 };
                am.insert(nm, json!([cpu, mem, 0.0, 0.0, 0.0, 0.0]));
            }
        }
        rows.push(json!({"at": ts, "cpu": d.get("cpu_pct").and_then(|v| v.as_f64()).unwrap_or(0.0), "apps": am}));
    }
    let mut g = HIST.lock().unwrap();
    let q = g.get_or_insert_with(VecDeque::new);
    let st = rows.len().saturating_sub(HIST_MAX);
    for r in rows.into_iter().skip(st) {
        if q.len() >= HIST_MAX {
            q.pop_front();
        }
        q.push_back(r);
    }
}

/// `_term_stat(apps)`：终端类进程的个数与内存合计（风暴看门狗用）。
fn term_stat(apps: &[Value]) -> Value {
    const KEYS: [&str; 9] = [
        "windowsterminal", "wt", "conhost", "openconsole", "terminal", "iterm2", "iterm",
        "alacritty", "wezterm",
    ];
    let mut n = 0i64;
    let mut mb = 0.0f64;
    let mut top: Vec<(String, f64)> = Vec::new();
    for a in apps {
        let mut k = str_or(a.get("name")).to_lowercase();
        if k.ends_with(".exe") {
            k.truncate(k.len() - 4);
        }
        if !KEYS.contains(&k.as_str()) {
            continue;
        }
        let m = a.get("mem").and_then(|v| v.as_f64()).unwrap_or(0.0);
        n += 1;
        mb += m;
        if top.len() < 6 {
            top.push((str_or(a.get("name")), round_n(m, 1)));
        }
    }
    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    json!({
        "n": n,
        "mb": super::util::fnum(round_n(mb, 1)),
        "top": top.into_iter().map(|(nm, m)| json!([nm, super::util::fnum(m)])).collect::<Vec<_>>(),
    })
}

/// `_dump_procs(rec)`：有变化照写，没变化也每小时留一条。
fn dump_procs(rec: &PState) {
    let Some(p) = proc_path() else { return };
    let mut top: Vec<Value> = Vec::new();
    for a in rec.apps.iter().take(15) {
        let cpu = a.get("cpu").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if cpu <= 0.2 {
            continue;
        }
        top.push(json!([
            str_or(a.get("name")),
            a.get("cpu").cloned().unwrap_or(json!(0.0)),
            a.get("mem").cloned().unwrap_or(json!(0.0)),
        ]));
    }
    let terms = term_stat(&rec.apps);
    let tn = terms.get("n").and_then(|v| v.as_i64()).unwrap_or(0);
    if top.is_empty() && tn == 0 {
        return;
    }
    let key = format!(
        "{}|{}",
        serde_json::to_string(&top).unwrap_or_default(),
        serde_json::to_string(&terms).unwrap_or_default()
    );
    let now = now_secs();
    {
        let g = LAST_DUMP.lock().unwrap();
        if let Some((k, at)) = g.as_ref() {
            if *k == key && now - *at < 3600.0 {
                return;
            }
        }
    }
    let line = format!(
        "{}\n",
        serde_json::to_string(&json!({
            "at": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            "cpu_pct": super::util::fnum(rec.cpu_pct),
            "top": top,
            "terms": terms,
        }))
        .unwrap_or_default()
    );
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        if f.write_all(line.as_bytes()).is_ok() {
            let mut g = LAST_DUMP.lock().unwrap();
            *g = Some((key, now));
        }
    }
}

pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // 先同步采一笔：Python 的 sampler_loop 在进程启动时立刻采第一笔，页面第一次
    // 打开就有进程榜。这里懒启动，就把第一笔挪进来 —— 否则第一次 overview 的
    // `apps` 是空表、`cpu_pct` 是 0。
    load_proc_history();
    let _ = sample_once();
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_secs(TICK_SECS as u64));
        let _ = std::panic::catch_unwind(|| sample_once());
    });
    std::thread::spawn(netp_loop);
}

/// overview 用：当前 `_PROC` 的一份深拷贝 + 近一小时的历史。
pub fn snapshot() -> (Value, Vec<Value>) {
    let st = STATE.lock().unwrap().clone().unwrap_or_default();
    let hist: Vec<Value> = HIST
        .lock()
        .unwrap()
        .as_ref()
        .map(|q| q.iter().cloned().collect())
        .unwrap_or_default();
    let mut m = Map::new();
    m.insert("at".into(), fnum(st.at));
    m.insert("apps".into(), Value::Array(st.apps));
    m.insert("cpu_pct".into(), fnum(st.cpu_pct));
    m.insert("cpu_true".into(), fnum(st.cpu_true));
    m.insert("cpu_ready".into(), json!(st.cpu_ready));
    m.insert("total_rss".into(), fnum(st.total_rss));
    (Value::Object(m), hist)
}

/// `netproc` 那三个字段。
pub fn netproc_info() -> Value {
    let g = NETP.lock().unwrap();
    match g.as_ref() {
        Some(p) => json!({
            "ok": match p.ok { Some(b) => json!(b), None => Value::Null },
            "every": NETP_EVERY,
            "err": p.err,
        }),
        None => json!({"ok": Value::Null, "every": NETP_EVERY, "err": ""}),
    }
}

/// `_hist_val(v, i)`：六个数取第 i 个；老格式（只有一个 CPU）只认 i=0。
pub fn hist_val(v: &Value, i: usize) -> f64 {
    match v {
        Value::Array(a) => {
            if i < a.len() {
                a[i].as_f64().unwrap_or(0.0)
            } else {
                0.0
            }
        }
        Value::Number(n) => {
            if i == 0 {
                n.as_f64().unwrap_or(0.0)
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}
