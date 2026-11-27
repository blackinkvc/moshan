// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：工作台「入口卡片」7 条路由 + 它们背后的 dash.py/extplugins.py 数据面。
// 不做：不碰 dash 的其它板块（笔记/巡检/存图）—— 那些是别的泳道。
//
// 权限：这 7 条用的是**主令牌**（server.py 的 `Depends(auth)`，三通道
// header X-Token → ?k= → cookie），不是虫巢那两把。别混。

#![allow(dead_code)]

use super::util;
use crate::jsonio;
use crate::paths::Ctx;
use crate::routes::common::{http_err, peek, q, CtxState};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// ------------------------------------------------------------------ 静态清单
//
// 这两份是从 moshan/dash.py:BUTTON_APPS / moshan/extplugins.py:CATALOG
// 原样导出的 JSON（不是手抄）。手抄 32 条配方一定会错一个字段，
// 而字段错一个，`/api/apps` 的字节就变了。

const BUILTIN_APPS_JSON: &str = r#"[{"id":"pocket","name":"口袋","scope":"service","port":null,"svc_port":8321,"svc_name":"口袋（传输/抓取）","self":true,"url":"/","note":"抓取 / 传输 / 工作台数据源","glyph":"pocket","group":"plugin"},{"id":"gallery","name":"看图","scope":"service","port":8771,"url":"http://127.0.0.1:8771/","note":"存图库","cmd":["04-存图","_看图.command"],"glyph":"gallery","group":"tool"},{"id":"reader","name":"阅读","scope":"service","port":8772,"url":"http://127.0.0.1:8772/","note":"长文阅读 / 标注","cmd":["08-阅读","_阅读.command"],"glyph":"reader","group":"tool"},{"id":"media","name":"影音","scope":"service","port":8773,"svc_name":"媒体墙","url":"http://127.0.0.1:8773/","note":"媒体墙 / 播放","cmd":["02-媒体","_媒体墙.command"],"glyph":"media","group":"tool"},{"id":"inspect","name":"巡检","scope":"service","port":8774,"svc_name":"系统巡检","url":"/dash?v=ops","note":"系统巡检","cmd":["_巡检","_系统巡检.command"],"glyph":"inspect","group":"plugin"},{"id":"hostwork","name":"Agent 监控","scope":"view","port":null,"url":"/dash?v=host","note":"进程 / 磁盘内存 / token 折算","glyph":"hostwork","group":"plugin"},{"id":"llmcost","name":"Token 价格追踪","scope":"service","port":8776,"svc_name":"Token 价格追踪","url":"http://127.0.0.1:8776/","note":"模型 API / 套餐 / 本地部署成本","cmd":["13-算价","_算价.command"],"glyph":"llmcost","group":"other"},{"id":"follow","name":"关注","scope":"service","port":8780,"svc_name":"关注追踪","url":"http://127.0.0.1:8780/","note":"微博/B站/小红书/抖音更新","cmd":["11-关注追踪","_关注追踪.command"],"glyph":"follow","group":"plugin"},{"id":"review","name":"界面评审","scope":"service","port":8785,"svc_name":"界面评审","url":"http://127.0.0.1:8785/","note":"UI 体检 / 待决定 / 已落地","cmd":["18-界面评审","_界面评审.command"],"glyph":"review","group":"other"}]"#;
const EXT_CATALOG_JSON: &str = r#"[{"id":"localsend","name":"LocalSend","cat":"传输","glyph":"ext","note":"同一 Wi-Fi 下跨设备传文件","desc":"手机、电脑连同一个局域网就能互传文件／照片，不经过服务器、不压缩画质。","app":"/Applications/LocalSend.app","bundle":"org.localsend.localsend_app","exe":"LocalSend","proc":"/LocalSend.app/Contents/MacOS/LocalSend","port":53317,"brew":"localsend","brew_kind":"cask","home":"https://localsend.org","repo":"https://github.com/localsend/localsend"},{"id":"syncthing","name":"Syncthing","cat":"同步","glyph":"ext","note":"多台机器之间同步文件夹","desc":"点对点同步，不经过云盘。自带网页管理界面，加完点开就能配。","cli":"syncthing","exe":"syncthing","proc":"syncthing serve","port":8384,"web":"http://127.0.0.1:8384/","start":["syncthing","serve","--no-browser","--no-restart"],"brew":"syncthing","brew_kind":"formula","home":"https://syncthing.net","repo":"https://github.com/syncthing/syncthing"},{"id":"iina","name":"IINA","cat":"播放","glyph":"ext","note":"macOS 上的开源播放器","desc":"能放的格式比 QuickTime 多得多，支持画中画和触控板手势。","app":"/Applications/IINA.app","bundle":"com.colliderli.iina","exe":"IINA","proc":"/IINA.app/Contents/MacOS/IINA","brew":"iina","brew_kind":"cask","home":"https://iina.io","repo":"https://github.com/iina/iina"},{"id":"rectangle","name":"Rectangle","cat":"效率","glyph":"ext","note":"用快捷键摆窗口","desc":"开源窗口管理：⌃⌥ + 方向键把窗口摆到左半屏／右半屏／最大化。","app":"/Applications/Rectangle.app","bundle":"com.knollsoft.Rectangle","exe":"Rectangle","proc":"/Rectangle.app/Contents/MacOS/Rectangle","brew":"rectangle","brew_kind":"cask","home":"https://rectangleapp.com","repo":"https://github.com/rxhanson/Rectangle"},{"id":"ollama","name":"Ollama","cat":"模型","glyph":"ext","note":"本机跑开源大模型","desc":"一条命令把开源模型拉下来在本机跑，中台里的本地模型就走它。","cli":"ollama","exe":"ollama","proc":"ollama serve","port":11434,"start":["ollama","serve"],"brew":"ollama","brew_kind":"formula","home":"https://ollama.com","repo":"https://github.com/ollama/ollama"},{"id":"cyberduck","name":"Cyberduck","cat":"传输","glyph":"ext","note":"连服务器 / 云盘","desc":"SFTP、FTP、WebDAV、S3 和常见网盘都能挂，连服务器像逛 Finder。","app":"/Applications/Cyberduck.app","brew":"cyberduck","brew_kind":"cask","home":"https://cyberduck.io"},{"id":"transmission","name":"Transmission","cat":"传输","glyph":"ext","note":"BT 下载","desc":"轻量 BT 客户端，没广告；下片子、下镜像都稳。","app":"/Applications/Transmission.app","brew":"transmission","brew_kind":"cask","home":"https://transmissionbt.com"},{"id":"vlc","name":"VLC","cat":"播放","glyph":"media","note":"万能播放器","desc":"几乎什么格式都能放，没下完的文件、坏掉的文件也能播。","app":"/Applications/VLC.app","brew":"vlc","brew_kind":"cask","home":"https://www.videolan.org/vlc/"},{"id":"jellyfin-media-player","name":"Jellyfin","cat":"播放","glyph":"media","note":"连家里的 Jellyfin","desc":"官方桌面客户端：连家里盒子的 Jellyfin，原盘直放、不转码。","app":"/Applications/Jellyfin Media Player.app","brew":"jellyfin-media-player","brew_kind":"cask","home":"https://jellyfin.org"},{"id":"handbrake","name":"HandBrake","cat":"音视频","glyph":"media","note":"视频转码压体积","desc":"把大视频压小、转格式，预设直接选 Apple 设备或网页用。","app":"/Applications/HandBrake.app","brew":"handbrake","brew_kind":"cask","home":"https://handbrake.fr"},{"id":"obs","name":"OBS","cat":"音视频","glyph":"media","note":"录屏与直播","desc":"录屏幕、录摄像头、多路推流，教程和直播都用它。","app":"/Applications/OBS.app","brew":"obs","brew_kind":"cask","home":"https://obsproject.com"},{"id":"krita","name":"Krita","cat":"图像","glyph":"gallery","note":"开源绘画","desc":"笔刷、图层、动画都齐的绘画软件，画插画和分镜够用。","app":"/Applications/krita.app","brew":"krita","brew_kind":"cask","home":"https://krita.org"},{"id":"gimp","name":"GIMP","cat":"图像","glyph":"gallery","note":"开源修图","desc":"接近 Photoshop 的那一套：图层、蒙版、滤镜，修图和抠图都能干。","app":"/Applications/GIMP.app","brew":"gimp","brew_kind":"cask","home":"https://www.gimp.org"},{"id":"inkscape","name":"Inkscape","cat":"图像","glyph":"gallery","note":"矢量图 / SVG","desc":"画 SVG、图标、示意图，输出能直接塞进网页。","app":"/Applications/Inkscape.app","brew":"inkscape","brew_kind":"cask","home":"https://inkscape.org"},{"id":"alt-tab","name":"AltTab","cat":"效率","glyph":"devices","note":"按窗口切 App","desc":"⌥+Tab 按窗口切换，而不是按 App；多窗口的活儿快很多。","app":"/Applications/AltTab.app","brew":"alt-tab","brew_kind":"cask","home":"https://alt-tab.app"},{"id":"maccy","name":"Maccy","cat":"效率","glyph":"devices","note":"剪贴板历史","desc":"⇧⌘C 翻之前复制过的东西，找刚复制的那段文字不用重来。","app":"/Applications/Maccy.app","brew":"maccy","brew_kind":"cask","home":"https://maccy.app"},{"id":"itsycal","name":"Itsycal","cat":"效率","glyph":"devices","note":"菜单栏日历","desc":"菜单栏显示日期，点开看日程，点某天直接开系统日历。","app":"/Applications/Itsycal.app","brew":"itsycal","brew_kind":"cask","home":"https://www.mowglii.com/itsycal/"},{"id":"hiddenbar","name":"Hidden Bar","cat":"效率","glyph":"devices","note":"收起菜单栏图标","desc":"把不常用的菜单栏图标折起来，只留想看的。","app":"/Applications/Hidden Bar.app","brew":"hiddenbar","brew_kind":"cask","home":"https://github.com/dwarvesf/hidden"},{"id":"keepingyouawake","name":"KeepingYouAwake","cat":"效率","glyph":"devices","note":"一键防休眠","desc":"菜单栏一个小咖啡杯，点了就不休眠；跑长任务、下大文件时用。","app":"/Applications/KeepingYouAwake.app","brew":"keepingyouawake","brew_kind":"cask","home":"https://keepingyouawake.app"},{"id":"espanso","name":"Espanso","cat":"效率","glyph":"devices","note":"打字自动展开","desc":"打一个缩写就展开成整段文字（地址、签名、常用回复），所有 App 都生效。","app":"/Applications/Espanso.app","brew":"espanso","brew_kind":"cask","home":"https://espanso.org"},{"id":"stats","name":"Stats","cat":"系统","glyph":"inspect","note":"菜单栏系统监控","desc":"菜单栏看 CPU、内存、温度、网速、磁盘，比活动监视器顺手。","app":"/Applications/Stats.app","brew":"stats","brew_kind":"cask","home":"https://github.com/exelban/stats"},{"id":"keka","name":"Keka","cat":"系统","glyph":"ext","note":"压缩解压","desc":"7z、rar、tar、zip 都认，比系统自带的归档实用工具全。","app":"/Applications/Keka.app","brew":"keka","brew_kind":"cask","home":"https://www.keka.io"},{"id":"imageoptim","name":"ImageOptim","cat":"系统","glyph":"gallery","note":"图片瘦身","desc":"把图拖进去就压小，肉眼几乎看不出差别；发网页、发文章前用。","app":"/Applications/ImageOptim.app","brew":"imageoptim","brew_kind":"cask","home":"https://imageoptim.com/mac"},{"id":"balenaetcher","name":"Etcher","cat":"系统","glyph":"ext","note":"镜像写 U 盘","desc":"把系统镜像写到 U 盘 / SD 卡，给盒子、树莓派刷机用。","app":"/Applications/balenaEtcher.app","brew":"balenaetcher","brew_kind":"cask","home":"https://etcher.balena.io"},{"id":"keepassxc","name":"KeePassXC","cat":"系统","glyph":"ext","note":"本地密码库","desc":"所有密码存一个加密文件，换机器拷走就能用，不依赖云。","app":"/Applications/KeePassXC.app","brew":"keepassxc","brew_kind":"cask","home":"https://keepassxc.org"},{"id":"iterm2","name":"iTerm2","cat":"开发","glyph":"github","note":"增强终端","desc":"系统终端的加强版：分屏、搜索、粘贴历史、触发器。","app":"/Applications/iTerm.app","brew":"iterm2","brew_kind":"cask","home":"https://iterm2.com"},{"id":"vscode","name":"VS Code","cat":"开发","glyph":"github","note":"写代码","desc":"编辑器里插件生态最大的一个，远程开发、Jupyter 都能接。","app":"/Applications/Visual Studio Code.app","brew":"visual-studio-code","brew_kind":"cask","home":"https://code.visualstudio.com"},{"id":"zed","name":"Zed","cat":"开发","glyph":"github","note":"新编辑器","desc":"Rust 写的新编辑器，开大文件和搜索都很快，内置协作。","app":"/Applications/Zed.app","brew":"zed","brew_kind":"cask","home":"https://zed.dev"},{"id":"ghostty","name":"Ghostty","cat":"开发","glyph":"github","note":"GPU 终端","desc":"GPU 渲染的终端，启动快、配置简单，原生支持分屏。","app":"/Applications/Ghostty.app","brew":"ghostty","brew_kind":"cask","home":"https://ghostty.org"},{"id":"bruno","name":"Bruno","cat":"开发","glyph":"github","note":"API 调试","desc":"调接口的客户端，集合存本地文件夹、能进 git，不往云上传。","app":"/Applications/Bruno.app","brew":"bruno","brew_kind":"cask","home":"https://www.usebruno.com"},{"id":"dbeaver","name":"DBeaver","cat":"开发","glyph":"github","note":"数据库客户端","desc":"连 MySQL、Postgres、SQLite 的图形客户端，SQL 补全和导出都顺手。","app":"/Applications/DBeaver.app","brew":"dbeaver-community","brew_kind":"cask","home":"https://dbeaver.io"},{"id":"jan","name":"Jan","cat":"模型","glyph":"llmcost","note":"本地模型聊天","desc":"在桌面端跑本地模型的聊天客户端，断网也能用。","app":"/Applications/Jan.app","brew":"jan","brew_kind":"cask","home":"https://jan.ai"},{"id":"chatbox","name":"Chatbox","cat":"模型","glyph":"llmcost","note":"多家模型客户端","desc":"一个客户端接各家模型的 API，桌面端能存多套对话。","app":"/Applications/Chatbox.app","brew":"chatbox","brew_kind":"cask","home":"https://chatboxai.app"}]"#;

