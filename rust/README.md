# 墨山服务端（Rust）

这是中台的服务端本体。**220 条 HTTP 路由已全部由 Rust 直接应答**，不再转发给 Python。

```
moshan/                 原 Python 实现（现在是"对照卷"，只在做逐字节对拍时启动）
rust/moshan-server/     服务端本体（axum 0.8）
rust/moshan-server/src/
  main.rs               装配路由 + 自举 + 角色开关
  boot.rs               首启自举：建目录、发令牌、选端口、写 config.json
  paths.rs              目录与配置口径（对齐 paths.py）
  auth.rs               三令牌通道（header → ?k= → cookie）
  storage.rs            文件层（路径守卫、回收站、分享、缩略图）
  ledger.rs             记账账本
  jsonio.rs             Python 风格 JSON 读写（indent=N，逐字节对齐）
  live/                 进程内运行态（调度器写给 HTTP 处理器读）
  hostwork/             会话/工作量采集层（多路由共用同一份数）
  routes/               路由：w1..w4 是早期波次，w4b…w7b/u 是按泳道并行施工的产物
```

## 怎么跑

```bash
# 独立运行（自举：建目录、发令牌、挑 8323–8399 的空闲端口、写 config.json）
cargo run --release --manifest-path rust/Cargo.toml

# 指定端口（测试/多实例；此时不改配置）
MOSHAN_RS_PORT=8400 cargo run --release --manifest-path rust/Cargo.toml

# 关掉后台调度（心跳/采样/巡检/OCR/趋势），只留路由
MOSHAN_NO_SCHED=1 ./moshan-server

# 只编一条泳道（定位问题时用；默认是全量）
cargo build --release --no-default-features --features lane_w5a
```

启动后日志会打印实际地址与令牌长度；`curl -s 127.0.0.1:<port>/api/ping` 应回 200。

## 环境变量

| 变量 | 作用 |
|---|---|
| `MOSHAN_RS_PORT` | 指定监听端口，**不写配置文件**（对拍/多实例用） |
| `MOSHAN_NO_BOOT` | 关掉自举，只读配置里的 `local_port`，缺省 8400 |
| `MOSHAN_NO_SCHED` | 关掉全部后台循环（手动触发路由仍可用） |
| `MOSHAN_PY_UPSTREAM` | 有值时把**未匹配**的路径转发给上游（迁移期残影，正常部署留空） |
| `MOSHAN_ROLE=nest` / `MOSHAN_FLEET=1` | 挂上虫巢（`/api/fleet/*`）—— Python 主服务本来就没挂 |
| `MOSHAN_GRAY=1` | 挂上灰测（`/api/gray/*`）—— 同上 |
| `MOSHAN_OPEN=1` | 启动后打开浏览器（桌面壳自己开窗口，所以默认不弹） |

## 怎么证明它跟 Python 一样

判据是**响应字节**，不是"看着差不多"。

```bash
# 1) 全量 GET：102 条读接口，一次比完
python3 scripts/rust-parity.py --all-get --seed --struct \
  --rs-bin /tmp/moshan-rs-target/release/moshan-server

# 2) 写族：两边各起一份干净数据根，依次跑同一串写请求，再逐字节比响应与落盘文件
python3 scripts/rust-parity.py --suite w3   # 文件层 27 步
python3 scripts/rust-parity.py --suite w4   # 账本 16 步
python3 scripts/parity_w4c.py               # 文章/去重/收件箱/报告 46 步
python3 scripts/parity_w5b.py               # 账单/流水 52 步

# 3) 覆盖率自证：把上游刻意置空，任何"其实还在转发"的路由都会露馅
python3 scripts/rust-coverage.py
```

`--struct` 的意思是「只掩活值、不掩结构」：时间戳、耗时、计时器这类每次采样都会变的
数字换成占位符，字段是否存在、顺序、类型、数组长度仍然照比。判定"抖动"还有一层自证：
两边不同时再打一次 Python，如果对照侧自己两次也不同，就不算迁移引入的差异。

最近一次结果：**220/220 由 Rust 应答**；102 条 GET 里一致 97 / 抖动 3 / 实质差异 0；
w2 15 步、w3 27 步、w4 16 步、w4c 46 步、w5b 52 步全为 0；w4b 68 步里 2 处差异已定位到
「Python 的进程内 120 秒 MEM 缓存 + 简报定时推送的 sent 记账」，属于推送通道未迁的连带项。

## 已知边界（不是"没迁"，是"环境里没有"）

- **微信推送通道（websocket）**：`/api/brief/push` 与定时补推只对齐到"网关不可用"这一支，
  连上网关真发消息没有实现；`<日期>.sent.json` 因此不会生成。
- **局域网 HTTPS（0.0.0.0:8321 + 自签 CA）**：Rust 侧只绑 127.0.0.1。手机/别的机器访问
  仍走原来的 Python 服务，或用反向代理终结 TLS。
- **`/api/update/apply`**：刻意不替换自身可执行文件、不拉安装器，只回与 Python 相同的
  流程与文案（真要替换，用原来的 Python 更新器或手动安装）。测试只覆盖失败分支。
- **灰测 / 虫巢**：Python 主服务本就没挂这两套（`fleet.register()` 被注释、`gray` 无人 import），
  迁完后默认同样不挂，需要时用角色开关打开。
- **OCR 收尾**（`post_process` / `refetch_missing`）：只跑 `ocr.py --sweep`，
  `classified/summarized/img_fixed` 恒 0，其余字段与 Python 一致。
- **平台**：只在本机 macOS 上实测过；Windows/Linux 分支按源码与文档实现，未在真机跑过。

## 迁移期留下的东西，什么时候可以删

- `moshan/`：**留着**。它是逐字节对拍的对照组，删了就没法验证"改得对不对"。
- `proxy.rs` + `MOSHAN_PY_UPSTREAM`：留作调试（把一条路径临时打回 Python 对照）。
- 泳道特性（`lane_*`）：留着，定位问题时能只编一条泳道。
- `scripts/stage-sidecar.sh --python`：旧打包路径，只在需要对照旧实现时用。
