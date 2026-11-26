# Rust 服务（迁移期）

Python 服务正在按波次改写为 Rust。这个目录是新的服务端本体，
**未迁的路由原样转发给 Python**，所以任何一波做完，产品都是完整可用的。

## 为什么是「影子转发」而不是一次全换

45,319 行 Python、220 条路由，其中 65 个模块互相引用。一次性替换的中间态是
「产品不可用」，且没有任何办法判断 215 条路由是否真的被破坏。影子转发把大爆炸
拆成一波波可回滚的小步：每波只把若干路由从"转发"改成"直接应答"，其余不动。

## 跑起来

```bash
# 1) 编译
cd rust && cargo build --release

# 2) 起服务（默认 8400，刻意避开 Python 的 8321/8322/8323，便于并排跑）
MOSHAN_PY_UPSTREAM=http://127.0.0.1:8323 ./target/release/moshan-server

# 3) 已迁路由由 Rust 应答，其余转给上游
curl -s localhost:8400/api/ping
```

环境变量：

| 变量 | 作用 |
|---|---|
| `MOSHAN_RS_PORT` | 监听端口，默认 8400 |
| `MOSHAN_PY_UPSTREAM` | 上游地址；缺省按 config.json 的 `local_port` 推 |
| `MOSHAN_HOME` | 数据根覆盖，与 `paths.py` 同一套规则 |
| `MOSHAN_APP_DIR` | 仅用于对拍：让 `/api/about` 的 `app_dir` 与 Python 一致 |
| `MOSHAN_FROZEN=1` | 声明"我是打包形态"，`/api/about` 的 `frozen` 随之为 true |

## 判定标准：逐字节对拍

```bash
python3 scripts/rust-parity.py             # 默认清单（已迁 + 代理样本）
python3 scripts/rust-parity.py --all-get   # 全部 102 条 GET
```

判据是**响应字节**，不是"看起来差不多"：`dash.html` 有 1.1 MB 内联脚本，
字段形状或顺序一变就可能成片白屏。

脚本对"抖动"有自证机制：某条两边不同时，它会再打一次 Python。如果对照侧自己两次
也不一样（实时采样、时间戳、缓存），标为「抖动」而不是「差异」——否则
`/api/sys`、`/api/disk/volumes` 这类会永远红着，把真差异淹掉。

当前结果：**102 条 GET：一致 96、抖动 6、实质差异 0**。

## 波次状态

| 波次 | 内容 | 状态 |
|---|---|---|
| W1 | 契约地基（ping / versions / about / appearance） | ✅ 已迁，字节一致 |
| W2 | 文件层只读（files/browse/usage/disk/trash/shares） | 待做 |
| W3 | 文件层写族（mkdir/rename/move/delete/upload/share…） | 待做 |
| W4 | 账本与计划（ledger/plan/notes/brief/report/inbox…） | 待做 |
| W5 | 聚合只读 + 运行态（overview/dash/pulse/workstat/heartbeat…） | 待做 |
| W6 | 多机与 Agent（fleet 23、agent 18…） | 待做 |
| W7 | 外部副作用（update/pack/ext/gray/ocr/transfer…） | 待做 |
| 不迁 | 10 个 HTML 页、8 个 `/welcome`、9 个二进制产物 | — |

`/api/heartbeat` 原本按"依赖零扇出"被划进 W1，落地时被对拍推翻：它的契约里包含
进程内运行期状态（`STATE.last_at` 在 Python 里启动即被填），Rust 没有自己的调度器
就必然给出不同字节。现归 W5，等 Rust 接管心跳调度再把状态一并迁过来。

## 目录

```
rust/moshan-server/src/
  main.rs     装配；已迁路由 + fallback 转发
  paths.rs    目录与配置解析（逐条对齐 moshan/paths.py）
  auth.rs     三通道令牌（header → ?k= → cookie），常量时间比较
  proxy.rs    透明转发（剥逐跳首部，流式透传）
  routes/w1.rs W1 的四个端点
```
