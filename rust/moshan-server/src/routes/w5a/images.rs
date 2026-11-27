// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：存图 / 阅读 / 影音 / 算价 / 收件箱 五项的只读快照（dash._gallery/_reader/_media/_llmcost/_files）。
// 不做：不调 8773 / 8776 的 HTTP —— Python 那边也只在端口探活时用一下 socket，数据一律读盘。
//
// 逐字节的要点：这几个块都是「原始 dict 直接进 JSON」，所以字段顺序 = Python 里字面量的键序，
// 少一个键或顺序换一下就整段不一样。下面的构建顺序都照着 dash.py 抄的，别重排。
#![allow(unused_imports, dead_code)]

use chrono::{Duration as ChronoDuration, Local};
use serde_json::{json, Map, Value};
use std::path::Path;

use super::platform;
use super::util::{
    fnum, human, ival, load_json, load_obj, mtime, now_ymd, obj, pct, py_float, py_str, rel_to,
    rglob_all, round1, round_n, scandir, sval, truthy,
};

/// `_SKIP_DIRS`（dash.py:55）：阅读目录里这些名字整段跳过。
const SKIP_DIRS: [&str; 5] = ["_回收站", "_缩略图", "_备份", "_backup", ".DS_Store"];

fn is_skip_dir(name: &str) -> bool {
    SKIP_DIRS.iter().any(|x| *x == name)
}

// ---------------------------------------------------------------- 读不到媒体根时
//
// 媒体根没被系统授权时 `open()` 会卡在内核里（见 `paths::spawn_library_probe`），
// 所以这几个块在动手读盘之前先问一句 `library_ready()`，不通就吐同形状的空块：
// 前端照旧能画，卡片是 0，而不是整页没响应。

fn empty_gallery() -> Value {
    json!({
        "articles": 0, "images": 0, "bytes_h": human(0.0),
        "live": 0, "video": 0, "gif": 0, "today_new": 0,
        "favs": 0, "authors": 0, "index_at": "", "recent": [],
    })
}

fn empty_reader() -> Value {
    json!({
        "total": 0, "read": 0, "unread": 0, "pct": fnum(0.0),
        "chars": 0, "chars_h": "0 字", "favs": 0,
        "unread_list": [], "recent": [],
        "today_new": 0, "week_new": 0, "ocr_done": 0, "need_ocr": 0,
    })
}

fn empty_media() -> Value {
    json!({
        "up": false, "titles": 0, "episodes": 0, "bytes_h": human(0.0),
        "pending": 0, "library_at": "", "last_at": "", "continue": [],
    })
}

fn empty_files() -> Value {
    json!({"inbox": 0, "today": 0, "today_h": human(0.0), "recent": []})
}

/// Python `round(x)`（无 ndigits）→ 整数，走二进制真值四舍六入五成双。
fn round_int(x: f64) -> i64 {
    if !x.is_finite() {
        return 0;
    }
    format!("{:.0}", x).parse().unwrap_or(0)
}

// ---------------------------------------------------------------- 04-存图

