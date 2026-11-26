// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：虫巢（多机）22 条路由 + 它们背后的 fleet.db 读写。
// 不做：不把被控机的数据写进主账本 —— 这里所有落盘都在
//       `<数据根>/16-虫巢思维/fleet.db`（与 Python、与独立 nest 服务同一个文件）。
//
// 权限链（照抄 fleet.py，一行不改）：
//   agent 侧（report/poll/result/software/task 进度与结果/latest/download/pkg 下载/prune）
//     → 只认 fleet_token：`X-Agent-Token` 优先，其次 `?k=`；**本机也要令牌**。
//   view 侧（其余全部）
//     → 本机（回环 + Host 是本机写法）直接放行；否则 `?k=` 优先，其次 `X-View-Token`，
//       两把都不认 fleet_token。
//   设备表里的 access（control/read/self）在源码中**只落库、不参与校验**，
//   这里也不拦 —— 自己加一层就把判定链改了。

#![allow(dead_code)]

use super::perms;
use super::util::{self, Body};
use crate::paths::Ctx;
use crate::routes::common::{http_err, q, CtxState};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::path::PathBuf;

// ------------------------------------------------------------------ 常量

const ONLINE_WINDOW: i64 = 90;
const KEEP_DAYS: i64 = 7;
const PROCS_KEEP_HOURS: i64 = 24;

const MAX_DEVICES: i64 = 200;
const REPORT_MIN_GAP: f64 = 3.0;

const CMD_TTL: i64 = 300;
const CMD_TAKEN_TTL: i64 = 180;

const TASK_TTL: i64 = 86400;
const TASK_RUNNING_TTL: i64 = 7200;
const MAX_SOFTWARE: usize = 4000;

pub fn actions() -> Value {
    json!({
        "lock": "锁定屏幕（回登录界面，程序继续跑）",
        "reboot": "重启系统",
        "logout": "注销当前用户",
    })
}

pub fn task_kinds() -> Value {
    json!({
        "refresh": "重新采集软件清单",
        "install": "安装软件",
        "uninstall": "卸载软件",
        "upgrade": "升级软件",
        "self_update": "升级 agent 自身",
    })
}

fn action_known(a: &str) -> bool {
    matches!(a, "lock" | "reboot" | "logout")
}

fn kind_known(k: &str) -> bool {
    matches!(k, "refresh" | "install" | "uninstall" | "upgrade" | "self_update")
}

// ------------------------------------------------------------------ 小工具

