// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：`/api/ext/*` —— 第三方插件配方（extplugins.py 的逐条移植）。
// 不做：不改 dash.py 的插件探活；这里只负责 ext 这十条路由用得到的部分。

#![allow(dead_code)]

use super::util::{self, expanduser, py_str, s_of, trunc};
use crate::routes::common::{body_obj, ok_err, peek, q, CtxState};
use axum::extract::State;
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

// ---------------------------------------------------------------- 常量

const RECIPE_FILE: &str = "第三方插件.json";
const PROMPT_FILE: &str = "第三方插件-Prompt.md";
const MODEL_FILE: &str = "第三方插件-模型.json";
const LOG_TAIL: usize = 40;

const CAT_ORDER: &[&str] = &["传输", "同步", "播放", "音视频", "图像", "效率", "系统", "开发", "模型"];
const ROW_FIELDS: &[&str] = &["id", "name", "note", "glyph", "cat", "web", "port", "brew", "brew_kind"];
const RECIPE_FIELDS: &[&str] = &[
    "id", "name", "cat", "glyph", "note", "desc", "app", "bundle", "exe", "proc", "cli", "port",
    "web", "brew", "brew_kind", "start", "home", "repo",
];

fn sv(s: &str) -> Value {
    Value::String(s.to_string())
}

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        m.insert(k.to_string(), v);
    }
    Value::Object(m)
}

fn app_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["/Applications", "/System/Applications", "/Applications/Utilities", "/System/Applications/Utilities"]
        .iter()
        .map(PathBuf::from)
        .collect();
    v.push(util::home().join("Applications"));
    v
}

