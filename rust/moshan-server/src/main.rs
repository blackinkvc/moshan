// SPDX-License-Identifier: MIT OR Apache-2.0
// 职责：装配服务端。已迁路由由 Rust 直接应答，其余原样转发给上游 Python。
// 不做：不建目录、不初始化数据根、不启动后台调度（这些随波次逐个接手，
//       W1 阶段保持"零副作用"，才能与 Python 逐字节对拍）。
//
// 监听矩阵（对齐 `moshan/server.py` 的 main() 尾部）：
//   - 主端口 host:port：tls 建得起来就 HTTPS，否则明文（host 是通配地址时
//     强制收回到 127.0.0.1 —— 明文令牌不能广播到局域网）；
//   - 127.0.0.1:local_port 无条件绑：本机浏览器对 127.0.0.1 上的自签证书
//     直接拦死，本机必须走明文；
//   - https 模式下再绑 tailscale_ip:plain_port：安卓不信任自签，手机走隧道明文。

mod auth;
mod boot;
mod hostwork;
mod imaging;
mod jsonio;
mod ledger;
mod live;
mod migrate;
mod paths;
mod proxy;
mod routes;
mod storage;
mod sysopen;
mod tls;
mod wechat;

use axum::routing::{get, post};
use axum::Router;
use serde_json::Value;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::watch;