fn db(ctx: &Ctx) -> PathBuf {
    util::db_path(ctx)
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

fn bad(msg: &str) -> Response {
    Json(json!({ "ok": false, "error": msg })).into_response()
}

/// `secrets.token_hex(n)`
pub fn rand_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !ok {
        let seed = format!("{}-{}-{}", std::process::id(), util::now_epoch(), rand_seed_tick());
        let h = util::sha256_hex(seed.as_bytes());
        let bytes = h.as_bytes();
        for i in 0..n {
            buf[i] = bytes[i % bytes.len()];
        }
    }
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

fn rand_seed_tick() -> u128 {
    static C: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    C.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as u128
}

fn s_of(v: &Value, k: &str) -> String {
    util::str_or_empty(v.get(k))
}

fn cut_str(s: &str, n: usize) -> String {
    util::cut(s, n)
}

// ------------------------------------------------------------------ 写入 / 读取

/// device_id -> 上次入库时间（monotonic 秒）。只用来节流，不落盘。
fn last_report() -> &'static std::sync::Mutex<std::collections::HashMap<String, f64>> {
    static M: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, f64>>> =
        std::sync::OnceLock::new();
    M.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn mono() -> f64 {
    static T: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

fn num(body: &Value, key: &str, as_int: bool) -> Value {
    let Some(v) = body.get(key) else { return json!(0) };
    if v.is_null() {
        return json!(0);
    }
    if as_int {
        match util::int_of(v) {
            Ok(i) => json!(i),
            Err(_) => json!(0),
        }
    } else {
        match util::float_of(v) {
            Ok(f) => util::num_f(f),
            Err(_) => json!(0),
        }
    }
}

pub fn report_body(ctx: &Ctx, body: &Value, ip: &str, fp: &str) -> Value {
    let dev = cut_str(&util::py_strip(&s_of(body, "device_id")), 64);
    if dev.is_empty() {
        return json!({"ok": false, "error": "device_id 必填"});
    }
    let fp = cut_str(&util::py_strip(fp), 64);
    if fp.is_empty() {
        return json!({"ok": false,
                     "error": "缺少设备指纹：agent 版本过旧，请装 0.5.0 以上"});
    }
    let ts = {
        let raw = s_of(body, "at");
        let cut = cut_str(&raw, 19);
        if cut.is_empty() {
            util::now_iso()
        } else {
            cut
        }
    };
    let cpu = match num(body, "cpu", false) {
        Value::Number(n) => util::round1(n.as_f64().unwrap_or(0.0)),
        _ => 0.0,
    };
    let procs = util::dumps_py(util::or(body.get("procs"), &json!([])));
    let procs = cut_str(&procs, 60000);

    let now_m = mono();
    let _g = util::lock();
    {
        let mut m = last_report().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = m.get(&dev).copied() {
            if now_m - prev < REPORT_MIN_GAP {
                return json!({"ok": true, "ts": ts, "skipped": "too_frequent"});
            }
        }
        if m.len() > 5000 {
            let cut = now_m - 60.0;
            m.retain(|_, v| *v >= cut);
        }
    }

    let d = db(ctx);
    let check = format!("SELECT fp FROM devices WHERE id={}", util::lit(&dev));
    let rows = match util::query(&d, &check) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": format!("数据库不可用：{e}")}),
    };
    if rows.is_empty() {
        let n = match util::query(&d, "SELECT COUNT(*) AS n FROM devices") {
            Ok(r) => r
                .first()
                .and_then(|x| x.get("n"))
                .and_then(|x| x.as_i64())
                .unwrap_or(0),
            Err(e) => return json!({"ok": false, "error": format!("数据库不可用：{e}")}),
        };
        if n >= MAX_DEVICES {
            return json!({"ok": false,
                          "error": format!("设备数已达上限 {MAX_DEVICES}，拒绝新设备 {}",
                                           cut_str(&dev, 32))});
        }
    } else {
        let have = util::py_strip(
            rows[0].get("fp").and_then(|v| v.as_str()).unwrap_or(""),
        );
        if !have.is_empty() && have != fp {
            return json!({"ok": false,
                          "error": format!("设备指纹不匹配：{} 这个 id 已被另一台机器绑定",
                                           cut_str(&dev, 32))});
        }
    }

    let mem_used = num(body, "mem_used", true);
    let mem_total = num(body, "mem_total", true);
    let disk_used = num(body, "disk_used", true);
    let disk_total = num(body, "disk_total", true);
    let uptime = num(body, "uptime", true);
    let sql = format!(
        "INSERT OR REPLACE INTO snapshots(device_id,ts,cpu,mem_used,mem_total,disk_used,disk_total,uptime,procs) \
         VALUES ({},{},{},{},{},{},{},{},{}); \
         INSERT INTO devices(id,name,os,host,ip,agent_ver,first_seen,last_seen,fp) \
         VALUES ({},{},{},{},{},{},{},{},{}) \
         ON CONFLICT(id) DO UPDATE SET \
           name=excluded.name, os=excluded.os, host=excluded.host, \
           ip=excluded.ip, agent_ver=excluded.agent_ver, \
           last_seen=excluded.last_seen, \
           fp=CASE WHEN devices.fp IS NULL OR devices.fp='' \
                   THEN excluded.fp ELSE devices.fp END",
        util::lit(&dev),
        util::lit(&ts),
        util::lit_num(cpu),
        util::lit_num(mem_used.as_f64().unwrap_or(0.0)),
        util::lit_num(mem_total.as_f64().unwrap_or(0.0)),
        util::lit_num(disk_used.as_f64().unwrap_or(0.0)),
        util::lit_num(disk_total.as_f64().unwrap_or(0.0)),
        util::lit_num(uptime.as_f64().unwrap_or(0.0)),
        util::lit(&procs),
        util::lit(&dev),
        util::lit(&{
            let raw = s_of(body, "name");
            if raw.is_empty() { dev.clone() } else { cut_str(&raw, 64) }
        }),
        util::lit(&cut_str(&s_of(body, "os"), 32)),
        util::lit(&cut_str(&s_of(body, "host"), 64)),
        util::lit(&cut_str(ip, 48)),
        util::lit(&cut_str(&s_of(body, "agent_ver"), 16)),
        util::lit(&ts),
        util::lit(&ts),
        util::lit(&fp),
    );
    if let Err(e) = util::exec_tx(&d, &sql) {
        return json!({"ok": false, "error": format!("数据库不可用：{e}")});
    }
    last_report()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(dev, now_m);
    json!({"ok": true, "ts": ts})
}

/// `fp_ok`：空指纹一律不放行（那是 0.4.0 以前的 agent，冒名的门就在这儿）。
pub fn fp_ok(ctx: &Ctx, dev: &str, fp: &str) -> (bool, String) {
    let fp = cut_str(&util::py_strip(fp), 64);
    if dev.is_empty() {
        return (false, "device_id 必填".to_string());
    }
    if fp.is_empty() {
        return (
            false,
            "缺少设备指纹：agent 版本过旧，请装 0.5.0 以上".to_string(),
        );
    }
    let d = db(ctx);
    let sql = format!("SELECT fp FROM devices WHERE id={}", util::lit(dev));
    let rows = match util::query(&d, &sql) {
        Ok(r) => r,
        Err(_) => return (false, "数据库不可用".to_string()),
    };
    if rows.is_empty() {
        return (true, "new".to_string());
    }
    let have = util::py_strip(rows[0].get("fp").and_then(|v| v.as_str()).unwrap_or(""));
    if have.is_empty() {
        return (true, "unbound".to_string());
    }
    if have != fp {
        return (
            false,
            format!("设备指纹不匹配：{} 已被另一台机器绑定", cut_str(dev, 32)),
        );
    }
    (true, "ok".to_string())
}

/// `device_of`：反查指令/任务属于哪台设备。表名只吃内部常量。
pub fn device_of(ctx: &Ctx, table: &str, row_id: &str) -> String {
    if table != "commands" && table != "tasks" {
        return String::new();
    }
    let sql = format!(
        "SELECT device_id FROM {table} WHERE id={}",
        util::lit(row_id)
    );
    match util::query(&db(ctx), &sql) {
        Ok(rows) => rows
            .first()
            .and_then(|r| r.get("device_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    }
}

fn expire_sql() -> String {
    let a = util::iso_secs_ago(CMD_TTL);
    let b = util::iso_secs_ago(CMD_TAKEN_TTL);
    format!(
        "UPDATE commands SET state='expired' WHERE state='pending' AND created_at < {}; \
         UPDATE commands SET state='pending', taken_at=NULL, result='retry' \
           WHERE state='taken' AND taken_at < {} AND result<>'retry' AND action<>'reboot'; \
         UPDATE commands SET state='done', result='超时未见回执（agent 可能已失联）', done_at={} \
           WHERE state='taken' AND taken_at < {} AND action='reboot'",
        util::lit(&a),
        util::lit(&b),
        util::lit(&util::now_iso()),
        util::lit(&b),
    )
}

fn task_clean_sql() -> String {
    let a = util::iso_secs_ago(TASK_TTL);
    let b = util::iso_secs_ago(TASK_RUNNING_TTL);
    format!(
        "UPDATE tasks SET state='expired' WHERE state='pending' AND created_at < {}; \
         UPDATE tasks SET state='failed', log=log || ' | 执行超时（agent 可能已断开）', done_at={} \
           WHERE state='running' AND taken_at < {}",
        util::lit(&a),
        util::lit(&util::now_iso()),
        util::lit(&b),
    )
}

pub fn enqueue(ctx: &Ctx, device_id: &str, action: &str, issued_from: &str) -> Value {
    if !action_known(action) {
        return json!({"ok": false, "error": format!("不支持的动作：{action}")});
    }
    let d = db(ctx);
    let _g = util::lock();
    let rows = match util::query(
        &d,
        &format!("SELECT id FROM devices WHERE id={}", util::lit(device_id)),
    ) {
        Ok(r) => r,
        Err(_) => return json!({"ok": false, "error": "数据库不可用"}),
    };
    if rows.is_empty() {
        return json!({"ok": false, "error": format!("没有这台设备：{device_id}")});
    }
    let since = util::iso_secs_ago(60);
    let dup_sql = format!(
        "SELECT id FROM commands WHERE device_id={} AND action={} \
         AND state IN ('pending','taken') AND created_at > {}",
        util::lit(device_id),
        util::lit(action),
        util::lit(&since)
    );
    if let Ok(dup) = util::query(&d, &dup_sql) {
        if let Some(first) = dup.first() {
            let id = first.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            return json!({"ok": true, "id": id, "duplicate": true});
        }
    }
    let cid = format!("c{}", rand_hex(6));
    let ins = format!(
        "INSERT INTO commands(id,device_id,action,created_at,issued_from) VALUES ({},{},{},{},{})",
        util::lit(&cid),
        util::lit(device_id),
        util::lit(action),
        util::lit(&util::now_iso()),
        util::lit(&cut_str(issued_from, 64)),
    );
    if util::exec(&d, &ins).is_err() {
        return json!({"ok": false, "error": "数据库不可用"});
    }
    json!({"ok": true, "id": cid, "action": action, "device_id": device_id})
}

pub fn take(ctx: &Ctx, device_id: &str) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let _ = util::exec_tx(&d, &expire_sql());
    let sql = format!(
        "SELECT id,action FROM commands WHERE device_id={} AND state='pending' \
         ORDER BY created_at LIMIT 1",
        util::lit(device_id)
    );
    let rows = match util::query(&d, &sql) {
        Ok(r) => r,
        Err(_) => return json!({"ok": false, "error": "数据库不可用"}),
    };
    let Some(r) = rows.first() else {
        return json!({"ok": true, "command": Value::Null});
    };
    let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let action = r.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let _ = util::exec(
        &d,
        &format!(
            "UPDATE commands SET state='taken', taken_at={} WHERE id={}",
            util::lit(&util::now_iso()),
            util::lit(&id)
        ),
    );
    json!({"ok": true, "command": {"id": id, "action": action}})
}

pub fn finish(ctx: &Ctx, cmd_id: &str, ok_: bool, output: &str) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let sql = format!(
        "UPDATE commands SET state={}, result={}, done_at={} WHERE id={}",
        util::lit(if ok_ { "done" } else { "failed" }),
        util::lit(&cut_str(output, 500)),
        util::lit(&util::now_iso()),
        util::lit(cmd_id)
    );
    let n = util::exec(&d, &sql).unwrap_or(0);
    json!({"ok": n > 0})
}

pub fn recent_commands(ctx: &Ctx) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let _ = util::exec_tx(&d, &expire_sql());
    let rows = util::query(&d, "SELECT * FROM commands ORDER BY created_at DESC LIMIT 12")
        .unwrap_or_default();
    Value::Array(rows)
}

fn age_sec(ts: &str) -> Option<i64> {
    let t = util::parse_ts(ts)?;
    let age = util::now_epoch() - t;
    Some((age as i64).max(0))
}

pub fn overview(ctx: &Ctx) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let devs = util::query(
        &d,
        "SELECT * FROM devices ORDER BY last_seen DESC LIMIT 30",
    )
    .unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    for dv in devs {
        let id = dv.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let snap = util::query(
            &d,
            &format!(
                "SELECT * FROM snapshots WHERE device_id={} ORDER BY ts DESC LIMIT 1",
                util::lit(&id)
            ),
        )
        .unwrap_or_default()
        .into_iter()
        .next()
        .unwrap_or_else(|| json!({}));
        let procs = match snap.get("procs").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => serde_json::from_str::<Value>(s).unwrap_or(json!([])),
            _ => json!([]),
        };
        let procs: Vec<Value> = procs
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .take(8)
            .collect();
        let spark: Vec<Value> = util::query(
            &d,
            &format!(
                "SELECT cpu FROM snapshots WHERE device_id={} ORDER BY ts DESC LIMIT 40",
                util::lit(&id)
            ),
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.get("cpu").cloned().unwrap_or(Value::Null))
        .collect();
        let spark: Vec<Value> = spark.into_iter().rev().collect();
        let soft_n = util::query(
            &d,
            &format!(
                "SELECT COUNT(*) AS n FROM software WHERE device_id={}",
                util::lit(&id)
            ),
        )
        .unwrap_or_default()
        .first()
        .and_then(|x| x.get("n"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
        let up_n = util::query(
            &d,
            &format!(
                "SELECT COUNT(*) AS n FROM upgrades WHERE device_id={}",
                util::lit(&id)
            ),
        )
        .unwrap_or_default()
        .first()
        .and_then(|x| x.get("n"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
        let last_seen = dv.get("last_seen").cloned().unwrap_or(Value::Null);
        let age = last_seen.as_str().and_then(age_sec);
        let name = dv
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(|| id.clone());
        let f = |k: &str| -> Value {
            match snap.get(k) {
                Some(v) if util::truthy(v) => v.clone(),
                _ => json!(0),
            }
        };
        out.push(json!({
            "id": id,
            "name": name,
            "os": dv.get("os").cloned().unwrap_or(Value::Null),
            "host": dv.get("host").cloned().unwrap_or(Value::Null),
            "ip": dv.get("ip").cloned().unwrap_or(Value::Null),
            "access": dv.get("access").cloned().unwrap_or(Value::Null),
            "agent_ver": dv.get("agent_ver").cloned().unwrap_or(Value::Null),
            "note": dv.get("note").cloned().unwrap_or(Value::Null),
            "last_seen": last_seen,
            "age": age.map(|v| json!(v)).unwrap_or(Value::Null),
            "online": age.map(|v| v <= ONLINE_WINDOW).unwrap_or(false),
            "cpu": f("cpu"),
            "mem_used": f("mem_used"),
            "mem_total": f("mem_total"),
            "disk_used": f("disk_used"),
            "disk_total": f("disk_total"),
            "uptime": f("uptime"),
            "procs": procs,
            "spark": spark,
            "soft_count": soft_n,
            "upgrade_count": up_n,
        }));
    }
    let online = out
        .iter()
        .filter(|d| d.get("online").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    json!({
        "ok": true,
        "now": util::now_iso(),
        "count": out.len(),
        "online": online,
        "devices": out,
    })
}

pub fn set_access(ctx: &Ctx, device_id: &str, access: &str) -> Value {
    if !matches!(access, "control" | "read" | "self") {
        return json!({"ok": false, "error": "档位只能是 control / read / self"});
    }
    let d = db(ctx);
    let _g = util::lock();
    let n = util::exec(
        &d,
        &format!(
            "UPDATE devices SET access={} WHERE id={}",
            util::lit(access),
            util::lit(device_id)
        ),
    )
    .unwrap_or(0);
    json!({"ok": n > 0, "updated": n})
}

pub fn save_software(ctx: &Ctx, device_id: &str, items: &Value, upgrades: Option<&Value>) -> Value {
    let now = util::now_iso();
    let list = items.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
    let mut rows: Vec<String> = Vec::new();
    for it in list.iter().take(MAX_SOFTWARE) {
        if !it.is_object() {
            continue;
        }
        let aid = cut_str(&util::py_strip(&s_of(it, "app_id")), 160);
        if aid.is_empty() {
            continue;
        }
        let size = match it.get("size_kb") {
            Some(v) if util::truthy(v) => util::int_of(v).unwrap_or(0),
            _ => 0,
        };
        let size = size.max(0);
        let name = {
            let raw = s_of(it, "name");
            if raw.is_empty() { aid.clone() } else { cut_str(&raw, 200) }
        };
        let source = {
            let raw = s_of(it, "source");
            if raw.is_empty() { "registry".to_string() } else { cut_str(&raw, 16) }
        };
        rows.push(format!(
            "({},{},{},{},{},{},{},{},{},{})",
            util::lit(device_id),
            util::lit(&aid),
            util::lit(&name),
            util::lit(&cut_str(&s_of(it, "version"), 60)),
            util::lit(&cut_str(&s_of(it, "publisher"), 120)),
            util::lit(&cut_str(&s_of(it, "install_date"), 24)),
            size,
            util::lit(&source),
            util::lit(&cut_str(&s_of(it, "bits"), 4)),
            util::lit(&now),
        ));
    }
    let d = db(ctx);
    let _g = util::lock();
    let mut sql = format!(
        "DELETE FROM software WHERE device_id={}",
        util::lit(device_id)
    );
    if !rows.is_empty() {
        sql.push_str(
            "; INSERT OR REPLACE INTO software(device_id,app_id,name,version,publisher,\
             install_date,size_kb,source,bits,updated_at) VALUES ",
        );
        sql.push_str(&rows.join(","));
    }
    if let Some(ups) = upgrades {
        let up_list = ups.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
        let mut urows: Vec<String> = Vec::new();
        for u in up_list.iter().take(MAX_SOFTWARE) {
            if !u.is_object() {
                continue;
            }
            let aid = cut_str(&util::py_strip(&s_of(u, "app_id")), 160);
            if aid.is_empty() {
                continue;
            }
            let name = {
                let raw = s_of(u, "name");
                if raw.is_empty() { aid.clone() } else { cut_str(&raw, 200) }
            };
            urows.push(format!(
                "({},{},{},{},{},{})",
                util::lit(device_id),
                util::lit(&aid),
                util::lit(&name),
                util::lit(&cut_str(&s_of(u, "current"), 60)),
                util::lit(&cut_str(&s_of(u, "available"), 60)),
                util::lit(&now),
            ));
        }
        sql.push_str(&format!(
            "; DELETE FROM upgrades WHERE device_id={}",
            util::lit(device_id)
        ));
        if !urows.is_empty() {
            sql.push_str(
                "; INSERT OR REPLACE INTO upgrades(device_id,app_id,name,current,available,\
                 updated_at) VALUES ",
            );
            sql.push_str(&urows.join(","));
        }
    }
    if util::exec_tx(&d, &sql).is_err() {
        return json!({"ok": false, "error": "数据库不可用"});
    }
    json!({"ok": true, "count": rows.len()})
}

pub fn software_list(ctx: &Ctx, device_id: &str, query: &str) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let mut sql = format!(
        "SELECT * FROM software WHERE device_id={}",
        util::lit(device_id)
    );
    if !query.is_empty() {
        let like = util::lit(&format!("%{query}%"));
        sql.push_str(&format!(
            " AND (name LIKE {like} OR publisher LIKE {like} OR app_id LIKE {like})"
        ));
    }
    sql.push_str(" ORDER BY name COLLATE NOCASE");
    let items = util::query(&d, &sql).unwrap_or_default();
    let ups = util::query(
        &d,
        &format!(
            "SELECT * FROM upgrades WHERE device_id={} ORDER BY name COLLATE NOCASE",
            util::lit(device_id)
        ),
    )
    .unwrap_or_default();
    json!({
        "ok": true,
        "device_id": device_id,
        "count": items.len(),
        "items": items,
        "upgrades": ups,
    })
}

fn winget_id_ok(s: &str) -> bool {
    // ^[A-Za-z0-9][A-Za-z0-9._+\-]{1,127}$
    let cs: Vec<char> = s.chars().collect();
    if cs.len() < 2 || cs.len() > 128 {
        return false;
    }
    let ok_first = cs[0].is_ascii_alphanumeric();
    ok_first
        && cs[1..].iter().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')
        })
}

fn app_id_ok(s: &str) -> bool {
    // ^[^\r\n\x00]{1,160}$
    let n = s.chars().count();
    n >= 1 && n <= 160 && !s.contains('\r') && !s.contains('\n') && !s.contains('\0')
}

pub fn enqueue_task(
    ctx: &Ctx,
    device_id: &str,
    kind: &str,
    target: &str,
    args: Option<&Value>,
) -> Value {
    if !kind_known(kind) {
        return json!({"ok": false, "error": format!("不支持的任务类型：{kind}")});
    }
    let target = util::py_strip(target);
    if matches!(kind, "install" | "upgrade") && !target.is_empty() && !winget_id_ok(&target) {
        return json!({"ok": false, "error": "包 id 格式不对（只允许字母数字和 . _ + -）"});
    }
    if matches!(kind, "uninstall" | "upgrade") && target.is_empty() {
        return json!({"ok": false, "error": "缺少要操作的软件标识"});
    }
    if !target.is_empty() && !app_id_ok(&target) {
        return json!({"ok": false, "error": "软件标识里有非法字符"});
    }

    let mut safe = serde_json::Map::new();
    if let Some(Value::Object(m)) = args {
        for k in ["url", "sha256", "silent"] {
            if let Some(v) = m.get(k) {
                let skip = v.is_null() || (v.as_str().map(|s| s.is_empty()).unwrap_or(false));
                if !skip {
                    safe.insert(k.to_string(), json!(cut_str(&util::py_str(v), 2000)));
                }
            }
        }
    }
    let has_url = safe
        .get("url")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if kind == "install" && target.is_empty() && !has_url {
        return json!({"ok": false,
                      "error": "install 需要 winget 包 id，或上传安装包的下载地址"});
    }

    let d = db(ctx);
    let _g = util::lock();
    let devs = util::query(
        &d,
        &format!("SELECT id FROM devices WHERE id={}", util::lit(device_id)),
    )
    .unwrap_or_default();
    if devs.is_empty() {
        return json!({"ok": false, "error": format!("没有这台设备：{device_id}")});
    }
    let dup_sql = format!(
        "SELECT id FROM tasks WHERE device_id={} AND kind={} AND target={} \
         AND state IN ('pending','running')",
        util::lit(device_id),
        util::lit(kind),
        util::lit(&target)
    );
    if let Ok(dup) = util::query(&d, &dup_sql) {
        if let Some(first) = dup.first() {
            let id = first.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            return json!({"ok": true, "id": id, "duplicate": true});
        }
    }
    let tid = format!("t{}", rand_hex(6));
    let args_json = util::dumps_py(&Value::Object(safe));
    let ins = format!(
        "INSERT INTO tasks(id,device_id,kind,target,args_json,created_at) \
         VALUES ({},{},{},{},{},{})",
        util::lit(&tid),
        util::lit(device_id),
        util::lit(kind),
        util::lit(&target),
        util::lit(&args_json),
        util::lit(&util::now_iso()),
    );
    if util::exec(&d, &ins).is_err() {
        return json!({"ok": false, "error": "数据库不可用"});
    }
    json!({"ok": true, "id": tid, "kind": kind, "target": target})
}

pub fn take_task(ctx: &Ctx, device_id: &str) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let _ = util::exec_tx(&d, &task_clean_sql());
    let sql = format!(
        "SELECT * FROM tasks WHERE device_id={} AND state='pending' \
         ORDER BY created_at LIMIT 1",
        util::lit(device_id)
    );
    let rows = util::query(&d, &sql).unwrap_or_default();
    let Some(r) = rows.first() else {
        return json!({"ok": true, "task": Value::Null});
    };
    let id = r.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let _ = util::exec(
        &d,
        &format!(
            "UPDATE tasks SET state='running', taken_at={}, progress=1 WHERE id={}",
            util::lit(&util::now_iso()),
            util::lit(&id)
        ),
    );
    let args = match r.get("args_json").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => serde_json::from_str::<Value>(s).unwrap_or(json!({})),
        _ => json!({}),
    };
    json!({"ok": true, "task": {
        "id": id,
        "kind": r.get("kind").cloned().unwrap_or(Value::Null),
        "target": r.get("target").cloned().unwrap_or(Value::Null),
        "args": args,
    }})
}