pub fn gallery(root: &Path) -> Value {
    if !crate::paths::library_ready() {
        return empty_gallery();
    }
    let base = root.join("04-存图");
    let d = load_obj(&base.join("_索引.json"));
    let total = match d.get("total") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let arts: Vec<Value> = match d.get("articles") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut live = 0i64;
    let mut video = 0i64;
    let mut gif = 0i64;
    let today = now_ymd();
    let mut today_new = 0i64;
    for a in &arts {
        live += super::util::int_of(a.get("n_live"));
        video += super::util::int_of(a.get("n_video"));
        gif += super::util::int_of(a.get("n_gif"));
        let at: String = super::util::str_or(a.get("archived_at")).chars().take(10).collect();
        if at == today {
            today_new += 1;
        }
    }
    let favs: Vec<Value> = load_obj(&base.join("_收藏.json"))
        .get("dirs")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let authors: Vec<Value> = load_obj(&base.join("_喜欢作者.json"))
        .get("authors")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut recent_src = arts.clone();
    recent_src.sort_by(|a, b| {
        super::util::str_or(b.get("archived_at")).cmp(&super::util::str_or(a.get("archived_at")))
    });
    let recent: Vec<Value> = recent_src
        .iter()
        .take(5)
        .map(|a| {
            let mut m = Map::new();
            m.insert("title".into(), super::util::or_empty(a.get("title")));
            m.insert("platform".into(), super::util::or_empty(a.get("platform")));
            m.insert("author".into(), super::util::or_empty(a.get("author")));
            m.insert(
                "count".into(),
                match a.get("count") {
                    Some(v) if truthy(v) => v.clone(),
                    _ => ival(0),
                },
            );
            m.insert(
                "live".into(),
                match a.get("n_live") {
                    Some(v) if truthy(v) => v.clone(),
                    _ => ival(0),
                },
            );
            m.insert("at".into(), sval(&super::util::str_or(a.get("archived_at"))));
            m.insert("dir".into(), super::util::or_empty(a.get("dir")));
            Value::Object(m)
        })
        .collect();

    let mut m = Map::new();
    m.insert(
        "articles".into(),
        ival(match total.get("articles") {
            Some(v) if truthy(v) => super::util::int_of(Some(v)),
            _ => arts.len() as i64,
        }),
    );
    m.insert("images".into(), ival(super::util::int_of(total.get("images"))));
    m.insert(
        "bytes_h".into(),
        sval(&human(match total.get("bytes") {
            Some(v) => py_float(v).unwrap_or(0.0),
            None => 0.0,
        })),
    );
    m.insert("live".into(), ival(live));
    m.insert("video".into(), ival(video));
    m.insert("gif".into(), ival(gif));
    m.insert("today_new".into(), ival(today_new));
    m.insert("favs".into(), ival(favs.len() as i64));
    m.insert("authors".into(), ival(authors.len() as i64));
    m.insert("index_at".into(), sval(&super::util::str_or(d.get("generated_at"))));
    m.insert("recent".into(), Value::Array(recent));
    Value::Object(m)
}

// ---------------------------------------------------------------- 08-阅读

