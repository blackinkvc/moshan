# 更新日志

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [未发布]

### 计划
- Windows 上补缩略图（用 PowerShell + System.Drawing 替代 macOS 的 sips）
- 应用内托盘图标与「退出」菜单
- 真正的原子更新（versions/ + current 符号链接，替换正在运行的程序）

## [0.2.1] - 2026-11-23

**这个版本修的是 0.2.0 的一个致命问题：安装版里所有 https 请求都会失败。**
装过 0.2.0 的人没法靠「检查更新」升上来（坏的正是这条链路本身），需要手动重新下载 0.2.1。

### 新增
- `netca.py`：启动最早期把 `SSL_CERT_FILE` 指向随包分发的 certifi，给整个进程
  建立可信的 HTTPS 信任链。打包入口 `launcher.py` 与 `server.py` 都会调用，
  两种启动方式行为一致
- `scripts/check.sh` 增加「网络信任链」一节：断言 certifi 已安装、`netca.install()`
  确实选中它、`ssl.create_default_context()` 里装进了根证书（离线可跑，不需要发请求）
- 下载元数据增加 `verified` 字段。GitHub 没回 `digest` 时界面不再谎称「已校验通过」，
  改为如实提示「未做完整性校验」

### 修复
- **安装版 https 全线失效**：PyInstaller 打出来的应用用自己的 OpenSSL，默认 CA 路径
  （macOS 上是 Homebrew 那种 `/usr/local/etc/openssl@3/cert.pem`）在别人机器上不存在，
  于是 `urllib.request.urlopen()` 一律抛
  `CERTIFICATE_VERIFY_FAILED: unable to get local issuer certificate`。
  影响的不只是「检查更新」——抓网页、存图、装插件（共 12 处 `urlopen`）全都用不了。
  **源码态完全复现不出来**：本机有 `/etc/ssl/cert.pem`、site-packages 里也有 certifi，
  所以本地自检、CI、手工验证全是绿的。0.2.0 就是这样发出去的
- `requirements.txt` 显式加 `certifi`，`packages/moshan.spec` 显式收它的数据文件。
  以前它是 httpx 的间接依赖，代码里没人 import 它，PyInstaller 就不会带进包
- `version.version_tuple()` 会剥掉 `v` 前缀。Git tag 与 `release.json` 都带 `v`，
  1.x 版本上会被解析错（`v1.2.3` → `0.2.3`），此前只是因为 0.x 才侥幸没出事
- CI 生成的 `release.json` 里 `version` 改为不带 `v`；原先把 tag 名原样写进去，
  谁拿它跟 `version.VERSION` 比较都会出岔子

### 说明
- Release 附件只有 dmg / exe / `release.json`，**不再单独发 `.sha256` 文件**。
  校验值在 `release.json` 和 GitHub API 每个资产的 `digest` 字段里（两者同源同值，
  0.2.0 的 dmg 实测三方一致），更新器读的是后者。单独一个 `.sha256` 附件只是噪音

## [0.2.0] - 2026-11-23

### 新增
- **关于 / 更新页 `/about`**：显示版本、构建、平台、三层数据目录，并能检查更新、
  下载（带进度）、打开安装包。首页右下角注入一个小入口，有新版本时自动变成
  「新版本 x.y.z」；后台每 6 小时静默查一次，等于把更新推到你眼前
- 更新器命令行入口：`--check` / `--download` / `--apply`，发版后可在终端自测更新链路
- `sysinfo.py`：内存 / CPU 核数 / 负载 / 开机时长 / 进程数的跨平台采集。装了 `psutil`
  就用它，没装则分平台兜底（macOS 调 `sysctl` + `vm_stat`、Linux 读 `/proc/meminfo`、
  Windows 用 `ctypes` 调 `GlobalMemoryStatusEx`）；量不到就返回 `null`，不编数字
- `GET /api/sys`：轻量机器状态，供需要高频轮询的卡片使用；与偏重的 `/api/status`
  分开，避免为了五个数把整包账算一遍
- `POST /api/trae/note`：Agent 收尾时自报「这一轮干了什么」，同一任务 5 分钟内只记一次；
  没装 cuntu 扩展时接口返回 503 并说明原因，不影响其他功能

### 修复
- **交付版缺 `textfmt.py`，新用户装完打不开**。`article.py` 在模块顶层 `import textfmt`，
  而这个文件只在作者的 cuntu 扩展目录里，仓库里没有；开发机上有 cuntu 所以本机永远正常，
  CI 一跑导入冒烟就红。现已把 `textfmt.py`（纯标准库，17KB）纳入仓库，并在同步脚本里
  加了 `EXTERNAL` 机制从 `cuntu/` 取这类文件，避免下次再漏