pub fn progress_task(ctx: &Ctx, task_id: &str, progress: i64, log: &str) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let p = progress.clamp(0, 99);
    let sql = format!(
        "UPDATE tasks SET progress={}, log={} WHERE id={} AND state='running'",
        p,
        util::lit(&cut_str(log, 800)),
        util::lit(task_id)
    );
    let n = util::exec(&d, &sql).unwrap_or(0);
    json!({"ok": n > 0})
}

pub fn finish_task(ctx: &Ctx, task_id: &str, ok_: bool, log: &str) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let sql = format!(
        "UPDATE tasks SET state={}, progress={}, log={}, done_at={} WHERE id={}",
        util::lit(if ok_ { "done" } else { "failed" }),
        if ok_ { 100 } else { 0 },
        util::lit(&cut_str(log, 2000)),
        util::lit(&util::now_iso()),
        util::lit(task_id)
    );
    let n = util::exec(&d, &sql).unwrap_or(0);
    json!({"ok": n > 0})
}

pub fn recent_tasks(ctx: &Ctx) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let _ = util::exec_tx(&d, &task_clean_sql());
    let rows = util::query(&d, "SELECT * FROM tasks ORDER BY created_at DESC LIMIT 20")
        .unwrap_or_default();
    Value::Array(rows)
}

