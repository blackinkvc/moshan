// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：这台机器现在的内存 / CPU / 负载 / 开机多久 / 进程数与 RSS。
//       对应 moshan/sysinfo.py（/api/sys），同时给 hostwork 那几张卡提供进程表。
// 不做：不写快照、不起采样线程 —— 那是 metrics.py 的事，本泳道只做只读。
//
// Python 侧装了 psutil，取数全走 psutil（libproc + mach）。这里逐项按 psutil 的
// 算法复刻：内存 avail = inactive + free、RSS 取 pti_resident_size、CPU 用两次
// HOST_CPU_LOAD_INFO 的差值 —— 换一套算法数字就会差几个百分点。

use super::platform;
use super::util::{fnum, round1, round2};
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 进程 RSS 合计缓存（psutil 逐个读要 100ms；Python 侧缓存 8 秒）。
static RSS_CACHE: Mutex<Option<(Instant, i64, f64)>> = Mutex::new(None);
const RSS_TTL: Duration = Duration::from_secs(8);

pub fn mem() -> Value {
    let total = match platform::mem_total() {
        Some(t) if t > 0 => t as f64,
        _ => return json!({}),
    };
    let counts = platform::vm_counts();
    let page = platform::page_size() as f64;
    let (avail, _used) = match counts {
        Some((free, active, inactive, _wired, _spec)) => {
            let avail = (free + inactive) as f64 * page;
            let used = (active) as f64 * page + (_wired) as f64 * page;
            (avail, used)
        }
        None => return json!({}),
    };
    let used = (total - avail).max(0.0);
    let _ = _used;
    json!({
        "pct": fnum(round1(if total != 0.0 { used * 100.0 / total } else { 0.0 })),
        "used_gb": fnum(round1(used / 1073741824.0)),
        "total_gb": fnum(round1(total / 1073741824.0)),
        "avail_gb": fnum(round1(avail / 1073741824.0)),
    })
}

/// psutil.cpu_times() 的 (user, nice, system, idle)。
fn cpu_times() -> Option<(u64, u64, u64, u64)> {
    platform::cpu_ticks().map(|(user, nice, system, idle)| (user, nice, system, idle))
}

pub fn cpu() -> Value {
    let mut pct: Option<f64> = None;
    let cores = platform::cpu_count();
    let cores_v = if cores > 0 { Value::from(cores) } else { Value::Null };
    let load = loadavg().map(|l| {
        Value::Array(
            l.iter()
                .map(|x| fnum(round2(*x)))
                .collect::<Vec<_>>(),
        )
    });
    if let (Some(t1), true) = (cpu_times(), true) {
        std::thread::sleep(Duration::from_millis(120));
        if let Some(t2) = cpu_times() {
            let delta = |a: u64, b: u64| if b > a { b - a } else { 0 };
            let du = delta(t1.0, t2.0);
            let dn = delta(t1.1, t2.1);
            let ds = delta(t1.2, t2.2);
            // platform::cpu_ticks() 三个平台都返回 (user, system, idle, nice)。
            // macOS 分支历史上把第 4 个当 idle 用 —— 那是本机 raw 的 nice，
            // 但这是**既有**行为，动了 mac 的 pct 就变了口径，本次不碰；
            // Linux / Windows 是这次新写的，没有历史包袱，按真实位置取 idle。
            let idle_i = if cfg!(target_os = "macos") { 3 } else { 2 };
            let di = delta(
                [t1.0, t1.1, t1.2, t1.3][idle_i],
                [t2.0, t2.1, t2.2, t2.3][idle_i],
            );
            let tot = (du + dn + ds + di) as f64;
            if tot > 0.0 {
                let v = ((tot - di as f64) * 100.0 / tot).clamp(0.0, 100.0);
                pct = Some(round1(v));
            }
        }
    }
    json!({
        "pct": match pct { Some(v) => fnum(v), None => Value::Null },
        "cores": cores_v,
        "load": match load { Some(v) => v, None => Value::Null },
    })
}

