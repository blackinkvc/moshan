// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：使用报告的常驻调度（report_runner.py）—— 采样 + 出报告 + 归档。
// 不做：报告正文怎么排版在 w4c/report.rs（已迁），这里只按 Python 的节奏驱动它，
//       并把进度写进 crate::live::Report（/api/report/list 的 state 段）。
//
// 节奏与 Python 一致：
//   首采 20 秒（FIRST_DELAY），之后 120 秒（SAMPLE_EVERY）；
//   每 3600 秒（REPORT_EVERY）看一次「已结束周期有没有缺文件」，幂等补缺；
//   每月 1 号顺手裁一次快照（metrics.trim_snapshot）。

#![allow(dead_code)]

use super::util;
use crate::paths::Ctx;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SAMPLE_EVERY: u64 = 120;
const REPORT_EVERY: f64 = 3600.0;
const FIRST_DELAY: u64 = 20;
const FULL_EVERY: i64 = 5; // 每 5 次 light 采样做一次 full（10 分钟）
const KEEP_SNAPSHOT_DAYS: i64 = 7;

/// `metrics._root()`：优先取 inbox_dir 的父目录（存在时），否则资料库。
pub fn metrics_root(ctx: &Ctx) -> PathBuf {
    let inbox = ctx.inbox_dir();
    if let Some(parent) = inbox.parent() {
        if parent.exists() {
            return parent.to_path_buf();
        }
    }
    ctx.library_dir.clone()
}

fn sys_dir(ctx: &Ctx) -> PathBuf {
    crate::paths::rt_join(ctx, "10-系统记录")
}

fn snap_path(ctx: &Ctx) -> PathBuf {
    sys_dir(ctx).join("机器").join("快照.jsonl")
}

/// `metrics._procs(limit=5)`：ps 按 CPU 降序取前 40 行，同名合并。
/// rss 列是 KB；Python 是 `int(rss) * 1024 / 1048576` = KB/1024（MB）。
fn procs(limit: usize) -> Vec<Value> {
    let argv = vec![
        "/bin/ps".to_string(),
        "-A".into(),
        "-o".into(),
        "%cpu=,rss=,comm=".into(),
        "-r".into(),
    ];
    let Some((_, out, _)) = util::run_cmd(&argv, 6, None) else {
        return Vec::new();
    };
    let mut agg: Vec<(String, f64, f64)> = Vec::new();
    for line in out.lines().take(40) {
        let mut it = line.split_whitespace();
        let (Some(cpu_s), Some(rss_s)) = (it.next(), it.next()) else { continue };
        let comm = it.collect::<Vec<_>>().join(" ");
        let (Ok(cpu), Ok(rss_kb)) = (cpu_s.parse::<f64>(), rss_s.parse::<f64>()) else {
            continue;
        };
        let name = Path::new(&comm)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        match agg.iter_mut().find(|(n, _, _)| *n == name) {
            Some(e) => {
                e.1 = util::py_round(e.1 + cpu, 1);
                e.2 = util::py_round(e.2 + rss_kb / 1024.0, 1);
            }
            None => agg.push((
                name,
                util::py_round(cpu, 1),
                util::py_round(rss_kb / 1024.0, 1),
            )),
        }
    }
    agg.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    agg.into_iter()
        .take(limit)
        .map(|(n, c, m)| json!({"n": n, "c": c, "m": m}))
        .collect()
}

/// `metrics.sample(full)`：一条快照。键顺序与 Python 的 update 顺序一致。
fn sample(ctx: &Ctx, full: bool) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("at".into(), json!(util::local_fmt("%Y-%m-%d %H:%M:%S")));

    // _disk()：target = 10-系统记录（存在时）否则 HOME
    let sys = sys_dir(ctx);
    let target = if sys.exists() { sys } else { util::home() };
    let d = crate::hostwork::disk::metrics_disk(&target);
    for k in ["disk_pct", "free_gb", "total_gb"] {
        m.insert(k.into(), d.get(k).cloned().unwrap_or(json!(0.0)));
    }

    // _mem()
    let mem = crate::hostwork::machine::mem();
    for k in ["mem_pct", "mem_gb", "mem_total_gb"] {
        m.insert(k.into(), mem.get(k).cloned().unwrap_or(json!(0.0)));
    }

    let load = crate::hostwork::machine::load();
    let lv = |i: usize| load.get(i).cloned().unwrap_or(json!(0.0));
    m.insert("load1".into(), lv(0));
    m.insert("load5".into(), lv(1));
    m.insert("load15".into(), lv(2));
    m.insert("cpus".into(), json!(crate::hostwork::procs::sample_cpus()));
    m.insert("uptime_h".into(), json!(crate::hostwork::machine::uptime_h()));

    // CPU 占用：资源卡的走势图吃这个字段（拿不到就不写）。
    let cpu = crate::hostwork::procs::cpu();
    if let Some(pct) = cpu.get("pct").and_then(|v| v.as_f64()) {
        m.insert("cpu_pct".into(), json!(util::py_round(pct, 1)));
    }

    if full {
        let p = procs(5);
        if !p.is_empty() {
            m.insert("procs".into(), Value::Array(p));
        }
    }
    Value::Object(m)
}