/// 内置目录。字段顺序与 extplugins.CATALOG 保持一字不差 —— `/api/ext/list`
/// 的每一行都是直接读这些键的。
fn catalog_rows() -> Vec<Value> {
    vec![
        obj(vec![
            ("id", sv("localsend")), ("name", sv("LocalSend")), ("cat", sv("传输")),
            ("glyph", sv("ext")), ("note", sv("同一 Wi-Fi 下跨设备传文件")),
            ("desc", sv("手机、电脑连同一个局域网就能互传文件／照片，不经过服务器、不压缩画质。")),
            ("app", sv("/Applications/LocalSend.app")), ("bundle", sv("org.localsend.localsend_app")),
            ("exe", sv("LocalSend")), ("proc", sv("/LocalSend.app/Contents/MacOS/LocalSend")),
            ("port", json!(53317)), ("brew", sv("localsend")), ("brew_kind", sv("cask")),
            ("home", sv("https://localsend.org")), ("repo", sv("https://github.com/localsend/localsend")),
        ]),
        obj(vec![
            ("id", sv("syncthing")), ("name", sv("Syncthing")), ("cat", sv("同步")),
            ("glyph", sv("ext")), ("note", sv("多台机器之间同步文件夹")),
            ("desc", sv("点对点同步，不经过云盘。自带网页管理界面，加完点开就能配。")),
            ("cli", sv("syncthing")), ("exe", sv("syncthing")), ("proc", sv("syncthing serve")),
            ("port", json!(8384)), ("web", sv("http://127.0.0.1:8384/")),
            ("start", json!(["syncthing", "serve", "--no-browser", "--no-restart"])),
            ("brew", sv("syncthing")), ("brew_kind", sv("formula")),
            ("home", sv("https://syncthing.net")), ("repo", sv("https://github.com/syncthing/syncthing")),
        ]),
        obj(vec![
            ("id", sv("iina")), ("name", sv("IINA")), ("cat", sv("播放")), ("glyph", sv("ext")),
            ("note", sv("macOS 上的开源播放器")),
            ("desc", sv("能放的格式比 QuickTime 多得多，支持画中画和触控板手势。")),
            ("app", sv("/Applications/IINA.app")), ("bundle", sv("com.colliderli.iina")),
            ("exe", sv("IINA")), ("proc", sv("/IINA.app/Contents/MacOS/IINA")),
            ("brew", sv("iina")), ("brew_kind", sv("cask")),
            ("home", sv("https://iina.io")), ("repo", sv("https://github.com/iina/iina")),
        ]),
        obj(vec![
            ("id", sv("rectangle")), ("name", sv("Rectangle")), ("cat", sv("效率")), ("glyph", sv("ext")),
            ("note", sv("用快捷键摆窗口")),
            ("desc", sv("开源窗口管理：⌃⌥ + 方向键把窗口摆到左半屏／右半屏／最大化。")),
            ("app", sv("/Applications/Rectangle.app")), ("bundle", sv("com.knollsoft.Rectangle")),
            ("exe", sv("Rectangle")), ("proc", sv("/Rectangle.app/Contents/MacOS/Rectangle")),
            ("brew", sv("rectangle")), ("brew_kind", sv("cask")),
            ("home", sv("https://rectangleapp.com")), ("repo", sv("https://github.com/rxhanson/Rectangle")),
        ]),
        obj(vec![
            ("id", sv("ollama")), ("name", sv("Ollama")), ("cat", sv("模型")), ("glyph", sv("ext")),
            ("note", sv("本机跑开源大模型")),
            ("desc", sv("一条命令把开源模型拉下来在本机跑，中台里的本地模型就走它。")),
            ("cli", sv("ollama")), ("exe", sv("ollama")), ("proc", sv("ollama serve")),
            ("port", json!(11434)), ("start", json!(["ollama", "serve"])),
            ("brew", sv("ollama")), ("brew_kind", sv("formula")),
            ("home", sv("https://ollama.com")), ("repo", sv("https://github.com/ollama/ollama")),
        ]),
        obj(vec![
            ("id", sv("cyberduck")), ("name", sv("Cyberduck")), ("cat", sv("传输")), ("glyph", sv("ext")),
            ("note", sv("连服务器 / 云盘")),
            ("desc", sv("SFTP、FTP、WebDAV、S3 和常见网盘都能挂，连服务器像逛 Finder。")),
            ("app", sv("/Applications/Cyberduck.app")), ("brew", sv("cyberduck")),
            ("brew_kind", sv("cask")), ("home", sv("https://cyberduck.io")),
        ]),
        obj(vec![
            ("id", sv("transmission")), ("name", sv("Transmission")), ("cat", sv("传输")), ("glyph", sv("ext")),
            ("note", sv("BT 下载")),
            ("desc", sv("轻量 BT 客户端，没广告；下片子、下镜像都稳。")),
            ("app", sv("/Applications/Transmission.app")), ("brew", sv("transmission")),
            ("brew_kind", sv("cask")), ("home", sv("https://transmissionbt.com")),
        ]),
        obj(vec![
            ("id", sv("vlc")), ("name", sv("VLC")), ("cat", sv("播放")), ("glyph", sv("media")),
            ("note", sv("万能播放器")),
            ("desc", sv("几乎什么格式都能放，没下完的文件、坏掉的文件也能播。")),
            ("app", sv("/Applications/VLC.app")), ("brew", sv("vlc")),
            ("brew_kind", sv("cask")), ("home", sv("https://www.videolan.org/vlc/")),
        ]),
        obj(vec![
            ("id", sv("jellyfin-media-player")), ("name", sv("Jellyfin")), ("cat", sv("播放")), ("glyph", sv("media")),
            ("note", sv("连家里的 Jellyfin")),
            ("desc", sv("官方桌面客户端：连家里盒子的 Jellyfin，原盘直放、不转码。")),
            ("app", sv("/Applications/Jellyfin Media Player.app")), ("brew", sv("jellyfin-media-player")),
            ("brew_kind", sv("cask")), ("home", sv("https://jellyfin.org")),
        ]),
        obj(vec![
            ("id", sv("handbrake")), ("name", sv("HandBrake")), ("cat", sv("音视频")), ("glyph", sv("media")),
            ("note", sv("视频转码压体积")),
            ("desc", sv("把大视频压小、转格式，预设直接选 Apple 设备或网页用。")),
            ("app", sv("/Applications/HandBrake.app")), ("brew", sv("handbrake")),
            ("brew_kind", sv("cask")), ("home", sv("https://handbrake.fr")),
        ]),
        obj(vec![
            ("id", sv("obs")), ("name", sv("OBS")), ("cat", sv("音视频")), ("glyph", sv("media")),
            ("note", sv("录屏与直播")),
            ("desc", sv("录屏幕、录摄像头、多路推流，教程和直播都用它。")),
            ("app", sv("/Applications/OBS.app")), ("brew", sv("obs")),
            ("brew_kind", sv("cask")), ("home", sv("https://obsproject.com")),
        ]),
        obj(vec![
            ("id", sv("krita")), ("name", sv("Krita")), ("cat", sv("图像")), ("glyph", sv("gallery")),
            ("note", sv("开源绘画")),
            ("desc", sv("笔刷、图层、动画都齐的绘画软件，画插画和分镜够用。")),
            ("app", sv("/Applications/krita.app")), ("brew", sv("krita")),
            ("brew_kind", sv("cask")), ("home", sv("https://krita.org")),
        ]),
        obj(vec![
            ("id", sv("gimp")), ("name", sv("GIMP")), ("cat", sv("图像")), ("glyph", sv("gallery")),
            ("note", sv("开源修图")),
            ("desc", sv("接近 Photoshop 的那一套：图层、蒙版、滤镜，修图和抠图都能干。")),
            ("app", sv("/Applications/GIMP.app")), ("brew", sv("gimp")),
            ("brew_kind", sv("cask")), ("home", sv("https://www.gimp.org")),
        ]),
        obj(vec![
            ("id", sv("inkscape")), ("name", sv("Inkscape")), ("cat", sv("图像")), ("glyph", sv("gallery")),
            ("note", sv("矢量图 / SVG")),
            ("desc", sv("画 SVG、图标、示意图，输出能直接塞进网页。")),
            ("app", sv("/Applications/Inkscape.app")), ("brew", sv("inkscape")),
            ("brew_kind", sv("cask")), ("home", sv("https://inkscape.org")),
        ]),
        obj(vec![
            ("id", sv("alt-tab")), ("name", sv("AltTab")), ("cat", sv("效率")), ("glyph", sv("devices")),
            ("note", sv("按窗口切 App")),
            ("desc", sv("⌥+Tab 按窗口切换，而不是按 App；多窗口的活儿快很多。")),
            ("app", sv("/Applications/AltTab.app")), ("brew", sv("alt-tab")),
            ("brew_kind", sv("cask")), ("home", sv("https://alt-tab.app")),
        ]),
        obj(vec![
            ("id", sv("maccy")), ("name", sv("Maccy")), ("cat", sv("效率")), ("glyph", sv("devices")),
            ("note", sv("剪贴板历史")),
            ("desc", sv("⇧⌘C 翻之前复制过的东西，找刚复制的那段文字不用重来。")),
            ("app", sv("/Applications/Maccy.app")), ("brew", sv("maccy")),
            ("brew_kind", sv("cask")), ("home", sv("https://maccy.app")),
        ]),
        obj(vec![
            ("id", sv("itsycal")), ("name", sv("Itsycal")), ("cat", sv("效率")), ("glyph", sv("devices")),
            ("note", sv("菜单栏日历")),
            ("desc", sv("菜单栏显示日期，点开看日程，点某天直接开系统日历。")),
            ("app", sv("/Applications/Itsycal.app")), ("brew", sv("itsycal")),
            ("brew_kind", sv("cask")), ("home", sv("https://www.mowglii.com/itsycal/")),
        ]),
        obj(vec![
            ("id", sv("hiddenbar")), ("name", sv("Hidden Bar")), ("cat", sv("效率")), ("glyph", sv("devices")),
            ("note", sv("收起菜单栏图标")),
            ("desc", sv("把不常用的菜单栏图标折起来，只留想看的。")),
            ("app", sv("/Applications/Hidden Bar.app")), ("brew", sv("hiddenbar")),
            ("brew_kind", sv("cask")), ("home", sv("https://github.com/dwarvesf/hidden")),
        ]),
        obj(vec![
            ("id", sv("keepingyouawake")), ("name", sv("KeepingYouAwake")), ("cat", sv("效率")), ("glyph", sv("devices")),
            ("note", sv("一键防休眠")),
            ("desc", sv("菜单栏一个小咖啡杯，点了就不休眠；跑长任务、下大文件时用。")),
            ("app", sv("/Applications/KeepingYouAwake.app")), ("brew", sv("keepingyouawake")),
            ("brew_kind", sv("cask")), ("home", sv("https://keepingyouawake.app")),
        ]),
        obj(vec![
            ("id", sv("espanso")), ("name", sv("Espanso")), ("cat", sv("效率")), ("glyph", sv("devices")),
            ("note", sv("打字自动展开")),
            ("desc", sv("打一个缩写就展开成整段文字（地址、签名、常用回复），所有 App 都生效。")),
            ("app", sv("/Applications/Espanso.app")), ("brew", sv("espanso")),
            ("brew_kind", sv("cask")), ("home", sv("https://espanso.org")),
        ]),
        obj(vec![
            ("id", sv("stats")), ("name", sv("Stats")), ("cat", sv("系统")), ("glyph", sv("inspect")),
            ("note", sv("菜单栏系统监控")),
            ("desc", sv("菜单栏看 CPU、内存、温度、网速、磁盘，比活动监视器顺手。")),
            ("app", sv("/Applications/Stats.app")), ("brew", sv("stats")),
            ("brew_kind", sv("cask")), ("home", sv("https://github.com/exelban/stats")),
        ]),
        obj(vec![
            ("id", sv("keka")), ("name", sv("Keka")), ("cat", sv("系统")), ("glyph", sv("ext")),
            ("note", sv("压缩解压")),
            ("desc", sv("7z、rar、tar、zip 都认，比系统自带的归档实用工具全。")),
            ("app", sv("/Applications/Keka.app")), ("brew", sv("keka")),
            ("brew_kind", sv("cask")), ("home", sv("https://www.keka.io")),
        ]),
        obj(vec![
            ("id", sv("imageoptim")), ("name", sv("ImageOptim")), ("cat", sv("系统")), ("glyph", sv("gallery")),
            ("note", sv("图片瘦身")),
            ("desc", sv("把图拖进去就压小，肉眼几乎看不出差别；发网页、发文章前用。")),
            ("app", sv("/Applications/ImageOptim.app")), ("brew", sv("imageoptim")),
            ("brew_kind", sv("cask")), ("home", sv("https://imageoptim.com/mac")),
        ]),
        obj(vec![
            ("id", sv("balenaetcher")), ("name", sv("Etcher")), ("cat", sv("系统")), ("glyph", sv("ext")),
            ("note", sv("镜像写 U 盘")),
            ("desc", sv("把系统镜像写到 U 盘 / SD 卡，给盒子、树莓派刷机用。")),
            ("app", sv("/Applications/balenaEtcher.app")), ("brew", sv("balenaetcher")),
            ("brew_kind", sv("cask")), ("home", sv("https://etcher.balena.io")),
        ]),
        obj(vec![
            ("id", sv("keepassxc")), ("name", sv("KeePassXC")), ("cat", sv("系统")), ("glyph", sv("ext")),
            ("note", sv("本地密码库")),
            ("desc", sv("所有密码存一个加密文件，换机器拷走就能用，不依赖云。")),
            ("app", sv("/Applications/KeePassXC.app")), ("brew", sv("keepassxc")),
            ("brew_kind", sv("cask")), ("home", sv("https://keepassxc.org")),
        ]),
        obj(vec![
            ("id", sv("iterm2")), ("name", sv("iTerm2")), ("cat", sv("开发")), ("glyph", sv("github")),
            ("note", sv("增强终端")),
            ("desc", sv("系统终端的加强版：分屏、搜索、粘贴历史、触发器。")),
            ("app", sv("/Applications/iTerm.app")), ("brew", sv("iterm2")),
            ("brew_kind", sv("cask")), ("home", sv("https://iterm2.com")),
        ]),
        obj(vec![
            ("id", sv("vscode")), ("name", sv("VS Code")), ("cat", sv("开发")), ("glyph", sv("github")),
            ("note", sv("写代码")),
            ("desc", sv("编辑器里插件生态最大的一个，远程开发、Jupyter 都能接。")),
            ("app", sv("/Applications/Visual Studio Code.app")), ("brew", sv("visual-studio-code")),
            ("brew_kind", sv("cask")), ("home", sv("https://code.visualstudio.com")),
        ]),
        obj(vec![
            ("id", sv("zed")), ("name", sv("Zed")), ("cat", sv("开发")), ("glyph", sv("github")),
            ("note", sv("新编辑器")),
            ("desc", sv("Rust 写的新编辑器，开大文件和搜索都很快，内置协作。")),
            ("app", sv("/Applications/Zed.app")), ("brew", sv("zed")),
            ("brew_kind", sv("cask")), ("home", sv("https://zed.dev")),
        ]),
        obj(vec![
            ("id", sv("ghostty")), ("name", sv("Ghostty")), ("cat", sv("开发")), ("glyph", sv("github")),
            ("note", sv("GPU 终端")),
            ("desc", sv("GPU 渲染的终端，启动快、配置简单，原生支持分屏。")),
            ("app", sv("/Applications/Ghostty.app")), ("brew", sv("ghostty")),
            ("brew_kind", sv("cask")), ("home", sv("https://ghostty.org")),
        ]),
        obj(vec![
            ("id", sv("bruno")), ("name", sv("Bruno")), ("cat", sv("开发")), ("glyph", sv("github")),
            ("note", sv("API 调试")),
            ("desc", sv("调接口的客户端，集合存本地文件夹、能进 git，不往云上传。")),
            ("app", sv("/Applications/Bruno.app")), ("brew", sv("bruno")),
            ("brew_kind", sv("cask")), ("home", sv("https://www.usebruno.com")),
        ]),
        obj(vec![
            ("id", sv("dbeaver")), ("name", sv("DBeaver")), ("cat", sv("开发")), ("glyph", sv("github")),
            ("note", sv("数据库客户端")),
            ("desc", sv("连 MySQL、Postgres、SQLite 的图形客户端，SQL 补全和导出都顺手。")),
            ("app", sv("/Applications/DBeaver.app")), ("brew", sv("dbeaver-community")),
            ("brew_kind", sv("cask")), ("home", sv("https://dbeaver.io")),
        ]),
        obj(vec![
            ("id", sv("jan")), ("name", sv("Jan")), ("cat", sv("模型")), ("glyph", sv("llmcost")),
            ("note", sv("本地模型聊天")),
            ("desc", sv("在桌面端跑本地模型的聊天客户端，断网也能用。")),
            ("app", sv("/Applications/Jan.app")), ("brew", sv("jan")),
            ("brew_kind", sv("cask")), ("home", sv("https://jan.ai")),
        ]),
        obj(vec![
            ("id", sv("chatbox")), ("name", sv("Chatbox")), ("cat", sv("模型")), ("glyph", sv("llmcost")),
            ("note", sv("多家模型客户端")),
            ("desc", sv("一个客户端接各家模型的 API，桌面端能存多套对话。")),
            ("app", sv("/Applications/Chatbox.app")), ("brew", sv("chatbox")),
            ("brew_kind", sv("cask")), ("home", sv("https://chatboxai.app")),
        ]),
    ]
}