// 线程数不是性能参数，是**可用性**参数：macOS 的隐私授权会把 `open()` 卡在内核里不返回，
// 而不少处理器是直接在 worker 上做文件 IO 的。10 核默认 10 个 worker，首屏那十来个请求
// 正好能把它们全占满 —— 之后 `/api/ping` 都排不上队，整页看起来就是死了。
// 给足余量，让「读不动的那些」卡住时其它请求还能照常应答。
#[tokio::main(flavor = "multi_thread", worker_threads = 32)]
async fn main() {
    // rustls 0.23 要求进程里只有一个默认 provider；但这个构建的依赖树同时打开了
    // ring 与 aws-lc-rs（reqwest 那条），`ServerConfig::builder()` 会直接 panic。
    // 显式装 ring —— 与 reqwest 手上那份实现一致，证书链/签名算法不会分叉。
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 端口三种来源，优先级从高到低：
    //   1. `MOSHAN_RS_PORT`：测试/多实例/对拍显式指定，此时**不动配置、不开 TLS**；
    //   2. `MOSHAN_NO_BOOT=1`：只读配置里的 port/local_port，缺省 8321/8323；
    //   3. 自举：建目录、发令牌、选空闲端口、写回 config.json（独立运行的默认路径）。
    //
    // 自举必须跑在 `Ctx::load()` 之前：它写的就是 `Ctx` 要读的那份配置。
    let explicit: Option<u16> = std::env::var("MOSHAN_RS_PORT").ok().and_then(|v| v.parse().ok());
    let booted = if explicit.is_none() && !env_flag("MOSHAN_NO_BOOT") {
        Some(boot::prepare())
    } else {
        None
    };

    let mut loaded = paths::Ctx::load();

    // 绑定端口前先确认媒体根能不能读：macOS 没授权时 `open()` 是同步卡死的，
    // 让第一个请求去撞它等于挂死整页。等最多 3 秒换一个确定结论。详见 paths.rs 看门狗。
    paths::set_library_root(&loaded.library_dir);
    if explicit.is_none() {
        paths::probe_early(&loaded.library_dir);
    }

    // ---- 对拍/测试路径：只绑 127.0.0.1 明文，不 TLS、不额外监听。
    //      整条 GET 对拍链都靠这条路径，改了全线红。
    if let Some(port) = explicit {
        let me = format!("127.0.0.1:{port}");
        if let Some(up) = loaded.upstream.clone() {
            if up.contains(&me) {
                eprintln!("[moshan-rs] 上游指向自身（{up}），已禁用转发");
                loaded.upstream = None;
            }
        }
        let ctx = Arc::new(loaded);
        let app = build_app(ctx.clone());
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[moshan-rs] 绑定 {addr} 失败：{e}");
                std::process::exit(1);
            }
        };
        eprintln!(
            "[moshan-rs] 监听 http://{addr}  上游={}  状态目录={}",
            ctx.upstream.as_deref().unwrap_or("（无，未迁路由将 404）"),
            ctx.state_dir.display()
        );
        start_background(ctx.clone());
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .expect("server");
        return;
    }

    // ---- 正常启动 ---------------------------------------------------------
    let port = num_or(&loaded.cfg, "port", booted.map(|b| b.port).unwrap_or(8321));
    let local_port = num_or(
        &loaded.cfg,
        "local_port",
        booted.map(|b| b.local_port).unwrap_or(8323),
    );
    let plain_port = num_or(&loaded.cfg, "plain_port", 8322);
    let tls_on = loaded.cfg.get("tls").map(truthy).unwrap_or(true);
    let tls_dir = loaded
        .cfg
        .get("tls_dir")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(storage::expanduser(s)))
        .unwrap_or_else(|| paths::state_dir().join("certs"));
    let mut host = loaded
        .cfg
        .get("host")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if host.is_empty() {
        host = "0.0.0.0".to_string();
    }

    // 「上游=自己」会让未迁路由转发给自己，一路递归把线程吃光。
    // 现在有两个本机端口（主端口 + 明文口），两个都要判。
    if let Some(up) = loaded.upstream.clone() {
        if points_to_self(&up, &[port, local_port]) {
            eprintln!("[moshan-rs] 上游指向自身（{up}），已禁用转发");
            loaded.upstream = None;
        }
    }

    // ---- 证书：拿不到就退化明文（Python 侧这里是硬保险，不能把启动带崩）
    let mut tls_cfg: Option<Arc<rustls::ServerConfig>> = None;
    if tls_on {
        match tls::ensure(&tls_dir) {
            Some((crt, key)) => match server_config(&crt, &key) {
                Some(c) => {
                    let trusted = tls::describe(&tls_dir)
                        .get("trusted_ca")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    eprintln!(
                        "证书 {}{}",
                        crt.display(),
                        if trusted {
                            "（Tailscale 公信证书）"
                        } else {
                            "（自签，手机首次访问需点“继续”）"
                        }
                    );
                    tls_cfg = Some(Arc::new(c));
                }
                None => eprintln!(
                    "[警告] 证书读不出来（{}），退化为明文 HTTP —— 令牌会以明文经过网络。",
                    crt.display()
                ),
            },
            None => eprintln!(
                "[警告] 证书生成不了（rcgen），退化为明文 HTTP —— 令牌会以明文经过局域网。"
            ),
        }
    } else {
        eprintln!("[警告] config.json 里 tls=false：主端口走明文 HTTP。");
    }

    let scheme = if tls_cfg.is_some() { "https" } else { "http" };
    // 只有「想开 TLS 但没开成」才收回 loopback：那是怕令牌明文广播出去。
    // 显式 `tls=false` 是用户的选择，Python 侧照样按 host 监听 —— 这里跟着放行，
    // 否则局域网明文部署（自己家网段）会被无声地断掉。
    if tls_on && tls_cfg.is_none() && matches!(host.as_str(), "0.0.0.0" | "::" | "") {
        host = "127.0.0.1".to_string();
        eprintln!(
            "[警告] 退化为明文 HTTP：主端口只监听 127.0.0.1，局域网/手机会连不上（这是有意的）。\
             修好证书或显式设置 host 后再放开。"
        );
    }

    let token = loaded.token.clone();
    let ctx = Arc::new(loaded);
    let app = build_app(ctx.clone());

    let ts_ip = tls::tailscale_ip();
    let ts_name = tls::tailscale_name();

    // ---- 绑定：任一成功就算启动成功；失败的只警告（Python 也只是警告）----
    enum Role {
        Main,
        Local,
        Network,
    }
    struct Bound {
        addr: SocketAddr,
        listener: TcpListener,
        tls: Option<Arc<rustls::ServerConfig>>,
        role: Role,
    }

    let mut bounds: Vec<Bound> = Vec::new();

    let main_addr = format!("{host}:{port}");
    match TcpListener::bind(&main_addr).await {
        Ok(l) => {
            let addr = l
                .local_addr()
                .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], port)));
            bounds.push(Bound {
                addr,
                listener: l,
                tls: tls_cfg.clone(),
                role: Role::Main,
            });
        }
        Err(e) => eprintln!("[警告] 主端口 {main_addr} 绑定失败（{e}）"),
    }

    if local_port != 0 && local_port != port {
        let a = format!("127.0.0.1:{local_port}");
        match TcpListener::bind(&a).await {
            Ok(l) => bounds.push(Bound {
                addr: l.local_addr().unwrap(),
                listener: l,
                tls: None,
                role: Role::Local,
            }),
            Err(_) => eprintln!("[警告] 本机明文端口 {a} 绑定失败（可能被占用）"),
        }
    }

    if tls_cfg.is_some() && plain_port != 0 {
        if let Some(ip) = ts_ip.as_deref() {
            let a = format!("{ip}:{plain_port}");
            if a != main_addr {
                match TcpListener::bind(&a).await {
                    Ok(l) => bounds.push(Bound {
                        addr: l.local_addr().unwrap(),
                        listener: l,
                        tls: None,
                        role: Role::Network,
                    }),
                    Err(_) => eprintln!("[警告] 明文端口 {a} 绑定失败（可能被占用）"),
                }
            }
        }
    }

    if bounds.is_empty() {
        eprintln!("[moshan-rs] 所有监听都没绑上，退出");
        std::process::exit(1);
    }

    // ---- 启动日志：照 Python 的形状打 ------------------------------------
    for b in &bounds {
        if !matches!(b.role, Role::Local | Role::Network) {
            continue;
        }
        let tip = if matches!(b.role, Role::Local) {
            "本机用这个，无证书警告"
        } else {
            "手机用这个，无证书警告"
        };
        eprintln!("口袋服务 http://{}/?k={token}   （{tip}）", b.addr);
    }
    eprintln!("口袋服务 {scheme}://127.0.0.1:{port}");
    if let Some(name) = ts_name.as_deref() {
        eprintln!("         {scheme}://{name}:{port}/?k={token}");
    }
    let lan_ip = tls::lan_ip();
    for ip in [ts_ip.as_deref(), lan_ip.as_deref()].into_iter().flatten() {
        eprintln!("         {scheme}://{ip}:{port}/?k={token}");
    }
    eprintln!("访问令牌 {token}");
    eprintln!("收件箱 {}", ctx.inbox_dir().display());
    if ctx.upstream.is_none() {
        eprintln!("[moshan-rs] 未迁路由无上游（将 404）");
    }

    // 实际监听了哪些地址，逐条落一行，出问题好查。
    for b in &bounds {
        let proto = if b.tls.is_some() { "https" } else { "http" };
        eprintln!("[moshan-rs] 监听 {proto}://{}", b.addr);
    }
    boot::announce(scheme, port, local_port, &token);

    // 端口都绑好了 —— 现在才起后台循环，且在线程里跑（见 start_background 的注释）。
    start_background(ctx.clone());

    // ---- 各自跑，ctrl_c 一起收 ------------------------------------------
    let (stop_tx, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();
    for b in bounds {
        let app = app.clone();
        let rx = stop_rx.clone();
        match b.tls {
            Some(cfg) => tasks.push(tokio::spawn(serve_tls(b.listener, cfg, app, rx))),
            None => tasks.push(tokio::spawn(async move {
                let mut rx = rx;
                let _ = axum::serve(b.listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = rx.changed().await;
                    })
                    .await;
            })),
        }
    }

    let _ = tokio::signal::ctrl_c().await;
    let _ = stop_tx.send(true);
    for t in tasks {
        let _ = t.await;
    }
}