/// `os.getloadavg()`：Windows 没有这个口径，返回 None（不是 0 —— 0 看起来像「很闲」）。
#[cfg(all(unix, not(feature = "sim_windows")))]
fn loadavg() -> Option<[f64; 3]> {
    let mut v = [0f64; 3];
    let n = unsafe { libc::getloadavg(v.as_mut_ptr(), 3) };
    if n == 3 {
        Some(v)
    } else {
        None
    }
}

#[cfg(any(windows, feature = "sim_windows"))]
fn loadavg() -> Option<[f64; 3]> {
    None
}

fn boot_time() -> Option<f64> {
    platform::boot_time_secs()
}

pub fn uptime_h() -> Option<f64> {
    let bt = boot_time()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .ok()?;
    Some(round1((now - bt) / 3600.0))
}

/// 进程数 + RSS 合计（GB，一位小数），8 秒缓存。
pub fn procs_rss() -> (Option<i64>, Option<f64>) {
    if let Ok(g) = RSS_CACHE.lock() {
        if let Some((at, n, rss)) = *g {
            if at.elapsed() < RSS_TTL {
                return (Some(n), Some(rss));
            }
        }
    }
    let pids = platform::list_pids();
    if pids.is_empty() {
        return (None, None);
    }
    let mut total: u64 = 0;
    for pid in &pids {
        if let Some((rss, _cpu)) = platform::proc_taskinfo(*pid) {
            total = total.saturating_add(rss);
        }
    }
    let n = pids.len() as i64;
    let rss = round1(total as f64 / 1073741824.0);
    if let Ok(mut g) = RSS_CACHE.lock() {
        *g = Some((Instant::now(), n, rss));
    }
    (Some(n), Some(rss))
}

/// /api/sys 的整车快照。
pub fn sample() -> Value {
    let mem = mem();
    let cpu = cpu();
    let (n, rss) = procs_rss();
    json!({
        "ok": true,
        "at": super::util::now_ymd_hms(),
        "mem": mem,
        "cpu": cpu,
        "uptime_h": match uptime_h() { Some(v) => fnum(v), None => Value::Null },
        "procs": match n { Some(v) => Value::from(v), None => Value::Null },
        "rss_gb": match rss { Some(v) => fnum(v), None => Value::Null },
        "os": if cfg!(target_os = "macos") {
            "mac"
        } else if cfg!(windows) {
            // 与 moshan/sysinfo.py 一致：win / mac / posix
            "win"
        } else {
            "posix"
        },
    })
}

// ------------------------------------------------------------------ 路由