fn builtin_ids() -> Vec<String> {
    catalog_rows()
        .iter()
        .filter_map(|r| r.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect()
}

// ---------------------------------------------------------------- 配方读写

fn recipes_path(cfg: &Value) -> PathBuf {
    util::data_root(cfg).join("_工作台").join(RECIPE_FILE)
}

fn prompt_path(cfg: &Value) -> PathBuf {
    util::data_root(cfg).join("_工作台").join(PROMPT_FILE)
}

fn model_path(cfg: &Value) -> PathBuf {
    util::data_root(cfg).join("_工作台").join(MODEL_FILE)
}

fn custom_recipes(cfg: &Value) -> Vec<Value> {
    let d = util::read_json(&recipes_path(cfg)).unwrap_or(Value::Null);
    let rows = match &d {
        Value::Object(o) => o.get("recipes").cloned(),
        Value::Array(_) => Some(d.clone()),
        _ => None,
    };
    rows.and_then(|v| v.as_array().cloned())
        .map(|a| {
            a.into_iter()
                .filter(|r| {
                    r.is_object()
                        && !s_of(r.get("id")).is_empty()
                        && !s_of(r.get("name")).is_empty()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn all_recipes(cfg: &Value) -> Vec<Value> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for r in custom_recipes(cfg) {
        let id = s_of(r.get("id"));
        if !seen.contains(&id) {
            seen.push(id);
            out.push(r);
        }
    }
    for r in catalog_rows() {
        let id = s_of(r.get("id"));
        if !seen.contains(&id) {
            seen.push(id);
            out.push(r);
        }
    }
    out
}

fn recipe(cfg: &Value, rid: &str) -> Option<Value> {
    let rid = rid.trim();
    all_recipes(cfg)
        .into_iter()
        .find(|r| s_of(r.get("id")) == rid)
}

fn save_recipe(cfg: &Value, r: &Value) -> std::io::Result<()> {
    let p = recipes_path(cfg);
    let rid = s_of(r.get("id"));
    let mut rows: Vec<Value> = custom_recipes(cfg)
        .into_iter()
        .filter(|x| s_of(x.get("id")) != rid)
        .collect();
    rows.push(r.clone());
    let payload = obj(vec![
        ("_说明", sv("用户自己加的第三方插件配方。粘 .app 路径时自动生成，也能手改。")),
        ("recipes", Value::Array(rows)),
        ("updated_at", sv(&util::now_iso())),
    ]);
    let tmp = util::with_json_tmp(&p);
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, crate::jsonio::dumps(&payload, 1))?;
    std::fs::rename(&tmp, &p)
}

// ---------------------------------------------------------------- 探测

fn find_app(query: &str) -> Option<PathBuf> {
    let q = query.trim().trim_end_matches('/');
    if q.is_empty() {
        return None;
    }
    let p = expanduser(q);
    if q.to_lowercase().ends_with(".app") && p.is_dir() {
        return Some(p);
    }
    let base_name = util::name_of(&p);
    let name = if base_name.to_lowercase().ends_with(".app") {
        base_name[..base_name.len() - 4].to_string()
    } else {
        base_name
    };
    if name.is_empty() {
        return None;
    }
    for d in app_dirs() {
        if !d.is_dir() {
            continue;
        }
        for cand in [d.join(format!("{name}.app")), d.join(&name)] {
            if cand.is_dir() {
                return Some(cand);
            }
        }
    }
    let ql = name.to_lowercase();
    let mut hits: Vec<PathBuf> = Vec::new();
    for d in app_dirs() {
        if !d.is_dir() {
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            for ent in rd.flatten() {
                let p = ent.path();
                if util::suffix_of(&p).to_lowercase() != ".app" {
                    continue;
                }
                let nl = util::stem_of(&p).to_lowercase();
                if nl == ql {
                    return Some(p);
                }
                if nl.contains(&ql) {
                    hits.push(p);
                }
            }
        }
    }
    if !hits.is_empty() {
        hits.sort_by_key(|x| util::stem_of(x).chars().count());
        return hits.into_iter().next();
    }
    for d in app_dirs() {
        if !d.is_dir() {
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&d) {
            for ent in rd.flatten() {
                let sub = ent.path();
                if !sub.is_dir() {
                    continue;
                }
                if let Ok(rd2) = std::fs::read_dir(&sub) {
                    for e2 in rd2.flatten() {
                        let p = e2.path();
                        if util::suffix_of(&p).to_lowercase() == ".app"
                            && util::stem_of(&p).to_lowercase().contains(&ql)
                        {
                            return Some(p);
                        }
                    }
                }
            }
        }
    }
    None
}

fn plist_json(app: &Path) -> Option<Value> {
    let pl = app.join("Contents").join("Info.plist");
    if !pl.is_file() {
        return None;
    }
    let argv = vec![
        "/usr/bin/plutil".to_string(),
        "-convert".into(),
        "json".into(),
        "-o".into(),
        "-".into(),
        pl.to_string_lossy().to_string(),
    ];
    let (rc, out, _) = util::run_cmd(&argv, 8, None)?;
    if rc != 0 {
        return None;
    }
    serde_json::from_str(&out).ok()
}

fn app_info(app: &Path) -> Value {
    let mut info = obj(vec![
        ("bundle", sv("")),
        ("exe", sv(&util::stem_of(app))),
        ("version", sv("")),
    ]);
    let Some(d) = plist_json(app) else { return info };
    if d.is_object() {
        if let Some(o) = info.as_object_mut() {
            let bundle = d.get("CFBundleIdentifier").map(py_str).unwrap_or_default();
            o.insert("bundle".into(), sv(&bundle));
            let exe = match d.get("CFBundleExecutable") {
                Some(Value::Null) | None => util::stem_of(app),
                Some(x) => py_str(x),
            };
            o.insert("exe".into(), sv(&exe));
            let ver = d.get("CFBundleShortVersionString").map(py_str).unwrap_or_default();
            o.insert("version".into(), sv(&ver));
        }
    }
    info
}

fn brew_installed(r: &Value) -> bool {
    let b = s_of(r.get("brew"));
    if b.is_empty() {
        return false;
    }
    let sub = if s_of(r.get("brew_kind")) == "cask" { "Caskroom" } else { "Cellar" };
    for prefix in ["/opt/homebrew", "/usr/local"] {
        if Path::new(prefix).join(sub).join(&b).is_dir() {
            return true;
        }
    }
    false
}

fn pgrep_exact(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let argv = vec![
        "/usr/bin/pgrep".to_string(),
        "-x".into(),
        "-q".into(),
        name.to_string(),
    ];
    matches!(util::run_cmd(&argv, 4, None), Some((0, _, _)))
}

fn exe_name(r: &Value) -> String {
    let app = s_of(r.get("app")).trim().to_string();
    if !app.is_empty() {
        let p = expanduser(&app);
        if p.is_dir() {
            let got = s_of(app_info(&p).get("exe"));
            if !got.is_empty() {
                return got;
            }
        }
    }
    let exe = s_of(r.get("exe")).trim().to_string();
    if !exe.is_empty() {
        return exe;
    }
    let pat = s_of(r.get("proc")).trim().to_string();
    if pat.is_empty() {
        return String::new();
    }
    if pat.contains('/') {
        util::name_of(Path::new(&pat))
    } else {
        pat.split_whitespace().next().unwrap_or("").to_string()
    }
}

fn bundle_id(r: &Value) -> String {
    let app = s_of(r.get("app")).trim().to_string();
    if !app.is_empty() {
        let p = expanduser(&app);
        if p.is_dir() {
            let got = s_of(app_info(&p).get("bundle"));
            if !got.is_empty() {
                return got;
            }
        }
    }
    s_of(r.get("bundle")).trim().to_string()
}

fn port_of(r: &Value) -> i64 {
    r.get("port").map(|v| util::i64_of(Some(v))).unwrap_or(0)
}

fn installed(r: &Value) -> bool {
    let app = s_of(r.get("app"));
    if !app.is_empty() && expanduser(&app).is_dir() {
        return true;
    }
    if app.is_empty() && !s_of(r.get("name")).is_empty() && find_app(&s_of(r.get("name"))).is_some() {
        return true;
    }
    let cli = s_of(r.get("cli"));
    if !cli.is_empty() && util::which(&cli).is_some() {
        return true;
    }
    brew_installed(r)
}

fn running(r: &Value) -> bool {
    if pgrep_exact(&exe_name(r)) {
        return true;
    }
    util::port_open(port_of(r), 350)
}

fn version_of(r: &Value) -> String {
    let app = s_of(r.get("app"));
    if !app.is_empty() && expanduser(&app).is_dir() {
        return s_of(app_info(&expanduser(&app)).get("version"));
    }
    if let Some(found) = find_app(&s_of(r.get("name"))) {
        return s_of(app_info(&found).get("version"));
    }
    String::new()
}

fn probe(r: &Value) -> Value {
    let inst = installed(r);
    let run = if inst { running(r) } else { false };
    let ver = if inst { version_of(r) } else { String::new() };
    let port = r.get("port").cloned().unwrap_or(Value::Null);
    let open = if inst { util::port_open(port_of(r), 350) } else { false };
    obj(vec![
        ("installed", json!(inst)),
        ("running", json!(run)),
        ("version", sv(&ver)),
        ("port", port),
        ("port_open", json!(open)),
    ])
}

fn cat_rank(cat: &str) -> usize {
    CAT_ORDER.iter().position(|x| *x == cat).unwrap_or(CAT_ORDER.len())
}

// ---------------------------------------------------------------- 任务表

fn tasks() -> &'static Mutex<Vec<(String, Value)>> {
    static T: OnceLock<Mutex<Vec<(String, Value)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Vec::new()))
}

fn task_copy(rid: &str) -> Option<Value> {
    let g = tasks().lock().unwrap();
    g.iter().find(|(k, _)| k == rid).map(|(_, v)| v.clone())
}

/// `_set_task(rid, **kw)`：初始三键 → 更新（原位）→ lines 截到 40。
fn set_task(rid: &str, kv: &[(&str, Value)]) {
    let mut g = tasks().lock().unwrap();
    let idx = match g.iter().position(|(k, _)| k == rid) {
        Some(i) => i,
        None => {
            let mut m = Map::new();
            m.insert("state".into(), sv("idle"));
            m.insert("lines".into(), json!([]));
            m.insert("started".into(), sv(""));
            g.push((rid.to_string(), Value::Object(m)));
            g.len() - 1
        }
    };
    if let Value::Object(m) = &mut g[idx].1 {
        for (k, v) in kv {
            if *k == "lines" {
                if let Value::Array(a) = v {
                    let mut a = a.clone();
                    if a.len() > LOG_TAIL {
                        a = a[a.len() - LOG_TAIL..].to_vec();
                    }
                    m.insert("lines".into(), Value::Array(a));
                    continue;
                }
            }
            m.insert((*k).to_string(), v.clone());
        }
        if let Some(Value::Array(a)) = m.get("lines").cloned() {
            if a.len() > LOG_TAIL {
                m.insert("lines".into(), Value::Array(a[a.len() - LOG_TAIL..].to_vec()));
            }
        }
    }
}

fn log_path(rid: &str) -> PathBuf {
    let d = util::home().join("Library").join("Logs");
    let d = if std::fs::create_dir_all(&d).is_ok() { d } else { PathBuf::from("/tmp") };
    d.join(format!("pocket-ext-{rid}.log"))
}

fn extended_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    format!("{base}:/opt/homebrew/bin:/usr/local/bin")
}