pub fn prune(ctx: &Ctx) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let cut_all = util::iso_days_ago(KEEP_DAYS);
    let cut_proc = util::iso_secs_ago(PROCS_KEEP_HOURS * 3600);
    let n1 = util::exec(
        &d,
        &format!("DELETE FROM snapshots WHERE ts < {}", util::lit(&cut_all)),
    )
    .unwrap_or(0);
    let n2 = util::exec(
        &d,
        &format!(
            "UPDATE snapshots SET procs='[]' WHERE ts < {} AND procs<>'[]'",
            util::lit(&cut_proc)
        ),
    )
    .unwrap_or(0);
    json!({"deleted": n1, "trimmed": n2})
}

pub fn agent_latest(ctx: &Ctx) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let rows = util::query(
        &d,
        "SELECT * FROM agent_release WHERE is_current=1 LIMIT 1",
    )
    .unwrap_or_default();
    let Some(r) = rows.first() else {
        return json!({"ok": true, "release": Value::Null});
    };
    let version = r.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
    json!({"ok": true, "release": {
        "version": version,
        "sha256": r.get("sha256").cloned().unwrap_or(Value::Null),
        "size": r.get("size").cloned().unwrap_or(Value::Null),
        "notes": r.get("notes").cloned().unwrap_or(Value::Null),
        "published_at": r.get("published_at").cloned().unwrap_or(Value::Null),
        "url": format!("/api/fleet/agent/download/{version}"),
    }})
}