pub fn reader(root: &Path) -> Value {
    if !crate::paths::library_ready() {
        return empty_reader();
    }
    let base = root.join("08-阅读");
    let read_state = load_obj(&base.join("_已读.json"));
    let read_rels: Vec<String> = match read_state.get("rels") {
        Some(Value::Array(a)) => a.iter().map(py_str).collect(),
        _ => Vec::new(),
    };
    let read_titles: Vec<String> = read_rels
        .iter()
        .map(|r| r.rsplit('/').next().unwrap_or("").to_string())
        .collect();
    let favs: Vec<Value> = load_obj(&base.join("_收藏.json"))
        .get("rels")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let mut arts: Vec<Value> = Vec::new();
    if base.exists() {
        let mut topics: Vec<(String, bool, bool)> = scandir(&base);
        topics.sort_by(|a, b| a.0.cmp(&b.0));
        for (tname, is_dir, _) in topics {
            if !is_dir || tname.starts_with('_') || is_skip_dir(&tname) {
                continue;
            }
            for (aname, adir, _) in scandir(&base.join(&tname)) {
                if !adir || aname.starts_with('_') {
                    continue;
                }
                let adir_path = base.join(&tname).join(&aname);
                let meta = match load_json(&adir_path.join("article.json")) {
                    Some(Value::Object(m)) => m,
                    _ => continue,
                };
                let rel = format!("{}/{}", tname, aname);
                let is_read = read_rels.iter().any(|r| *r == rel)
                    || read_titles.iter().any(|t| *t == aname);
                let mut m = Map::new();
                m.insert("rel".into(), sval(&rel));
                m.insert(
                    "title".into(),
                    match meta.get("title") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => sval(&aname),
                    },
                );
                m.insert(
                    "topic".into(),
                    match meta.get("topic") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => sval(&tname),
                    },
                );
                m.insert("platform".into(), super::util::or_empty(meta.get("platform")));
                m.insert("author".into(), super::util::or_empty(meta.get("author")));
                m.insert("chars".into(), ival(super::util::int_of(meta.get("chars"))));
                m.insert("at".into(), sval(&super::util::str_or(meta.get("saved_at"))));
                m.insert("read".into(), Value::Bool(is_read));
                m.insert(
                    "ocr".into(),
                    ival(super::util::int_of(meta.get("ocr_chars"))),
                );
                m.insert(
                    "need_ocr".into(),
                    Value::Bool(meta.get("needs_ocr").map(truthy).unwrap_or(false)),
                );
                arts.push(Value::Object(m));
            }
        }
    }

    let total = arts.len() as i64;
    let read_n = arts
        .iter()
        .filter(|a| a.get("read").map(truthy).unwrap_or(false))
        .count() as i64;
    let chars: i64 = arts
        .iter()
        .map(|a| a.get("chars").map(|v| super::util::int_of(Some(v))).unwrap_or(0))
        .sum();
    let today = now_ymd();
    let week = (Local::now().date_naive() - ChronoDuration::days(7))
        .format("%Y-%m-%d")
        .to_string();
    arts.sort_by(|a, b| {
        super::util::str_or(b.get("at")).cmp(&super::util::str_or(a.get("at")))
    });

    let unread_list: Vec<Value> = arts
        .iter()
        .filter(|a| !a.get("read").map(truthy).unwrap_or(false))
        .take(6)
        .cloned()
        .collect();
    let recent: Vec<Value> = arts.iter().take(8).cloned().collect();
    let today_new = arts
        .iter()
        .filter(|a| super::util::str_or(a.get("at")).chars().take(10).collect::<String>() == today)
        .count() as i64;
    let week_new = arts
        .iter()
        .filter(|a| super::util::str_or(a.get("at")).chars().take(10).collect::<String>() >= week)
        .count() as i64;
    let ocr_done = arts
        .iter()
        .filter(|a| a.get("ocr").map(truthy).unwrap_or(false))
        .count() as i64;
    let need_ocr = arts
        .iter()
        .filter(|a| a.get("need_ocr").map(truthy).unwrap_or(false))
        .count() as i64;

    let mut m = Map::new();
    m.insert("total".into(), ival(total));
    m.insert("read".into(), ival(read_n));
    m.insert("unread".into(), ival(total - read_n));
    m.insert("pct".into(), fnum(pct(read_n as f64, total as f64)));
    m.insert("chars".into(), ival(chars));
    m.insert(
        "chars_h".into(),
        sval(&if chars >= 1000 {
            format!("{:.1}k 字", chars as f64 / 1000.0)
        } else {
            format!("{} 字", chars)
        }),
    );
    m.insert("favs".into(), ival(favs.len() as i64));
    m.insert("unread_list".into(), Value::Array(unread_list));
    m.insert("recent".into(), Value::Array(recent));
    m.insert("today_new".into(), ival(today_new));
    m.insert("week_new".into(), ival(week_new));
    m.insert("ocr_done".into(), ival(ocr_done));
    m.insert("need_ocr".into(), ival(need_ocr));
    Value::Object(m)
}

// ---------------------------------------------------------------- 02-媒体

