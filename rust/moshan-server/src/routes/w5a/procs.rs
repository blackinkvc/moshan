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
    let cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    let cores_v = if cores > 0 { Value::from(cores as i64) } else { Value::Null };
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
            let di = delta(t1.3, t2.3);
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

fn loadavg() -> Option<[f64; 3]> {
    let mut v = [0f64; 3];
    let n = unsafe { libc::getloadavg(v.as_mut_ptr(), 3) };
    if n == 3 {
        Some(v)
    } else {
        None
    }
}

fn boot_time() -> Option<f64> {
    #[cfg(target_os = "macos")]
    {
        #[repr(C)]
        struct TimeVal {
            tv_sec: i64,
            tv_usec: i32,
        }
        let mut tv = TimeVal { tv_sec: 0, tv_usec: 0 };
        let mut len = std::mem::size_of::<TimeVal>() as libc::size_t;
        let name = std::ffi::CString::new("kern.boottime").ok()?;
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut tv as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && tv.tv_sec > 0 {
            Some(tv.tv_sec as f64 + tv.tv_usec as f64 / 1_000_000.0)
        } else {
            None
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
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
        "os": if cfg!(target_os = "macos") { "mac" } else { "posix" },
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

// ------------------------------------------------------------------ 进程表（hostwork 用）

pub struct ProcRow {
    pub pid: i32,
    pub ppid: i32,
    pub name: String,
    pub path: String,
    pub rss: u64,
    pub cpu_us: u64,
    pub started: u64,
}

/// 采一遍进程表。hostwork 的 sample_procs 每 10 秒采一次，这里按需采。
pub fn collect_procs() -> Vec<ProcRow> {
    let mut out = Vec::new();
    let pids = platform::list_pids();
    for pid in pids {
        let (ppid, started, name) = match platform::proc_bsdinfo(pid) {
            Some(x) => (x.0, x.2, x.3),
            None => continue,
        };
        let path = platform::proc_path(pid).unwrap_or_default();
        let (rss, cpu_us) = platform::proc_taskinfo(pid).unwrap_or((0, 0));
        out.push(ProcRow { pid, ppid, name, path, rss, cpu_us, started });
    }
    out
}