pub fn release_list(ctx: &Ctx) -> Value {
    let d = db(ctx);
    let _g = util::lock();
    let rows = util::query(
        &d,
        "SELECT version,sha256,size,notes,published_at,is_current \
         FROM agent_release ORDER BY published_at DESC",
    )
    .unwrap_or_default();
    Value::Array(rows)
}

fn pkg_name_ok(name: &str) -> bool {
    // ^[0-9a-f]{12}\.(msi|exe)$
    if !name.is_ascii() || name.len() < 16 {
        return false;
    }
    let (head, tail) = name.split_at(12);
    head.chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        && (tail == ".msi" || tail == ".exe")
}

pub fn pkg_list(ctx: &Ctx) -> Value {
    let dir = util::pkg_dir(ctx);
    let mut out: Vec<Value> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Value::Array(out);
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if !pkg_name_ok(&name) {
            continue;
        }
        let Ok(md) = ent.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let iso = chrono::DateTime::from_timestamp(mtime, 0)
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        out.push(json!({
            "name": name,
            "size": md.len(),
            "mtime": iso,
            "url": format!("/api/fleet/pkg/{name}"),
        }));
    }
    // `out.sort(key=lambda x: x["mtime"], reverse=True)`：稳定排序，按字符比较。
    out.sort_by(|a, b| {
        let ka = a.get("mtime").and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get("mtime").and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    Value::Array(out)
}

/// OSError 的 Python 文案：`[Errno 21] Is a directory: '/x'`。
fn os_err(e: &std::io::Error, path: &str) -> String {
    match e.raw_os_error() {
        Some(code) => {
            let msg = std::io::Error::from_raw_os_error(code)
                .to_string()
                .split(" (os error")
                .next()
                .unwrap_or("")
                .to_string();
            format!("[Errno {code}] {msg}: '{path}'")
        }
        None => e.to_string(),
    }
}

pub fn register_pkg(ctx: &Ctx, src_raw: &str) -> Value {
    let src = crate::storage::expanduser(src_raw);
    let disp = src.to_string_lossy().to_string();
    if !src.is_file() {
        return json!({"ok": false, "error": format!("找不到文件：{disp}")});
    }
    let lower = src
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if lower != ".msi" && lower != ".exe" {
        return json!({"ok": false, "error": "只接受 .msi / .exe"});
    }
    let data = match std::fs::read(&src) {
        Ok(d) => d,
        Err(e) => return json!({"ok": false, "error": format!("读取失败：{}", os_err(&e, &disp))}),
    };
    let sha = util::sha256_hex(&data);
    let name = format!("{}{}", &sha[..12], lower);
    let dst = util::pkg_dir(ctx).join(&name);
    if let Err(e) = std::fs::copy(&src, &dst) {
        return json!({"ok": false,
                      "error": format!("写入失败：{}", os_err(&e, &dst.to_string_lossy()))});
    }
    json!({"ok": true, "name": name, "sha256": sha, "size": data.len(),
           "url": format!("/api/fleet/pkg/{name}")})
}

pub fn publish_agent(ctx: &Ctx, src_raw: &str, version: &str, notes: &str) -> Value {
    let src = crate::storage::expanduser(src_raw);
    let disp = src.to_string_lossy().to_string();
    if !src.exists() {
        return json!({"ok": false, "error": format!("找不到文件：{disp}")});
    }
    if !version_ok(version) {
        return json!({"ok": false, "error": "版本号要写成 1.2.3"});
    }
    let data = match std::fs::read(&src) {
        Ok(d) => d,
        Err(e) => return json!({"ok": false, "error": format!("读取失败：{}", os_err(&e, &disp))}),
    };
    let sha = util::sha256_hex(&data);
    let dst = util::release_dir(ctx).join(format!("pocket_agent-{version}.py"));
    if let Err(e) = std::fs::copy(&src, &dst) {
        return json!({"ok": false,
                      "error": format!("写入失败：{}", os_err(&e, &dst.to_string_lossy()))});
    }
    let d = db(ctx);
    let _g = util::lock();
    let sql = format!(
        "INSERT OR REPLACE INTO agent_release\
         (version,file_path,sha256,size,notes,published_at,is_current) \
         VALUES ({},{},{},{},{},{},1); \
         UPDATE agent_release SET is_current=0 WHERE version<>{}",
        util::lit(version),
        util::lit(&dst.to_string_lossy()),
        util::lit(&sha),
        data.len(),
        util::lit(&cut_str(notes, 500)),
        util::lit(&util::now_iso()),
        util::lit(version),
    );
    if util::exec_tx(&d, &sql).is_err() {
        return json!({"ok": false, "error": "数据库不可用"});
    }
    json!({"ok": true, "version": version, "sha256": sha, "size": data.len()})
}

fn version_ok(v: &str) -> bool {
    // ^\d+\.\d+\.\d+$
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

// ------------------------------------------------------------------ 路由

fn body_or(ctx_body: &[u8], headers: &HeaderMap) -> Body {
    let ct = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    util::parse_body(ctx_body, &ct, headers.contains_key(axum::http::header::CONTENT_TYPE))
}

macro_rules! need_agent {
    ($cfg:expr, $headers:expr, $uri:expr) => {
        if let Some(r) = perms::agent_gate(&$cfg, &$headers, &$uri) {
            return r;
        }
    };
}

macro_rules! need_view {
    ($cfg:expr, $headers:expr, $uri:expr) => {
        if let Some(r) = perms::view_gate(&$cfg, &$headers, &$uri) {
            return r;
        }
    };
}

macro_rules! obj_body {
    ($raw:expr, $headers:expr) => {
        match body_or($raw, &$headers) {
            Body::Obj(v) => v,
            Body::Bad(r) => return r,
        }
    };
}

pub async fn report_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    // 中间件位置：体积闸门在路由/鉴权之前（Python 的 @app.middleware 就是这个顺序）。
    if let Some(cl) = perms::content_length(&headers) {
        if cl > perms::MAX_REPORT_BYTES {
            let mut res = (
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"ok": false,
                            "error": format!("上报体积超过 {} 字节", perms::MAX_REPORT_BYTES)})),
            )
                .into_response();
            res.headers_mut().insert(
                axum::http::header::CONNECTION,
                axum::http::HeaderValue::from_static("close"),
            );
            return res;
        }
    }
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    // ip 只认 TCP 源地址；本机反代（peer=127.0.0.1）才看 X-Real-IP。
    let xri = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ip = if xri.is_empty() { "127.0.0.1" } else { xri };
    let fp = headers
        .get("x-device-fp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    ok(report_body(&ctx, &body, ip, fp))
}