fn builtin_apps() -> &'static Vec<Value> {
    static S: OnceLock<Vec<Value>> = OnceLock::new();
    S.get_or_init(|| {
        serde_json::from_str::<Vec<Value>>(BUILTIN_APPS_JSON).unwrap_or_default()
    })
}

fn ext_catalog() -> &'static Vec<Value> {
    static S: OnceLock<Vec<Value>> = OnceLock::new();
    S.get_or_init(|| {
        serde_json::from_str::<Vec<Value>>(EXT_CATALOG_JSON).unwrap_or_default()
    })
}

const APP_MAX: usize = 30;
const POSIX_ONLY: &[&str] = &[
    "gallery", "reader", "media", "inspect", "llmcost", "follow", "review", "gateway", "qdrant",
];

fn posix_ok(aid: &str) -> bool {
    cfg!(not(windows)) || !POSIX_ONLY.contains(&aid)
}

// ------------------------------------------------------------------ 路径

/// `dash._root(cfg)`：allowed_roots[0]，否则 inbox_dir 的父目录。
fn root(ctx: &Ctx) -> PathBuf {
    if let Some(first) = ctx
        .cfg
        .get("allowed_roots")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
    {
        if !first.trim().is_empty() {
            return PathBuf::from(first);
        }
    }
    let inbox = ctx
        .cfg
        .get("inbox_dir")
        .and_then(|v| v.as_str())
        .unwrap_or(".");
    PathBuf::from(inbox)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn workbench(ctx: &Ctx) -> PathBuf {
    crate::paths::workbench_dir(ctx)
}

fn launchers_path(ctx: &Ctx) -> PathBuf {
    workbench(ctx).join("launchers.json")
}

fn plugins_path(ctx: &Ctx) -> PathBuf {
    workbench(ctx).join("plugins.json")
}

fn recipes_path(ctx: &Ctx) -> PathBuf {
    workbench(ctx).join("第三方插件.json")
}

fn icon_dir(ctx: &Ctx) -> PathBuf {
    workbench(ctx).join("_图标")
}

fn page_path(ctx: &Ctx) -> PathBuf {
    workbench(ctx).join("dash.html")
}

fn load(path: &Path) -> Option<Value> {
    jsonio::read_json(path)
}

fn s_or<'a>(v: &'a Value, k: &str, d: &'a str) -> &'a str {
    v.get(k)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(d)
}

fn str_or_empty(v: &Value, k: &str) -> String {
    util::str_or_empty(v.get(k))
}

// ------------------------------------------------------------------ 插件清单

static PLUGIN_ID: OnceLock<Regex> = OnceLock::new();
fn plugin_id_re() -> &'static Regex {
    PLUGIN_ID.get_or_init(|| Regex::new(r"^[a-z][a-z0-9_-]{0,31}$").unwrap())
}

