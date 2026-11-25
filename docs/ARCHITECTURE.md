# 架构说明

这份文档解释几个「看起来可以更简单、但故意没那样做」的决定。改代码前值得读一遍。

## 一、三层目录

```
程序目录  app_dir()     只读，更新时整体替换
状态目录  state_dir()   配置、令牌、证书、日志、更新缓存
资料库    library_dir() 用户的文件、收件箱、回收站、工作台页面
```

| 平台 | 状态目录 | 资料库 |
|---|---|---|
| macOS | `~/Library/Application Support/Moshan` | `~/Documents/Moshan` |
| Windows | `%APPDATA%\Moshan` | `%USERPROFILE%\Documents\Moshan` |
| 其他 | `$XDG_STATE_HOME/moshan` | `~/Documents/Moshan` |

**为什么一定要分开**：安装包只能覆盖程序目录。只要用户的配置与文件不在程序目录里，
「覆盖安装」在物理上就不可能碰到它们 —— 不需要在代码里写一堆「保留用户数据」的判断，
也就不存在漏判。这是本项目对「更新不丢数据」的实现方式。

环境变量 `MOSHAN_HOME` 可以把状态与资料库一起指到别处，用于测试与多开。

## 二、端口策略

| 用途 | 默认 | 绑定地址 | 说明 |
|---|---|---|---|
| HTTPS 主端口 | 8321 | `0.0.0.0` | 手机经隧道访问 |
| Tailscale 明文 | 8322 | 隧道 IP | 安卓不信任自签证书 |
| 本机明文 | 8323 | `127.0.0.1` | 桌面浏览器用，避开自签证书警告 |

启动器会先用 `bind + listen` 实测端口是否真能独占，被占就顺延，并把结果写回配置。

**这里有个坑**：探测端口绝对不能设 `SO_REUSEADDR`。设了之后，端口即使已被别的进程监听，
`bind` 也会返回成功，于是我们误判为空闲、把服务绑到别人的端口上（实测踩过：
主端口误选 8323，和另一个正在运行的服务撞车）。要的是「能不能独占」，不是「能不能重用」。

## 三、启动流程

```
launcher.main()
  ├─ paths.ensure_layout()        建目录（幂等）
  ├─ _redirect_stdio()            桌面形态把输出落 logs/moshan.log
  ├─ import server                延迟导入：server 导入时会读配置、建 app
  ├─ pick_port() × 2              本机明文端口 + HTTPS 主端口，写回配置
  ├─ 起后台线程：等端口通了再开浏览器
  └─ server.main()                同进程跑 uvicorn（阻塞）
```

**为什么同进程而不是 subprocess**：PyInstaller 打包后没有独立的 python 解释器，
再起一个进程去跑 `server.py` 是不可行的。所以 `server.py` 的启动逻辑必须是函数
（`main()`）而不是 `if __name__ == "__main__"` 块 —— 这正是当初改造它的原因。

浏览器打开 `/welcome`（首次）还是 `/?k=token`（之后），由状态目录里的 `welcome.done` 决定。

## 四、新手引导

`/welcome` 免 token，但仍受 `guard_network` 限制：只接受本机与隧道来源，
公网与局域网默认拒绝。所以免 token 不等于对外公开。

模型获取走 Ollama 而不是自建 GGUF 下载器，理由：

1. Ollama 自带断点续传、分片校验、多源回退；
2. 模型存在 Ollama 自己的目录，卸载重装 Moshan 不会重复下载 2.4 GB；
3. 用户已装 Ollama 时直接复用，不产生第二份副本。

代价是依赖 Ollama。所以引导页允许「跳过」，且明确告诉用户：不装模型时
文件、传输、分享照常可用，只有「对话」降级。

## 五、更新流程

两条路，按「这台机器能不能连上 GitHub」分：

**A. 自己查 GitHub（默认）**

```
检查  GET /repos/{repo}/releases/latest  → 取 tag_name 与 assets
比对  version.is_newer(latest, current)
下载  浏览器下载资产到 state/updates/，边下边算 sha256
校验  与 GitHub 提供的 digest 比对，不匹配立刻删除
安装  用系统默认程序打开 dmg / exe，交给系统安装流程覆盖程序目录
```

**B. 主机推送（`update.mode = push`，给连不上 github.com 的机器）**