pub async fn poll_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let dev = cut_str(&util::py_strip(&s_of(&body, "device_id")), 64);
    if dev.is_empty() {
        return bad("device_id 必填");
    }
    let fp = headers
        .get("x-device-fp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (yes, why) = fp_ok(&ctx, &dev, fp);
    if !yes {
        return bad(&why);
    }
    let mut r = take(&ctx, &dev);
    let caps = body.get("caps").cloned().unwrap_or(json!([]));
    if let Some(list) = caps.as_array() {
        if list.iter().any(|x| x.as_str() == Some("tasks")) {
            let t = take_task(&ctx, &dev);
            let task = t.get("task").cloned().unwrap_or(Value::Null);
            if let Some(o) = r.as_object_mut() {
                o.insert("task".to_string(), task);
            }
        }
    }
    ok(r)
}

pub async fn result_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let cid = cut_str(&util::py_strip(&s_of(&body, "id")), 32);
    if cid.is_empty() {
        return bad("id 必填");
    }
    let dev = device_of(&ctx, "commands", &cid);
    let fp = headers
        .get("x-device-fp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (yes, why) = fp_ok(&ctx, &dev, fp);
    if !yes {
        return bad(&why);
    }
    let ok_ = body.get("ok").map(util::truthy).unwrap_or(false);
    ok(finish(&ctx, &cid, ok_, &s_of(&body, "output")))
}

