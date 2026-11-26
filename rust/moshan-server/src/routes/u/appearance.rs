// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：外观（皮肤 + 强调色 + 壁纸）的读与 CSS 生成，逐条对齐 `moshan/appearance.py`。
// 不做：不挂路由（路由在 files.rs / pages.rs），不写默认值以外的配置。

#![allow(dead_code)]

use crate::jsonio;
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const SKINS: [&str; 4] = ["classic", "g1", "g2", "g3"];
pub const DEFAULT_ACCENT: &str = "#0a84ff";
pub const BG_KEYS: [&str; 7] = ["dawn", "mist", "glass", "dusk", "moss", "graphite", "custom"];
pub const SKIN_DEFAULT_BG: [(&str, &str); 4] = [
    ("classic", ""),
    ("g1", "dawn"),
    ("g2", "mist"),
    ("g3", "glass"),
];

const VEIL_LIGHT: &str = "linear-gradient(180deg, rgba(255,255,255,.24), \
rgba(255,255,255,.10) 52%, rgba(255,255,255,.18))";
const VEIL_DARK: &str = "linear-gradient(180deg, rgba(6,10,18,.40), \
rgba(6,10,18,.55))";

/// 三段皮肤的静态 CSS（`_skin_g1()+_skin_g2()+_skin_g3()` 的原文，编译期嵌入）。
const SKINS_CSS: &str = include_str!("assets/appearance_skins.css");

const WALL_HEAD: &str = r#"/* ===================== 背景 =====================
   强调色是全局一个值（四套皮肤天然共用）；背景反过来 ——
   auto 时各皮肤用自己的默认底，选了某一张则是四套共用那一张。 */
:root{--wall:none}
:root[data-theme="dark"]{--wall:none}
"#;

const CLASSIC_BODY: &str = r#":root[data-skin="classic"] body{
  background-image:var(--wall, none);
  background-color:var(--bg);
  background-size:cover;background-position:center;background-attachment:fixed;
}"#;