- **`check.sh` 的导入冒烟只抽查 8 个模块，且用真实 HOME 跑**，所以「本地全绿、CI 全红」
  成了常态。现在改成导入 `moshan/` 下全部 43 个模块，并把 `HOME` 指向空目录 ——
  开发机上的 `~/Documents/cuntu` 再也掩盖不了缺文件
- **交付版会把数据写进别人的 `~/Documents/AI项目`**：`article` / `linkbox` / `heartbeat` /
  `metrics` / `pending` / `ocr_runner` 各自写死了开发机的目录结构，其中三处在启动时
  `mkdir`，等于在用户的 `Documents` 里凭空造出一个中文目录。新增 `paths.data_root()`
  作为唯一出口，分发版即资料库；`MOSHAN_CUNTU` 统一到这几个模块，Windows 的
  cuntu 解释器路径也改对了（`Scripts/python.exe`）
- **`metrics.py` 读的是程序目录里的 `config.json`**，打包后那个位置没有配置，
  于是永远退化到写死路径。改用 `paths.config_file()`
- **`updater._asset_pattern` 的正则写错**：`(?i)` 只能出现在表达式开头，写成
  `a|(?i)b` 会抛 `re.error: global flags not at the start of the expression`。
  结果「检查更新」对所有人都是 500。改用 `re.IGNORECASE`，并在 `scripts/check.sh`
  里加了一条断言（用 CI 真实产出的资产名做匹配），防止再犯
- 更新源的默认仓库地址写错（`pamphile0117/moshan` → `blackinkvc/moshan`），
  会导致「检查更新」直接 404
- `CUNTU_DIR` 从写死的家目录改为可被 `MOSHAN_CUNTU` 覆盖，与 `ocr_runner.py` 一致；
  分发版没有这个扩展目录时静默跳过

### 文档
- README 重写：能力清单（标出哪些功能需要模型）、安装与首次引导、数据三层与
  「更新不丢东西」的原因、更新流程、目录结构、构建与发版、Agent 接口、安全模型、
  已知限制
- README 版式：居中标题 + 手绘「山」字标 + 徽章行 + 首屏截图；界面、引导、更新
  三处各配一张真实截图，手机端单独一张竖图
- 新增 `docs/images/`（5 张界面截图 + `logo.svg`），CONTRIBUTING 补「截图怎么更新」，
  写明截图必须用临时数据目录，不许拿真实资料库拍

## [0.1.0] - 2026-11-22

首个可分发的版本。

### 新增
- 桌面启动器：自动选端口（冲突顺延）、建目录、唤起浏览器
- 三层目录约定：程序 / 状态 / 资料库物理分离，覆盖安装不丢数据与设置
- 新手引导页 `/welcome`：环境检查、Ollama 探测、一键拉取模型、进度实时回显
- 更新器：查询 GitHub Release、下载、sha256 校验（不匹配即丢弃）、唤起安装包
- macOS 打包（PyInstaller onedir + .app + ad-hoc 签名 + dmg）
- Windows 打包（PyInstaller + Inno Setup 单文件安装程序）
- GitHub Actions：推 `v*` 标签自动双平台构建并发布 Release，含 `release.json` 与校验值
- CI：语法检查、隔离数据目录的导入冒烟测试、密钥与硬编码路径扫描
- Agent 接入 `/api/agent/link/*`：认人（scan）→ 装说明书（install，写进 Trae/Cursor/
  Claude/WorkBuddy 的技能目录）→ 收回报（report）→ 按回执导入（import），
  把别的 Agent 的会话记忆导进中台自报账

### 变更
- `server.py` 的启动逻辑抽成 `main()`，供启动器在同一进程内调用
- 配置默认路径从写死的 `/Users/pamphile/...` 改为跨平台取值，老用户仍可继承
  同目录下 `feishu_bridge/config.json` 的网关密钥
- 桌面形态下 stdout/stderr 重定向到 `状态目录/logs/moshan.log`
- 服务方阵按平台过滤：`gallery / reader / media / inspect / llmcost / follow /
  review / gateway / qdrant` 只在 POSIX 上显示，Windows 上不再一开就是一排红点
- 工作流水里导入/补记的账最多占 24 格，不会被近期会话挤出列表
- Agent 接入在 Windows 上会扫描 `C:\Users\*` 找真正的家目录（以服务身份运行时
  `Path.home()` 会指到 `systemprofile`，那里面没有任何 Agent 数据），并给扫描结果
  加 20 秒缓存，避免反复开关面板把几百 MB 目录走一遍

### 已知限制
- macOS 安装包未做 Apple 公证，首次打开需右键「打开」
- Windows 上缩略图功能降级为显示原图
- 无系统托盘，退出需在 Dock 上 Cmd+Q（macOS）或结束进程（Windows）
- 模型依赖 Ollama；自定义 OpenAI 兼容端点需手工填配置