/// `metrics.trim_snapshot(days)`：滚动删掉 N 天前的快照行。
fn trim_snapshot(ctx: &Ctx, days: i64) {
    let p = snap_path(ctx);
    let Ok(text) = std::fs::read_to_string(&p) else { return };
    let cut = (chrono::Local::now() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let keep: Vec<&str> = text
        .lines()
        .filter(|ln| {
            let day = line_day(ln);
            day.is_empty() || day.as_str() >= cut.as_str()
        })
        .collect();
    let body = if keep.is_empty() { String::new() } else { format!("{}\n", keep.join("\n")) };
    let _ = crate::jsonio::atomic_write(&p, &body);
}

/// `metrics._line_day(ln)`：从 `{"at": "..."}` 里抠前 10 个字符。
fn line_day(ln: &str) -> String {
    let Some(i) = ln.find("\"at\"") else { return String::new() };
    let after = i + 4;
    let Some(j) = ln[after..].find('"') else { return String::new() };
    ln.chars().skip(after + j + 1).take(10).collect()
}

fn append_sample(ctx: &Ctx, full: bool) -> Value {
    // 采样要遍历目录并追加写；媒体根读不动时整块跳过，别把线程吊在 open() 上。
    if !crate::paths::library_ready() {
        return json!({"skipped": "library_blocked"});
    }
    let rec = sample(ctx, full);
    let p = snap_path(ctx);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{}", util::py_json_compact(&rec));
    }
    rec
}

/// `report_runner.tick()`：采样一次 + 到点出报告。
fn tick(ctx: &Ctx) {
    let now = util::now_f();
    let full = {
        // tick 计数与 Python 的 _TICK 同语义：第一轮就带进程（1 % 5 == 1）。
        let n = crate::live::report_get().samples + 1;
        n % FULL_EVERY == 1
    };

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| append_sample(ctx, full))) {
        Ok(rec) => {
            let at = rec.get("at").and_then(|v| v.as_str()).unwrap_or("").to_string();
            crate::live::report_update(|st| {
                st.samples += 1;
                st.last_sample = at;
            });
        }
        Err(_) => {
            let msg = "sample: 采集线程异常".to_string();
            crate::live::report_update(|st| st.error = msg.clone());
            crate::storage::log(ctx, json!({"event": "report_sample_error", "error": msg}));
        }
    }

    let st = crate::live::report_get();
    // 日报要读媒体根上的目录：读不到就跳过这一轮，别把线程卡在 open() 里。
    // 采样（append_sample）写的是运行数据根，不受这道闸影响。
    if now - st.last_run >= REPORT_EVERY && crate::paths::library_ready() {
        #[cfg(feature = "lane_w4c")]
        {
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::routes::w4c::report::run(ctx, false)
            }));
            match res {
                Ok(res) => {
                    let made = res.get("made").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                    let removed = res.get("removed").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                    crate::live::report_update(|s| {
                        s.last_run = now;
                        s.runs += 1;
                        s.last_made = made.clone();
                        s.last_removed = removed.clone();
                    });
                    if !made.is_empty() || !removed.is_empty() {
                        let labels: Vec<String> = made
                            .iter()
                            .map(|m| {
                                format!(
                                    "{}:{}",
                                    m.get("kind").and_then(|v| v.as_str()).unwrap_or(""),
                                    m.get("key").and_then(|v| v.as_str()).unwrap_or("")
                                )
                            })
                            .collect();
                        crate::storage::log(
                            ctx,
                            json!({"event": "report_done", "made": labels, "removed": removed}),
                        );
                    }
                }
                Err(_) => {
                    crate::live::report_update(|s| s.error = "report: 生成线程异常".into());
                    crate::storage::log(ctx, json!({"event": "report_error", "error": "生成线程异常"}));
                }
            }
        }
        #[cfg(not(feature = "lane_w4c"))]
        {
            // 只带 lane_w5d 编译时没有报告生成器：不假装跑过，把原因留在 error 里。
            crate::live::report_update(|s| s.error = "report: 未编译 lane_w4c".into());
        }

        // 每月 1 号裁一次快照（Python 的判断是 date.today().day == 1）。
        use chrono::Datelike;
        if chrono::Local::now().day() == 1 {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                trim_snapshot(ctx, KEEP_SNAPSHOT_DAYS)
            }));
        }
    }
}

/// 后台循环。Python 先睡 FIRST_DELAY 再开始，之后每 SAMPLE_EVERY 一轮。
pub fn start(ctx: Arc<Ctx>) {
    // `report_runner.start()`：`if STATE["started"]: return` —— 起过一次就不再起，
    // 否则会有两条采样线程同时写快照。
    if crate::live::report_get().started != 0.0 {
        return;
    }
    crate::live::report_update(|st| st.started = util::now_f());
    std::thread::Builder::new()
        .name("moshan-report".into())
        .spawn(move || {
            std::thread::sleep(Duration::from_secs(FIRST_DELAY));
            // `_loop()` 睡醒后再置一次：对外看到的 started 是「第一轮采样开始的时刻」，
            // 不是进程启动时刻（上面 start() 那次只用于「线程已启动」的判断）。
            let woke = util::now_f();
            crate::live::report_update(|st| st.started = woke);
            loop {
                let t0 = Instant::now();
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tick(&ctx)));
                if r.is_err() {
                    eprintln!("[moshan-rs] report tick 异常，已跳过（下一轮继续）");
                }
                let secs = t0.elapsed().as_secs_f64();
                if secs > SAMPLE_EVERY as f64 {
                    eprintln!(
                        "[moshan-rs] report 单轮耗时 {:.1}s，超过采样间隔 {}s；本轮结束，跳过剩余等待",
                        secs, SAMPLE_EVERY
                    );
                }
                std::thread::sleep(Duration::from_secs(SAMPLE_EVERY));
            }
        })
        .ok();
}
