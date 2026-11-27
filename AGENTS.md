# AGENTS.md — 给所有 Agent 的硬约束

这个仓库（墨山中台 Moshan）由**多个 WorkBuddy 窗口并行维护**，它们共用同一份工作目录
（`/Volumes/AI仓库/Moshan`，ExFAT 移动硬盘上的 git 仓库）。改文件是安全的，**危险的是 git 操作**：
被拒的 push 一旦用强推/重置「解决」，另一个窗口的提交就没了，而且不会报错。

读到这里就不要跳过了。下面每一条都对应一次真实事故或一次险些发生的事故。

## 零、架构定论：服务端是 Rust，Python 只是对照卷（2026-11-28）

**中台本体 = `rust/moshan-server/`（axum 0.8）。220 条 HTTP 路由全部由 Rust 直接应答，
一条都没有转发给 Python。** 这个结论不是估计，是 `scripts/rust-coverage.py` 把上游置空后
逐条打出来的（判定依据是响应来源，不是有没有响应：出现 `x-moshan-proxy` 或 502 才算没迁）。

```
rust/moshan-server/     唯一的服务端本体 —— 修 bug、加接口、改行为，只改这里
moshan/*.py             对照卷。只在 scripts/rust-parity.py 做逐字节对拍时启动，
                        不参与运行时；不是"另一份实现"，是"标准答案的存档"
src-tauri/              桌面壳（Tauri + Rust），仍在用
```

所以：

- **不要再往 `moshan/*.py` 里加功能或修 bug。** 改了不会生效（线上一秒都不会跑到它），
  还会让对拍基线漂移，把后面所有人的回归测试搞花。
- 如果一个问题"必须改 Python 才能修"，那说明找错地方了 —— 去找对应的 `routes/wX*/`。
- 顺手看到的过期注释（例如 `routes/w5a/mod.rs` 里仍写着 workstat 未迁，其实已在 `w5e` 迁完）
  可以直接更正，别让下一个人被注释骗去改 Python。
- 迁移进度与已判定差异记在 `rust/README.md`；动过路由表就同步更新它。

自证命令（改完跑一遍，两条都得过）：

```bash
cd rust && CARGO_PROFILE_RELEASE_STRIP=none cargo build --release --bin moshan-server
cd .. && python3 scripts/rust-coverage.py --rs-bin rust/target/release/moshan-server
#   期望：共 220 条：Rust 自应答 220，未迁 0
```

`CARGO_PROFILE_RELEASE_STRIP=none` 不能省：默认 strip 会写坏 proc-macro dylib，
报出来的却是 `can't find crate for xxx_macros`，看着像依赖缺失。

## 一、绝对禁止

```
git push --force / -f / --force-with-lease
git reset --hard
git checkout -- .
git restore .
git clean -fd
git add -A / git add .
git rebase <别人的分支>
git tag -d + git push --delete（删远端 tag/分支）
git commit --amend（已推过的提交）
```

`.git/hooks/pre-push` 会拦下非快进推送和删除操作，**不要绕过它**：不要 `--no-verify`，
不要删钩子，不要 `MOSHAN_FORCE=1`（那是用户本人排障用的）。

`git add -A` 单独列出来，是因为这是最容易被忽略的一条：另一个窗口未提交的改动就躺在
同一个工作区里，`add -A` 会把它的半成品一起提交进你的提交，出问题时没人分得清是谁的。
**只 `git add <具体路径>`。**

## 二、main 只能走 PR

`main` 上了 GitHub Ruleset：

- 必须走 Pull Request 才能合并（`required_approving_review_count = 0`，不需要他人批准，
  但要有一个 PR 作为变更记录）
- 禁止强推、禁止删除分支
- **没有 bypass 名单**，仓库管理员也绕不过去

所以直接 `git push origin main` 一定失败，这不是网络问题。你的活推到自己的分支：

```bash
git push origin HEAD:refs/heads/agent/<你的名字>
```

然后告诉用户「已推到 agent/xxx，等你合并」。**不要自己建 PR、不要自己合并。**

## 三、被拒绝时的规定动作

push 被拒（`! [rejected] ... non-fast-forward`）**只有一个正确反应**：

```bash
git fetch origin
git rebase origin/main
git push origin HEAD:refs/heads/agent/<你的名字>
```

不是重试、不是加 `-f`、不是 `reset --hard`。

如果 rebase 出现冲突：**停下来报告**，不要「保留我的版本」。冲突里丢掉的可能是别人
已经验证过的修复，而你的改动还没被验证。

## 四、并行任务的正确开局

首选 WorkBuddy 建任务时的 **Worktree 开关**（输入框下方，默认关）：它会给你一份完全
独立的目录，文件级就不会互踩。

命令行等价物：

```bash
bash ~/Documents/cuntu/new-worktree.sh <任务名>     # 建 /Volumes/AI仓库/worktrees/<任务名>
```

不要自己在主工作目录里 `git checkout -b` —— 切分支会把别人未提交的改动一起带走。

## 五、动代码前必须知道的

- 自检：`MOSHAN_PY=/tmp/.mosh110/bin/python bash scripts/check.sh`（8 节全绿才算过）。
  直接 `bash scripts/check.sh` 会因为本机 python 的 fastapi 版本不对而假红。
- 改完**必须回灌**到运行目录：`bash ~/Documents/cuntu/backfill.sh`（`--dry-run` 先看）。
  方向是**仓库 → 运行目录**，不是反过来。
- `pocket/*.py`、`cuntu/*.py` 改后要重启：
  `launchctl kickstart -k gui/$(id -u)/com.local-ai.pocket`；`fleet.py` 那套是 `com.local-ai.nest`。
- 仓库在 ExFAT 上：不要手跑 `git gc`，不要指望文件权限位（`chmod +x` 不落地）。
- 版本号两处一起改：`moshan/version.py` 与 `VERSION`，再补 `CHANGELOG.md`。
- **打 tag = 对外发版**（Actions 构建 mac/win 安装包并直接发 Release）。没用户点头就别打。

## 六、给非 WorkBuddy 的工具

Trae / Codex / DeepSeek harness 等如果只是**被使用**而不是维护这个仓库：可以改文件，
但不要 commit、不要 push、不要 tag，把结论交给用户。

如果你干了活想进中台的工作台，收尾时报一笔（不占仓库，不动 git）：

```bash
curl -s -X POST http://127.0.0.1:8323/api/agent/note -H 'content-type: application/json' \
  -d '{"agent":"<你的名字>","task":"<这轮干了什么>","summary":"<结论>","project":"moshan","in":0,"out":0,"calls":0}'
```

首次使用时中台会给你投一份完整说明书（`~/.<你的工具>/skills/pocket-bridge/SKILL.md`）。
