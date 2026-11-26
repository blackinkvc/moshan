// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`moshan/pending.py` 的 Rust 版 —— 「存图还是文章」待确认队列（3 条路由）。
// 不做：不跑后台发送线程（`flush` 要靠 wechat_push 的 websocket 网关），
//       也不注册 linkbox/articlebox 的处理函数 —— 所以 gallery/article 两种回答
//       在 Rust 侧落成 `failed`，只有 `drop`（算了）与 Python 语义完全一致。
//
// 状态机与落盘时机照抄：working 先落盘再干活（处理要几秒，别让同一个问题被答两次），
// 关闭时 `_prune` 只留最近 40 条已答。

use super::util::{self, get, s_or, truthy};
use crate::paths::Ctx;
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MAX_AGE: f64 = 7.0 * 86400.0;
const SHORT_AGE: f64 = 6.0 * 3600.0;
const KEEP_DONE: usize = 40;

pub fn pend_path(ctx: &Ctx) -> PathBuf {
    ctx.library_dir.join("04-存图").join("_待定.json")
}

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn num(v: &Value) -> f64 {
    util::as_f64(v).unwrap_or(0.0)
}

// ---------------------------------------------------------------- 存取

fn load(ctx: &Ctx) -> Vec<Value> {
    let raw = match util::load_json(&pend_path(ctx)) {
        Some(v) => v,
        None => return Vec::new(),
    };
    let items = match &raw {
        Value::Object(_) => raw.get("items").cloned().unwrap_or(json!([])),
        other => other.clone(),
    };
    items
        .as_array()
        .map(|a| a.iter().filter(|x| x.is_object()).cloned().collect())
        .unwrap_or_default()
}

fn save(ctx: &Ctx, items: &[Value]) {
    let p = pend_path(ctx);
    util::ensure_dir(p.parent().unwrap_or(Path::new(".")));
    let body = json!({"v": 1, "items": items});
    let _ = util::write_json(&p, &body, 1);
}

fn prune(items: &mut Vec<Value>) {
    let now = now_ts();
    let mut keep: Vec<Value> = Vec::new();
    let mut done = 0usize;
    for i in items.iter().rev() {
        let mut i = i.clone();
        if s_or(get(&i, "state")) != "open" {
            done += 1;
            if done > KEEP_DONE {
                continue;
            }
        } else if now - num(get(&i, "asked_at")) > MAX_AGE {
            set(&mut i, "state", json!("expired"));
        }
        keep.push(i);
    }
    keep.reverse();
    *items = keep;
}

/// 还没答的，新的在前。顺手把过期的清掉。
pub fn open_items(ctx: &Ctx) -> Vec<Value> {
    let items = load(ctx);
    let mut pruned = items.clone();
    prune(&mut pruned);
    if pruned.len() != items.len() {
        save(ctx, &pruned);
    }
    let mut out: Vec<Value> = pruned
        .into_iter()
        .rev()
        .filter(|i| s_or(get(i, "state")) == "open")
        .collect();
    out.shrink_to_fit();
    out
}

fn get_item(ctx: &Ctx, pid: &str) -> Option<Value> {
    open_items(ctx).into_iter().find(|i| s_or(get(i, "id")) == pid)
}

fn set(v: &mut Value, k: &str, val: Value) {
    if let Some(o) = v.as_object_mut() {
        o.insert(k.to_string(), val);
    }
}

// ---------------------------------------------------------------- 回答解析

fn re_g() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        RegexBuilder::new(
            r"^(?:1|g|gallery|图|图集|存图|存图片|照片|相册|要图|按图|存成图|1[.、)]?存图|存图[。.!！]?)(?:吧|啊|呗|嗯)?$",
        )
        .case_insensitive(true)
        .build()
        .unwrap()
    })
}

fn re_a() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        RegexBuilder::new(
            r"^(?:2|a|article|文|文章|正文|阅读|存文章|要文章|按文章|存成文章|2[.、)]?存文章|存文章[。.!！]?)(?:吧|啊|呗|嗯)?$",
        )
        .case_insensitive(true)
        .build()
        .unwrap()
    })
}

fn re_d() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r"^(?:3|算了|不存|不存了|别存|别存了|不要了|取消|丢掉|删掉|都不用了|都不用|都不要)(?:吧|啊|呗|嗯)?$",
        )
        .unwrap()
    })
}