const ACCENT_TPL: &str = r#"
/* ===================== 强调色（只作用于 g1/g2/g3） ===================== */
:root:not([data-skin="classic"]):not([data-theme="dark"]){
  --pri:@@LIGHT@@;
  --pri-hi:color-mix(in srgb, var(--pri) 62%, #fff);
  --pri-solid:var(--pri);
  --pri-soft:color-mix(in srgb, var(--pri) 11%, var(--card));
  --pri-line:color-mix(in srgb, var(--pri) 30%, var(--card));
  --sh-pri:0 6px 18px -8px color-mix(in srgb, var(--pri) 55%, transparent);
}
:root:not([data-skin="classic"])[data-theme="dark"]{
  --pri:@@DARK@@;
  --pri-hi:color-mix(in srgb, var(--pri) 58%, #fff);
  --pri-solid:color-mix(in srgb, var(--pri) 82%, #000);
  --pri-soft:color-mix(in srgb, var(--pri) 22%, var(--card));
  --pri-line:color-mix(in srgb, var(--pri) 42%, var(--card));
  --sh-pri:0 6px 20px -8px color-mix(in srgb, var(--pri) 65%, transparent);
}
"#;

pub struct Settings {
    pub skin: String,
    pub accent: String,
    pub bg: String,
}

fn root(cfg: &Value) -> PathBuf {
    if let Some(first) = cfg
        .get("allowed_roots")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    {
        return PathBuf::from(first);
    }
    let inbox = cfg.get("inbox_dir").and_then(|v| v.as_str()).unwrap_or(".");
    Path::new(inbox)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn folder(cfg: &Value) -> PathBuf {
    root(cfg).join("_工作台")
}

pub fn json_path(cfg: &Value) -> PathBuf {
    folder(cfg).join("appearance.json")
}

pub fn css_path(cfg: &Value) -> PathBuf {
    folder(cfg).join("appearance.css")
}

pub fn bg_path(cfg: &Value) -> PathBuf {
    folder(cfg).join("appearance-bg.jpg")
}

pub fn wall_dir(cfg: &Value) -> PathBuf {
    folder(cfg).join("wallpapers")
}

pub fn wall_path(cfg: &Value, key: &str) -> PathBuf {
    let k = key.trim();
    if !BG_KEYS.contains(&k) {
        return wall_dir(cfg).join("_none_.jpg");
    }
    wall_dir(cfg).join(format!("{k}.jpg"))
}

pub fn has_wall(cfg: &Value, key: &str) -> bool {
    wall_path(cfg, key).is_file()
}

pub fn wall_ver(cfg: &Value, key: &str) -> String {
    if key != "custom" {
        return String::new();
    }
    match std::fs::metadata(wall_path(cfg, key)).and_then(|m| m.modified()) {
        Ok(t) => match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => format!("?v={}", d.as_secs() as i64),
            Err(_) => String::new(),
        },
        Err(_) => String::new(),
    }
}

fn norm_hex(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix('#').unwrap_or(t);
    if t.len() == 6 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        format!("#{}", t.to_lowercase())
    } else {
        String::new()
    }
}

pub fn load(cfg: &Value) -> Settings {
    let mut out = Settings {
        skin: "classic".to_string(),
        accent: DEFAULT_ACCENT.to_string(),
        bg: "auto".to_string(),
    };
    let data = jsonio::read_json(&json_path(cfg)).unwrap_or(Value::Null);
    if let Some(d) = data.as_object() {
        let skin = d.get("skin").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if SKINS.contains(&skin.as_str()) {
            out.skin = skin;
        }
        let accent = norm_hex(d.get("accent").and_then(|v| v.as_str()).unwrap_or(""));
        if !accent.is_empty() {
            out.accent = accent;
        }
        let bg = d.get("bg").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if bg == "auto" || BG_KEYS.contains(&bg.as_str()) {
            out.bg = bg;
        }
    }
    out
}

pub fn effective_bg(cfg: &Value) -> String {
    let a = load(cfg);
    let key = if a.bg == "auto" {
        SKIN_DEFAULT_BG
            .iter()
            .find(|(s, _)| *s == a.skin)
            .map(|(_, k)| *k)
            .unwrap_or("")
            .to_string()
    } else {
        a.bg.clone()
    };
    if has_wall(cfg, &key) {
        key
    } else {
        String::new()
    }
}

// ---------------------------------------------------------------- 强调色

fn rgb(h: &str) -> (f64, f64, f64) {
    let h = h.trim_start_matches('#');
    let g = |i: usize| -> f64 {
        u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0) as f64 / 255.0
    };
    (g(0), g(2), g(4))
}

fn hex_of((r, g, b): (f64, f64, f64)) -> String {
    let f = |v: f64| -> i64 {
        let x = crate::routes::u::util::py_round(v * 255.0);
        x.clamp(0.0, 255.0) as i64
    };
    format!("#{:02x}{:02x}{:02x}", f(r), f(g), f(b))
}

/// Python `colorsys.rgb_to_hls`
fn rgb_to_hls(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let maxc = r.max(g).max(b);
    let minc = r.min(g).min(b);
    let sum = maxc + minc;
    let range = maxc - minc;
    let l = sum / 2.0;
    if range == 0.0 {
        return (0.0, l, 0.0);
    }
    let s = if l <= 0.5 { range / sum } else { range / (2.0 - maxc - minc) };
    let rc = (maxc - r) / range;
    let gc = (maxc - g) / range;
    let bc = (maxc - b) / range;
    let h = if r == maxc {
        bc - gc
    } else if g == maxc {
        2.0 + rc - bc
    } else {
        4.0 + gc - rc
    };
    let h = (h / 6.0) % 1.0;
    let h = if h < 0.0 { h + 1.0 } else { h };
    (h, l, s)
}

/// Python `colorsys.hls_to_rgb`
fn hls_to_rgb(h: f64, l: f64, s: f64) -> (f64, f64, f64) {
    if s == 0.0 {
        return (l, l, l);
    }
    let m2 = if l <= 0.5 { l * (1.0 + s) } else { l + s - (l * s) };
    let m1 = 2.0 * l - m2;
    let hue = |mut h: f64| -> f64 {
        if h < 0.0 {
            h += 1.0;
        }
        if h > 1.0 {
            h -= 1.0;
        }
        if h * 6.0 < 1.0 {
            m1 + (m2 - m1) * h * 6.0
        } else if h * 2.0 < 1.0 {
            m2
        } else if h * 3.0 < 2.0 {
            m1 + (m2 - m1) * (2.0 / 3.0 - h) * 6.0
        } else {
            m1
        }
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

fn lum(h: &str) -> f64 {
    let f = |c: f64| -> f64 {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = rgb(h);
    0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
}

fn contrast(a: &str, b: &str) -> f64 {
    let la = lum(a);
    let lb = lum(b);
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn tune_floor(hh: f64, ss: f64, bg: &str, mut lo: f64, mut hi: f64, dark: bool) -> String {
    let target = 3.4;
    let mut best: Option<String> = None;
    for _ in 0..18 {
        let mid = (lo + hi) / 2.0;
        let cand = hex_of(hls_to_rgb(hh, mid, ss));
        let ok = contrast(&cand, bg) >= target;
        if ok {
            best = Some(cand);
            if dark {
                hi = mid;
            } else {
                lo = mid;
            }
        } else if dark {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    best.unwrap_or_else(|| hex_of(hls_to_rgb(hh, 0.45, ss)))
}

pub fn tune(h: &str, dark: bool) -> String {
    let (r, g, b) = rgb(h);
    let (hh, ll, ss) = rgb_to_hls(r, g, b);
    let bg = if dark { "#0c1016" } else { "#ffffff" };
    let at = |level: f64| hex_of(hls_to_rgb(hh, level, ss));
    if contrast(&at(ll), bg) >= 3.4 {
        return at(ll);
    }
    tune_floor(hh, ss, bg, 0.0, 1.0, dark)
}

// ---------------------------------------------------------------- CSS 生成

fn wall_block(cfg: &Value) -> String {
    let a = load(cfg);
    let ver = wall_ver(cfg, "custom");
    let light_tpl = format!("{VEIL_LIGHT}, url(\"/__wall/@@K@@.jpg@@V@@\")");
    let dark_tpl = format!("{VEIL_DARK}, url(\"/__wall/@@K@@.jpg@@V@@\")");
    let fill = |tpl: &str, k: &str| -> String {
        tpl.replace("@@K@@", k)
            .replace("@@V@@", if k == "custom" { &ver } else { "" })
    };
    let mut lines: Vec<String> = Vec::new();
    if a.bg == "auto" {
        for (skin, key) in SKIN_DEFAULT_BG.iter() {
            if key.is_empty() || !has_wall(cfg, key) {
                continue;
            }
            lines.push(format!(
                ":root[data-skin=\"{skin}\"]{{--wall:{}}}",
                fill(&light_tpl, key)
            ));
            lines.push(format!(
                ":root[data-skin=\"{skin}\"][data-theme=\"dark\"]{{--wall:{}}}",
                fill(&dark_tpl, key)
            ));
        }
    } else if has_wall(cfg, &a.bg) {
        let k = a.bg.clone();
        lines.push(format!(":root{{--wall:{}}}", fill(&light_tpl, &k)));
        lines.push(format!(
            ":root[data-theme=\"dark\"]{{--wall:{}}}",
            fill(&dark_tpl, &k)
        ));
    }
    lines.push(CLASSIC_BODY.to_string());
    format!("{WALL_HEAD}{}\n", lines.join("\n"))
}

fn accent_block(light: &str, dark: &str) -> String {
    ACCENT_TPL.replace("@@LIGHT@@", light).replace("@@DARK@@", dark)
}

pub fn css(cfg: &Value) -> String {
    let a = load(cfg);
    let eff = effective_bg(cfg);
    let head = format!(
        "/* 由 pocket/appearance.py 生成 —— 别手改，改 json 或设置面板。\n   皮肤 {} · 强调色 {} · 背景 {}{} · 生成于 {} */\n",
        a.skin,
        a.accent,
        a.bg,
        if eff.is_empty() {
            String::new()
        } else {
            format!("（{eff}）")
        },
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
    );
    format!(
        "{head}{SKINS_CSS}{}{}",
        wall_block(cfg),
        accent_block(&tune(&a.accent, false), &tune(&a.accent, true))
    )
}

/// `appmod.sync()`：生成并落盘，返回内容。
pub fn sync(cfg: &Value) -> String {
    let text = css(cfg);
    let p = css_path(cfg);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, text.as_bytes());
    text
}

pub fn bootstrap(cfg: &Value, href: &str) -> String {
    let a = load(cfg);
    format!(
        "<link rel=\"stylesheet\" href=\"{href}\">\n<script>(function(){{document.documentElement.dataset.skin={};}})();</script>\n",
        serde_json::to_string(&a.skin).unwrap_or_else(|_| "\"classic\"".to_string())
    )
}