fn slug(text: &str) -> String {
    let re = Regex::new(r"[^a-z0-9]+").unwrap();
    let lower = text.to_lowercase();
    let s = re.replace_all(&lower, "-");
    let s = s.trim_matches('-');
    util::cut(s, 24)
}

fn norm_plugin(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let name = util::cut(&util::py_strip(&str_or_empty(raw, "name")), 24);
    if name.is_empty() {
        return None;
    }
    let soon = raw.get("soon").map(util::truthy).unwrap_or(false);
    let ext = util::py_strip(&str_or_empty(raw, "ext"));
    let port: Option<i64> = match raw.get("port") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() || s == "0" => None,
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => None,
        Some(v) => util::int_of(v).ok(),
    };
    let mut url = util::py_strip(&str_or_empty(raw, "url"));
    if url.is_empty() && port.is_some() && ext.is_empty() {
        url = format!("http://127.0.0.1:{}/", port.unwrap());
    }
    if url.is_empty() && !soon && ext.is_empty() {
        return None;
    }
    let mut pid = util::py_strip(&str_or_empty(raw, "id")).to_lowercase();
    if !plugin_id_re().is_match(&pid) {
        let alt = slug(&str_or_empty(raw, "id"));
        let alt = if alt.is_empty() {
            match port {
                Some(p) => format!("p{p}"),
                None => String::new(),
            }
        } else {
            alt
        };
        let alt = if alt.is_empty() {
            format!("p{}", uuid_hex(6))
        } else {
            alt
        };
        pid = if plugin_id_re().is_match(&alt) {
            alt
        } else {
            format!("p{}", uuid_hex(6))
        };
    }
    let cmd: Option<(String, String)> = match raw.get("cmd") {
        Some(Value::Array(a)) if a.len() == 2 => {
            let d = a[0].as_str().map(|s| s.trim().to_string()).unwrap_or_default();
            let f = a[1].as_str().map(|s| s.trim().to_string()).unwrap_or_default();
            if d.is_empty() || f.is_empty() { None } else { Some((d, f)) }
        }
        Some(Value::String(s)) if s.contains('/') => {
            let (d, f) = s.trim().rsplit_once('/').unwrap_or(("", ""));
            if d.trim().is_empty() || f.trim().is_empty() {
                None
            } else {
                Some((d.trim().to_string(), f.trim().to_string()))
            }
        }
        _ => None,
    };
    let mut group = util::py_strip(&str_or_empty(raw, "group")).to_lowercase();
    if !matches!(group.as_str(), "tool" | "plugin" | "other") {
        group = if port.is_some() || cmd.is_some() {
            "plugin".to_string()
        } else {
            "tool".to_string()
        };
    }
    let scope = if !ext.is_empty() {
        "ext"
    } else if soon {
        "soon"
    } else if port.is_some() || cmd.is_some() {
        "service"
    } else {
        "view"
    };
    let glyph = {
        let g = util::py_strip(&str_or_empty(raw, "glyph"));
        if g.is_empty() {
            "plugin".to_string()
        } else {
            g
        }
    };
    let svc_name = {
        let s = util::py_strip(&str_or_empty(raw, "svc_name"));
        if s.is_empty() {
            name.clone()
        } else {
            s
        }
    };
    let mut m = Map::new();
    m.insert("id".into(), json!(pid));
    m.insert("name".into(), json!(name));
    m.insert("glyph".into(), json!(glyph));
    m.insert(
        "note".into(),
        json!(util::cut(&util::py_strip(&str_or_empty(raw, "note")), 60)),
    );
    m.insert("url".into(), json!(url));
    m.insert("port".into(), port.map(|p| json!(p)).unwrap_or(Value::Null));
    m.insert(
        "host".into(),
        json!(util::cut(&util::py_strip(&str_or_empty(raw, "host")), 64)),
    );
    m.insert("scope".into(), json!(scope));
    m.insert(
        "cmd".into(),
        match cmd {
            Some((d, f)) => json!([d, f]),
            None => Value::Null,
        },
    );
    m.insert("svc_name".into(), json!(svc_name));
    m.insert("source".into(), json!("plugin"));
    m.insert("group".into(), json!(group));
    m.insert("soon".into(), json!(soon));
    m.insert("ext".into(), json!(ext));
    Some(Value::Object(m))
}

