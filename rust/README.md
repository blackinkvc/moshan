# 墨山服务端（Rust）

这是中台的服务端本体。**220 条 HTTP 路由已全部由 Rust 直接应答**，不再转发给 Python。

```
moshan/                 原 Python 实现（现在是"对照卷"，只在做逐字节对拍时启动）
rust/moshan-server/     服务端本体（axum 0.8）
rust/moshan-server/src/
  main.rs               装配路由 + 自举 + 角色开关
  boot.rs               首启自举：建目录、发令牌、选端口、写 config.json
  paths.rs              目录与配置口径（对齐 paths.py）
  sysopen.rs            打开路径/网址/在文件管理器里定位（mac/Linux/Windows 三份命令）
  imaging.rs            缩略图与 heic→jpg（mac 用 sips，其它平台探测外挂工具）
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

## 两个数据根（媒体 / 运行）

`paths.rs` 里有两个根，不是一个。分家的理由是「大而少」和「小而勤」不是一种磁盘：

| 根 | 缺省 | 放什么 | 覆盖方式 |
|---|---|---|---|
| 媒体根 `library_dir` | `~/Documents/Moshan` | `01-研究` `02-媒体` `04-存图` `07-收件箱` `08-阅读` `09-个人工作台` `11-关注追踪` … 体积大、写得少，适合外挂盘 | `MOSHAN_HOME`（→ `$MOSHAN_HOME/library`）或 `config.json` 的 `library_dir` |
| 运行根 `runtime_dir` | `<状态目录>/data`，mac 上是 `~/Library/Application Support/Moshan/data` | `12-记账` `10-系统记录` `13-算价` `_本机工作` `_工作台` `_巡检` `05-采集` `16-虫巢思维` `16-多机` `14-传输` `19-AI工作计划` … 文件小、高频追加，放系统盘 | 同上，或 `config.json` 的 `runtime_dir` |

归属只认一处：`paths::RUNTIME_SUBDIRS` + `is_runtime_sub(name)`。新写代码时，
**要写盘/要追加的路径一律走 `rt_join(ctx, "12-记账")` 或 `paths::workbench_dir(ctx)`**，
不要再用 `ctx.library_dir.join(...)` —— 那会把账目又写回装满图片的盘上。

首次启动会把老用户媒体根里的这些目录搬到运行根（`migrate::runtime_once`）：
先复制、校验文件数与字节数、一致才删源；任何一步不对就丢副本保源。搬完写
`.runtime-migrated.json`，之后不再搬。目标已存在且非空则跳过，不覆盖。

macOS 上媒体根常落在「文稿」或外挂卷上，那要「完全磁盘访问权限」。没授权时
`open()` 会**同步卡死**而不是报错，所以有 `paths::spawn_library_probe` 看门狗兜底，
细节见 `docs/PLATFORMS.md` 第八节。

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

最近一次结果：**220/220 由 Rust 应答**；102 条 GET 里一致 99 / 抖动 2 / 实质差异 1
（那一条是 `/api/report/list` 的夹具时序，见「已知边界」）；
w2 15 步、w3 27 步、w4 16 步、w4c 46 步全为 0；w5b 52 步里 2 步读的是**实时数据**
（`/api/worklog` 的「当轮在做的事」、`/api/llm` 的本机模型），两边取样时刻不同就会不同，
其余 50 步为 0（其中「空请求体」那步只比状态码：错误文案来自 pydantic，见「已知边界」）。
简报推送已接通：`<日期>.sent.json` 掩掉 `at` 后与 Python 逐字节相等，假网关（18790）上的
四帧 `connect / conversations.list / conversations.send / send` 参数逐字段对齐。

## 已知边界（不是"没迁"，是"环境里没有"）

- **畸形/缺失请求体的错误体**：Python 是 FastAPI，`body: dict` 校验失败回 422 + pydantic 的
  `detail` 数组；Rust 已对齐**状态码 422 与 `detail` 结构**，但不模仿 pydantic 额外带的
  `url`（指向 `errors.pydantic.dev/2.14/…`）—— 那是库内部文案，写死等于把上游版本号焊进响应。
  `parity_w5b.py` 里这一步只比状态码。
- **`/api/llm` 的模型↔进程归属**：Python 用 `psutil.net_connections()` 建「端口 → pid」表，
  系统不给看时（本机沙箱、没装 psutil）整张表为空，探到的端口只能退回「OpenAI 兼容」；
  Rust 走 `lsof` / `/proc/net/tcp` / `netstat -ano`，能归位到真正的族名。同一个端口上
  两边可能给出不同 `family` —— 这是**对照侧降级**，不是迁移差异。
- **`/api/report/list` 的 `state.samples`**：对拍夹具让两边的调度器共用同一份 `快照.jsonl`，
  后 tick 的一边多看见一条样本就会写日报（`rollup_day` 要 `<2 → None`）。三次实验
  （独立根 / 共享根先 py / 共享根先 rs）都证明"谁后 tick 谁写"，判定语义本身一致；
  `--paths /api/report/list --seed` 单独跑为一致。
- **灰测 / 虫巢**：Python 主服务本就没挂这两套（`fleet.register()` 被注释、`gray` 无人 import），
  迁完后默认同样不挂，需要时用角色开关打开。
- **OCR 收尾**（`post_process` / `refetch_missing`）：只跑 `ocr.py --sweep`，
  `classified/summarized/img_fixed` 恒 0，其余字段与 Python 一致。
- **缩略图**：`/api/thumb` / `/api/ledger/img` / `/api/browse?inline=1` / 账本换图四个入口
  现在都走 `storage::make_thumb` —— mac 用 `sips`（与 Python 逐字节一致），其它平台交给
  `imaging`（`magick` → `convert` → `ffmpeg`），三个都没有就退回原图。Python 在非 mac 上
  本来就没有缩略图（`sips` 写死），所以这里只多不少。
- **HEIC → JPEG**：只有 mac 能转（`sips`）。Linux/Windows 上这类图会被跳过，图集其余图片照常入库；
  Python 的 `linkbox` 同样写死 `/usr/bin/sips`。
- **打包（非 macOS）**：`/api/zip` 在 mac 上仍是自写 zlib FFI（与 `zipfile` 逐字节一致）；
  Linux/Windows 走 `zip` crate，条目名/顺序/内容一致，但容器头（version made by 等）可能与
  `zipfile` 有字节差。需要「逐字节」判据时只在 mac 上比。
- **自启动 / 服务安装**：服务端不写 launchd plist、systemd user unit 或计划任务，也不注册登录项。
  自启由安装包与桌面壳负责（mac `.app`、Windows NSIS、Linux `.deb`/AppImage 的桌面项）。
- **在文件管理器里定位到某个文件**：`sysopen::reveal_path` 已实现（mac `open -R` /
  Windows `explorer /select,` / Linux 打开父目录），但仓库里还没有这样的路由，暂无调用点。
- **应用启动器（`/api/apps` 的 `.app` / `open -a`）**：仍是 macOS 语义，非 mac 上这部分不可用；
  它不属于「打开路径」这一类，没有做跨平台映射。
- **平台**：macOS 上实测过（含 `open` 打点、缩略图字节、zip 逐字节对拍、微信推送、局域网
  HTTPS、原地更新）；Windows/Linux 分支按源码实现，本地用 `--features sim_linux|sim_windows`
  + `scripts/rust-portability.py` 双向卡住，再由 CI 的 matrix 原生 `cargo build` +
  服务端冒烟（`/api/ping` 200）覆盖到「能跑」，未在真机点过 UI。

### 三平台怎么在 mac 上验

```bash
cd rust
CARGO_TARGET_DIR=/tmp/rs-lin cargo check --features sim_linux   --message-format short
CARGO_TARGET_DIR=/tmp/rs-win cargo check --features sim_windows --message-format short
cd .. && python3 scripts/rust-portability.py     # 未隔离的平台专有调用应当是 0
```

`sim_*` 只是把 cfg 分支翻到对面（写成 `all(unix, not(feature = "sim_windows"))` 而不是裸
`unix`），在 mac 上做类型检查；`rust-portability.py` 反过来查每处 `std::os::unix` / `libc::`
是不是真被「对面平台会编掉」的 cfg 挡住 —— 编译器看不出裸 `#[cfg(unix)]` 的问题，它能。

## 迁移期留下的东西，什么时候可以删

- `moshan/`：**留着**。它是逐字节对拍的对照组，删了就没法验证"改得对不对"。
- `proxy.rs` + `MOSHAN_PY_UPSTREAM`：留作调试（把一条路径临时打回 Python 对照）。
- 泳道特性（`lane_*`）：留着，定位问题时能只编一条泳道。
- `scripts/stage-sidecar.sh --python`：旧打包路径，只在需要对照旧实现时用。
