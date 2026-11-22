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
python -m compileall -q moshan
cd moshan && MOSHAN_HOME=/tmp/moshan-ci PYTHONPATH=. python -c "
import paths, server
print(len(server.app.routes), '条路由')
assert '/welcome' in {r.path for r in server.app.routes}
"
```

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
3. 创建 Release 并上传 dmg / exe / release.json

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

## 不要做的事

- 不要把 `config.json`、`certs/`、`*.jsonl` 提交上去（`.gitignore` 已挡，别用 `-f` 绕过）
- 不要在代码里写死用户的绝对路径
- 不要把模型文件放进仓库（2 GB 以上，且 GitHub 单文件上限 100 MB）
- 不要用 `git pull` 做更新分发 —— 用户目录里会有本地改动，冲突处理不了。
  更新只走「安装包 + 覆盖程序目录」，与 git 无关。
