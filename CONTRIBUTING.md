# 参与开发

## 从开发目录同步

日常开发在本机另一个目录里进行，改完用 `bash scripts/sync-from-dev.sh` 搬进来。
它做了三方合并，不会覆盖交付版的跨平台移植补丁。完整说明见
[`docs/SYNC.md`](docs/SYNC.md)。

```bash
bash scripts/sync-from-dev.sh --dry-run   # 预演
bash scripts/sync-from-dev.sh             # 同步 + 语法检查
```

## 目录约定

```
moshan/            业务代码，平铺模块。新增模块直接在 moshan/ 下加 .py，
                 其它模块用 `import 模块名` 引用（不要写成包内相对导入）。
packages/        打包相关，与运行时代码无关
scripts/         同步、自检、升版本、ExFAT 修复等工具
.github/         CI 与发布
```

改动前先读 `moshan/paths.py` —— 所有路径都必须从它取，不要在代码里拼
`~/Documents/...` 或 `/Users/xxx`。CI 会扫描硬编码家目录并让构建失败。

## 本地跑起来

```bash
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
MOSHAN_HOME=/tmp/moshan-dev MOSHAN_NO_BROWSER=1 .venv/bin/python moshan/launcher.py
```

`MOSHAN_HOME` 把状态与资料库指到临时目录，不污染你自己的数据。
`MOSHAN_NO_BROWSER=1` 不弹浏览器，方便反复重启。

## 提交前自检

```bash
bash scripts/check.sh
```

它和 CI 的检查项一致：语法、导入冒烟（含 `/welcome`、`/about` 路由）、
更新资产名匹配（防止正则写错）、硬编码家目录扫描、密钥与运行时数据、版本号一致性。
全绿再提交。

**导入冒烟为什么要把 `HOME` 换成空目录**：开发机上有 `~/Documents/cuntu`，
交付版里几个模块会去那里拿东西（`article.py` 拿 `textfmt`）。用真实 `HOME` 跑，
本机永远全绿，而 CI（干净的机器）直接红。空 `HOME` 才是新用户的真实处境。

同类道理：**别用 `Path.home() / "Documents" / ...` 拼路径**，一律走 `paths.data_root()`，
否则你会在别人的 `Documents` 里造出自己的目录结构。

## README 里的截图怎么更新

截图放在 `docs/images/`，用真实界面截取，不要 P 图或手工改字：

| 文件 | 内容 | 拍法 |
|---|---|---|
| `01-files.png` | 主界面（资料库根目录） | 1280×720 视口、`deviceScaleFactor: 2` |
| `02-upload.png` | 「入库」标签页 | 同上，点底部「入库」 |
| `03-about-update.png` | `/about` 关于页 | 同上，访问 `?k=<令牌>` |
| `04-welcome.png` | `/welcome` 引导页 | 同上，免令牌 |
| `05-mobile.png` | 手机窄屏 | 390×844、`deviceScaleFactor: 3`、`isMobile` |

做法：用一个**临时**数据目录（`MOSHAN_HOME=/tmp/moshan-shot`）放几份示例文件，
启动服务后用 Playwright 截图，再 `sips -Z 1600` 缩到目标宽度。
**不要拿真实资料库截图**——路径、文件名会带出个人信息。

`docs/images/logo.svg` 是手写的：一座黑色小山（主峰 + 一道压淡的远山），坐在纸色圆角底卡上。
底卡不是装饰——纯黑剪影在 GitHub 深色模式下会整块消失，有底卡才留得住形状。
换色只改两个 `fill`，别去掉底卡。

## 版本与发布

版本号的唯一来源是 `moshan/version.py` 里的 `VERSION`，`VERSION` 文件与
`.gitignore`、CI 都从它读。改版本时**两处一起改**：

1. `moshan/version.py` → `VERSION = "0.2.0"`
2. `VERSION` 文件 → `0.2.0`
3. `CHANGELOG.md` 增加 `## [0.2.0] - YYYY-MM-DD` 段落

然后：

```bash
git add -A
git commit -m "release: v0.2.0"
git tag v0.2.0
git push origin main --tags
```