/// TLS 监听：`axum::serve` 只吃 TcpListener，TLS 流要自己喂 hyper。
async fn serve_tls(
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    app: Router,
    mut stop: watch::Receiver<bool>,
) {
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    loop {
        let accepted = tokio::select! {
            r = listener.accept() => r,
            _ = stop.changed() => break,
        };
        let tcp = match accepted {
            Ok((s, _peer)) => s,
            Err(e) => {
                eprintln!("[moshan-rs] TLS 接入失败：{e}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let Ok(stream) = acceptor.accept(tcp).await else {
                return;
            };
            let io = hyper_util::rt::TokioIo::new(stream);
            let svc = hyper_util::service::TowerToHyperService::new(app);
            let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                .serve_connection_with_upgrades(io, svc)
                .await;
        });
    }
}

fn server_config(crt: &Path, key: &Path) -> Option<rustls::ServerConfig> {
    let (certs, key) = tls::load_pair(crt, key)?;
    let mut cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .ok()?;
    // 不声明 h2：这个构建里 hyper-util 的 http2 没开（auto 只会说 http/1.1），
    // 声明了反而会在协商后对不上。
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Some(cfg)
}

/// 上游是不是指向自己（只看回环/通配主机 + 端口）。
fn points_to_self(up: &str, ports: &[u16]) -> bool {
    let rest = up.split("://").nth(1).unwrap_or(up);
    let hostport = rest.split('/').next().unwrap_or(rest);
    let Some((h, p)) = hostport.rsplit_once(':') else {
        return false;
    };
    let Ok(p) = p.parse::<u16>() else {
        return false;
    };
    ports.contains(&p) && matches!(h, "127.0.0.1" | "localhost" | "0.0.0.0" | "[::1]" | "::1")
}