// ---------------------------------------------------------------- 目录接口

pub async fn list(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let rows = catalog(&ctx.cfg);
    Json(json!({ "ok": true, "catalog": rows })).into_response()
}

fn catalog(cfg: &Value) -> Vec<Value> {
    let builtin = builtin_ids();
    let mut rows = all_recipes(cfg);
    rows.sort_by(|a, b| {
        let ka = builtin.contains(&s_of(a.get("id"))) as i32;
        let kb = builtin.contains(&s_of(b.get("id"))) as i32;
        ka.cmp(&kb).then(cat_rank(&s_of(a.get("cat"))).cmp(&cat_rank(&s_of(b.get("cat")))))
    });
    let mut out = Vec::new();
    for r in rows {
        let mut item = Map::new();
        for k in ROW_FIELDS {
            if let Some(v) = r.get(*k) {
                if !matches!(v, Value::Null) && !(v.is_string() && v.as_str() == Some("")) {
                    item.insert((*k).to_string(), v.clone());
                }
            }
        }
        item.insert("desc".into(), r.get("desc").cloned().unwrap_or_else(|| sv("")));
        item.insert("home".into(), r.get("home").cloned().unwrap_or_else(|| sv("")));
        item.insert("repo".into(), r.get("repo").cloned().unwrap_or_else(|| sv("")));
        let rid = s_of(r.get("id"));
        item.insert("custom".into(), json!(!builtin.contains(&rid)));
        item.insert("state".into(), probe(&r));
        item.insert("task".into(), task_copy(&rid).unwrap_or(Value::Null));
        out.push(Value::Object(item));
    }
    out
}

// ---------------------------------------------------------------- 预览 / 加

pub async fn preview(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    Json(draft_from_text(&s_of(b.get("text")))).into_response()
}

fn draft_from_text(text: &str) -> Value {
    let raw = text.trim();
    if raw.is_empty() {
        return ok_err("先粘一个 .app 路径或者应用名").into_response_value();
    }
    let Some(app) = find_app(raw) else {
        return ok_err(&format!("没找到这个应用：{raw}")).into_response_value();
    };
    let info = app_info(&app);
    let name = util::stem_of(&app);
    let mut rid = util::slug(&name);
    if rid.is_empty() {
        rid = format!("ext-{}", &util::md5_hex(app.to_string_lossy().as_bytes())[..6]);
    }
    let exe = s_of(info.get("exe"));
    let proc = format!("/{}/Contents/MacOS/{exe}", util::name_of(&app));
    obj(vec![
        ("ok", json!(true)),
        ("app", sv(&app.to_string_lossy())),
        (
            "recipe",
            obj(vec![
                ("id", sv(&rid)),
                ("name", sv(&name)),
                ("cat", sv("自加")),
                ("glyph", sv("ext")),
                ("note", sv(&format!("{name}（本机程序）"))),
                ("desc", sv("")),
                ("app", sv(&app.to_string_lossy())),
                ("bundle", info.get("bundle").cloned().unwrap_or_else(|| sv(""))),
                ("exe", sv(&exe)),
                ("proc", sv(&proc)),
                ("brew", sv("")),
                ("brew_kind", sv("")),
                ("version", info.get("version").cloned().unwrap_or_else(|| sv(""))),
            ]),
        ),
    ])
}

pub async fn add(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    let mut rid = s_of(b.get("id"));
    if let Some(draft) = b.get("recipe").filter(|v| v.is_object()) {
        let Some(clean) = sanitize_recipe(draft) else {
            return ok_err("这份配方不完整：得有名字和启动方式").into_response();
        };
        let _ = save_recipe(&ctx.cfg, &clean);
        rid = s_of(clean.get("id"));
    }
    Json(add_ext_plugin(&ctx.cfg, &rid)).into_response()
}

fn plugin_row(r: &Value) -> Value {
    obj(vec![
        ("id", r.get("id").cloned().unwrap_or_else(|| sv(""))),
        ("name", r.get("name").cloned().unwrap_or_else(|| sv(""))),
        ("glyph", {
            let g = s_of(r.get("glyph"));
            if g.is_empty() { sv("ext") } else { sv(&g) }
        }),
        ("note", r.get("note").cloned().unwrap_or_else(|| sv(""))),
        ("url", {
            let w = s_of(r.get("web"));
            if w.is_empty() { sv("") } else { sv(&w) }
        }),
        ("port", r.get("port").cloned().unwrap_or(Value::Null)),
        ("group", sv("tool")),
        ("ext", r.get("id").cloned().unwrap_or_else(|| sv(""))),
    ])
}

fn plugins_path(cfg: &Value) -> PathBuf {
    util::data_root(cfg).join("_工作台").join("plugins.json")
}

const PLUGIN_TEMPLATE: &str = concat!(
    "加插件就往下写一条：{\"id\":\"backup\",\"name\":\"备份\",\"port\":8781,",
    "\"cmd\":\"14-备份/_备份.command\",\"note\":\"一句话说明\",\"group\":\"other\"}。",
    "group 决定弹窗里归哪一档（tool=小工具 / plugin=内置插件 / ",
    "other=其他插件 —— 自己写的、自带数据与页面、既不是随手打开也不是中台运维；",
    "不写按有没有端口猜）；",
    "还没做的加 \"soon\":true 先占位。写完刷新页面即可，不用重启口袋；",
    "id 跟内置重名会覆盖内置那一行。"
);

fn read_plugins(cfg: &Value) -> Vec<Value> {
    let d = util::read_json(&plugins_path(cfg)).unwrap_or(Value::Null);
    let rows = match &d {
        Value::Object(o) => o.get("plugins").cloned(),
        Value::Array(_) => Some(d.clone()),
        _ => None,
    };
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    if let Some(arr) = rows.and_then(|v| v.as_array().cloned()) {
        for (i, raw) in arr.into_iter().enumerate() {
            if let Some(p) = norm_plugin(&raw, i) {
                let id = s_of(p.get("id"));
                if !seen.contains(&id) {
                    seen.push(id);
                    out.push(p);
                }
            }
        }
    }
    out
}