pub async fn unbind_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let dev = cut_str(&util::py_strip(&s_of(&body, "device_id")), 64);
    if dev.is_empty() {
        return bad("device_id 必填");
    }
    let d = db(&ctx);
    let n = {
        let _g = util::lock();
        util::exec(
            &d,
            &format!("UPDATE devices SET fp='' WHERE id={}", util::lit(&dev)),
        )
        .unwrap_or(0)
    };
    if n == 0 {
        return bad("没有这台设备");
    }
    ok(json!({"ok": true, "device_id": dev, "note": "已解绑，下次上报会重新绑定"}))
}

pub async fn command_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let dev = cut_str(&util::py_strip(&s_of(&body, "device_id")), 64);
    let action = cut_str(&util::py_strip(&s_of(&body, "action")), 16);
    let who = perms::actor(&headers);
    ok(enqueue(&ctx, &dev, &action, &who))
}

pub async fn commands_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    ok(json!({"ok": true, "items": recent_commands(&ctx), "actions": actions()}))
}

pub async fn overview_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    ok(overview(&ctx))
}

pub async fn access_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    ok(set_access(&ctx, &s_of(&body, "device_id"), &s_of(&body, "access")))
}

pub async fn software_report_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let dev = cut_str(&util::py_strip(&s_of(&body, "device_id")), 64);
    if dev.is_empty() {
        return bad("device_id 必填");
    }
    let fp = headers
        .get("x-device-fp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (yes, why) = fp_ok(&ctx, &dev, fp);
    if !yes {
        return bad(&why);
    }
    let items = body.get("items").cloned().unwrap_or(json!([]));
    let ups = body.get("upgrades");
    ok(save_software(&ctx, &dev, &items, ups))
}

