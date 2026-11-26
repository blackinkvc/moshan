# Rust 迁移 · 泳道施工说明

本文件给**并行施工的人（或 agent）**看。一次只认一条泳道，边界在这里写死。

## 1. 目标与判据

把 `moshan/*.py` 的 HTTP 接口逐条搬进 `rust/moshan-server`（axum 0.8），
**响应必须与 Python 逐字节一致**（状态码 + 响应体）。判据是字节，不是「看起来差不多」。

迁移期架构是影子转发：

```
客户端 → Rust(8400) ─ 已迁路由：自己应答
                    └ 未迁路由：原样转发给上游 Python（MOSHAN_PY_UPSTREAM）
```

所以任一波做完，产品都完整可用；做完一条就少一条转发。

## 2. 边界（重要）

**只能改自己泳道目录里的文件**：`rust/moshan-server/src/routes/<lane>/`。

**不许改**：`main.rs`、`routes/mod.rs`、`Cargo.toml`、`auth.rs`、`paths.rs`、
`storage.rs`、`jsonio.rs`、`ledger.rs`、`routes/common.rs`、`routes/w1..w4.rs`、
`moshan/*.py`（Python 是对照组，改它等于改考卷）。

需要新的公共能力（时间格式化、采样、HTTP 调用……）时，**写在自己泳道目录里**，
并按 Python 的模块名分文件（例：`routes/w5a/dash.rs` 对应 `moshan/dash.py`）。
这些能力最后由集成者统一收编，重复一份不影响正确性。

**不要提交 git**，由集成者统一提交（多泳道并行时 `git add` 必须按路径精确）。

## 3. 已有的地基（直接调，别重写）

| 模块 | 能做什么 |
|---|---|
| `crate::auth::token_ok(ctx, headers, uri)` / `deny()` | 三通道令牌校验，与 `server.py:auth()` 同口径 |
| `crate::paths::Ctx` | `cfg`（config.json 原始对象）、`token`、`state_dir`、`library_dir`、`inbox_dir()`、`allowed_roots()`、`trash_dir()`、`shares_file()`、`max_upload_mb()`、`trash_days()`、`workbench_dir()` |
| `crate::storage` | `safe_path` `expanduser` `py_resolve` `entry` `crumbs` `list_dir` `usage` `dir_size` `list_trash` `make_thumb` `mkdir` `rename` `move_paths` `delete_to_trash` `restore` `purge` `share_create` `share_revoke` `human` `category` `log` |
| `crate::jsonio` | `dumps(&Value, indent)` —— **Python `json.dumps(ensure_ascii=False, indent=N)` 的逐字节复刻**；`atomic_write`；`read_json` |
| `crate::ledger` | 账本读写（W4 已迁） |
| `crate::routes::common` | `http_err(code,msg)`（`{"detail":...}`）、`ok_err(msg)`（`{"ok":false,"error":...}`）、`peek()`、`body_obj()`、`s/i/b` 取值、`q/query_map/q_int` 查询参数解码 |
| `crate::proxy` | `forward`（fallback 用，泳道里别调） |

写文件一律 `jsonio::dumps(&v, 1)` + `jsonio::atomic_write`：Python 侧是
`json.dumps(..., ensure_ascii=False, indent=1)` 落盘的，缩进差一格就是磁盘分叉。

## 4. 处理函数模板

```rust
use crate::paths::Ctx;
use crate::routes::common::{body_obj, http_err, peek, CtxState};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

pub async fn thing(State(ctx): CtxState, headers: HeaderMap, uri: Uri) -> Response {
    if let Some(r) = peek(&ctx, &headers, &uri) { return r; }   // 先鉴权，不通过 403
    let p = match crate::storage::safe_path(&ctx, common::q(&uri, "path").as_str(), true) {
        Ok(p) => p,
        Err(e) => return http_err(e.code, &e.msg),
    };
    Json(json!({ "ok": true })).into_response()
}
```

要点：

- **顺序就是契约**：先鉴权、再解析参数、再判存在。FastAPI 的 401/403/404/422 谁先来，
  决定了最终回哪个码，顺序错了对拍必红。