推送标签后 GitHub Actions 会自动：

1. 构建 macOS（当前只出 arm64）与 Windows（x64）安装包
2. 计算每个包的 sha256，并生成一份 `release.json` 作为人读的清单
3. 从 `CHANGELOG.md` 里抽出**本版那一节**当 Release 正文，后面接固定的「安装」说明
4. 创建 Release 并上传 dmg / exe / release.json

**所以「写更新日志」= 写 `CHANGELOG.md`。** Release 正文由 CI 从那里取，
不再手工往 GitHub 页面上贴 —— 手贴的东西一定会漏，而且是漏了没人发现的那种。
`scripts/bump-version.sh` 会顺手在 CHANGELOG 里开好新版本的空段落。

构建失败不会产生 Release，所以不会出现「版本号已经宣布但包不存在」的情况。

**仓库必须是 public。** 客户端更新器是用匿名请求读
`https://api.github.com/repos/<owner>/<repo>/releases/latest` 的，私有仓库会返回 404，
所有用户都会看到「检查更新失败」。真要私有分发，得在更新器里加 token，那是另一套设计。

**当前只出 Apple Silicon 包。** Intel Mac 用户能在 Release 里看到版本号，但没有可下载的
dmg（更新器会返回 `asset: null`，界面提示去 Release 页面手动找）。需要 Intel 包时，
在 Intel 机器上本地跑 `packages/macos/build.sh`，或把 `build.yml` 里 `macos-13` 那行
解除注释后确认该 runner 仍可用。

## 版本号规则

语义化版本，但目前只用到两段：

- `0.x.y` — 0.x 阶段允许破坏性变更，次版本号 +1 表示有功能变化
- 补丁号 +1 表示只有修复

预发布用 `0.2.0-beta.1`。注意 `updater.version_tuple()` 把预发布排在正式版**之前**，
所以 `0.2.0-beta.1` 不会被认为是 `0.2.0` 的更新。

## 更新器怎么判断

`updater.check()` 调 GitHub API 取 latest release，按资产名匹配当前平台：

| 平台 | 匹配规则 | 产物名 |
|---|---|---|
| macOS arm64 | 含 `macos` 与 `arm64`，扩展名 dmg/zip | `Moshan-0.2.0-macos-arm64.dmg` |
| macOS x64 | 含 `macos` 与 `x64` | `Moshan-0.2.0-macos-x64.dmg` |
| Windows x64 | 含 `windows` 与 `setup`，扩展名 exe | `Moshan-0.2.0-windows-x64-Setup.exe` |

**改产物命名时必须同步改 `updater._asset_pattern()`**，否则客户端找不到包。

## 排障：仓库放在 ExFAT 磁盘（U 盘 / 移动硬盘）上

如果把本仓库放在 macOS 挂载的 ExFAT 卷上，git 会不定期报这种错：

```
error: non-monotonic index .git/objects/pack/._pack-xxxx.idx
error: packfile .git/objects/pack/._pack-xxxx.pack index not opened
```

这时**任何** git 命令都会失败，看起来像仓库坏了，其实不是。

原因是 ExFAT 不支持扩展属性，macOS 会为每个文件额外生成一个 `._名字` 的边车文件。
git 扫描 `objects/pack/*.idx` 时，会把 `._pack-xxx.idx` 也当成索引打开，一读就崩。

修复：

```bash
bash scripts/fix-exfat.sh
```

脚本会清掉所有 `._*`、校验对象库；如果边车文件在 `pack/` 里立刻复发，会自动关掉
自动 gc（`gc.auto=0`），让 git 保持散装对象——散装目录里的 `._` 文件名不以 38 位
哈希开头，git 直接忽略，无害。本仓库首次初始化时已做过这一步。

两个相关注意点：

- `.git/` 内有大量 `._*` 属于**正常现象**，fsck 能过就不用管。
- 在 ExFAT 卷上**不要手跑 `git gc`**。散装对象文件多、簇又大（常见 256 KB/簇），
  `du` 看到的 `.git` 体积会远超真实内容（例如真实 1 MB 显示 80 MB），这是块放大，
  不影响功能，也不用清。