pub fn media(root: &Path) -> Value {
    if !crate::paths::library_ready() {
        return empty_media();
    }
    let base = root.join("02-媒体");
    let lib = load_obj(&base.join("_墙").join("library.json"));
    let state = load_obj(&base.join("_墙").join("watch.json"));
    let items: Vec<Value> = match lib.get("items") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut n_ep = 0i64;
    let mut total = 0.0f64;
    let mut pending = 0i64;
    let mut cont: Vec<Value> = Vec::new();
    let mut last_at = String::new();

    for it in &items {
        let eps: Vec<Value> = match it.get("episodes") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };
        n_ep += eps.len() as i64;
        total += py_float(it.get("bytes").unwrap_or(&Value::Null)).unwrap_or(0.0);
        let meta = it.get("meta").cloned().unwrap_or_else(|| json!({}));
        if !truthy(meta.get("scraped_at").unwrap_or(&Value::Null)) {
            pending += 1;
        }
        let st = match it.get("id") {
            Some(Value::String(k)) => state.get(k).cloned().unwrap_or_else(|| json!({})),
            _ => json!({}),
        };
        let up = super::util::str_or(st.get("updated"));
        if up > last_at {
            last_at = up.clone();
        }
        let watched = st.get("watched").cloned().unwrap_or_else(|| json!({}));
        let prog = st.get("progress").cloned().unwrap_or_else(|| json!({}));
        let mut pick: Option<Map<String, Value>> = None;
        for e in &eps {
            let k = py_str(e.get("index").unwrap_or(&Value::Null));
            if watched.get(&k).map(truthy).unwrap_or(false) {
                continue;
            }
            let p = prog.get(&k).cloned().unwrap_or_else(|| json!({}));
            let pos = py_float(p.get("pos").unwrap_or(&Value::Null)).unwrap_or(0.0);
            let dur = match p.get("dur") {
                Some(v) if truthy(v) => py_float(v).unwrap_or(0.0),
                _ => py_float(e.get("duration").unwrap_or(&Value::Null)).unwrap_or(0.0),
            };
            if pos > 3.0 && dur > 0.0 && pos / dur < 0.95 {
                let mut m = Map::new();
                m.insert(
                    "ep".into(),
                    match e.get("n") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => e.get("index").cloned().unwrap_or(Value::Null),
                    },
                );
                m.insert("pct".into(), ival(round_int(pos / dur * 100.0)));
                m.insert("pos".into(), fnum(pos));
                m.insert("dur".into(), fnum(dur));
                pick = Some(m);
                break;
            }
        }
        let n_watched = match &watched {
            Value::Object(m) => m.values().filter(|v| truthy(v)).count(),
            _ => 0,
        };
        if pick.is_none() && n_watched > 0 && n_watched < eps.len() {
            let nxt = eps
                .iter()
                .find(|e| !watched.get(&py_str(e.get("index").unwrap_or(&Value::Null))).map(truthy).unwrap_or(false));
            if let Some(nxt) = nxt {
                let mut m = Map::new();
                m.insert(
                    "ep".into(),
                    match nxt.get("n") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => nxt.get("index").cloned().unwrap_or(Value::Null),
                    },
                );
                m.insert("pct".into(), ival(0));
                m.insert("pos".into(), ival(0));
                m.insert(
                    "dur".into(),
                    match nxt.get("duration") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => ival(0),
                    },
                );
                pick = Some(m);
            }
        }
        if let Some(mut m) = pick {
            m.insert(
                "title".into(),
                match meta.get("title") {
                    Some(v) if truthy(v) => v.clone(),
                    _ => match it.get("title") {
                        Some(v) if truthy(v) => v.clone(),
                        _ => sval(""),
                    },
                },
            );
            m.insert("at".into(), sval(&up));
            m.insert("poster".into(), meta.get("poster").cloned().unwrap_or(Value::Null));
            cont.push(Value::Object(m));
        }
    }
    cont.sort_by(|a, b| super::util::str_or(b.get("at")).cmp(&super::util::str_or(a.get("at"))));

    let mut m = Map::new();
    m.insert(
        "up".into(),
        Value::Bool(platform::port_open(8773, "127.0.0.1", std::time::Duration::from_millis(350))),
    );
    m.insert("titles".into(), ival(items.len() as i64));
    m.insert("episodes".into(), ival(n_ep));
    m.insert("bytes_h".into(), sval(&human(total)));
    m.insert("pending".into(), ival(pending));
    m.insert("library_at".into(), sval(&super::util::str_or(lib.get("updated_at"))));
    m.insert("last_at".into(), sval(&last_at));
    m.insert("continue".into(), Value::Array(cont.into_iter().take(3).collect()));
    Value::Object(m)
}

// ---------------------------------------------------------------- 13-算价