fn re_ws() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\s+").unwrap())
}

fn has_verb(s: &str) -> bool {
    s.contains('存') || s.contains('按') || s.contains('当') || s.contains('要')
}

/// 把一句话解析成 (mode, scope)。mode 为空表示「这不是回答」。
pub fn parse_answer(text: &str) -> (String, String) {
    let s = re_ws()
        .replace_all(text, "")
        .trim_matches(|c: char| "。.!！,，~ ".contains(c))
        .to_string();
    if s.is_empty() {
        return (String::new(), String::new());
    }
    // Python: re.sub(r"^回(?:复)?(?=[123])", "", s) —— regex crate 不支持前瞻，
    // 这里等价地写成「剥掉开头那个回/回复，且下一个字符是 1/2/3」。
    let s = {
        let t = s.as_str();
        let stripped = if let Some(rest) = t.strip_prefix('回') {
            Some(rest.strip_prefix('复').unwrap_or(rest))
        } else {
            None
        };
        match stripped {
            Some(rest) if rest.starts_with(['1', '2', '3']) => rest.to_string(),
            _ => t.to_string(),
        }
    };
    if re_d().is_match(&s) {
        return ("drop".into(), "one".into());
    }
    for prefix in ["都", "全", "两条", "全部", "一起", "俩"] {
        if s.starts_with(prefix) && s.chars().count() <= 8 {
            let rest = &s[prefix.len()..];
            if re_g().is_match(rest) {
                return ("gallery".into(), "all".into());
            }
            if re_a().is_match(rest) {
                return ("article".into(), "all".into());
            }
        }
    }
    if re_g().is_match(&s) {
        return ("gallery".into(), "one".into());
    }
    if re_a().is_match(&s) {
        return ("article".into(), "one".into());
    }
    let short = s.chars().count() <= 14;
    let tail = s.ends_with('吧') || s.ends_with('呗') || s.ends_with('啊')
        || s.ends_with('嗯') || s.ends_with('了') || s.ends_with('的');
    if short && (has_verb(&s) || tail || s.chars().count() <= 4) {
        for w in ["算了", "不存", "别存", "不用了", "都不要", "取消", "丢掉", "删掉"] {
            if s.contains(w) {
                return ("drop".into(), "one".into());
            }
        }
        for w in ["存图", "图集", "存图片", "照片", "相册"] {
            if s.contains(w) {
                return ("gallery".into(), "one".into());
            }
        }
        for w in ["存文章", "文章", "正文", "阅读"] {
            if s.contains(w) {
                return ("article".into(), "one".into());
            }
        }
    }
    (String::new(), String::new())
}

fn clean_title(t: &str) -> String {
    static R: OnceLock<Regex> = OnceLock::new();
    let re = R.get_or_init(|| {
        Regex::new(r#"[\s，。！？、,.!?:：；;「」『』《》“”"'()（）\[\]【】|｜/\\-—_·…]+"#).unwrap()
    });
    re.replace_all(t, "").to_string()
}

fn pick_by_title(text: &str, items: &[Value]) -> Option<Value> {
    let t = clean_title(text);
    if t.chars().count() < 3 {
        return None;
    }
    for e in items {
        let title = clean_title(&s_or(get(e, "title")));
        if title.is_empty() {
            continue;
        }
        for n in [10usize, 8, 6, 4, 3] {
            let frag: String = title.chars().take(n).collect();
            if frag.chars().count() >= 3 && t.contains(&frag) {
                return Some(e.clone());
            }
        }
    }
    None
}

fn scope_all(text: &str) -> bool {
    let s = re_ws().replace_all(text, "").to_string();
    ["都", "全部", "两条", "俩", "一起", "全存", "全按"]
        .iter()
        .any(|w| s.contains(w))
}

fn mode_word(mode: &str) -> &'static str {
    if mode == "gallery" {
        "图集"
    } else {
        "文章"
    }
}