```
分片   主机把程序目录打成 zip，按 1.5 MB 切片（虫巢 put 单请求 4 MB 上限）
投递   --http 直连目标机 / --fleet 走虫巢（目标机 proc_start 回环 POST）
校验   POST /api/update/push 先算 sha256，不符整包丢弃，不落半成品
解包   只认程序文件后缀白名单，拒绝绝对路径与 ..，不写白名单外的任何文件
备份   cp 当前程序目录 → state/updates/pkgs/<ver>-bak-<时间戳>
替换   frozen 安装落 pkgs/<ver>；源码部署原地替换
重启   写 restart.ps1 + restart.cmd，**把 USERPROFILE / APPDATA / TEMP 等显式写进去**
回退   POST /api/update/rollback 把备份拷回去
```

重启链为什么要显式带环境变量：以 SYSTEM 身份（计划任务）拉起的新进程默认没有
`USERPROFILE`，于是 `Path.home()` 落到 `systemprofile`，服务会把状态目录认成
「另一个用户的家」，表现为「重启后变成第一次启动、配置和令牌全丢」。
这个坑只有在真机上重启过一次才看得见，源码态本机复现不了。

**为什么 GitHub 那条路不做热替换**：macOS 上 `.app` 运行中无法安全替换自身，
Windows 上运行中的 exe 被锁。真要做必须「拉起新进程 → 旧进程自杀 → 失败回滚」三段式，
任一段出错就是一个半个坏软件。所以走 GitHub 的机器交给系统安装流程 ——
失败也不会破坏现有安装。

主机推送那条路没法交给安装器（源码部署没有安装包），所以它把上面那套风险逐条兜住：
备份、校验、白名单解包、环境变量钉死、rollback。它仍然不是「原子」更新
（替换过程中进程是停的，替换中途断电会留下半个目录）—— 真正的原子更新仍需
`versions/1.4.2/` + `current` 符号链接，那是下一个大版本的事。

**资产命名与匹配必须同步**：`updater._asset_pattern()` 按名字找包。
改产物命名时务必同步改它，否则客户端会认为「该版本没有可下载的安装包」。

## 六、跨平台差异

| 能力 | macOS | Windows | 处理方式 |
|---|---|---|---|
| 缩略图 | `sips` | 无 | Windows 直接返回 None，前端显示原图 |
| 通知 | 无 | 无 | 暂未实现 |
| 路径 | `/` | `\` | 一律用 `pathlib.Path`，不拼字符串 |
| TRAE 数据目录 | `~/Library/Application Support/Trae CN` | `%APPDATA%\Trae CN` | `trae_work._trae_cn_dir()` 按平台取，内部结构一致 |
| 侧栏 OCR（TRAE 任务名） | Vision 框架（`bin/trae_ocr.swift`） | 无 | Windows 上该源为空，逐轮改由「会话记忆」独占 |
| Cherry Studio 账本 | `~/Library/Application Support/CherryStudio` | 未接 | Windows 上取不到就采空，不报错 |

`sips` 是 macOS 系统自带，不引入 Pillow（省 5 MB 依赖）。
Windows 上缩略图降级是首版的已知限制，已写进 README 与 CHANGELOG。

## 八、四档端与版本号

虫巢把设备分四档，靠颜色区分：**橙 owner 主端 / 紫 control 自管 / 蓝 limited 受限 /
绿 read 只读**（档位表在 `16-虫巢思维/fleet.py`，能力位怎么给在那里定）。

规则一句话：**四档共用同一个项目版本号，功能和权限在运行时按档位区分。**

- 一份代码、一个 `VERSION`。**不按档位分叉版本号** —— 不出现「主端 0.3.7、
  受限端 0.3.6」这种局面。看到两台版本号不同，那是「没升齐」，不是「功能不同」。
- 差异只体现在「看不看得见 / 能不能动」，不体现在「装的是哪一版」：
  - 权限由档位表决定（写文件 `put` 只给橙和紫，蓝档只读，绿档只挂牌）；
  - 功能由各自中台的模块开关决定，同一版本换台机器就按那台的权限走。
- 所以升级永远整批升；对某台机器做灰度，靠「先不升它」实现，不靠改版本号。

## 九、已知限制

- macOS 包只做 ad-hoc 签名，未公证，首次打开需右键「打开」
- 无系统托盘，退出靠 Dock 右键或结束进程
- 更新为「下载 + 引导安装」或「主机推送 + 重启」，都不是静默自动更新
- 主机推送是原地替换，不是原子更新：替换途中断电会留下半个程序目录（有备份可回退）
- 只构建 Apple Silicon 包；Intel 需自行构建