pub fn llmcost(root: &Path) -> Value {
    let base = root.join("13-算价");
    let out = base.join("out").join("accuracy.json");
    let up = platform::port_open(8776, "127.0.0.1", std::time::Duration::from_millis(350));
    let d = load_obj(&out);
    if d.is_empty() {
        let mut m = Map::new();
        m.insert("ready".into(), Value::Bool(false));
        m.insert("up".into(), Value::Bool(up));
        return Value::Object(m);
    }
    let counts = d.get("counts").cloned().unwrap_or_else(|| json!({}));
    let raw_api: Vec<Value> = match d.get("api") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut api: Vec<Value> = raw_api
        .iter()
        .filter(|r| truthy(r.get("output_cny").unwrap_or(&Value::Null)))
        .cloned()
        .collect();
    api.sort_by(|a, b| {
        let x = py_float(a.get("output_cny").unwrap_or(&Value::Null)).unwrap_or(0.0);
        let y = py_float(b.get("output_cny").unwrap_or(&Value::Null)).unwrap_or(0.0);
        x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
    });
    let std: Vec<Value> = api
        .iter()
        .filter(|r| {
            let k = match r.get("price_kind") {
                Some(v) if truthy(v) => py_str(v),
                _ => "standard".to_string(),
            };
            k == "standard"
        })
        .cloned()
        .collect();
    let cheap = std.first().or_else(|| api.first());
    let cheap_any = api.first();

    let mut conf: Vec<Value> = Vec::new();
    for r in &raw_api {
        let cs: Vec<Value> = match r.get("conflicts") {
            Some(Value::Array(a)) => a.iter().take(1).cloned().collect(),
            _ => Vec::new(),
        };
        for c in cs {
            let mut m = Map::new();
            m.insert(
                "name".into(),
                match r.get("display") {
                    Some(v) if truthy(v) => v.clone(),
                    _ => super::util::or_empty(r.get("model")),
                },
            );
            m.insert("provider".into(), super::util::or_empty(r.get("provider")));
            m.insert("metric".into(), super::util::or_empty(c.get("metric")));
            let rel = py_float(c.get("rel").unwrap_or(&Value::Null)).unwrap_or(0.0);
            m.insert("rel".into(), ival(round_int(rel * 100.0)));
            conf.push(Value::Object(m));
        }
    }

    let today = now_ymd();
    let raw_plans: Vec<Value> = match d.get("plans") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    let mut plans: Vec<Value> = raw_plans
        .iter()
        .filter(|p| {
            if !truthy(p.get("convertible").unwrap_or(&Value::Null))
                || !truthy(p.get("cny_per_1m").unwrap_or(&Value::Null))
            {
                return false;
            }
            match p.get("promo_until") {
                Some(v) if truthy(v) => py_str(v).chars().take(10).collect::<String>() >= today,
                _ => true,
            }
        })
        .cloned()
        .collect();
    plans.sort_by(|a, b| {
        let x = py_float(a.get("cny_per_1m").unwrap_or(&Value::Null)).unwrap_or(0.0);
        let y = py_float(b.get("cny_per_1m").unwrap_or(&Value::Null)).unwrap_or(0.0);
        x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
    });

    let age = {
        let mt = mtime(&out);
        if mt <= 0.0 {
            999i64
        } else {
            let now = Local::now().timestamp_millis() as f64 / 1000.0;
            ((now - mt) / 86400.0) as i64
        }
    };

    let mut m = Map::new();
    m.insert("ready".into(), Value::Bool(true));
    m.insert("up".into(), Value::Bool(up));
    m.insert(
        "at".into(),
        sval(&super::util::str_or(d.get("data_date")).chars().take(10).collect::<String>()),
    );
    m.insert("age".into(), ival(age));
    m.insert(
        "generated_at".into(),
        sval(
            &super::util::str_or(d.get("generated_at"))
                .chars()
                .take(19)
                .collect::<String>()
                .replace('T', " "),
        ),
    );
    m.insert("n_api".into(), ival(raw_api.len() as i64));
    m.insert("n_plans".into(), ival(raw_plans.len() as i64));
    m.insert(
        "n_rigs".into(),
        ival(match d.get("rigs") {
            Some(Value::Array(a)) => a.len() as i64,
            _ => 0,
        }),
    );
    m.insert("counts".into(), counts);
    m.insert("obs".into(), ival(super::util::int_of(d.get("observations_total"))));
    m.insert(
        "n_sources".into(),
        ival(match d.get("source_coverage") {
            Some(Value::Object(o)) => o.len() as i64,
            _ => 0,
        }),
    );
    m.insert(
        "fx".into(),
        d.get("fx_usd_cny").cloned().unwrap_or(Value::Null),
    );
    m.insert(
        "cheapest".into(),
        match cheap {
            Some(c) => {
                let mut x = Map::new();
                x.insert("name".into(), c.get("display").cloned().unwrap_or(Value::Null));
                x.insert("cny".into(), c.get("output_cny").cloned().unwrap_or(Value::Null));
                x.insert("status".into(), c.get("status").cloned().unwrap_or(Value::Null));
                Value::Object(x)
            }
            None => Value::Null,
        },
    );
    m.insert(
        "cheapest_any".into(),
        match cheap_any {
            Some(c) => {
                let mut x = Map::new();
                x.insert("name".into(), c.get("display").cloned().unwrap_or(Value::Null));
                x.insert("cny".into(), c.get("output_cny").cloned().unwrap_or(Value::Null));
                x.insert("status".into(), c.get("status").cloned().unwrap_or(Value::Null));
                Value::Object(x)
            }
            None => Value::Null,
        },
    );
    m.insert(
        "best_plan".into(),
        match plans.first() {
            Some(p) => {
                let mut x = Map::new();
                x.insert(
                    "name".into(),
                    sval(&format!(
                        "{} · {}",
                        py_str(p.get("platform").unwrap_or(&Value::Null)),
                        py_str(p.get("plan").unwrap_or(&Value::Null))
                    )),
                );
                x.insert("cny".into(), p.get("cny_per_1m").cloned().unwrap_or(Value::Null));
                Value::Object(x)
            }
            None => Value::Null,
        },
    );
    m.insert("conflicts".into(), Value::Array(conf.into_iter().take(3).collect()));
    m.insert(
        "changes".into(),
        ival(match d.get("changes") {
            Some(Value::Array(a)) => a.len() as i64,
            _ => 0,
        }),
    );
    m.insert("history".into(), llmcost_history(&d));
    Value::Object(m)
}

