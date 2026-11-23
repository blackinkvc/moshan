# 更新日志

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [未发布]

### 计划
- Windows 上补缩略图（用 PowerShell + System.Drawing 替代 macOS 的 sips）
- 应用内托盘图标与「退出」菜单
- 真正的原子更新（versions/ + current 符号链接，替换正在运行的程序）

## [0.2.4] - 2026-11-23

**把「本机工作」的采集层收进交付包：别人机器上的 Moshan 不再是空壳。**

以前 `hostwork` / `trae_work` 只躺在作者本机的 `cuntu/` 里，交付版没带，于是每台
新装的机器都显示「本机工作模块没装上」，只能退到 `_CLIENTS` 那份降级账 ——
而且作者本机永远复现不了这个问题。

### 新增
- `hostwork.py`（172 KB）、`trae_work.py`、`cherry_usage.py`、`billing.py` 随包分发。
  `hostwork` 负责本机工作采集（会话记忆 / 进程 / 用量 / 计费对账），`trae_work` 是
  它的 TRAE 取数层，另外两个分别是 Cherry Studio 用量账本与供应商账单
- `hostwork_ui.html` 内置：`/hostwork` 详情页不再依赖外挂目录
- `/hostwork` 的查找顺序改成「先 cuntu、再包内」：开发机行为不变，打包版不再 404

### 修复
- `moshan/paths.py` 补 `ensure()` 与 `CONF_FILE`。`hostwork` / `billing` 在**模块级**
  就调用 `paths.ensure(data_root() / "_本机工作")`，缺这个函数是导入当场 AttributeError，
  不是运行期降级 —— 整个模块直接不可用
- `packages/moshan.spec` 的 `datas` 增加 `hostwork_ui.html`；`scripts/sync-from-dev.sh`
  的 `EXTERNAL` 增加这五个文件，以后同步不会再漏

### 说明
- `cherry_usage` 的账本路径仍是 macOS 的 `~/Library/Application Support/CherryStudio`，
  Windows 上取不到就采空（不报错）；要支持 Windows 得另接 `%APPDATA%`，这一版没做
- `trae_work` 里的窗口标题采样走 CoreGraphics，非 macOS 会在 `try` 里失败并跳过，
  不影响其余采集
- 采集线程在 `server` 里启动，异常只写日志（`hostwork_thread_error`），不会拖死中台

## [0.2.3] - 2026-11-23

**让 Trae 自己把「干了什么 + 花了多少」报进中台，装完新版本不用手点。**

### 新增
- `Trae 自述通道`：`POST /api/trae/note` 现在收用量（`model` / `in` / `out` / `cached` /
  `calls`），落 `_本机工作/TRAE自述.jsonl`；工作台把这份自述折成逐轮记录，
  token 就显示在那一行上，跟会话记忆读出来的条目并列（两者 id 不冲突）
- **升级后自动投放说明书**：启动时若 `~/.trae-cn` 存在且投放版本与当前版本不一致，
  就把 `pocket-bridge` 技能写进 Trae 的技能目录，日志会留一行
  `已更新 TRAE 自述说明书 x.y.z → <路径>`。没装 Trae 的机器直接跳过，不凭空造目录
- `scripts/check.sh` 增加第 7 节：断言说明书里写的是 `/api/trae/note`（不是总线那条
  `/api/agent/event`）、带用量字段、`auto_install` 存在、工作台确实读自述文件

### 修复
- **干净机器上 `/api/trae/note` 直接 503**：接口依赖 `cuntu/trae_work.py` 取路径，
  那模块没随包分发，于是新装的机器一条自述都收不进来。现在模块不在就自己拼
  `<数据根>/_本机工作/TRAE自述.jsonl`，写和读指向同一个位置
- 说明书原来让人把账报给 `/api/agent/event`（agent 总线）和 `/api/agent/usage`，
  而工作台 TRAE 卡读的是自述文件 —— **报得再勤，卡片也是空的**。现在工作面明确指向
  `/api/trae/note`，总线那条降级为「给别的 agent 看」的可选项

### 说明
- Trae 的命令 / 进程 / 会话记忆那三路自动采集仍依赖开发目录里的 `trae_work` /
  `hostwork`，这两个模块没进仓库。这一版保证的是「自述」这条**不依赖 Trae 本地库**
  的通道：Trae 主动 POST，换机器、换 Trae 版本都不会断
- 用量估不出来就填 0。说明书里写死了这条，卡片宁可显示 0 也不显示编出来的数字

## [0.2.2] - 2026-11-23

**修 Windows 版首次启动必崩：双击图标没反应，界面不出来。**

### 修复
- **Windows 首次启动崩溃**：`tls._generate()` 里 `tempfile.mkstemp()` 返回的是
  「已打开的 fd + 路径」，代码只取了路径，fd 一直没关。证书生成完在 `finally` 里
  `unlink()` 那个临时 `.cnf` 时，POSIX 允许删除仍被打开的文件（所以 macOS 一直正常），
  Windows 直接 `PermissionError: [WinError 32]`；异常从 `finally` 穿出去打死
  `server.main()`，PyInstaller 再弹一个 traceback 对话框把进程挂住 —— 用户看到的
  就是「双击没反应」。现在 fd 立刻关闭，`unlink` 失败也只忽略
- **证书生不出来不再拖死启动**：`tls.ensure()` 整体兜一层，`openssl` 缺失、调用超时、
  权限问题一律退化成明文 HTTP 继续启动。证书只决定手机端能不能用 https，
  不该决定程序能不能打开
- **TLS 建不起来时本机端口根本不监听**：本机明文端口 `local_port` 的绑定原先写在
  `if scheme == "https"` 里，一旦退化成 http 就一个都不绑；而启动器打开的正是
  `127.0.0.1:{local_port}`，等于把用户领到一个没人监听的端口。现在无条件绑定（仅 loopback）

### 说明
- 这一版的问题是 CI 头一回真正「把安装包跑起来」才发现的：构建日志全绿、
  macOS 侧冒烟也过，只有 Windows 会崩。**构建成功 ≠ 装完能用** —— 上一版缺 CA
  证书是同一个教训，所以这轮没有靠读代码下结论，而是把安装包拉起来打接口
- 冒烟判据改成「以程序自己打印出来的本机访问地址为准」，不再写死某个端口；
  失败时自动 dump 启动日志、`config.json`、`netstat` 监听项与进程退出码
- **0.2.0 的用户直接下 0.2.2**：0.2.0 的安装版缺 CA 证书（任何 https 都会失败，
  包括「检查更新」本身），Windows 侧还叠加了上面这个启动崩溃，自动升级这条路走不通

## [0.2.1] - 2026-11-23

**这个版本修的是 0.2.0 的一个致命问题：安装版里所有 https 请求都会失败。**
装过 0.2.0 的人没法靠「检查更新」升上来（坏的正是这条链路本身），需要手动重新下载。

> 注：0.2.1 的 tag 打出去了，但构建阶段 Windows 冒烟没过，Release 没有发布出去 ——
> 请直接使用 0.2.2（Windows 启动崩溃也是在这一版后续修掉的）。

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