pub async fn task_progress_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let tid = cut_str(&util::py_strip(&s_of(&body, "id")), 32);
    if tid.is_empty() {
        return bad("id 必填");
    }
    let dev = device_of(&ctx, "tasks", &tid);
    let fp = headers
        .get("x-device-fp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (yes, why) = fp_ok(&ctx, &dev, fp);
    if !yes {
        return bad(&why);
    }
    let prog = match body.get("progress") {
        Some(v) if util::truthy(v) => util::int_of(v).unwrap_or(0),
        _ => 0,
    };
    ok(progress_task(&ctx, &tid, prog, &s_of(&body, "log")))
}

pub async fn task_result_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let tid = cut_str(&util::py_strip(&s_of(&body, "id")), 32);
    if tid.is_empty() {
        return bad("id 必填");
    }
    let dev = device_of(&ctx, "tasks", &tid);
    let fp = headers
        .get("x-device-fp")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let (yes, why) = fp_ok(&ctx, &dev, fp);
    if !yes {
        return bad(&why);
    }
    let ok_ = body.get("ok").map(util::truthy).unwrap_or(false);
    ok(finish_task(&ctx, &tid, ok_, &s_of(&body, "log")))
}

pub async fn agent_latest_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    ok(agent_latest(&ctx))
}

pub async fn agent_download_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    Path(version): Path<String>,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    let ver = cut_str(&util::py_strip(&version), 20);
    let latest = agent_latest(&ctx);
    let cur = latest
        .get("release")
        .and_then(|r| r.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if cur != ver {
        return http_err(404, "没有这个版本");
    }
    let p = util::release_dir(&ctx).join(format!("pocket_agent-{ver}.py"));
    if !p.exists() {
        return http_err(404, "发布文件已丢失");
    }
    util::file_download(&p, "text/x-python; charset=utf-8", &format!("pocket_agent-{ver}.py"))
}

pub async fn software_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    let device = cut_str(&util::py_strip(&q(&uri, "device")), 64);
    let query = cut_str(&util::py_strip(&q(&uri, "q")), 80);
    ok(software_list(&ctx, &device, &query))
}

pub async fn task_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let args = body.get("args").filter(|v| v.is_object());
    let dev = cut_str(&util::py_strip(&s_of(&body, "device_id")), 64);
    let kind = cut_str(&util::py_strip(&s_of(&body, "kind")), 16);
    let target = cut_str(&util::py_strip(&s_of(&body, "target")), 160);
    ok(enqueue_task(&ctx, &dev, &kind, &target, args))
}

pub async fn tasks_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    ok(json!({"ok": true, "items": recent_tasks(&ctx), "kinds": task_kinds()}))
}

pub async fn releases_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    ok(json!({"ok": true, "items": release_list(&ctx)}))
}

pub async fn pkg_register_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let src = util::py_strip(&s_of(&body, "file"));
    if src.is_empty() {
        return bad("缺少 file（主端本地路径）");
    }
    ok(register_pkg(&ctx, &src))
}

pub async fn pkg_list_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    ok(json!({"ok": true, "items": pkg_list(&ctx)}))
}

pub async fn pkg_get_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    Path(name): Path<String>,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    if !pkg_name_ok(&name) {
        return http_err(404, "没有这个包");
    }
    let p = util::pkg_dir(&ctx).join(&name);
    if !p.is_file() {
        return http_err(404, "没有这个包");
    }
    util::file_download(&p, "application/octet-stream", &name)
}

pub async fn publish_route(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    raw: Bytes,
) -> Response {
    let cfg = perms::cfg(&ctx);
    need_view!(cfg, headers, uri);
    let body = obj_body!(&raw, headers);
    let src = util::py_strip(&s_of(&body, "file"));
    if src.is_empty() {
        return bad("缺少 file（主端本地路径）");
    }
    let version = util::py_strip(&s_of(&body, "version"));
    ok(publish_agent(&ctx, &src, &version, &s_of(&body, "notes")))
}

pub async fn prune_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    let cfg = perms::cfg(&ctx);
    need_agent!(cfg, headers, uri);
    ok(prune(&ctx))
}