fn llmcost_history(d: &Map<String, Value>) -> Value {
    let h = d.get("history").cloned().unwrap_or_else(|| json!({}));
    let sm = match h.get("summary") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    if !truthy(sm.get("n_models").unwrap_or(&Value::Null)) {
        return json!({});
    }
    let mut moves: Vec<Value> = match h.get("changes") {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    moves.sort_by(|a, b| {
        let x = py_float(a.get("pct").unwrap_or(&Value::Null)).unwrap_or(0.0).abs();
        let y = py_float(b.get("pct").unwrap_or(&Value::Null)).unwrap_or(0.0).abs();
        y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal)
    });
    let top: Vec<Value> = moves
        .iter()
        .take(2)
        .map(|c| {
            let mut m = Map::new();
            m.insert(
                "name".into(),
                match c.get("display") {
                    Some(v) if truthy(v) => v.clone(),
                    _ => super::util::or_empty(c.get("model")),
                },
            );
            let pp = py_float(c.get("pct").unwrap_or(&Value::Null)).unwrap_or(0.0);
            m.insert("pct".into(), ival(round_int(pp * 100.0)));
            m.insert(
                "date".into(),
                sval(&super::util::str_or(c.get("date")).chars().take(10).collect::<String>()),
            );
            Value::Object(m)
        })
        .collect();
    let pano = d.get("panorama").cloned().unwrap_or_else(|| json!({}));
    let psm = match pano.get("summary") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    let mut m = Map::new();
    m.insert(
        "n_dates".into(),
        match sm.get("n_dates") {
            Some(v) if truthy(v) => v.clone(),
            _ => ival(0),
        },
    );
    m.insert(
        "n_changed".into(),
        match sm.get("n_changed") {
            Some(v) if truthy(v) => v.clone(),
            _ => ival(0),
        },
    );
    m.insert(
        "n_models".into(),
        match sm.get("n_models") {
            Some(v) if truthy(v) => v.clone(),
            _ => ival(0),
        },
    );
    m.insert(
        "n_platforms".into(),
        match psm.get("n_platforms") {
            Some(v) if truthy(v) => v.clone(),
            _ => ival(0),
        },
    );
    m.insert(
        "spread_max".into(),
        match psm.get("spread_max") {
            Some(v) if truthy(v) => v.clone(),
            _ => ival(0),
        },
    );
    m.insert(
        "from".into(),
        sval(&super::util::str_or(sm.get("span_from")).chars().take(10).collect::<String>()),
    );
    m.insert(
        "to".into(),
        sval(&super::util::str_or(sm.get("span_to")).chars().take(10).collect::<String>()),
    );
    let lc = py_float(sm.get("level_chg").unwrap_or(&Value::Null)).unwrap_or(0.0);
    m.insert("level_chg".into(), fnum(round1(lc * 100.0)));
    m.insert("top".into(), Value::Array(top));
    Value::Object(m)
}