fn read_plugins(ctx: &Ctx) -> Vec<Value> {
    let d = load(&plugins_path(ctx)).unwrap_or(Value::Null);
    let rows = match &d {
        Value::Object(o) => o.get("plugins").cloned(),
        Value::Array(_) => Some(d.clone()),
        _ => None,
    };
    let mut out: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    if let Some(Value::Array(a)) = rows {
        for raw in a {
            if let Some(p) = norm_plugin(&raw) {
                let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !seen.contains(&id) {
                    seen.push(id);
                    out.push(p);
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ 卡片自报入口

fn auto_view_apps(ctx: &Ctx) -> Vec<Value> {
    let Ok(html) = std::fs::read_to_string(page_path(ctx)) else {
        return Vec::new();
    };
    let tag = Regex::new(r#"<[a-zA-Z][^>]*\bdata-card="([A-Za-z0-9_-]+)"[^>]*>"#).unwrap();
    let attr = Regex::new(r#"data-tool(-glyph|-note|-group)?="([^"]*)""#).unwrap();
    let mut out = Vec::new();
    for m in tag.captures_iter(&html) {
        let whole = m.get(0).map(|x| x.as_str()).unwrap_or("");
        let cid = m.get(1).map(|x| x.as_str()).unwrap_or("").to_string();
        let mut attrs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for a in attr.captures_iter(whole) {
            let k = a.get(1).map(|x| x.as_str()).unwrap_or("");
            let v = a.get(2).map(|x| x.as_str()).unwrap_or("");
            attrs.insert(k.to_string(), v.to_string());
        }
        let name = util::py_strip(attrs.get("").map(|s| s.as_str()).unwrap_or(""));
        if name.is_empty() {
            continue;
        }
        let glyph = {
            let g = util::py_strip(attrs.get("-glyph").map(|s| s.as_str()).unwrap_or(""));
            if g.is_empty() { "pocket".to_string() } else { g }
        };
        let note = util::py_strip(attrs.get("-note").map(|s| s.as_str()).unwrap_or(""));
        let group = if util::py_strip(attrs.get("-group").map(|s| s.as_str()).unwrap_or(""))
            .to_lowercase()
            == "other"
        {
            "other"
        } else {
            "tool"
        };
        out.push(json!({
            "id": cid,
            "name": name,
            "kind": "builtin",
            "glyph": glyph,
            "note": note,
            "url": format!("/dash?v=more#card={cid}"),
            "port": Value::Null,
            "scope": "view",
            "up": true,
            "group": group,
        }));
    }
    out
}

/// `_all_specs`：plugins.json > BUILTIN_APPS > 卡片自报。
fn all_specs(ctx: &Ctx) -> Vec<Value> {
    let mut specs: Vec<Value> = builtin_apps().clone();
    // 口袋的 svc_port 在常量里写的是 8321（Python 版的口），全 Rust 之后实际监听
    // local_port（本机 8325 / LAN 8327），不改就会被永远判「掉线」。
    let pp = crate::routes::w5a::cards::pocket_port(ctx);
    for b in specs.iter_mut() {
        if b.get("id").and_then(|v| v.as_str()) == Some("pocket") {
            if let Some(o) = b.as_object_mut() {
                o.insert("svc_port".into(), json!(pp));
            }
        }
    }
    for p in read_plugins(ctx) {
        let pid = p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let mut merged = false;
        for b in specs.iter_mut() {
            if b.get("id").and_then(|v| v.as_str()) == Some(pid.as_str()) {
                if let (Some(bo), Some(po)) = (b.as_object_mut(), p.as_object()) {
                    for (k, v) in po {
                        if v.is_null() || v.as_str().map(|s| s.is_empty()).unwrap_or(false) {
                            continue;
                        }
                        bo.insert(k.clone(), v.clone());
                    }
                }
                merged = true;
                break;
            }
        }
        if !merged {
            specs.push(p);
        }
    }
    let mut have: Vec<String> = specs
        .iter()
        .map(|a| a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string())
        .collect();
    for a in auto_view_apps(ctx) {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if !have.contains(&id) {
            have.push(id);
            specs.push(a);
        }
    }
    specs.retain(|a| posix_ok(a.get("id").and_then(|v| v.as_str()).unwrap_or("")));
    specs
}

fn builtin_spec(ctx: &Ctx, aid: &str) -> Option<Value> {
    all_specs(ctx)
        .into_iter()
        .find(|a| a.get("id").and_then(|v| v.as_str()) == Some(aid))
}

/// 插件端口：给 `/px/<pid>/` 反代查「往哪转」。
///
/// 只读配置里的 `port`，**不去探测端口通不通** —— 反代本身就会把「连不上」
/// 变成 502 回给 iframe，那里能给出更准的话。在这儿探测等于每次 iframe 里的
/// 每个子请求都多等 350ms 超时，一个页面几十个请求，主页会卡成幻灯片。
pub(crate) fn plugin_port(ctx: &Ctx, pid: &str) -> u16 {
    builtin_spec(ctx, pid)
        .and_then(|a| a.get("port").and_then(|v| util::int_of(v).ok()))
        .unwrap_or(0) as u16
}

/// 插件主机：默认本机，`taskboard` 这类配了 `host` 的在别的机器上。
pub(crate) fn plugin_host(ctx: &Ctx, pid: &str) -> String {
    let h = builtin_spec(ctx, pid)
        .map(|a| str_or_empty(&a, "host"))
        .unwrap_or_default();
    if h.is_empty() {
        "127.0.0.1".to_string()
    } else {
        h
    }
}

// ------------------------------------------------------------------ 第三方配方

fn custom_recipes(ctx: &Ctx) -> Vec<Value> {
    let d = load(&recipes_path(ctx)).unwrap_or(Value::Null);
    let rows = match &d {
        Value::Object(o) => o.get("recipes").cloned(),
        Value::Array(_) => Some(d.clone()),
        _ => None,
    };
    match rows {
        Some(Value::Array(a)) => a
            .into_iter()
            .filter(|r| {
                r.is_object()
                    && !str_or_empty(r, "id").is_empty()
                    && !str_or_empty(r, "name").is_empty()
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn all_recipes(ctx: &Ctx) -> Vec<Value> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for r in custom_recipes(ctx) {
        let id = str_or_empty(&r, "id");
        if !seen.contains(&id) {
            seen.push(id);
            out.push(r);
        }
    }
    for r in ext_catalog() {
        let id = str_or_empty(r, "id");
        if !seen.contains(&id) {
            seen.push(id);
            out.push(r.clone());
        }
    }
    out
}

fn recipe(ctx: &Ctx, rid: &str) -> Option<Value> {
    let rid = util::py_strip(rid);
    all_recipes(ctx)
        .into_iter()
        .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(rid.as_str()))
}

static APP_DIRS: OnceLock<Vec<String>> = OnceLock::new();
fn app_dirs() -> &'static Vec<String> {
    APP_DIRS.get_or_init(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        vec![
            "/Applications".to_string(),
            "/System/Applications".to_string(),
            "/Applications/Utilities".to_string(),
            "/System/Applications/Utilities".to_string(),
            format!("{home}/Applications"),
        ]
    })
}

fn path_env() -> String {
    format!(
        "{}:/opt/homebrew/bin:/usr/local/bin",
        std::env::var("PATH").unwrap_or_default()
    )
}

fn which(name: &str) -> String {
    if name.is_empty() {
        return String::new();
    }
    for dir in path_env().split(':') {
        if dir.is_empty() {
            continue;
        }
        let p = Path::new(dir).join(name);
        if p.is_file() {
            return p.to_string_lossy().to_string();
        }
    }
    String::new()
}

/// `extplugins.find_app`：完整路径 → 各 Applications 目录 → 模糊两层。
fn find_app(query: &str) -> Option<PathBuf> {
    let q0 = util::py_strip(query);
    let q0 = q0.trim_end_matches('/').to_string();
    if q0.is_empty() {
        return None;
    }
    let p = crate::storage::expanduser(&q0);
    if q0.to_lowercase().ends_with(".app") && p.is_dir() {
        return Some(p);
    }
    let base_name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let name = if base_name.to_lowercase().ends_with(".app") {
        base_name[..base_name.len() - 4].to_string()
    } else {
        base_name.clone()
    };
    if name.is_empty() {
        return None;
    }
    for d in app_dirs() {
        let base = Path::new(d);
        if !base.is_dir() {
            continue;
        }
        for cand in [base.join(format!("{name}.app")), base.join(&name)] {
            if cand.is_dir() {
                return Some(cand);
            }
        }
    }
    let ql = name.to_lowercase();
    let mut hits: Vec<PathBuf> = Vec::new();
    for d in app_dirs() {
        let base = Path::new(d);
        if !base.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(base) else { continue };
        for ent in rd.flatten() {
            let n = ent.file_name().to_string_lossy().to_string();
            if !n.to_lowercase().ends_with(".app") {
                continue;
            }
            let stem = n[..n.len() - 4].to_string();
            let nl = stem.to_lowercase();
            if nl == ql {
                return Some(ent.path());
            }
            if nl.contains(&ql) {
                hits.push(ent.path());
            }
        }
    }
    if !hits.is_empty() {
        hits.sort_by_key(|x| {
            x.file_stem()
                .map(|s| s.to_string_lossy().chars().count())
                .unwrap_or(0)
        });
        return hits.into_iter().next();
    }
    for d in app_dirs() {
        let base = Path::new(d);
        if !base.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(base) else { continue };
        for sub in rd.flatten() {
            if !sub.path().is_dir() {
                continue;
            }
            let Ok(rd2) = std::fs::read_dir(sub.path()) else { continue };
            for ent in rd2.flatten() {
                let n = ent.file_name().to_string_lossy().to_string();
                if !n.to_lowercase().ends_with(".app") {
                    continue;
                }
                let stem = n[..n.len() - 4].to_string();
                if stem.to_lowercase().contains(&ql) {
                    return Some(ent.path());
                }
            }
        }
    }
    None
}

/// `extplugins.app_info`：bundle id / 可执行名 / 版本。二进制 plist 先转 XML。
fn app_info(app: &Path) -> Map<String, Value> {
    let mut out = Map::new();
    let stem = app
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    out.insert("bundle".into(), json!(""));
    out.insert("exe".into(), json!(stem));
    out.insert("version".into(), json!(""));
    let pl = app.join("Contents").join("Info.plist");
    let Ok(raw) = std::fs::read(&pl) else { return out };
    let text = if raw.starts_with(b"bplist") {
        match run_capture(
            "/usr/bin/plutil",
            &["-convert", "xml1", "-o", "-", &pl.to_string_lossy()],
            6000,
        ) {
            Some(t) => t,
            None => return out,
        }
    } else {
        String::from_utf8_lossy(&raw).to_string()
    };
    let get = |key: &str| -> String {
        let pat = format!(
            r"<key>{}</key>\s*<string>([^<]*)</string>",
            regex::escape(key)
        );
        Regex::new(&pat)
            .ok()
            .and_then(|re| re.captures(&text).map(|c| c[1].to_string()))
            .unwrap_or_default()
    };
    let bundle = get("CFBundleIdentifier");
    let exe = get("CFBundleExecutable");
    let version = get("CFBundleShortVersionString");
    out.insert("bundle".into(), json!(bundle));
    out.insert(
        "exe".into(),
        json!(if exe.is_empty() {
            app.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            exe
        }),
    );
    out.insert("version".into(), json!(version));
    out
}

fn _exe_name(r: &Value) -> String {
    let app = util::py_strip(&str_or_empty(r, "app"));
    if !app.is_empty() {
        let p = crate::storage::expanduser(&app);
        if p.is_dir() {
            let got = app_info(&p);
            let exe = got.get("exe").and_then(|v| v.as_str()).unwrap_or("");
            if !exe.is_empty() {
                return exe.to_string();
            }
        }
    }
    let exe = util::py_strip(&str_or_empty(r, "exe"));
    if !exe.is_empty() {
        return exe;
    }
    let pat = util::py_strip(&str_or_empty(r, "proc"));
    if pat.is_empty() {
        return String::new();
    }
    if pat.contains('/') {
        return Path::new(&pat)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
    }
    pat.split_whitespace().next().unwrap_or("").to_string()
}

fn brew_installed(r: &Value) -> bool {
    let b = str_or_empty(r, "brew");
    if b.is_empty() {
        return false;
    }
    let sub = if r.get("brew_kind").and_then(|v| v.as_str()) == Some("cask") {
        "Caskroom"
    } else {
        "Cellar"
    };
    ["/opt/homebrew", "/usr/local"]
        .iter()
        .any(|prefix| Path::new(prefix).join(sub).join(&b).is_dir())
}

fn installed(r: &Value) -> bool {
    let app = util::py_strip(&str_or_empty(r, "app"));
    if !app.is_empty() && crate::storage::expanduser(&app).is_dir() {
        return true;
    }
    if app.is_empty() {
        let name = str_or_empty(r, "name");
        if !name.is_empty() && find_app(&name).is_some() {
            return true;
        }
    }
    let cli = str_or_empty(r, "cli");
    if !cli.is_empty() && !which(&cli).is_empty() {
        return true;
    }
    if cfg!(windows) {
        return false;
    }
    brew_installed(r)
}

fn pgrep_exact(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let ok = std::process::Command::new("/usr/bin/pgrep")
        .arg("-x")
        .arg("-q")
        .arg(name)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok
}

fn port_of(r: &Value) -> Option<u16> {
    r.get("port").and_then(|v| util::int_of(v).ok()).and_then(|n| {
        if (1..=65535).contains(&n) {
            Some(n as u16)
        } else {
            None
        }
    })
}

fn running(r: &Value) -> bool {
    if pgrep_exact(&_exe_name(r)) {
        return true;
    }
    match port_of(r) {
        Some(p) => util::port_open("127.0.0.1", p, 350),
        None => false,
    }
}

fn version_of(r: &Value) -> String {
    let app = util::py_strip(&str_or_empty(r, "app"));
    if !app.is_empty() {
        let p = crate::storage::expanduser(&app);
        if p.is_dir() {
            return app_info(&p)
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
    }
    let name = str_or_empty(r, "name");
    if let Some(found) = find_app(&name) {
        return app_info(&found)
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    String::new()
}

fn probe(r: &Value) -> Value {
    let inst = installed(r);
    let run = if inst { running(r) } else { false };
    let port_open = if inst {
        match port_of(r) {
            Some(p) => util::port_open("127.0.0.1", p, 350),
            None => false,
        }
    } else {
        false
    };
    json!({
        "installed": inst,
        "running": run,
        "version": if inst { version_of(r) } else { String::new() },
        "port": r.get("port").cloned().unwrap_or(Value::Null),
        "port_open": port_open,
    })
}

// ------------------------------------------------------------------ 子进程

fn run_capture(bin: &str, args: &[&str], timeout_ms: u64) -> Option<String> {
    let mut cmd = std::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = child.wait_with_output().ok()?;
                return Some(String::from_utf8_lossy(&out.stdout).to_string());
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// `dash._spawn`：脱离当前进程组，输出进 ~/Library/Logs。
fn spawn_detached(cmd: &[String], log_name: &str) -> std::io::Result<()> {
    let home = std::env::var("HOME").unwrap_or_default();
    let logdir = Path::new(&home).join("Library").join("Logs");
    let dir = if std::fs::create_dir_all(&logdir).is_ok() {
        logdir
    } else {
        PathBuf::from("/tmp")
    };
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(log_name))?;
    let err = out.try_clone()?;
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .env("CUNTU_NO_OPEN", "1");
    // 脱离当前进程组：父进程（本服务）被关掉时子进程不能跟着死。
    #[cfg(all(unix, not(feature = "sim_windows")))]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    c.spawn()?;
    Ok(())
}

/// `uuid.uuid4().hex[:n]`：n 是**字符数**，不是字节数。
fn uuid_hex(n: usize) -> String {
    let mut s = super::fleet::rand_hex((n + 1) / 2);
    s.truncate(n);
    s
}

// ------------------------------------------------------------------ 图标

fn extract_icon(app_path: &Path, dest: &Path) -> bool {
    let res = app_path.join("Contents").join("Resources");
    let plist = app_path.join("Contents").join("Info.plist");
    let mut names: Vec<String> = Vec::new();
    if let Some(v) = run_capture(
        "/usr/bin/plutil",
        &["-extract", "CFBundleIconFile", "raw", &plist.to_string_lossy()],
        6000,
    ) {
        let v = util::py_strip(&v);
        if !v.is_empty() {
            names.push(v);
        }
    }
    names.push("AppIcon".to_string());
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    for n in names {
        let first = if n.to_lowercase().ends_with(".icns") {
            res.join(&n)
        } else {
            res.join(format!("{n}.icns"))
        };
        for f in [first, res.join(&n)] {
            if !f.is_file() {
                continue;
            }
            let _ = run_capture(
                "/usr/bin/sips",
                &[
                    "-Z",
                    "128",
                    "-s",
                    "format",
                    "png",
                    &f.to_string_lossy(),
                    "--out",
                    &dest.to_string_lossy(),
                ],
                12000,
            );
            if let Ok(md) = std::fs::metadata(dest) {
                if md.is_file() && md.len() > 200 {
                    return true;
                }
            }
        }
    }
    false
}

// ------------------------------------------------------------------ 入口清单

fn read_launchers(ctx: &Ctx) -> Vec<Value> {
    let d = load(&launchers_path(ctx)).unwrap_or(Value::Null);
    let items = match &d {
        Value::Object(o) => o.get("items").cloned(),
        _ => None,
    };
    let mut out = Vec::new();
    if let Some(Value::Array(a)) = items {
        for it in a {
            if !it.is_object() {
                continue;
            }
            if str_or_empty(&it, "id").is_empty() {
                continue;
            }
            let kind = {
                let k = str_or_empty(&it, "kind");
                if k.is_empty() { "app".to_string() } else { k }
            };
            let name = {
                let n = str_or_empty(&it, "name");
                if n.is_empty() { "入口".to_string() } else { n }
            };
            let created = match it.get("created") {
                Some(v) if util::truthy(v) => v.clone(),
                _ => json!(""),
            };
            out.push(json!({
                "id": str_or_empty(&it, "id"),
                "name": name,
                "kind": kind,
                "target": str_or_empty(&it, "target"),
                "url": str_or_empty(&it, "url"),
                "hint": str_or_empty(&it, "hint"),
                "icon": str_or_empty(&it, "icon"),
                "card": it.get("card").map(|v| v.as_bool() != Some(false)).unwrap_or(true),
                "created": created,
            }));
        }
    }
    out
}

fn write_launchers(ctx: &Ctx, items: &[Value]) {
    let payload = json!({"items": items, "updated_at": util::now_iso()});
    util::write_json_file(&launchers_path(ctx), &payload);
}

fn app_exists(p: &str) -> bool {
    Path::new(p).exists()
}

// ------------------------------------------------------------------ parse_launcher

static RE_OPEN: OnceLock<Regex> = OnceLock::new();
static RE_URL: OnceLock<Regex> = OnceLock::new();
static RE_SCHEME: OnceLock<Regex> = OnceLock::new();
static RE_BUNDLE: OnceLock<Regex> = OnceLock::new();

fn re_open() -> &'static Regex {
    RE_OPEN.get_or_init(|| Regex::new(r"(?is)^open\s+((-a|-b)\s+)?(.*)$").unwrap())
}
fn re_url() -> &'static Regex {
    RE_URL.get_or_init(|| Regex::new(r"(?i)^(https?|ftp)://").unwrap())
}
fn re_scheme() -> &'static Regex {
    RE_SCHEME.get_or_init(|| Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").unwrap())
}
fn re_bundle() -> &'static Regex {
    RE_BUNDLE.get_or_init(|| Regex::new(r"^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+){2,}$").unwrap())
}

fn title_last_dot(s: &str) -> String {
    let last = s.rsplit('.').next().unwrap_or(s);
    // Python str.title()：下划线后也大写，这里够用（bundle 末段是字母）。
    let mut out = String::new();
    let mut up = true;
    for c in last.chars() {
        if c.is_alphabetic() {
            if up {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            up = false;
        } else {
            out.push(c);
            up = true;
        }
    }
    out
}

pub fn parse_launcher(text: &str) -> Value {
    let raw = util::py_strip(text);
    if raw.is_empty() {
        return json!({"error": "先粘贴点东西进来"});
    }
    if raw.chars().count() > 400 {
        return json!({"error": "太长了，粘个路径或名字就行"});
    }
    let mut s = raw.clone();
    if let Some(c) = re_open().captures(&s) {
        let flag = c.get(2).map(|m| m.as_str().to_lowercase()).unwrap_or_default();
        let rest = c.get(3).map(|m| m.as_str()).unwrap_or("").to_string();
        let rest = util::py_strip(&rest);
        let rest = util::py_strip(&rest.trim_matches('"'));
        let rest = util::py_strip(&rest.trim_matches('\''));
        if flag == "-b" && !rest.is_empty() {
            return json!({"kind": "bundle", "name": title_last_dot(&rest),
                          "target": rest, "hint": rest});
        }
        s = rest;
    }
    s = util::py_strip(&s);
    s = util::py_strip(&s.trim_matches('"'));
    s = util::py_strip(&s.trim_matches('\''));
    if s.is_empty() {
        return json!({"error": "只粘了个 open，后面还差应用名"});
    }
    if re_url().is_match(&s) {
        let no_scheme = Regex::new(r"(?i)^[a-z]+://").unwrap().replace(&s, "");
        let host = no_scheme.split('/').next().unwrap_or("").to_string();
        let name = if host.is_empty() { s.clone() } else { host };
        return json!({"kind": "url", "name": name, "target": s, "url": s, "hint": s});
    }
    if s.to_lowercase().starts_with("www.") {
        let host = s.split('/').next().unwrap_or("").to_string();
        return json!({"kind": "url", "name": host, "target": format!("https://{s}"),
                      "url": format!("https://{s}"), "hint": s});
    }
    if re_scheme().is_match(&s) {
        return json!({"error": "这个链接不是 http(s)，浏览器打不开；换成网址或应用名吧"});
    }
    if let Some(app) = find_app(&s) {
        let stem = app
            .file_stem()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        return json!({"kind": "app", "name": stem, "target": app.to_string_lossy(),
                      "hint": s});
    }
    let p = crate::storage::expanduser(&s);
    if p.exists() {
        let name = p
            .file_name()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        return json!({"kind": "path", "name": name, "target": p.to_string_lossy(),
                      "hint": s});
    }
    if re_bundle().is_match(&s) {
        return json!({"kind": "bundle", "name": title_last_dot(&s), "target": s, "hint": s});
    }
    if let Some(_out) = run_capture("/usr/bin/open", &["-Ra", &s], 6000) {
        return json!({"kind": "appname", "name": s, "target": s, "hint": s});
    }
    json!({"error": format!("没找到「{s}」这个应用。可以粘贴 /Applications 里的 .app 全路径，\
                             或者直接写应用名（如 Safari、Music）")})
}

// ------------------------------------------------------------------ /api/apps

pub fn apps_payload(ctx: &Ctx) -> Value {
    let mut builtin: Vec<Value> = Vec::new();
    let builtin_ids: Vec<String> = builtin_apps()
        .iter()
        .map(|a| a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string())
        .collect();
    for a in all_specs(ctx) {
        let id = a.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let source = {
            let s = str_or_empty(&a, "source");
            if !s.is_empty() {
                s
            } else if builtin_ids.contains(&id) {
                "builtin".to_string()
            } else {
                "card".to_string()
            }
        };
        let ext_id = str_or_empty(&a, "ext");
        let rec = if ext_id.is_empty() {
            None
        } else {
            recipe(ctx, &ext_id)
        };
        let soon = a.get("soon").map(util::truthy).unwrap_or(false);
        let up = if soon {
            false
        } else if let Some(r) = &rec {
            running(r)
        } else if a.get("port").map(|v| !util::truthy(v)).unwrap_or(true) {
            true
        } else {
            let port = a.get("port").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
            let host = {
                let h = str_or_empty(&a, "host");
                if h.is_empty() { "127.0.0.1".to_string() } else { h }
            };
            util::port_open(&host, port, 350)
        };
        let installed_flag = rec.as_ref().map(|r| installed(r)).unwrap_or(false);
        let host_v = str_or_empty(&a, "host");
        let scope_v = {
            let s = str_or_empty(&a, "scope");
            if s.is_empty() { "service".to_string() } else { s }
        };
        let group_v = {
            let g = str_or_empty(&a, "group");
            if g.is_empty() { "plugin".to_string() } else { g }
        };
        builtin.push(json!({
            "id": id,
            "name": a.get("name").cloned().unwrap_or(Value::Null),
            "kind": "builtin",
            "glyph": a.get("glyph").cloned().unwrap_or(Value::Null),
            "note": a.get("note").cloned().unwrap_or(Value::Null),
            "url": a.get("url").cloned().unwrap_or(Value::Null),
            "port": a.get("port").cloned().unwrap_or(Value::Null),
            "host": host_v,
            "scope": scope_v,
            "group": group_v,
            "soon": soon,
            "ext": ext_id,
            "installed": installed_flag,
            "source": source,
            "editable": source == "plugin",
            "up": up,
        }));
    }
    let mut custom: Vec<Value> = Vec::new();
    for it in read_launchers(ctx) {
        let kind = it.get("kind").and_then(|v| v.as_str()).unwrap_or("app").to_string();
        let alive = matches!(kind.as_str(), "url" | "bundle" | "appname")
            || app_exists(&str_or_empty(&it, "target"));
        let mut m = it.as_object().cloned().unwrap_or_default();
        m.insert("alive".into(), json!(alive));
        custom.push(Value::Object(m));
    }
    json!({"builtin": builtin, "custom": custom})
}

fn media_boot_note(base: &Path, kind: &str, error: &str, secs: i64) -> bool {
    let p = base.join("_墙").join("记录.jsonl");
    let rec = json!({
        "ts": util::now_space(),
        "kind": kind,
        "ok": false,
        "secs": secs,
        "done": 0,
        "total": 0,
        "error": error,
    });
    let line = format!("{}\n", util::dumps_py(&rec));
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    match std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        Ok(mut f) => f.write_all(line.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

fn media_boot_recent(base: &Path, secs: i64) -> bool {
    let p = base.join("_墙").join("记录.jsonl");
    let Ok(text) = std::fs::read_to_string(&p) else { return false };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    for line in lines.iter().rev().take(60) {
        let Ok(r) = serde_json::from_str::<Value>(line) else { continue };
        let kind = r.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(kind, "boot_timeout" | "launch_timeout") {
            continue;
        }
        let ts = r.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let t = util::parse_ts(ts).unwrap_or(0.0);
        return t > 0.0 && (util::now_epoch() - t) <= secs as f64;
    }
    false
}

fn start_recipe(ctx: &Ctx, rid: &str) -> Value {
    let Some(r) = recipe(ctx, rid) else {
        return json!({"ok": false, "error": "没有这条配方"});
    };
    let name = str_or_empty(&r, "name");
    if !installed(&r) {
        return json!({"ok": false, "error": format!("{name} 还没装")});
    }
    if running(&r) {
        return json!({"ok": true, "started": false, "state": probe(&r)});
    }
    let argv: Vec<String> = if let Some(Value::Array(a)) = r.get("start") {
        if a.is_empty() {
            Vec::new()
        } else {
            let first = a[0].as_str().unwrap_or("");
            let which_first = which(first);
            let mut v = vec![if which_first.is_empty() { first.to_string() } else { which_first }];
            for x in &a[1..] {
                v.push(x.as_str().unwrap_or("").to_string());
            }
            v
        }
    } else {
        let app = util::py_strip(&str_or_empty(&r, "app"));
        if !app.is_empty() && crate::storage::expanduser(&app).is_dir() {
            vec!["/usr/bin/open".into(), "-a".into(), app]
        } else if !str_or_empty(&r, "bundle").is_empty() {
            vec![
                "/usr/bin/open".into(),
                "-b".into(),
                str_or_empty(&r, "bundle"),
            ]
        } else if !name.is_empty() {
            match find_app(&name) {
                Some(app) => vec![
                    "/usr/bin/open".into(),
                    "-a".into(),
                    app.to_string_lossy().to_string(),
                ],
                None => return json!({"ok": false, "error": format!("找不到 {name}.app")}),
            }
        } else {
            return json!({"ok": false, "error": "这条配方没写怎么启动"});
        }
    };
    if argv.is_empty() {
        return json!({"ok": false, "error": "这条配方没写怎么启动"});
    }
    let log = {
        let home = std::env::var("HOME").unwrap_or_default();
        let d = Path::new(&home).join("Library").join("Logs");
        let _ = std::fs::create_dir_all(&d);
        d.join(format!("pocket-ext-{rid}.log"))
    };
    let spawn = (|| -> std::io::Result<()> {
        let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        let err = out.try_clone()?;
        let mut c = std::process::Command::new(&argv[0]);
        c.args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err));
        #[cfg(all(unix, not(feature = "sim_windows")))]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            c.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        }
        c.spawn()?;
        Ok(())
    })();
    if let Err(e) = spawn {
        return json!({"ok": false, "error": util::cut(&e.to_string(), 200)});
    }
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs_f64() < 10.0 {
        if running(&r) {
            return json!({
                "ok": true,
                "started": true,
                "seconds": util::round1(t0.elapsed().as_secs_f64()),
                "state": probe(&r),
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
    if port_of(&r).is_none() && str_or_empty(&r, "proc").is_empty() && !str_or_empty(&r, "app").is_empty()
    {
        return json!({"ok": true, "started": true, "state": probe(&r)});
    }
    json!({"ok": false, "started": true, "state": probe(&r),
           "error": format!("{name} 起了但没确认到在跑，看日志 ~/Library/Logs/pocket-ext-{rid}.log")})
}

pub fn launch_app(ctx: &Ctx, aid: &str, builtin: bool) -> Value {
    if builtin {
        let Some(spec) = builtin_spec(ctx, aid) else {
            return json!({"ok": false, "error": "没有这个入口"});
        };
        let name = str_or_empty(&spec, "name");
        if spec.get("soon").map(util::truthy).unwrap_or(false) {
            return json!({"ok": false, "error": format!("{name} 还是占位，还没做")});
        }
        let ext = str_or_empty(&spec, "ext");
        if !ext.is_empty() {
            let Some(_r) = recipe(ctx, &ext) else {
                return json!({"ok": false, "error": "这条第三方配方已经不在了"});
            };
            if !installed(&recipe(ctx, &ext).unwrap()) {
                return json!({"ok": false,
                              "error": format!("{name} 还没装：加插件 → 第三方插件 → 装它")});
            }
            if running(&recipe(ctx, &ext).unwrap()) {
                return json!({"ok": true, "up": true, "started": false,
                              "url": str_or_empty(&spec, "url")});
            }
            let mut got = start_recipe(ctx, &ext);
            if let Some(o) = got.as_object_mut() {
                o.insert("url".into(), json!(str_or_empty(&spec, "url")));
            }
            return got;
        }
        let port = spec.get("port").and_then(|v| util::int_of(v).ok());
        if port.is_none() || port == Some(0) {
            return json!({"ok": true, "up": true, "url": str_or_empty(&spec, "url"),
                          "started": false});
        }
        let port = port.unwrap();
        let remote = util::py_strip(&str_or_empty(&spec, "host"));
        let probe_host = if remote.is_empty() { "127.0.0.1".to_string() } else { remote.clone() };
        if util::port_open(&probe_host, port as u16, 350) {
            return json!({"ok": true, "up": true, "url": str_or_empty(&spec, "url"),
                          "started": false});
        }
        if !remote.is_empty() {
            return json!({"ok": false, "up": false,
                          "error": format!("{name}：{remote}:{port} 连不上 —— 那台机器上的服务没开，\
                                           或者端口映射（portproxy）没生效")});
        }
        let cmd = spec
            .get("cmd")
            .and_then(|v| v.as_array())
            .filter(|a| a.len() == 2)
            .map(|a| {
                (
                    a[0].as_str().unwrap_or("").to_string(),
                    a[1].as_str().unwrap_or("").to_string(),
                )
            });
        let Some((d, f)) = cmd else {
            return json!({"ok": false,
                          "error": format!("{name} 没配启动脚本，先手动把它起来（端口 {port}）")});
        };
        // 启动脚本可能挂在两个根上：_巡检 / 13-算价 在运行根，
        // 02-媒体 / 08-阅读 在媒体根。按目录名分派，别一律用其中一个。
        let base = if crate::paths::is_runtime_sub(&d) {
            ctx.runtime_dir.clone()
        } else {
            root(ctx)
        };
        let cmd_p = base.join(&d).join(&f);
        if !cmd_p.exists() {
            return json!({"ok": false,
                          "error": format!("启动脚本不在：{}", cmd_p.to_string_lossy())});
        }
        let argv = vec![
            "/bin/bash".to_string(),
            cmd_p.to_string_lossy().to_string(),
        ];
        if let Err(e) = spawn_detached(&argv, &format!("cuntu-{aid}.log")) {
            return json!({"ok": false, "error": util::cut(&e.to_string(), 200)});
        }
        let patience: f64 = if aid == "media" { 100.0 } else { 14.0 };
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_secs_f64() < patience {
            if util::port_open("127.0.0.1", port as u16, 250) {
                return json!({"ok": true, "up": true, "url": str_or_empty(&spec, "url"),
                              "started": true,
                              "seconds": util::round1(t0.elapsed().as_secs_f64())});
            }
            std::thread::sleep(std::time::Duration::from_millis(350));
        }
        let waited = t0.elapsed().as_secs() as i64;
        if patience > 20.0 {
            let base = root(ctx).join("02-媒体");
            if !media_boot_recent(&base, 300) {
                media_boot_note(
                    &base,
                    "launch_timeout",
                    &format!("{name}：口袋等了 {waited}s 端口还没通（多半还在扫盘）"),
                    waited,
                );
            }
            return json!({"ok": true, "up": false, "started": true, "pending": true,
                          "url": str_or_empty(&spec, "url"), "seconds": waited});
        }
        return json!({"ok": false, "up": false, "started": true,
                      "error": format!("{name} {} 秒内没起来，看 ~/Library/Logs/cuntu-{aid}.log",
                                       patience as i64)});
    }

    let items = read_launchers(ctx);
    let Some(it) = items
        .into_iter()
        .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(aid))
    else {
        return json!({"ok": false, "error": "入口不存在，可能被删了"});
    };
    let kind = str_or_empty(&it, "kind");
    if kind == "url" {
        let target = str_or_empty(&it, "target");
        let url = if target.is_empty() { str_or_empty(&it, "url") } else { target };
        return json!({"ok": true, "url": url, "started": false});
    }
    let target = str_or_empty(&it, "target");
    if matches!(kind.as_str(), "app" | "path") && !app_exists(&target) {
        return json!({"ok": false, "error": format!("目标不在了：{target}")});
    }
    let argv = {
        let mut v = vec!["/usr/bin/open".to_string()];
        if kind == "bundle" {
            v.push("-b".into());
            v.push(target);
        } else if kind == "app" || kind == "appname" {
            v.push("-a".into());
            v.push(target);
        } else {
            v.push(target);
        }
        v
    };
    if let Err(e) = spawn_detached(&argv, "dash-open.log") {
        return json!({"ok": false, "error": util::cut(&e.to_string(), 200)});
    }
    json!({"ok": true, "name": str_or_empty(&it, "name"), "started": true})
}

pub fn add_launcher(ctx: &Ctx, body: &Value) -> Value {
    let raw = str_or_empty(body, "text");
    let parsed = parse_launcher(&raw);
    if let Some(e) = parsed.get("error") {
        return json!({"ok": false, "error": e.clone()});
    }
    let items = read_launchers(ctx);
    if items.len() >= APP_MAX {
        return json!({"ok": false, "error": format!("入口最多 {APP_MAX} 个，先删几个")});
    }
    let name = {
        let n = util::py_strip(&str_or_empty(body, "name"));
        if n.is_empty() {
            util::cut(&str_or_empty(&parsed, "name"), 40)
        } else {
            util::cut(&n, 40)
        }
    };
    let lid = format!("l{}", uuid_hex(10));
    let kind = str_or_empty(&parsed, "kind");
    let target = str_or_empty(&parsed, "target");
    let mut icon = String::new();
    if matches!(kind.as_str(), "app" | "path") {
        let dest = icon_dir(ctx).join(format!("{lid}.png"));
        let src = Path::new(&target);
        let mut app = if src.extension().map(|e| e == "app").unwrap_or(false) {
            Some(src.to_path_buf())
        } else {
            None
        };
        if app.is_none() && src.is_dir() {
            if let Ok(rd) = std::fs::read_dir(src) {
                app = rd
                    .flatten()
                    .map(|e| e.path())
                    .find(|p| p.extension().map(|e| e == "app").unwrap_or(false));
            }
        }
        if let Some(a) = app {
            if extract_icon(&a, &dest) {
                icon = dest
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
            }
        }
    }
    let card = body.get("card").map(|v| v.as_bool() != Some(false)).unwrap_or(true);
    let purl = str_or_empty(&parsed, "url");
    let item = json!({
        "id": lid,
        "name": name,
        "kind": kind,
        "target": target,
        "url": purl,
        "hint": util::cut(&util::py_strip(&raw), 200),
        "icon": icon,
        "card": card,
        "created": util::now_iso(),
    });
    let mut all = items;
    all.push(item.clone());
    write_launchers(ctx, &all);
    let mut out = item.as_object().cloned().unwrap_or_default();
    out.insert("alive".into(), json!(true));
    json!({"ok": true, "item": Value::Object(out)})
}

pub fn preview_launcher(body: &Value) -> Value {
    let parsed = parse_launcher(&str_or_empty(body, "text"));
    if let Some(e) = parsed.get("error") {
        return json!({"ok": false, "error": e.clone()});
    }
    let mut m = Map::new();
    m.insert("ok".into(), json!(true));
    if let Some(o) = parsed.as_object() {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    Value::Object(m)
}

pub fn update_launcher(ctx: &Ctx, body: &Value) -> Value {
    let lid = str_or_empty(body, "id");
    let mut items = read_launchers(ctx);
    let Some(it) = items
        .iter_mut()
        .find(|x| x.get("id").and_then(|v| v.as_str()) == Some(lid.as_str()))
    else {
        return json!({"ok": false, "error": "没有这个入口"});
    };
    if body.get("card").is_some() {
        let card = body.get("card").map(|v| v.as_bool() != Some(false)).unwrap_or(true);
        if let Some(o) = it.as_object_mut() {
            o.insert("card".into(), json!(card));
        }
    }
    let nm = util::py_strip(&str_or_empty(body, "name"));
    if !nm.is_empty() {
        if let Some(o) = it.as_object_mut() {
            o.insert("name".into(), json!(util::cut(&nm, 40)));
        }
    }
    let snapshot = it.clone();
    write_launchers(ctx, &items);
    json!({"ok": true, "item": snapshot})
}

pub fn remove_launcher(ctx: &Ctx, lid: &str) -> Value {
    let items = read_launchers(ctx);
    let before = items.len();
    let keep: Vec<Value> = items
        .into_iter()
        .filter(|x| x.get("id").and_then(|v| v.as_str()) != Some(lid))
        .collect();
    if keep.len() == before {
        return json!({"ok": false, "error": "没有这个入口"});
    }
    write_launchers(ctx, &keep);
    let p = icon_dir(ctx).join(format!("{lid}.png"));
    if p.is_file() {
        let _ = std::fs::remove_file(&p);
    }
    json!({"ok": true})
}

pub fn launcher_icon(ctx: &Ctx, lid: &str) -> Option<PathBuf> {
    let re = Regex::new(r"[^A-Za-z0-9_-]").unwrap();
    let name = re.replace_all(lid, "").to_string();
    let p = icon_dir(ctx).join(format!("{name}.png"));
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

// ------------------------------------------------------------------ 路由

fn body_obj(raw: &[u8], headers: &HeaderMap) -> Result<Value, Response> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    match util::parse_body(raw, &ct, headers.contains_key(header::CONTENT_TYPE)) {
        util::Body::Obj(v) => Ok(v),
        util::Body::Bad(r) => Err(r),
    }
}

pub async fn list(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    Json(apps_payload(&ctx)).into_response()
}

pub async fn icon(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let id = q(&uri, "id");
    match launcher_icon(&ctx, &id) {
        None => http_err(404, "没有图标"),
        Some(p) => match std::fs::read(&p) {
            Ok(bytes) => {
                let mut res = bytes.into_response();
                let h = res.headers_mut();
                h.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("image/png"),
                );
                h.insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("max-age=86400"),
                );
                res
            }
            Err(_) => http_err(404, "没有图标"),
        },
    }
}

pub async fn launch(State(ctx): CtxState, headers: HeaderMap, uri: Uri, raw: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match body_obj(&raw, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let aid = str_or_empty(&body, "id");
    let builtin = body.get("builtin").map(util::truthy).unwrap_or(false);
    // 起进程最长要等 14~100 秒，扔到阻塞线程池，别把 tokio 的工作线程占死。
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || launch_app(&ctx2, &aid, builtin))
        .await
        .unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}));
    Json(out).into_response()
}

pub async fn add(State(ctx): CtxState, headers: HeaderMap, uri: Uri, raw: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match body_obj(&raw, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let ctx2 = ctx.clone();
    let out = tokio::task::spawn_blocking(move || add_launcher(&ctx2, &body))
        .await
        .unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}));
    Json(out).into_response()
}

pub async fn preview(State(ctx): CtxState, headers: HeaderMap, uri: Uri, raw: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match body_obj(&raw, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let out = tokio::task::spawn_blocking(move || preview_launcher(&body))
        .await
        .unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}));
    Json(out).into_response()
}

pub async fn update(State(ctx): CtxState, headers: HeaderMap, uri: Uri, raw: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match body_obj(&raw, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    Json(update_launcher(&ctx, &body)).into_response()
}

pub async fn remove(State(ctx): CtxState, headers: HeaderMap, uri: Uri, raw: Bytes) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) {
        return r;
    }
    let body = match body_obj(&raw, &headers) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let lid = str_or_empty(&body, "id");
    Json(remove_launcher(&ctx, &lid)).into_response()
}
