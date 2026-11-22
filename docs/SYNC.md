# 从开发目录同步到交付仓库

开发在 WorkBuddy / 本机里进行，分发给别人的成品在本仓库。这份文档说明两者的关系，
以及每次更新后怎么把改动搬进来而不把打包版搞坏。

## 一句话流程

```bash
bash scripts/sync-from-dev.sh --dry-run   # 先看会发生什么
bash scripts/sync-from-dev.sh             # 正式同步
git diff moshan/                          # 看清改了哪些行
bash scripts/check.sh                     # 自检
git add -A && git commit -m "feat: ..."   # 提交
```

需要发版的提交另外做：`bash scripts/bump-version.sh X.Y.Z` → 改 CHANGELOG → 提交 → 打 tag。

## 两个目录的关系

| | 路径 | 角色 |
|---|---|---|
| 开发目录 | `~/.trae-cn/.../local-ai-workspace/pocket/` | 你日常改代码的地方，跑在本机服务上 |
| 交付目录 | 本仓库的 `moshan/` | 给别人装的版本，带跨平台移植改造 |

开发目录里还有 `.bak-*` 备份、`config.json`、`pocket.jsonl` 日志、`certs/`、`tools/`、
`agent/` 等，这些**都不是分发资产**。同步脚本只取 `*.py` 与 `ui.html`。

交付目录里另有 7 个开发目录没有的模块，是分发形态才需要的，同步脚本永不删除它们：

```
paths.py     程序/状态/资料库三层目录的唯一来源
version.py   版本号与更新源
updater.py   查 Release、下载、校验、唤起安装包
launcher.py  选端口、建目录、唤起浏览器
bootstrap.py 检测 Ollama、拉模型
welcome.py   /welcome 新手引导页
about.py     /about 页、注入首页入口、后台自动查更新
```

## 从 cuntu/ 取的文件

开发目录 `pocket/` 不是自足的：它靠 `sys.path.insert(0, ~/Documents/cuntu)` 去拿
cuntu 里的模块。**开发机上有 cuntu，所以这件事永远不报错**；交付版没有这个目录，
少一个文件就是「新用户装完打不开」，而且本机复现不出来。

2026-11-23 就是这么炸的：`article.py` 顶层 `import textfmt`，而 `textfmt.py` 只在
`cuntu/` 里，仓库里没有 → CI 的导入冒烟直接红，本地全绿。

脚本顶部用 `EXTERNAL` 登记这类文件，同步时从 `CUNTU_DIR`（默认 `~/Documents/cuntu`，
可用 `MOSHAN_CUNTU` 覆盖）取，和 `pocket/` 的文件一样逐字镜像：

```bash
EXTERNAL="textfmt.py"
```

判定标准只有一条：**纯标准库、且被交付版模块在顶层 import**。像 `layout`、`cuntu`、
`hostwork` 这些要么在函数内延迟导入、要么有 try/except 兜底，缺了只是少个功能，
不用搬。

## 为什么不能直接 cp

有 9 份在交付版里带着**移植补丁**，直接 `cp` 会把补丁冲掉，打出来的包双击就崩：

| 文件 | 补丁内容 |
|---|---|
| `server.py` | 路径走 `paths.*()`；`_bootstrap_config()` 不继承开发机的 config；启动段抽成 `main()` |
| `ocr_runner.py` | cuntu 解释器路径可配置，缺失时静默跳过；阅读目录走资料库 |
| `plan_mcp.py` | 文档路径修正 |
| `article.py` | cuntu 位置可配；`READ_ROOT` / `GALLERY_ROOT` 走 `paths.data_root()` |
| `linkbox.py` | 同上，外加 `CUNTU_PY` 按平台取（Windows 是 `Scripts/python.exe`） |
| `articlebox.py` | 解释器复用 `linkbox.CUNTU_PY`；分类落点走 `linkbox.READ_ROOT` |
| `heartbeat.py` | `SYS_DIR` / `INSPECT_CMD` / `_data_root()` 走 `paths.data_root()` |
| `metrics.py` | 读配置改用 `paths.config_file()`（原来读程序目录，打包后永远读不到） |
| `pending.py` | 待定队列挂到资料库 |

所以脚本对这几份用三方合并：

```
基线   .sync-baseline/<文件>    上次同步时开发目录的样子
我方   moshan/<文件>            基线 + 补丁 + 已合入的改动
对方   <开发目录>/<文件>         你这次的新改动
```

结果 = 你的新改动叠加在补丁之上。验证过的行为：在 `server.py` 中段新增一条接口，
同步后 `git diff` 正好是 `+6` 行，补丁一行没动。

## 冲突

只有「你和补丁改到同一处」才冲突。最常见的是在 `server.py` **文件末尾**追加代码 ——
那里正是 `main()` 被改造的地方。

冲突时脚本**不写交付文件**，把结果另存为 `.sync-conflicts/<文件>.merged`，退出码 1。
打开它，`<<<<<<<` 到 `>>>>>>>` 之间就是两边的内容，保留需要的部分，粘回
`moshan/<文件>`，再跑一次脚本推进基线即可。

## 其余规则

- **新增补丁文件必须登记**：如果你再把某个文件改造成分发形态，把文件名加进
  `scripts/sync-from-dev.sh` 顶部的 `PATCHED` 列表，否则它会被逐字覆盖。
- **新依赖 cuntu 的文件登记 `EXTERNAL`**：标准同上（纯标准库 + 顶层 import）。
- **数据根只有 `paths.data_root()` 一个出口**：开发版是 `~/Documents/AI项目`，
  分发版是资料库。别再写 `Path.home() / "Documents" / ...` —— 那会在别人的
  `Documents` 里造出你的目录结构，而且工作台会读不到阅读记录（各写各的根）。
- **`--init-baseline` 只在两种情况下用**：刚把某个文件加进 `PATCHED`；或者你确认
  「开发版此刻等同于上次同步的状态」。误用会让那一轮的增量被当成"无改动"而跳过。
  用之前先 `diff -u moshan/<文件> <开发目录>/<文件>` 看一眼，确认差异只剩补丁。
- 别手跑 `git gc`：仓库在 ExFAT 上，打包会产生 `._pack-*.idx` 边车文件把 git 读崩。
  真出问题跑 `bash scripts/fix-exfat.sh`。
- `dist/` 不进仓库，安装包由 CI 出。更新器读的是 GitHub Release，不是仓库里的文件。