// ---------------------------------------------------------------- 07-收件箱

pub fn files(root: &Path) -> Value {
    if !crate::paths::library_ready() {
        return empty_files();
    }
    let inbox = root.join("07-收件箱");
    let today = now_ymd();
    let today_dir = inbox.join(&today);
    let mut count = 0i64;
    let mut today_count = 0i64;
    let mut today_bytes = 0i64;
    let mut recent: Vec<(f64, Value)> = Vec::new();
    if inbox.exists() {
        for p in rglob_all(&inbox, usize::MAX) {
            let md = match std::fs::symlink_metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !md.is_file() {
                continue;
            }
            let name = p
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.starts_with('.') || name == "index.jsonl" {
                continue;
            }
            let st = match std::fs::metadata(&p) {
                Ok(m) => m,
                Err(_) => continue,
            };
            count += 1;
            let size = st.len() as i64;
            let mt = st
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let in_today = p.parent() == Some(today_dir.as_path())
                || p.ancestors().skip(1).any(|a| a == today_dir);
            let mut m = Map::new();
            m.insert("name".into(), sval(&name));
            m.insert("size".into(), sval(&human(size as f64)));
            m.insert("bytes".into(), ival(size));
            m.insert("mtime".into(), fnum(mt));
            m.insert("rel".into(), sval(&rel_to(&p, &inbox)));
            m.insert("in_today".into(), Value::Bool(in_today));
            recent.push((mt, Value::Object(m)));
        }
    }
    for (_, r) in &recent {
        if r.get("in_today").map(truthy).unwrap_or(false) {
            today_count += 1;
            today_bytes += r.get("bytes").map(|v| super::util::int_of(Some(v))).unwrap_or(0);
        }
    }
    recent.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let out: Vec<Value> = recent
        .iter()
        .take(5)
        .map(|(_, r)| {
            let mut m = Map::new();
            m.insert("name".into(), r.get("name").cloned().unwrap_or(Value::Null));
            m.insert("size".into(), r.get("size").cloned().unwrap_or(Value::Null));
            m.insert("bytes".into(), r.get("bytes").cloned().unwrap_or(Value::Null));
            m.insert("rel".into(), r.get("rel").cloned().unwrap_or(Value::Null));
            Value::Object(m)
        })
        .collect();
    let mut m = Map::new();
    m.insert("inbox".into(), ival(count));
    m.insert("today".into(), ival(today_count));
    m.insert("today_h".into(), sval(&human(today_bytes as f64)));
    m.insert("recent".into(), Value::Array(out));
    Value::Object(m)
}