/// 用户回了一句话：认出来就处理，认不出来就老实说没认出。
pub fn answer(ctx: &Ctx, text: &str, by: &str) -> Value {
    let (mode, scope) = parse_answer(text);
    if mode.is_empty() {
        return json!({"matched": false, "reason": "not_an_answer"});
    }
    let items = open_items(ctx);
    if items.is_empty() {
        return json!({"matched": false, "reason": "no_pending"});
    }
    let bare = re_ws().replace_all(text, "").to_string();
    if bare.chars().count() <= 1 && now_ts() - num(get(&items[0], "asked_at")) > SHORT_AGE {
        return json!({"matched": false, "reason": "too_old"});
    }
    let todo: Vec<Value> = if scope == "all" || scope_all(&bare) {
        items.clone()
    } else {
        match pick_by_title(&bare, &items) {
            Some(hit) => vec![hit],
            None => vec![items[0].clone()],
        }
    };
    for e in &todo {
        dispatch(ctx, e, &mode, by);
    }
    let left = items.len() as i64 - todo.len() as i64;
    let mut receipt = if mode == "drop" {
        if todo.len() == 1 {
            "好，这条不存了。".to_string()
        } else {
            format!("好，这 {} 条都不存了。", todo.len())
        }
    } else if todo.len() > 1 {
        format!("好，这 {} 条都按{}存，弄好回你。", todo.len(), mode_word(&mode))
    } else {
        let title = s_or(get(&todo[0], "title"));
        let name = if title.is_empty() { "这条".to_string() } else { format!("《{}》", title) };
        format!("好，{}按{}存，弄好回你。", name, mode_word(&mode))
    };
    if left > 0 {
        receipt.push_str(&format!("另外还有 {} 条等你定。", left));
    }
    json!({
        "matched": true,
        "mode": mode,
        "count": todo.len(),
        "ids": todo.iter().map(|e| s_or(get(e, "id"))).collect::<Vec<_>>(),
        "receipt": receipt,
        "job": {"id": get(&todo[0], "job").clone(), "kind": get(&todo[0], "kind").clone()},
    })
}

/// 点名处理某一条（手机页上点按钮走这里）。
pub fn resolve(ctx: &Ctx, pid: &str, mode: &str, by: &str) -> Value {
    if !["gallery", "article", "drop"].contains(&mode) {
        return json!({"ok": false, "error": "mode 只能是 gallery / article / drop"});
    }
    let Some(e) = get_item(ctx, pid) else {
        return json!({"ok": false, "error": "这条已经处理过或过期了"});
    };
    dispatch(ctx, &e, mode, by);
    let word = if mode == "drop" {
        "你说的".to_string()
    } else {
        mode_word(mode).to_string()
    };
    json!({
        "ok": true, "matched": true, "mode": mode,
        "receipt": format!("好，按{}存，弄好回你。", word),
        "job": {"id": e.get("job").cloned().unwrap_or(Value::Null),
                "kind": e.get("kind").cloned().unwrap_or(Value::Null)},
    })
}

fn dispatch(ctx: &Ctx, e: &Value, mode: &str, by: &str) {
    // 先标一下「已经在办了」：处理要几秒，这期间别让同一个问题再被问/再被答
    let pid = s_or(get(e, "id"));
    {
        let mut items = load(ctx);
        for i in items.iter_mut() {
            if s_or(get(i, "id")) == pid {
                set(i, "state", json!("working"));
                set(i, "answered_at", json!(now_ts()));
                set(i, "by", json!(by));
                break;
            }
        }
        save(ctx, &items);
    }
    // Python 这里是后台线程 + linkbox/articlebox 的处理函数；Rust 侧没有那两个盒子，
    // drop 直接落定，另外两种如实记成失败（不假装存成功）。
    let res = if mode == "drop" {
        json!({"ok": true, "message": "这条不存了（已经落盘的留在原处）"})
    } else {
        json!({"ok": false, "message": "处理失败：Rust 侧未接入 linkbox/articlebox"})
    };
    close(ctx, &pid, mode, by, &res);
}

fn close(ctx: &Ctx, pid: &str, mode: &str, by: &str, res: &Value) {
    let mut items = load(ctx);
    for i in items.iter_mut() {
        if s_or(get(i, "id")) == pid {
            set(i, "state", json!(if truthy(get(res, "ok")) { "done" } else { "failed" }));
            set(i, "mode", json!(mode));
            set(i, "answered_at", json!(now_ts()));
            set(i, "by", json!(by));
            set(i, "result", json!(util::cut(&s_or(get(res, "message")), 200)));
            break;
        }
    }
    prune(&mut items);
    save(ctx, &items);
}