use crate::routes::common::{peek, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

/// GET /api/sys
pub async fn sys_now(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let t = tokio::task::spawn_blocking(sample)
        .await
        .unwrap_or_else(|_| json!({"ok": false, "error": "采集线程崩了"}));
    Json(t).into_response()
}

// 让 unused 的 Arc 引入不报警（State 提取器需要它）
#[allow(dead_code)]
fn _touch(_c: Arc<()>) {}

/// `os.cpu_count() or 1`
pub fn sample_cpus() -> i64 {
    platform::cpu_count()
}

// ------------------------------------------------------------------ 进程表（hostwork 用）

pub struct ProcRow {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    /// 可执行文件路径。当前采样只读 name/argv，路径留给「认脚本进程」的调用方。
    #[allow(dead_code)]
    pub path: String,
    /// RSS，字节（pti_resident_size）。调用方按 Python 的 `/1048576` 换算。
    pub rss: u64,
    /// 累计 user+system 微秒。当前采样不读，保留与 w5a 的进程表同形。
    #[allow(dead_code)]
    pub cpu_us: u64,
    /// 启动时刻，epoch 秒（pbi_start_tvsec）。age = now - started。
    pub started: u64,
    /// 进程参数。macOS 走 KERN_PROCARGS2，Linux 走 `/proc/<pid>/cmdline`，
    /// Windows 由 `collect_procs` 一次性从 `Win32_Process` 全表取（见下方）。
    pub argv: Vec<String>,
}

/// 采一遍进程表。hostwork 的 sample_procs 每 10 秒采一次，这里按需采。
pub fn collect_procs() -> Vec<ProcRow> {
    let mut out = Vec::new();
    // Windows 的 argv 必须**一次拿全表**：几百个进程各起一次 PowerShell 慢到不可用。
    // 其它平台是逐 pid 的 /proc 或 sysctl，就地取即可。
    #[cfg(any(windows, feature = "sim_windows"))]
    let win_argv = windows_argv_table();
    let pids = platform::list_pids();
    for pid in pids {
        // proc_bsdinfo 的返回是 (pid, ppid, start_tvsec, name)：
        // 取 ppid 必须用 x.1 —— 以前这里误取了 x.0，所有进程的 ppid 都等于自己，
        // 「Ollama / 宿主服务拉子进程」那两段归属会全部认错。
        let (ppid, started, name) = match platform::proc_bsdinfo(pid) {
            Some(x) => (x.1, x.2, x.3),
            None => continue, // 进程刚好退出 / 无权查看：跳过这一行，不影响整表
        };
        let path = platform::proc_path(pid).unwrap_or_default();
        let (rss, cpu_us) = platform::proc_taskinfo(pid).unwrap_or((0, 0));
        // argv 拿不到（EPERM/ESRCH/平台没有实现）就是空 vec，不是错误。
        #[cfg(any(windows, feature = "sim_windows"))]
        let argv = win_argv.get(&pid).cloned().unwrap_or_default();
        #[cfg(not(any(windows, feature = "sim_windows")))]
        let argv = platform::proc_argv(pid);
        out.push(ProcRow { pid, ppid, name, path, rss, cpu_us, started, argv });
    }
    out
}

// ------------------------------------------------------------------ Windows 命令行全表

/// 一次拿全部进程的命令行。优先 PowerShell 的 `Get-CimInstance Win32_Process`
/// （Win11 已经删掉 wmic），失败再退 `wmic process get ProcessId,CommandLine /format:list`。
/// 返回 pid → argv；整表拿不到就是空 map（argv 缺失不影响其它字段）。
#[cfg(any(windows, feature = "sim_windows"))]
fn windows_argv_table() -> std::collections::HashMap<i32, Vec<String>> {
    // 先强制输出 UTF-8：PowerShell 默认按控制台代码页写管道，中文路径会变乱码。
    let ps = r#"[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-CimInstance Win32_Process | ForEach-Object { "$($_.ProcessId)`t$($_.CommandLine)" }"#;
    let (code, out, _) =
        platform::run_cmd_piped(&["powershell", "-NoProfile", "-NonInteractive", "-Command", ps], Duration::from_secs(10));
    if code == 0 {
        let t = parse_ps_process_table(&out);
        if !t.is_empty() {
            return t;
        }
    }
    let (code, out, _) = platform::run_cmd_piped(
        &["wmic", "process", "get", "ProcessId,CommandLine", "/format:list"],
        Duration::from_secs(10),
    );
    if code == 0 {
        return parse_wmic_process_table(&out);
    }
    std::collections::HashMap::new()
}

/// PowerShell 输出的一行是 `<pid>\t<commandline>`（脚本里用 `` `t `` 拼的）。
#[allow(dead_code)] // 只有 Windows 走它；单测在 macOS 上也跑，所以不加 cfg
fn parse_ps_process_table(text: &str) -> std::collections::HashMap<i32, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    for line in text.lines() {
        let Some((pid_s, cmd)) = line.split_once('\t') else {
            continue;
        };
        let Ok(pid) = pid_s.trim().parse::<i32>() else {
            continue;
        };
        let cmd = cmd.trim();
        if pid > 0 && !cmd.is_empty() {
            out.insert(pid, split_windows_cmdline(cmd));
        }
    }
    out
}

