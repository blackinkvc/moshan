# 墨山中台 · Moshan

[![Release](https://img.shields.io/github/v/release/blackinkvc/moshan?label=release)](https://github.com/blackinkvc/moshan/releases)
[![Build](https://img.shields.io/github/actions/workflow/status/blackinkvc/moshan/build.yml?label=build)](https://github.com/blackinkvc/moshan/actions)
[![License](https://img.shields.io/github/license/blackinkvc/moshan)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows-lightgrey)

**把一台电脑变成你自己的中台。** 装完双击打开，浏览器里就是一个界面：手机往电脑传文件、
电脑把文件分享给朋友、随手把网页存下来读、需要时让本地模型帮你看文件回答问题。

没有账号，没有云端，没有遥测。**程序、状态、资料库三层物理分离**，所以换版本、覆盖安装、
重装都不会动你的设置和文件 —— 这不是靠代码小心，是目录结构决定的。

> 中文名「墨山中台」，英文名 Moshan（拼音，无同名软件）。

---

## 目录

- [它到底能干什么](#它到底能干什么)
- [下载安装](#下载安装)
- [第一次打开：三步引导](#第一次打开三步引导)
- [界面怎么用](#界面怎么用)
- [数据放在哪（也是「更新不丢东西」的原因）](#数据放在哪也是更新不丢东西的原因)
- [更新](#更新)
- [从源码运行](#从源码运行)
- [目录结构](#目录结构)
- [自己构建安装包](#自己构建安装包)
- [给 Agent 和脚本用的接口](#给-agent-和脚本用的接口)
- [安全模型](#安全模型)
- [常见问题](#常见问题)
- [已知限制](#已知限制)
- [参与开发](#参与开发)
- [许可](#许可)

---

## 它到底能干什么

| 能力 | 说明 | 要不要模型 |
|---|---|---|
| **文件管理** | 浏览器里浏览、上传、下载、重命名、移动、打包下载，按名称/时间/大小排序 | 不要 |
| **手机传文件** | 手机打开同一地址，选个文件就进电脑的「收件箱」；上传有进度、可中断 | 不要 |
| **分享给别人** | 给文件生成一条带令牌的链接，对方不用装软件，到期自动失效 | 不要 |
| **回收站** | 删除先进回收站，可还原；30 天后自动彻底清掉（天数可改） | 不要 |
| **网页存下来读** | 粘一条链接，服务端抓正文、留原文，抽出可读版本 | 不要 |
| **对话** | 接本机 Ollama，或任何 OpenAI 兼容端点（LM Studio / vLLM / 云 API） | 要 |
| **读文件问问题** | 让模型看收件箱里的文档（PDF / 文本）、网页正文，回答你的问题 | 要 |
| **WebDAV** | 打开后可以用系统「连接服务器」挂载资料库（默认关闭） | 不要 |

一句话：**不装模型也完全能用**，文件、传输、分享、回收站都不依赖模型；
只有「对话」和「读文件问问题」在没有模型时降级。

---

## 下载安装

到 [Releases](https://github.com/blackinkvc/moshan/releases) 下载对应系统的安装包：

| 系统 | 文件 | 说明 |
|---|---|---|
| macOS（Apple 芯片） | `Moshan-<版本>-macos-arm64.dmg` | M1 / M2 / M3 / M4 |
| Windows 10 / 11（64 位） | `Moshan-<版本>-windows-x64-Setup.exe` | 一键安装，无需管理员权限 |

安装包自带 Python 运行时，**不需要另外装 Python、也不需要装 Node**。

<details>
<summary><b>macOS 提示「无法验证开发者」怎么办</b></summary>

安装包没有 Apple 付费开发者签名（$99/年），这是系统对未签名应用的固定行为，**不是文件损坏**。

任选一种，只需一次：

- 访达里 **右键点 Moshan → 打开 → 再点「打开」**
- 或「系统设置 → 隐私与安全性」，页面下方点「仍要打开」
- 或终端执行：`xattr -dr com.apple.quarantine /Applications/Moshan.app`

</details>

<details>
<summary><b>Windows 提示「Windows 已保护你的电脑」怎么办</b></summary>

同样是未做代码签名导致的 SmartScreen 提示。点「更多信息 → 仍要运行」即可，只需一次。

安装程序本身按当前用户安装（`PrivilegesRequired=lowest`），不会弹 UAC、不写系统目录。

</details>

**没有 Intel Mac 的安装包**：CI 只为 Apple Silicon 构建 —— `macos-13`（Intel）运行器
已被 GitHub 列入退役计划，放进矩阵会让整个发布流程因「runner 不存在」而失败。
需要 Intel 包时在 Intel 机器上跑一次 `bash packages/macos/build.sh` 即可，脚本本身两平台通用。

---

## 第一次打开：三步引导

装完直接启动，浏览器自动打开 `/welcome`。**引导在软件里，不在安装器里** —— 这样
「装完等下载」的阶段你能看见进度，而不是对着一个不动的安装窗口猜它卡在哪。

1. **环境检查**
   显示程序目录、状态目录、资料库目录（确认三者分开），并探测本机是否已有 Ollama。

2. **准备模型（可选，可跳过）**
   检测到 Ollama → 一键 `pull` 模型（约 2.4 GB，自带断点续传，进度实时回显）。
   没有 Ollama → 可跳过，或改填任意 OpenAI 兼容端点。
   模型放 Ollama 自己的目录，**卸载重装 Moshan 不会重复下载**。

3. **进入中台**
   打开工作界面。之后左下角 **?** 随时能找回这份引导，也可以直接访问 `/welcome`。

> 引导页本身免令牌，但仍受网络守卫保护：只对本机与 Tailscale 来源开放。

---

## 界面怎么用

启动器会打印这样几行（端口被占会自动顺延，所以数字可能不同）：

```
口袋服务 http://127.0.0.1:8323/?k=<令牌>   （本机用这个，无证书警告）
口袋服务 https://127.0.0.1:8321
         https://<机器名>.<tailnet>.ts.net:8321/?k=<令牌>
访问令牌 <令牌>
```

- **本机**：用 `http://127.0.0.1:8323/?k=...`，明文端口，没有证书警告。
- **手机 / 另一台电脑**：用 `https://<隧道地址>:8321/?k=...`，第一次会提示自签证书，
  点「继续」即可。控制台里打印的就是完整可用地址，复制过去就能开。
- 令牌丢了：看状态目录里的 `config.json`（`token` 字段），或跑
  `python moshan/show_token.py`，它会把地址、令牌、证书、WebDAV 状态一起打出来。

界面左侧是文件区，右侧可以开对话。上传、下载、分享、回收站都在顶栏；
「分享」生成的链接自带有效期，过期自动失效。

---

## 数据放在哪（也是「更新不丢东西」的原因）

| 层 | macOS | Windows | 装什么 | 更新时 |
|---|---|---|---|---|
| **程序** | `/Applications/Moshan.app` | `%LOCALAPPDATA%\Programs\Moshan` | 代码与运行时 | 整体替换 |
| **状态** | `~/Library/Application Support/Moshan` | `%APPDATA%\Moshan` | 配置、令牌、证书、日志、下载缓存 | **不动** |
| **资料库** | `~/Documents/Moshan` | `%USERPROFILE%\Documents\Moshan` | 你的文件、收件箱、回收站、工作台页面 | **不动** |

安装包只覆盖「程序」这一层。所以覆盖安装、升级、卸载重装，你的设置和文件都原样还在。

- **想换机器**：拷走「状态」和「资料库」两个目录，程序重装一次即可。
- **想彻底删干净**：三个目录一起删。
- **想自定义位置**：用环境变量 `MOSHAN_HOME` 指定一个根，三层目录会在它下面重建（测试用）。

---

## 更新

界面**右下角有个「关于」**，点进去是 `/about`：版本、数据目录、检查更新都在这一页。
后台每 6 小时自己查一次 GitHub Release，**有新版本时这个入口会直接变成橙色的
「新版本 x.y.z」**，不用你主动去点。

更新流程：

1. 有新版本 → 显示版本号、更新说明、安装包体积
2. 点下载 → 边下边算 sha256，与 GitHub 给出的资产 digest 比对
3. **校验不一致立刻丢弃**，不会让你装上一个损坏或被替换的包
4. 校验通过 → 点「打开安装包」，按系统流程覆盖安装
5. 装完重开，配置与文件原样

下载的包留在状态目录的 `updates/` 下（只保留最近两个），所以「今天下载、明天再装」
也不会白下。命令行同样能查：

```bash
python moshan/updater.py --check      # 只看有没有新版
python moshan/updater.py --download   # 下载并校验，不安装
python moshan/updater.py --apply      # 下载、校验、打开安装包
```

**为什么不做「热替换运行中的程序」**：macOS 上 `.app` 运行时无法安全替换自身，
Windows 上运行中的 exe 被锁。强行做要「拉新进程 → 旧进程自杀 → 失败回滚」三段式，
任一段出错就是半个坏软件。首版选择「下载 → 校验 → 交给系统安装器」，失败也破坏不了现有安装。

---

## 从源码运行

需要 Python 3.11+（打包用的是 3.11，3.10 一般也能跑）。

```bash
git clone https://github.com/blackinkvc/moshan.git
cd moshan
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python moshan/launcher.py
```

Windows 把 `.venv/bin/` 换成 `.venv\Scripts\`。

启动器做四件事：建目录骨架 → 选一个可用端口 → 后台等端口就绪再开浏览器 → 同进程起服务。

调试时常用开关：

```bash
MOSHAN_NO_BROWSER=1 .venv/bin/python moshan/launcher.py   # 不自动开浏览器
MOSHAN_HOME=/tmp/moshan-test .venv/bin/python moshan/launcher.py   # 数据全放临时目录
```

跑一遍自检（语法、导入冒烟、硬编码路径扫描、版本号一致性）：

```bash
bash scripts/check.sh
```

---

## 目录结构

```
moshan/                 业务代码，平铺模块，import 直接用名字
  launcher.py           桌面入口：建目录、选端口、起服务、开浏览器
  server.py             服务本体（FastAPI），路由与网络守卫
  paths.py              三层目录约定 —— 改路径先看这里，别处不许拼路径
  welcome.py            新手引导页 /welcome
  about.py              关于 / 更新页 /about（服务端注入入口，不改 ui.html）
  bootstrap.py          环境探测、Ollama 拉模型、端点连通性检查
  updater.py            检查更新、下载、sha256 校验、唤起安装包（含命令行入口）
  storage.py            文件读写、移动、回收站
  transfer.py           上传下载、打包、断点
  webdav.py             WebDAV 挂载（默认关闭）
  dash.py               工作台聚合数据（/api/dash）
  agentbus.py           Agent 总线：会话记账，给别的 agent 读
  agent_link.py         把 Trae / Cursor / Claude 等 Agent 接进来
  plan.py / plan_mcp.py 工作计划（含 MCP 服务端）
  sysinfo.py            内存 / CPU / 负载，跨平台
  ui.html               主界面（单文件，无构建步骤）
packages/
  moshan.spec           两平台共用的 PyInstaller 配置
  macos/build.sh        源码 → Moshan.app → dmg
  windows/build.ps1     源码 → dist\Moshan → 安装程序
  windows/moshan.iss    Inno Setup 脚本
scripts/
  check.sh              本地自检，与 CI 检查项对齐
  bump-version.sh       一次改全版本号（VERSION / version.py / CHANGELOG）
  sync-from-dev.sh      把上游开发目录同步进来（含三方合并，见 docs/SYNC.md）
docs/
  ARCHITECTURE.md       架构与关键取舍
  SYNC.md               同步机制：哪些文件是逐字镜像、哪些带补丁
.github/workflows/
  build.yml             推 v* 标签 → 双平台构建 → 发 Release
  ci.yml                推代码 → 语法检查、导入冒烟、密钥与路径扫描
```

界面是**单文件 HTML**，没有前端构建步骤：改 `moshan/ui.html`，刷新页面即可见效果，不用重启服务。
改 `.py` 则要重启。

---

## 自己构建安装包

```bash
# macOS：产出 dist/Moshan.app 与 dist/Moshan-<版本>-macos-arm64.dmg
bash packages/macos/build.sh
bash packages/macos/build.sh --no-dmg      # 只要 .app，省一步

# Windows：产出 dist\Moshan 与安装程序
powershell -ExecutionPolicy Bypass -File packages\windows\build.ps1
```

Windows 的 `.exe` 只能在 Windows 上构建（PyInstaller 不支持交叉编译），所以
`build.ps1` 走 CI 跑，或者你在 Windows 机器上本地跑。

发版流程：

```bash
bash scripts/bump-version.sh 0.2.0     # 改 VERSION / version.py / CHANGELOG
bash scripts/check.sh                  # 自检
git add -A && git commit -m "chore: 0.2.0"
git tag -a v0.2.0 -m "Moshan 0.2.0"
git push origin main && git push origin v0.2.0
```

推 `v*` 标签后 Actions 自动在两个平台构建、跑校验、把 dmg / exe / 校验值发到 Release。
两个 job 全绿才算发完（Release job 会等构建 job）。

---

## 给 Agent 和脚本用的接口

所有接口都在 `/api/agent/*`，用同一个访问令牌鉴权（本机回环来源免令牌）：

| 接口 | 作用 |
|---|---|
| `GET /api/agent/schema` | 自描述：有哪些接口、要什么参数 |
| `GET /api/agent/brief?days=7` | 一份给 agent 看的「现在什么情况」 |
| `POST /api/agent/event` | 记一条会话事件（进 agent 总线） |
| `POST /api/agent/usage` | 记一笔 token 用量 |
| `GET /api/agent/read?ns=all&since=7d` | 读总线 |
| `GET /api/agent/link/scan` | 扫本机装了哪些 Agent、技能目录在哪 |
| `GET /api/agent/link/prompt` | 生成给那个 Agent 的接入说明书 |
| `POST /api/agent/link/install` | 把说明书写进它的技能目录（一次装好，长期生效） |
| `POST /api/agent/link/report` | Agent 回执：它把数据放哪了 |
| `POST /api/agent/link/import` | 按回执把它的账导进中台 |

还有 `GET /api/sys`（内存 / CPU / 负载 / 运行时长，轻量，适合轮询）和
`GET /api/status`（完整状态，较重）。

```bash
# 例子：本机查一句
curl -s http://127.0.0.1:8323/api/sys
# {"ok":true,"at":"...","mem":{"pct":62.1,"used_gb":10.0,"total_gb":16.0,...},...}
```

---

## 安全模型

| 面 | 做法 |
|---|---|
| **网络入口** | 只接受本机回环与 Tailscale 隧道来源；公网一律拒绝（端口转发也没用），局域网默认同样拒绝，可用 `allow_lan` 临时放开并按 IP 白名单收窄 |
| **鉴权** | 每次请求带访问令牌（`X-Token` 头或 `?k=`）；令牌在首启时随机生成，存状态目录，权限 600 |
| **传输** | 隧道侧走 HTTPS，证书首次启动自签（这就是手机第一次要点「继续」的原因）；本机明文端口只绑回环 |
| **数据** | 全部落本机。依赖里没有任何遥测库 |
| **出站请求** | 只有两类：访问你自己配的模型端点；查更新（一个 GET 到 GitHub API，启动后 90 秒一次、之后每 6 小时一次，也可在「关于」页手动触发） |
| **校验** | 更新包边下边算 sha256，与 GitHub 资产 digest 比对，不一致即丢弃 |

局域网默认被挡是刻意的：同网段的人可能替换掉你访问的页面。要临时给平板/电视用，
把 `config.json` 里的 `allow_lan` 设为 `true`，并把 `lan_allow_ips` 填成具体设备 IP。

---

## 常见问题

**端口被占用怎么办**
不用管。明文端口从 8323 起、HTTPS 主端口从 8321 起，各自探测，被占就往后顺延，
实际端口会写回配置并打印在控制台。

**没有模型能用吗**
能。文件浏览、上传下载、分享、回收站、网页存档都不依赖模型，只有对话降级。

**手机上能用吗**
能。装 Tailscale 后手机和电脑在同一 tailnet，用控制台打印的 `https://<机器名>...ts.net:8324/?k=...` 访问。
不建议把端口直接映射到公网 —— 网络守卫会拒绝，这是设计如此。

**缩略图在 Windows 上没有**
macOS 用系统自带 `sips` 缩图；Windows 没有这个命令，列表退回显示原图。已知降级，不影响功能。

**工作台页面（`/dash`）打不开**
`/dash` 读的是资料库里的 `_工作台/dash.html`，你自己放一个 HTML 进去就能用；
没放就返回 404 并告诉你应该放在哪。它不随安装包分发。

**会不会偷偷上传我的数据**
不会。没有账号体系也没有遥测。出站请求只有两类：你自己配的模型端点，
以及查更新（一个 GET `api.github.com`，只带版本号，不带任何使用数据）。
不想让它自动查，把 `config.json` 里的 `update.auto` 设为 `false` 即可。

**卸载干净要删什么**
删程序（拖到废纸篓 / 卸载程序），再按需删状态目录与资料库目录（见[数据放在哪](#数据放在哪也是更新不丢东西的原因)）。

---

## 已知限制

- **macOS 未公证**，首次需右键打开（见上方折叠说明）。
- **Windows 无缩略图**（缺 `sips` 的等价实现）。
- **暂无 Intel Mac 官方包**（CI 只构 arm64，可本地自建）。
- **暂无 Linux 包**，但代码本身没有 macOS 专属依赖（除缩略图一条），理论上可直接跑源码。
- **更新不是原子替换**，需要走一次系统安装流程；正在运行的程序不会被热替换。
- **无托盘图标**：关闭浏览器标签不会退出服务，要退出请从启动器窗口 `Ctrl+C`，或结束进程。

---

## 参与开发

欢迎 issue 与 PR。动手前请先看：

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) —— 为什么这么分层、几个关键取舍
- [`docs/SYNC.md`](docs/SYNC.md) —— 本仓库与上游开发目录的同步规则（哪些文件带补丁）
- [`CONTRIBUTING.md`](CONTRIBUTING.md) —— 提交前要跑什么

改代码请守住两条底线：

1. **路径只能从 `paths.py` 取**，任何模块里都不许拼 `~/Documents/...` 这种字符串 ——
   这是「别人装了也能跑」的前提，CI 会扫描硬编码家目录。
2. **不要在程序目录写运行时数据**，配置、日志、缓存一律进状态目录，用户文件进资料库。

---

## 许可

[MIT](LICENSE) © Moshan contributors