fn num_or(cfg: &Value, key: &str, default: u16) -> u16 {
    cfg.get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .filter(|p| *p != 0)
        .unwrap_or(default)
}

/// 与 Python 的 `bool(x)` 同口径。
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn build_app(ctx: Arc<paths::Ctx>) -> Router {
    // 泳道全关时 `mut` 用不上，这是迁移期的正常状态，不必报警。
    #[allow(unused_mut)]
    let mut app = Router::new()
        // ---- W1：契约地基（4 条，依赖零扇出，响应与运行环境无关）
        .route("/api/ping", get(routes::w1::ping))
        .route("/api/versions", get(routes::w1::versions))
        .route("/api/about", get(routes::w1::about))
        .route("/api/appearance", get(routes::w1::appearance))
        // ---- W2：文件层只读（5 条）。路径语义（链接/越界/面包屑）全在 storage.rs
        .route("/api/files", get(routes::w2::files))
        .route("/api/browse", get(routes::w2::browse))
        .route("/api/usage", get(routes::w2::usage))
        .route("/api/trash", get(routes::w2::trash))
        .route("/api/shares", get(routes::w2::shares))
        // ---- W3：文件层写族（8 条）。判定链顺序与 Python 完全一致
        .route("/api/mkdir", post(routes::w3::mkdir))
        .route("/api/rename", post(routes::w3::rename))
        .route("/api/move", post(routes::w3::move_))
        .route("/api/delete", post(routes::w3::delete))
        .route("/api/trash/restore", post(routes::w3::trash_restore))
        .route("/api/trash/purge", post(routes::w3::trash_purge))
        .route("/api/share", post(routes::w3::share_create))
        .route("/api/shares/{sid}", axum::routing::delete(routes::w3::share_revoke))
        .route("/api/upload", post(routes::w3::upload))
        // ---- W4：记账账本（6 条；上传那两条等 multipart 基建）
        .route("/api/ledger", get(routes::w4::all))
        .route("/api/ledger/item", get(routes::w4::one))
        .route("/api/ledger/img", get(routes::w4::img))
        .route("/api/ledger/update", post(routes::w4::update))
        .route("/api/ledger/remove", post(routes::w4::remove));

    // ---- 泳道：分批施工时各开一个特性，单独编译单独对拍。
    //      全部迁完后把这些 cfg 拆掉、直接 merge 即可。
    #[cfg(feature = "lane_u")]
    {
        app = app.merge(routes::u::router());
    }
    #[cfg(feature = "lane_w4b")]
    {
        app = app.merge(routes::w4b::router());
    }
    #[cfg(feature = "lane_w4c")]
    {
        app = app.merge(routes::w4c::router());
    }
    #[cfg(feature = "lane_w5a")]
    {
        app = app.merge(routes::w5a::router());
    }
    #[cfg(feature = "lane_w5b")]
    {
        app = app.merge(routes::w5b::router());
    }
    #[cfg(feature = "lane_w6a")]
    {
        app = app.merge(routes::w6a::router());
    }
    #[cfg(feature = "lane_w6b")]
    {
        app = app.merge(routes::w6b::router());
    }
    #[cfg(feature = "lane_w5d")]
    {
        app = app.merge(routes::w5d::router());
    }
    #[cfg(feature = "lane_w5e")]
    {
        app = app.merge(routes::w5e::router());
    }
    #[cfg(feature = "lane_w7a")]
    {
        app = app.merge(routes::w7a::router());
    }
    #[cfg(feature = "lane_w7b")]
    {
        app = app.merge(routes::w7b::router());
    }

    // ---- 角色开关：有两套接口在 Python 里本来就不是主服务挂的，
    //      默认跟着 Python 保持「不挂」，免得迁完之后 404 变 200。
    let role = std::env::var("MOSHAN_ROLE").unwrap_or_default();
    let nest = role == "nest" || env_flag("MOSHAN_FLEET");
    let gray = env_flag("MOSHAN_GRAY");
    #[cfg(feature = "lane_w6a")]
    if nest {
        app = app.merge(routes::w6a::fleet_router());
    }
    #[cfg(feature = "lane_w7a")]
    if gray {
        app = app.merge(routes::w7a::gray_router());
    }
    #[cfg(not(feature = "lane_w6a"))]
    let _ = nest;
    #[cfg(not(feature = "lane_w7a"))]
    let _ = gray;

    // ---- 其余未迁路由：镜像给 Python，行为完全不变
    let app = app.fallback(proxy::forward).with_state(ctx.clone());

    app
}