- **`def` 与 `async def` 的区别在 Python 那边**：同步 `def` 跑在线程池，异步 `async def`
  跑事件循环。Rust 侧统一 `async fn`，阻塞 IO 直接写（单次都是毫秒级），
  但**不要**在 handler 里 `sleep` 长等或跑几十秒的子进程 —— 那种要么 `spawn_blocking`，
  要么交给后台任务，别把 tokio 的工作线程占死。
- **两种错误形状不能统一**：`HTTPException` → `{"detail": msg}`；业务自回 →
  `{"ok": false, "error": ...}`（HTTP 200）。看 Python 源码那一行返回的是什么就用哪个。
- 路径参数 `{sid}` → `axum::extract::Path<String>`；通配 `{name:path}` → `Path<String>` +
  路由写成 `/s/{sid}/{*name}`（axum 0.8 语法）。
- 中文正则用 `regex` crate，**不要**手写字符扫描：全角标点、不可见空格上极容易分叉。

## 5. 单泳道编译与对拍

```bash
cd /Volumes/AI仓库/Moshan/rust

# 只编译自己这条泳道（编译目标目录分开，互不干扰）
CARGO_TARGET_DIR=/tmp/rs-<lane> cargo build --release \
  --no-default-features --features lane_<lane>

# 对拍：两边各一份干净数据根，同一批 URL 打两遍，看字节
python3 ../scripts/rust-parity.py \
  --paths /api/xxx /api/yyy \
  --rs-bin /tmp/rs-<lane>/release/moshan-server \
  --seed
```

`--seed` 会铺一整套夹具（中文名、符号链接、回收站残条、真 JPEG、账本三件商品……），
但**不改你给的清单**。不带 `--seed` 就只有空目录，字段顺序和排序键都验不到。

Rust 侧起来时 `MOSHAN_PY_UPSTREAM` 由脚本设成 Python 端口：走 fallback 的端点会回
Python 的响应，**看起来「一致」其实没迁**。所以：

- 判断「真迁了」用 `--rs-bin` 指向**只带自己泳道特性**的二进制 —— 未迁端点会 404；
- 或者直接看 `curl -s localhost:<rs端口>/api/xxx` 的响应头里有没有 `x-moshan-proxy`
  （`proxy.rs` 转发时会加标记，自己应答的没有）。

写族（有副作用的）对拍：`python3 scripts/rust-parity.py --suite w4` 是现成的「两边各跑
一遍同一脚本再比字节」范式，泳道可以照抄 `w4_steps()` 的形状，在脚本里加自己的步骤清单
（**不要改 `--suite w4` 已有的步骤**，那是已验收的基线）。

## 6. 交付时报什么

1. 迁完的路由清单（方法 + 路径 + 处理函数名），以及**没迁的**和原因；
2. 对拍结果：每条 URL 的 `py/rs` 状态码 + 结论（一致 / 抖动 / 差异），实质差异必须为 0；
3. 已知分叉：没法本地复现的（要外网、要硬件、要交互），如实写清楚，
   别用「大概一致」糊过去；
4. 你新加的文件清单（都在自己泳道目录里）。

## 7. 常见坑（都踩过）

- **`cfg!(target_os = "darwin")` 恒假**：Rust 里叫 `macos`，`darwin` 是 Python 的 `sys.platform`。
- **路径要 canonicalize**：macOS `/var` → `/private/var`，不解析就会「同一台机器两种写法」。
- **Python 的 `sort` 是稳定排序**，Rust `sort_by` 也是；但按键比较的**类型**要对
  （数字别按字符串比，`None` 的位置按 Python 规则放最前/最后）。
- **浮点数**：Python `json` 输出 `1299.0`，Rust `serde_json` 输出 `1299.0`；但
  `1.0/3` 这类要按 `repr` 口径格式化（`jsonio::dumps` 已处理常见情形）。
- **时间**：一律走 `chrono::Local`，格式串照抄 Python 的 `strftime`。
- **目录遍历顺序**：`read_dir` 与 Python `os.scandir` 同源，**别排序**除非 Python 排了。
- **`/api/ping` 是就绪探针**：任何泳道启动慢都别改它。