fn po_id_ok(pid: &str) -> bool {
    let re = regex::Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").unwrap();
    re.is_match(pid)
}

fn norm_plugin(raw: &Value, _idx: usize) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let name = trunc(s_of(raw.get("name")).trim(), 24);
    if name.is_empty() {
        return None;
    }
    let soon = raw.get("soon").map(|v| {
        !(v.is_null() || v == &json!(false) || v == &json!(0) || v == &json!(""))
    }).unwrap_or(false);
    let ext = s_of(raw.get("ext")).trim().to_string();
    let port_raw = raw.get("port").cloned().unwrap_or(Value::Null);
    let port: Option<i64> = match &port_raw {
        Value::Null => None,
        Value::String(s) if s.is_empty() || s == "0" => None,
        Value::Number(n) if n.as_i64() == Some(0) => None,
        _ => Some(util::i64_of(Some(&port_raw))),
    };
    let mut url = s_of(raw.get("url")).trim().to_string();
    if url.is_empty() && port.is_some() && ext.is_empty() {
        url = format!("http://127.0.0.1:{}/", port.unwrap());
    }
    if url.is_empty() && !soon && ext.is_empty() {
        return None;
    }
    let mut pid = s_of(raw.get("id")).trim().to_lowercase();
    if !po_id_ok(&pid) {
        let src = if !s_of(raw.get("id")).is_empty() { s_of(raw.get("id")) } else { name.clone() };
        pid = util::slug(&src);
        if pid.is_empty() && port.is_some() {
            pid = format!("p{}", port.unwrap());
        }
        if pid.is_empty() {
            pid = format!("p{}", util::rand_hex(3));
        }
        if !po_id_ok(&pid) {
            pid = format!("p{}", util::rand_hex(3));
        }
    }
    let cmd = raw.get("cmd");
    let mut cmd_t: Option<(String, String)> = None;
    if let Some(Value::Array(a)) = cmd {
        if a.len() == 2 {
            let d = s_of(a.first()).trim().to_string();
            let f = s_of(a.get(1)).trim().to_string();
            if !d.is_empty() && !f.is_empty() {
                cmd_t = Some((d, f));
            }
        }
    } else if let Some(Value::String(s)) = cmd {
        if s.contains('/') {
            if let Some((d, f)) = s.trim().rsplit_once('/') {
                if !d.trim().is_empty() && !f.trim().is_empty() {
                    cmd_t = Some((d.trim().to_string(), f.trim().to_string()));
                }
            }
        }
    }
    let mut group = s_of(raw.get("group")).trim().to_lowercase();
    if !matches!(group.as_str(), "tool" | "plugin" | "other") {
        group = if port.is_some() || cmd_t.is_some() { "plugin".into() } else { "tool".into() };
    }
    let scope = if !ext.is_empty() {
        "ext"
    } else if soon {
        "soon"
    } else if port.is_some() || cmd_t.is_some() {
        "service"
    } else {
        "view"
    };
    Some(obj(vec![
        ("id", sv(&pid)),
        ("name", sv(&name)),
        ("glyph", {
            let g = s_of(raw.get("glyph")).trim().to_string();
            if g.is_empty() { sv("plugin") } else { sv(&g) }
        }),
        ("note", sv(&trunc(s_of(raw.get("note")).trim(), 60))),
        ("url", sv(&url)),
        ("port", port.map(|p| json!(p)).unwrap_or(Value::Null)),
        ("host", sv(&trunc(s_of(raw.get("host")).trim(), 64))),
        ("scope", sv(scope)),
        (
            "cmd",
            match cmd_t {
                Some((d, f)) => json!([d, f]),
                None => Value::Null,
            },
        ),
        ("svc_name", sv(&{
            let s = s_of(raw.get("svc_name")).trim().to_string();
            if s.is_empty() { name.clone() } else { s }
        })),
        ("source", sv("plugin")),
        ("group", sv(&group)),
        ("soon", json!(soon)),
        ("ext", sv(&ext)),
    ]))
}