## 推送、并行任务与 main 保护

维护者是 **WorkBuddy**，但经常同时开着好几个窗口改这个仓库。真正会丢东西的不是「谁改文件」，
而是「**多个写者抢同一条分支**」：push 被拒时，AI 会自己 fetch / 强推 / `reset --hard`
「保留我的版本」，把别人的提交抹掉，而且不报错。所以用三层堵，缺一层都堵不住。

### 第 1 层：文件系统隔离（治「写同一份文件」）

新建任务时打开 **Worktree 开关**（输入框下方，默认关），选 Base Branch = `main`，
WorkBuddy 会建独立目录 + `workbuddy/main-<hash>` 分支，文件级互不污染。
命令行等价物：

```bash
bash ~/Documents/cuntu/new-worktree.sh <任务名>          # 建 /Volumes/AI仓库/worktrees/<任务名>
bash ~/Documents/cuntu/new-worktree.sh --rm <任务名>     # 用完删掉
```

不要在主工作目录里 `git checkout -b` —— 本机只有一份工作目录，切分支会把别人未提交的
改动一起带走。主工作目录永远停在 `main`。

### 第 2 层：服务端锁 main（治「推同一条分支」，唯一无法绕过的一层）

仓库上了 GitHub Ruleset（Settings → Rules → Rulesets，`main-baseline`）：

- Require a pull request before merging（approvals = 0，单人仓库不能自己批自己）
- Block force pushes
- Restrict deletions
- **Bypass list 为空** —— 管理员也绕不过

所以 `main` 的直接推送现在服务端会拒。日常流程变成：

```bash
git push origin HEAD:refs/heads/agent/<名字>     # 推自己的分支
# 建 PR → 合并（走 merge commit，不是 squash，这样本地能 fast-forward 上去）
git fetch origin && git merge --ff-only origin/main
```

发版（打 tag）不受影响，tag 不是分支引用。

### 第 3 层：钩子 + 规约（治「Agent 自作主张」）

`.git/hooks/pre-push` 拦四类：

| 情形 | 结果 |
|---|---|
| 非快进推送 / 改已有 tag / 删分支（任何分支） | 拒（`MOSHAN_FORCE=1` + 令牌才放，仅限用户排障） |
| 推 `main` / `master` / 非 `agent/*` 分支 | 拒（`MOSHAN_PUSH=workbuddy` 放行） |
| 推 `refs/heads/agent/*` | 放行 |
| 推 tag | 拒 |

每次尝试记 `~/Documents/cuntu/push-guard.log`（谁、什么引用、是否强推）。
钩子不依赖具体 Agent，任何工具在这份仓库里执行 git 都会撞上它。行为规约写在根目录
**`AGENTS.md`**（跨工具通用）和 `.trae/rules/project_rules.md`。

**别绕过钩子**：`--no-verify`、删钩子、卸载后重装都不算解决方式，服务端那道门还在，
只是错误信息变难懂了。

### 已经撞了怎么收

1. 先在 GitHub 上确认 ruleset 是 active，防止继续被抹。
2. 本地丢的提交：`git reflog` 找 SHA → `git cherry-pick`。
3. 远端被覆盖的提交：仓库 Events / `git ls-remote` 翻旧 SHA，同样 cherry-pick 回来。
4. 这一轮**不要交给 Agent 做**，手动来。

### 一条经验

Git 是版本历史，不是共享内存。需要多窗口实时共享的状态（配置表、进度数据、看板）
别放仓库里，放中台的接口后面 —— 现在 `Agent自述.jsonl`、`pocket.jsonl` 就是这么处理的。

## 不要做的事

- 不要把 `config.json`、`certs/`、`*.jsonl` 提交上去（`.gitignore` 已挡，别用 `-f` 绕过）
- 不要在代码里写死用户的绝对路径
- 不要把模型文件放进仓库（2 GB 以上，且 GitHub 单文件上限 100 MB）
- 不要用 `git pull` 做更新分发 —— 用户目录里会有本地改动，冲突处理不了。
  更新只走「安装包 + 覆盖程序目录」，与 git 无关。