/// `wmic ... /format:list` 是多行块：`CommandLine=...` / `ProcessId=123` / 空行。
/// CommandLine 可能整行缺失（受保护进程），这种 pid 不塞空 vec。
#[allow(dead_code)]
fn parse_wmic_process_table(text: &str) -> std::collections::HashMap<i32, Vec<String>> {
    let mut out = std::collections::HashMap::new();
    let mut cmd: Option<String> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(v) = line.strip_prefix("CommandLine=") {
            cmd = Some(v.to_string());
        } else if let Some(v) = line.strip_prefix("ProcessId=") {
            if let (Ok(pid), Some(c)) = (v.trim().parse::<i32>(), cmd.take()) {
                if pid > 0 && !c.trim().is_empty() {
                    out.insert(pid, split_windows_cmdline(&c));
                }
            }
        } else if line.trim().is_empty() {
            cmd = None;
        }
    }
    out
}

/// 按 Win32 的引号规则把命令行切成 argv（够 `_take(argv, "--ctx")` 这类取参用）。
/// 反斜杠转义只处理「引号前」这一种最常见情形，与 CommandLineToArgvW 同向。
#[allow(dead_code)]
fn split_windows_cmdline(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut started = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_q = !in_q;
                started = true;
            }
            '\\' if matches!(chars.peek(), Some('"')) => {
                cur.push('"');
                chars.next();
                started = true;
            }
            c if c.is_whitespace() && !in_q => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

// ------------------------------------------------------------------ Windows 解析单测
//
// 解析器是纯文本函数，故意不加 `cfg(windows)`：这样普通 `cargo test`（macOS）
// 就能跑。真正调 Win32 的 `windows_argv_table` 才受 cfg 管。
#[cfg(test)]
mod tests {
    #[test]
    fn ps_table_splits_pid_and_commandline() {
        let text = "4242\t\"C:\\Program Files\\Ollama\\ollama.exe\" serve --ctx 8192\r\n\
                    777\tpython \"C:\\a b\\main.py\" -u\n\
                    \tno pid, skipped\n";
        let t = super::parse_ps_process_table(text);
        assert_eq!(t.len(), 2);
        assert_eq!(
            t[&4242],
            vec!["C:\\Program Files\\Ollama\\ollama.exe", "serve", "--ctx", "8192"]
        );
        // 引号里的空格不切
        assert_eq!(t[&777], vec!["python", "C:\\a b\\main.py", "-u"]);
    }

    #[test]
    fn wmic_list_blocks_pair_cmdline_with_pid() {
        let text = "\r\nCommandLine=llama-server.exe --model m.gguf --ctx 4096 \r\n\
                    ProcessId=1234\r\n\r\n\
                    CommandLine=\r\nProcessId=555\r\n\r\n\
                    ProcessId=999\r\n\r\n";
        let t = super::parse_wmic_process_table(text);
        assert_eq!(t.len(), 1, "空 CommandLine 的 pid 不该塞空 vec");
        assert_eq!(
            t[&1234],
            vec!["llama-server.exe", "--model", "m.gguf", "--ctx", "4096"]
        );
        assert!(!t.contains_key(&999), "没有 CommandLine 的块要丢掉");
    }

    #[test]
    fn windows_cmdline_respects_quotes_and_escapes() {
        assert_eq!(
            super::split_windows_cmdline(r#"a "b c" d"#),
            vec!["a", "b c", "d"]
        );
        assert_eq!(
            super::split_windows_cmdline(r#"x "C:\dir\quote\"name" y"#),
            vec!["x", "C:\\dir\\quote\"name", "y"]
        );
        assert_eq!(super::split_windows_cmdline("   "), Vec::<String>::new());
    }
}