fn write_plugins(cfg: &Value, rows: &[Value]) {
    let p = plugins_path(cfg);
    let mut items = Vec::new();
    for r in rows {
        let mut it = Map::new();
        it.insert("id".into(), r.get("id").cloned().unwrap_or_else(|| sv("")));
        it.insert("name".into(), r.get("name").cloned().unwrap_or_else(|| sv("")));
        let url = s_of(r.get("url"));
        it.insert("url".into(), sv(&url));
        let port = util::i64_of(r.get("port"));
        if port != 0 {
            it.insert("port".into(), json!(port));
        }
        let host = s_of(r.get("host"));
        if !host.is_empty() {
            it.insert("host".into(), sv(&host));
        }
        if let Some(Value::Array(a)) = r.get("cmd") {
            let d = s_of(a.first());
            let f = s_of(a.get(1));
            if !d.is_empty() && !f.is_empty() {
                it.insert("cmd".into(), sv(&format!("{d}/{f}")));
            }
        }
        let glyph = s_of(r.get("glyph"));
        if !glyph.is_empty() && glyph != "plugin" {
            it.insert("glyph".into(), sv(&glyph));
        }
        let note = s_of(r.get("note"));
        if !note.is_empty() {
            it.insert("note".into(), sv(&note));
        }
        let svc = s_of(r.get("svc_name"));
        if !svc.is_empty() && svc != s_of(r.get("name")) {
            it.insert("svc_name".into(), sv(&svc));
        }
        let group = s_of(r.get("group"));
        if matches!(group.as_str(), "tool" | "plugin" | "other") {
            it.insert("group".into(), sv(&group));
        }
        if r.get("soon").and_then(|v| v.as_bool()).unwrap_or(false) {
            it.insert("soon".into(), json!(true));
        }
        let ext = s_of(r.get("ext"));
        if !ext.is_empty() {
            it.insert("ext".into(), sv(&ext));
        }
        items.push(Value::Object(it));
    }
    let payload = obj(vec![
        ("_说明", sv(PLUGIN_TEMPLATE)),
        ("plugins", Value::Array(items)),
        ("updated_at", sv(&util::now_iso())),
    ]);
    let _ = std::fs::create_dir_all(p.parent().unwrap_or(Path::new(".")));
    let tmp = util::with_json_tmp(&p);
    if std::fs::write(&tmp, crate::jsonio::dumps(&payload, 1)).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

fn add_ext_plugin(cfg: &Value, rid: &str) -> Value {
    let Some(r) = recipe(cfg, rid) else {
        return obj(vec![("ok", json!(false)), ("error", sv("没有这条第三方配方"))]);
    };
    let row = plugin_row(&r);
    let mut rows = read_plugins(cfg);
    let want = s_of(row.get("id"));
    if let Some(p) = rows.iter().find(|x| s_of(x.get("id")) == want).cloned() {
        return obj(vec![
            ("ok", json!(true)),
            ("already", json!(true)),
            ("plugin", p),
            ("state", probe(&r)),
        ]);
    }
    let Some(p) = norm_plugin(&row, rows.len()) else {
        return obj(vec![("ok", json!(false)), ("error", sv("配方不完整，加不进来"))]);
    };
    rows.push(p.clone());
    write_plugins(cfg, &rows);
    obj(vec![("ok", json!(true)), ("plugin", p), ("state", probe(&r))])
}

// ---------------------------------------------------------------- 安装 / 开退

pub async fn install(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    Json(ext_install(&ctx.cfg, &s_of(b.get("id")))).into_response()
}

fn install_cmd(r: &Value) -> Vec<String> {
    let Some(brew) = util::which("brew") else { return vec![] };
    let pkg = s_of(r.get("brew"));
    if pkg.is_empty() {
        return vec![];
    }
    let mut cmd = vec![brew.to_string_lossy().to_string(), "install".into()];
    if s_of(r.get("brew_kind")) == "cask" {
        cmd.push("--cask".into());
    }
    cmd.push(pkg);
    cmd
}

fn ext_install(cfg: &Value, rid: &str) -> Value {
    let Some(r) = recipe(cfg, rid) else {
        return obj(vec![("ok", json!(false)), ("error", sv("没有这条配方"))]);
    };
    if installed(&r) {
        return obj(vec![("ok", json!(true)), ("already", json!(true)), ("state", probe(&r))]);
    }
    let cmd = install_cmd(&r);
    if cmd.is_empty() {
        return obj(vec![
            ("ok", json!(false)),
            ("error", sv(&format!("{} 没有 brew 配方，得手动装", s_of(r.get("name"))))),
        ]);
    }
    if let Some(t) = task_copy(rid) {
        if s_of(t.get("state")) == "running" {
            return obj(vec![("ok", json!(true)), ("state", t)]);
        }
    }
    set_task(
        rid,
        &[
            ("state", sv("running")),
            ("lines", json!([])),
            ("cmd", Value::Array(cmd.iter().map(|x| sv(x)).collect())),
            ("started", sv(&util::now_iso())),
            ("ended", sv("")),
            ("rc", Value::Null),
        ],
    );
    let rid2 = rid.to_string();
    let cmd2 = cmd.clone();
    std::thread::spawn(move || do_install(&rid2, &cmd2));
    let t = task_copy(rid).unwrap_or(Value::Null);
    obj(vec![("ok", json!(true)), ("state", t)])
}

fn do_install(rid: &str, cmd: &[String]) {
    use std::io::{BufRead, BufReader, Write};
    let log = log_path(rid);
    let rc;
    {
        let mut lf = match std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            Ok(f) => f,
            Err(e) => {
                set_task(rid, &[
                    ("state", sv("failed")),
                    ("ended", sv(&util::now_iso())),
                    ("error", sv(&trunc(&e.to_string(), 200))),
                ]);
                return;
            }
        };
        let _ = write!(lf, "\n===== {} {}\n", util::now_iso(), cmd.join(" "));
        let _ = lf.flush();
        let mut c = std::process::Command::new(&cmd[0]);
        c.args(&cmd[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("PATH", extended_path())
            .env("HOMEBREW_NO_AUTO_UPDATE", "1");
        match c.spawn() {
            Ok(mut child) => {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let mut lines: Vec<String> = task_copy(rid)
                    .and_then(|t| t.get("lines").and_then(|v| v.as_array()).cloned())
                    .map(|a| a.iter().map(|x| s_of(Some(x))).collect())
                    .unwrap_or_default();
                if let Some(out) = stdout {
                    for line in BufReader::new(out).lines().map_while(Result::ok) {
                        let line = line.trim_end_matches('\n').to_string();
                        let _ = writeln!(lf, "{line}");
                        let _ = lf.flush();
                        lines.push(line);
                        if lines.len() > LOG_TAIL {
                            let cut = lines.len() - LOG_TAIL;
                            lines.drain(..cut);
                        }
                        set_task(rid, &[("lines", Value::Array(lines.iter().map(|x| sv(x)).collect()))]);
                    }
                }
                if let Some(err) = stderr {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        let _ = writeln!(lf, "{line}");
                    }
                }
                rc = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            }
            Err(e) => {
                set_task(rid, &[
                    ("state", sv("failed")),
                    ("ended", sv(&util::now_iso())),
                    ("error", sv(&trunc(&e.to_string(), 200))),
                ]);
                return;
            }
        }
    }
    set_task(rid, &[
        ("state", sv(if rc == 0 { "done" } else { "failed" })),
        ("rc", json!(rc)),
        ("ended", sv(&util::now_iso())),
        ("log", sv(&log.to_string_lossy())),
    ]);
}

pub async fn start(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    Json(ext_start(&ctx.cfg, &s_of(b.get("id")))).into_response()
}

fn spawn_log(cmd: &[String], rid: &str) -> std::io::Result<()> {
    let log = log_path(rid);
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(std::process::Stdio::null())
        .env("PATH", extended_path());
    c.spawn().map(|_| ())
}

fn ext_start(cfg: &Value, rid: &str) -> Value {
    let Some(r) = recipe(cfg, rid) else {
        return obj(vec![("ok", json!(false)), ("error", sv("没有这条配方"))]);
    };
    let name = s_of(r.get("name"));
    if !installed(&r) {
        return obj(vec![("ok", json!(false)), ("error", sv(&format!("{name} 还没装")))]);
    }
    if running(&r) {
        return obj(vec![("ok", json!(true)), ("started", json!(false)), ("state", probe(&r))]);
    }
    let res: std::io::Result<()> = if let Some(Value::Array(start)) = r.get("start") {
        let mut argv: Vec<String> = Vec::new();
        if let Some(first) = start.first() {
            let f = s_of(Some(first));
            argv.push(util::which(&f).map(|p| p.to_string_lossy().to_string()).unwrap_or(f));
        }
        for x in start.iter().skip(1) {
            argv.push(s_of(Some(x)));
        }
        spawn_log(&argv, rid)
    } else if !s_of(r.get("app")).is_empty() && expanduser(&s_of(r.get("app"))).is_dir() {
        spawn_log(
            &[
                "/usr/bin/open".into(),
                "-a".into(),
                expanduser(&s_of(r.get("app"))).to_string_lossy().to_string(),
            ],
            rid,
        )
    } else if !s_of(r.get("bundle")).is_empty() {
        spawn_log(&["/usr/bin/open".into(), "-b".into(), s_of(r.get("bundle"))], rid)
    } else if !name.is_empty() {
        let Some(app) = find_app(&name) else {
            return obj(vec![("ok", json!(false)), ("error", sv(&format!("找不到 {name}.app")))]);
        };
        spawn_log(
            &["/usr/bin/open".into(), "-a".into(), app.to_string_lossy().to_string()],
            rid,
        )
    } else {
        return obj(vec![("ok", json!(false)), ("error", sv("这条配方没写怎么启动"))]);
    };
    if let Err(e) = res {
        return obj(vec![("ok", json!(false)), ("error", sv(&trunc(&e.to_string(), 200)))]);
    }
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < 10.0 {
        if running(&r) {
            let sec = util::py_round(t0.elapsed().as_secs_f64(), 1);
            return obj(vec![
                ("ok", json!(true)),
                ("started", json!(true)),
                ("seconds", json!(sec)),
                ("state", probe(&r)),
            ]);
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    if port_of(&r) == 0 && s_of(r.get("proc")).is_empty() && !s_of(r.get("app")).is_empty() {
        return obj(vec![("ok", json!(true)), ("started", json!(true)), ("state", probe(&r))]);
    }
    obj(vec![
        ("ok", json!(false)),
        ("started", json!(true)),
        ("state", probe(&r)),
        (
            "error",
            sv(&format!("{name} 起了但没确认到在跑，看日志 ~/Library/Logs/pocket-ext-{rid}.log")),
        ),
    ])
}

pub async fn stop(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    Json(ext_stop(&ctx.cfg, &s_of(b.get("id")))).into_response()
}

fn safe_app_id(v: &str) -> String {
    let re = regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._\-]{0,120}$").unwrap();
    let v = v.trim();
    if re.is_match(v) { v.to_string() } else { String::new() }
}

fn safe_proc_pattern(v: &str) -> String {
    let re = regex::Regex::new(r"^[A-Za-z0-9._/][A-Za-z0-9._/ +\-]{0,119}$").unwrap();
    let v = v.trim();
    if re.is_match(v) { v.to_string() } else { String::new() }
}

fn ext_stop(cfg: &Value, rid: &str) -> Value {
    let Some(r) = recipe(cfg, rid) else {
        return obj(vec![("ok", json!(false)), ("error", sv("没有这条配方"))]);
    };
    let name = s_of(r.get("name"));
    if !running(&r) {
        return obj(vec![("ok", json!(true)), ("stopped", json!(false)), ("state", probe(&r))]);
    }
    let bundle = safe_app_id(&bundle_id(&r));
    let appname = name.replace('"', "").replace('\\', "");
    let script = if !bundle.is_empty() {
        format!("tell application id \"{bundle}\" to quit")
    } else if !appname.is_empty() && (!s_of(r.get("app")).is_empty() || !s_of(r.get("bundle")).is_empty()) {
        format!("tell application \"{appname}\" to quit")
    } else {
        String::new()
    };
    let mut told = false;
    if !script.is_empty() {
        let argv = vec!["/usr/bin/osascript".to_string(), "-e".into(), script];
        told = matches!(util::run_cmd(&argv, 8, None), Some((0, _, _)));
    }
    if !told {
        let exe = exe_name(&r);
        let pat = safe_proc_pattern(&s_of(r.get("proc")));
        if !exe.is_empty() {
            let _ = util::run_cmd(&["/usr/bin/pkill".into(), "-x".into(), exe], 5, None);
        } else if !pat.is_empty() {
            let _ = util::run_cmd(&["/usr/bin/pkill".into(), "-f".into(), pat], 5, None);
        }
    }
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < 6.0 {
        if !running(&r) {
            return obj(vec![("ok", json!(true)), ("stopped", json!(true)), ("state", probe(&r))]);
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    obj(vec![("ok", json!(false)), ("error", sv(&format!("{name} 还在跑，可能要手动退出")))])
}

// ---------------------------------------------------------------- spec / prompt / parse

const DEFAULT_PROMPT: &str = r##"你是 macOS「个人中台」的插件配方助手。中台用一条 JSON 配方管一个第三方程序：
装没装、在不在跑、怎么装、怎么开、怎么退。

## 输出
只输出**一个 JSON 对象**。不要解释、不要 markdown 代码围栏、不要注释。

## 字段
| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| id | string | 是 | 小写连字符，唯一，如 ice |
| name | string | 是 | 显示名，≤24 字 |
| cat | string | 否 | 传输/同步/播放/音视频/图像/效率/系统/开发/模型 |
| note | string | 否 | 一句话，≤18 字 |
| desc | string | 否 | 一到两句说明 |
| app | string | 三选一 | 装好后的 .app 绝对路径，如 /Applications/Keka.app |
| cli | string | 三选一 | 命令行可执行名，如 syncthing |
| start | string[] | 否 | 命令行启动 argv，如 ["syncthing","serve"] |
| brew | string | 建议 | brew 包名 |
| brew_kind | cask / formula | 有 brew 必填 | 桌面程序 cask，命令行 formula |
| port | number | 否 | 本地网页端口，只有自带网页界面才填 |
| web | string | 否 | 通常 http://127.0.0.1:<port>/ |
| home | string | 否 | 官网 |

## 硬约束
1. app / cli / brew 至少给一个，否则这条配方没用。
2. brew 包名必须真实存在：桌面程序是 `brew install --cask <名>`，命令行是 `brew install <名>`；
   不确定就**别写 brew**（按钮会变成「手动装」，用户自己装完照样能用），不要编一个名字。
3. 桌面程序：brew_kind=cask + app=/Applications/<App>.app。
4. 有网页界面的（Syncthing、code-server 这类）才填 port 和 web；端口不确定就不要猜。
5. exe / bundle 不用写 —— 中台会从 .app 的 Info.plist 读。
6. 只有网页、没有桌面程序的，不要写 app。
7. id 全小写连字符；同一个程序不要起两个 id。

## 例子
{"id":"keka","name":"Keka","cat":"系统","note":"压缩解压","app":"/Applications/Keka.app","brew":"keka","brew_kind":"cask"}
{"id":"syncthing","name":"Syncthing","cat":"同步","note":"文件夹同步","cli":"syncthing","start":["syncthing","serve","--no-browser"],"brew":"syncthing","brew_kind":"formula","port":8384,"web":"http://127.0.0.1:8384/"}
{"id":"ice","name":"Ice","cat":"效率","note":"菜单栏图标管理","app":"/Applications/Ice.app","brew":"jordanbaird-ice","brew_kind":"cask"}

## 这台机器
已有 id（别重复）：{{IDS}}
这次要加：{{GOAL}}

现在输出 JSON。
"##;

fn field_doc() -> Value {
    let rows = vec![
        ("id", "string", "必填", "小写连字符，唯一，如 ice、localsend"),
        ("name", "string", "必填", "显示名，≤24 字符，顶栏药丸就用它"),
        ("cat", "string", "可选", "传输/同步/播放/音视频/图像/效率/系统/开发/模型"),
        ("note", "string", "可选", "一句话，≤18 字，列表里显示"),
        ("desc", "string", "可选", "一到两句说明，悬停时显示"),
        ("app", "string", "三选一", "装好后的 .app 绝对路径，如 /Applications/Keka.app"),
        ("cli", "string", "三选一", "命令行可执行名（PATH 里找得到，如 syncthing）"),
        ("start", "string[]", "可选", "命令行程序怎么起，argv 数组，如 [\"syncthing\",\"serve\"]"),
        ("brew", "string", "建议", "brew 包名；没有就留空，按钮会变成「手动装」"),
        ("brew_kind", "cask | formula", "有 brew 时必填", "桌面程序 cask，命令行 formula"),
        ("port", "number", "可选", "本地网页端口，只有自带网页界面的程序才填"),
        ("web", "string", "可选", "网页地址，通常 http://127.0.0.1:<port>/"),
        ("exe", "string", "可省略", "进程名；中台会从 .app 的 Info.plist 读，别猜"),
        ("bundle", "string", "可省略", "bundle id；同上，装了自动读"),
        ("home", "string", "可选", "官网"),
        ("repo", "string", "可选", "源码仓库"),
    ];
    Value::Array(
        rows.into_iter()
            .map(|(k, t, n, d)| obj(vec![("k", sv(k)), ("type", sv(t)), ("need", sv(n)), ("doc", sv(d))]))
            .collect(),
    )
}

fn examples() -> Value {
    json!([
        {"id": "keka", "name": "Keka", "cat": "系统", "note": "压缩解压",
         "desc": "7z、rar、tar、zip 都认。",
         "app": "/Applications/Keka.app", "brew": "keka", "brew_kind": "cask",
         "home": "https://www.keka.io"},
        {"id": "syncthing", "name": "Syncthing", "cat": "同步", "note": "文件夹同步",
         "cli": "syncthing", "start": ["syncthing", "serve", "--no-browser"],
         "brew": "syncthing", "brew_kind": "formula",
         "port": 8384, "web": "http://127.0.0.1:8384/"},
        {"id": "ice", "name": "Ice", "cat": "效率", "note": "菜单栏图标管理",
         "desc": "把菜单栏图标分组、隐藏、调顺序。",
         "app": "/Applications/Ice.app", "brew": "jordanbaird-ice", "brew_kind": "cask",
         "home": "https://icemenubar.app"}
    ])
}

fn endpoints() -> Value {
    json!([
        {"method": "GET", "path": "/api/ext/list", "doc": "目录 + 现况（装没装 / 跑没跑 / 安装进度）"},
        {"method": "GET", "path": "/api/ext/spec", "doc": "这份说明（JSON）"},
        {"method": "GET", "path": "/api/ext/prompt?goal=", "doc": "提示词纯文本，可直接喂给任意模型"},
        {"method": "POST", "path": "/api/ext/parse", "doc": "body {text, verify}：从模型回答里抠配方，不落盘"},
        {"method": "POST", "path": "/api/ext/add", "doc": "body {recipe} 或 {id}：落配方并加进中台"},
        {"method": "POST", "path": "/api/ext/compose", "doc": "body {goal}：配了模型就直接生成，没配返回提示词"},
        {"method": "POST", "path": "/api/ext/install", "doc": "body {id}：brew 后台安装"},
        {"method": "POST", "path": "/api/ext/start", "doc": "body {id}：启动"},
        {"method": "POST", "path": "/api/ext/stop", "doc": "body {id}：退出"}
    ])
}

fn ensure_prompt(cfg: &Value) -> PathBuf {
    let p = prompt_path(cfg);
    if p.exists() {
        return p;
    }
    let head = concat!(
        "<!-- 中台「第三方插件」的配方提示词。改完立刻生效（下一轮截图/复制就用新的）。\n",
        "     {{IDS}} 会替换成已有 id，{{GOAL}} 替换成这次要加的东西。 -->\n\n"
    );
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, format!("{head}{DEFAULT_PROMPT}"));
    p
}

pub fn prompt_text(cfg: &Value, goal: &str) -> String {
    let p = ensure_prompt(cfg);
    let base = std::fs::read_to_string(&p).unwrap_or_else(|_| DEFAULT_PROMPT.to_string());
    let re = regex::Regex::new(r"(?s)<!--.*?-->\s*").unwrap();
    let base = re.replace_all(&base, "").into_owned();
    let ids = all_recipes(cfg)
        .iter()
        .map(|r| s_of(r.get("id")))
        .collect::<Vec<_>>()
        .join(", ");
    let goal = goal.trim();
    let goal = if goal.is_empty() {
        "按你的判断挑一个常用、开源、brew 能装的程序"
    } else {
        goal
    };
    let mut out = base.replace("{{IDS}}", &ids).replace("{{GOAL}}", goal);
    if !out.contains("已有 id") {
        out.push_str(&format!("\n\n已有 id：{ids}\n这次要加：{goal}\n"));
    }
    out
}

pub async fn spec_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let goal = q(&uri, "goal");
    Json(spec(&ctx.cfg, &goal)).into_response()
}

fn spec(cfg: &Value, goal: &str) -> Value {
    let mut d = Map::new();
    d.insert("v".into(), json!(1));
    d.insert(
        "what".into(),
        sv("第三方插件 = 一条 JSON 配方（怎么认装没装 / 怎么装 / 怎么开退），加进来就变成中台顶栏的一颗药丸。"),
    );
    d.insert("recipe_fields".into(), field_doc());
    d.insert("endpoints".into(), endpoints());
    d.insert("examples".into(), examples());
    d.insert("prompt".into(), sv(&prompt_text(cfg, goal)));
    d.insert(
        "files".into(),
        obj(vec![("prompt", sv(PROMPT_FILE)), ("model", sv(MODEL_FILE)), ("recipes", sv(RECIPE_FILE))]),
    );
    d.insert(
        "ids".into(),
        Value::Array(all_recipes(cfg).iter().map(|r| r.get("id").cloned().unwrap_or(Value::Null)).collect()),
    );
    d.insert(
        "paths".into(),
        obj(vec![
            ("prompt", sv(&prompt_path(cfg).to_string_lossy())),
            ("model", sv(&model_path(cfg).to_string_lossy())),
            ("recipes", sv(&recipes_path(cfg).to_string_lossy())),
        ]),
    );
    d.insert("model_configured".into(), json!(!s_of(model_config(cfg).get("api_key")).is_empty()));
    Value::Object(d)
}

pub async fn prompt_route(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let goal = q(&uri, "goal");
    let text = prompt_text(&ctx.cfg, &goal);
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        text,
    )
        .into_response()
}

pub async fn parse(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    let verify = b
        .get("verify")
        .map(|v| !(v.is_null() || v == &json!(false) || v == &json!(0) || v == &json!("")))
        .unwrap_or(true);
    Json(parse_recipe_text(&ctx.cfg, &s_of(b.get("text")), verify)).into_response()
}

fn extract_json(text: &str) -> Option<Value> {
    let s = text.trim();
    if s.is_empty() {
        return None;
    }
    let fence = regex::Regex::new(r"(?s)```(?:json|JSON)?\s*([\s\S]*?)```").unwrap();
    let mut cands: Vec<String> = Vec::new();
    if let Some(c) = fence.captures(s) {
        cands.push(c.get(1).map(|m| m.as_str().to_string()).unwrap_or_default());
    }
    cands.push(s.to_string());
    for c in cands {
        if let Ok(v) = serde_json::from_str::<Value>(c.trim()) {
            return Some(v);
        }
    }
    let bytes: Vec<char> = s.chars().collect();
    for (i, ch) in bytes.iter().enumerate() {
        if *ch != '{' {
            continue;
        }
        let mut depth = 0i32;
        let mut instr = false;
        let mut esc = false;
        for (j, cj) in bytes.iter().enumerate().skip(i) {
            if instr {
                if esc {
                    esc = false;
                } else if *cj == '\\' {
                    esc = true;
                } else if *cj == '"' {
                    instr = false;
                }
                continue;
            }
            if *cj == '"' {
                instr = true;
            } else if *cj == '{' {
                depth += 1;
            } else if *cj == '}' {
                depth -= 1;
                if depth == 0 {
                    let frag: String = bytes[i..=j].iter().collect();
                    if let Ok(v) = serde_json::from_str::<Value>(&frag) {
                        return Some(v);
                    }
                    break;
                }
            }
        }
        break;
    }
    None
}

fn sanitize_recipe(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let mut r = Map::new();
    for k in RECIPE_FIELDS {
        let Some(v) = raw.get(*k) else { continue };
        if v.is_null() || (v.is_string() && v.as_str() == Some("")) {
            continue;
        }
        if *k == "port" {
            let n = util::i64_of(Some(v));
            if n == 0 && !(v.is_number() || v.is_string() && v.as_str().map(|s| s.trim().parse::<i64>().is_ok()).unwrap_or(false)) {
                continue;
            }
            r.insert("port".into(), json!(n));
            continue;
        }
        if *k == "start" {
            let Some(a) = v.as_array() else { continue };
            if a.is_empty() {
                continue;
            }
            r.insert("start".into(), Value::Array(a.iter().map(|x| sv(&py_str(x))).collect()));
            continue;
        }
        r.insert((*k).to_string(), v.clone());
    }
    if s_of(r.get("id")).is_empty() || s_of(r.get("name")).is_empty() {
        return None;
    }
    let any = ["app", "bundle", "cli", "start", "brew"].iter().any(|k| {
        r.get(*k).map(|v| {
            !(v.is_null() || v == &json!("") || v == &json!(false) || v == &json!(0) || v.as_array().map(|a| a.is_empty()).unwrap_or(false))
        }).unwrap_or(false)
    });
    if !any {
        return None;
    }
    let src = if !s_of(r.get("app")).is_empty() {
        s_of(r.get("app"))
    } else {
        s_of(r.get("name"))
    };
    let mut id = util::slug(&s_of(r.get("id")));
    if id.is_empty() {
        id = format!("ext-{}", &util::md5_hex(src.as_bytes())[..6]);
    }
    r.insert("id".into(), sv(&id));
    r.insert("name".into(), sv(&trunc(&s_of(r.get("name")), 24)));
    Some(Value::Object(r))
}

fn brew_has(name: &str, kind: &str) -> bool {
    let name = name.trim();
    if name.is_empty() {
        return false;
    }
    let Some(brew) = util::which("brew") else { return false };
    let tries: Vec<Vec<&str>> = if kind == "cask" {
        vec![vec!["--cask"], vec![]]
    } else {
        vec![vec![], vec!["--cask"]]
    };
    for extra in tries {
        let mut argv = vec![brew.to_string_lossy().to_string(), "info".into()];
        argv.extend(extra.into_iter().map(|s| s.to_string()));
        argv.push("--json=v2".into());
        argv.push(name.to_string());
        if let Some((0, _, _)) = util::run_cmd(&argv, 20, None) {
            return true;
        }
    }
    false
}

fn parse_recipe_text(_cfg: &Value, text: &str, verify: bool) -> Value {
    let Some(mut d) = extract_json(text) else {
        return obj(vec![("ok", json!(false)), ("error", sv("没从这段文字里找到 JSON 对象"))]);
    };
    if d.get("recipe").map(|v| v.is_object()).unwrap_or(false) {
        d = d.get("recipe").cloned().unwrap_or(Value::Null);
    } else if let Some(Value::Array(a)) = d.get("recipes") {
        if !a.is_empty() {
            d = a[0].clone();
        }
    }
    if !d.is_object() {
        return obj(vec![("ok", json!(false)), ("error", sv("JSON 里没有配方对象"))]);
    }
    let Some(mut r) = sanitize_recipe(&d) else {
        return obj(vec![
            ("ok", json!(false)),
            ("error", sv("配方缺东西：至少要有 id、name，以及 app / cli / start / brew 之一")),
        ]);
    };
    let mut warns: Vec<String> = Vec::new();
    let app = s_of(r.get("app"));
    if !app.is_empty() && !expanduser(&app).is_dir() {
        warns.push(format!("{app} 不在本机，装完才会出现（没装就正常）"));
    }
    let brew = s_of(r.get("brew"));
    if !brew.is_empty() && verify && !brew_has(&brew, &s_of(r.get("brew_kind"))) {
        warns.push(format!("brew 里没查到 {brew}：包名可能写错，或者得手动装"));
    }
    if r.get("port").map(|v| !v.is_null()).unwrap_or(false) && s_of(r.get("web")).is_empty() {
        let port = util::i64_of(r.get("port"));
        if let Some(o) = r.as_object_mut() {
            o.insert("web".into(), sv(&format!("http://127.0.0.1:{port}/")));
        }
    }
    let id = s_of(r.get("id"));
    if builtin_ids().contains(&id) {
        warns.push(format!("{id} 是目录里已有的内置程序，加进来会覆盖内置那条"));
    }
    let state = if installed(&r) { probe(&r) } else { Value::Null };
    obj(vec![
        ("ok", json!(true)),
        ("recipe", r),
        ("warnings", Value::Array(warns.into_iter().map(|w| sv(&w)).collect())),
        ("state", state),
    ])
}

// ---------------------------------------------------------------- compose

fn model_config(cfg: &Value) -> Value {
    let d = util::read_json(&model_path(cfg)).unwrap_or(Value::Null);
    if !d.is_object() {
        return obj(vec![]);
    }
    let mut key = s_of(d.get("api_key")).trim().to_string();
    if key.is_empty() {
        let env = s_of(d.get("api_key_env")).trim().to_string();
        if !env.is_empty() {
            key = std::env::var(&env).unwrap_or_default();
        }
    }
    if key.is_empty() {
        return obj(vec![]);
    }
    let base = {
        let b = s_of(d.get("base_url"));
        let b = if b.is_empty() { "https://api.openai.com/v1".to_string() } else { b };
        b.trim_end_matches('/').to_string()
    };
    let model = {
        let m = s_of(d.get("model"));
        if m.is_empty() { "gpt-4o-mini".to_string() } else { m }
    };
    obj(vec![("base_url", sv(&base)), ("model", sv(&model)), ("api_key", sv(&key))])
}

pub async fn compose(
    State(ctx): CtxState,
    headers: HeaderMap,
    uri: Uri,
    body: Option<Json<Value>>,
) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let b = body_obj(body);
    let goal = s_of(b.get("goal"));
    Json(llm_compose(&ctx.cfg, &goal).await).into_response()
}