/// 起后台调度器：心跳/采样/巡检/OCR/简报推送。
///
/// **必须在端口绑成功之后调用**，而且要在独立线程里 —— 这两个约束都是踩出来的：
/// `heartbeat::start` 第一件事就是往 `app_dir/pocket.jsonl` 追加一行，那是同步文件 IO。
/// 从 GUI（launchd）启动时，`app_dir` 往往落在 ~/Documents 或外挂卷上，macOS 的 TCC
/// 会弹「想访问…」的授权框，而 `open()` 会一直等用户点。以前这段写在 `build_app` 里，
/// 结果就是：弹窗没人点 → 主线程卡在 open → 端口一个都没绑 → 窗口白屏，
/// 用户还以为应用坏了。放到绑定之后的独立线程里，端口先能服务，授权框慢慢点。
fn start_background(ctx: Arc<paths::Ctx>) {
    // 媒体根看门狗：没被系统隐私授权时 `open()` 会同步卡死，必须先在后台探出来，
    // 否则请求会一条条堆在 `open()` 里永远不返回。详见 `paths::spawn_library_probe`。
    paths::spawn_library_probe(ctx.library_dir.clone());

    // 先把「运行数据」从媒体根搬到 runtime_dir 一次（老用户升级路径）。
    // 与调度器同在后台线程：源目录可能在 ~/Documents，读它会等 TCC 授权。
    let mig = ctx.clone();
    std::thread::Builder::new()
        .name("moshan-migrate".into())
        .spawn(move || {
            let r = migrate::runtime_once(&mig);
            if r.get("skipped").and_then(|v| v.as_bool()).unwrap_or(false) {
                return;
            }
            let moved = r.get("moved").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            let failed = r.get("failed").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if moved > 0 || !failed.is_empty() {
                eprintln!(
                    "[moshan-rs] 运行数据搬迁：{} 个目录就位{}",
                    moved,
                    if failed.is_empty() { String::new() } else { format!("，{} 个跳过（详见 {}/.runtime-migrated.json）", failed.len(), mig.runtime_dir.display()) }
                );
            }
        })
        .ok();

    #[cfg(feature = "lane_w5d")]
    std::thread::Builder::new()
        .name("moshan-sched".into())
        .spawn(move || routes::w5d::spawn(ctx))
        .ok();
    #[cfg(not(feature = "lane_w5d"))]
    let _ = ctx;
}

/// `MOSHAN_*` 这类开关只认「非空且不是 0/false」——与 Python 侧读环境变量的口径一致。
fn env_flag(key: &str) -> bool {
    match std::env::var(key) {
        Ok(v) => {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        }
        Err(_) => false,
    }
}