async fn llm_compose(cfg: &Value, goal: &str) -> Value {
    let mc = model_config(cfg);
    if s_of(mc.get("api_key")).is_empty() {
        return obj(vec![
            ("ok", json!(false)),
            ("need", sv("token")),
            ("prompt", sv(&prompt_text(cfg, goal))),
            (
                "hint",
                sv(&format!(
                    "没配模型。想要一键生成，就写 {}（见 /api/ext/spec）；否则把 prompt 复制给任意模型，答案粘回 /api/ext/parse。",
                    model_path(cfg).to_string_lossy()
                )),
            ),
        ]);
    }
    let payload = json!({
        "model": s_of(mc.get("model")),
        "temperature": 0.2,
        "messages": [{"role": "user", "content": prompt_text(cfg, goal)}],
    });
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return obj(vec![
                ("ok", json!(false)),
                ("via", sv("model")),
                ("need", sv("token")),
                ("error", sv(&format!("模型没调通：{}", trunc(&e.to_string(), 160)))),
                ("prompt", sv(&prompt_text(cfg, goal))),
            ])
        }
    };
    let url = format!("{}/chat/completions", s_of(mc.get("base_url")));
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {}", s_of(mc.get("api_key"))))
        .json(&payload)
        .send()
        .await;
    let content = match resp {
        Ok(r) => match r.text().await.ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            Some(d) => s_of(
                d.get("choices")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content")),
            ),
            None => String::new(),
        },
        Err(e) => {
            return obj(vec![
                ("ok", json!(false)),
                ("via", sv("model")),
                ("need", sv("token")),
                ("error", sv(&format!("模型没调通：{}", trunc(&e.to_string(), 160)))),
                ("prompt", sv(&prompt_text(cfg, goal))),
            ])
        }
    };
    let mut got = parse_recipe_text(cfg, &content, true);
    if let Some(o) = got.as_object_mut() {
        o.insert("via".into(), sv("model"));
        o.insert("raw".into(), sv(&trunc(&content, 2000)));
    }
    got
}

// ---- 小垫片：ok_err 是 Json<Value>，这里拿它的内层 Value 用于拼对象
trait IntoValue {
    fn into_response_value(self) -> Value;
}

impl IntoValue for Json<Value> {
    fn into_response_value(self) -> Value {
        self.0
    }
}
