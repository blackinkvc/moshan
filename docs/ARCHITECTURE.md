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

```
检查  GET /repos/{repo}/releases/latest  → 取 tag_name 与 assets
比对  version.is_newer(latest, current)
下载  浏览器下载资产到 state/updates/，边下边算 sha256
校验  与 GitHub 提供的 digest 比对，不匹配立刻删除
安装  用系统默认程序打开 dmg / exe，交给系统安装流程覆盖程序目录
```

**为什么不做「自我热替换」**：macOS 上 `.app` 运行中无法安全替换自身，Windows 上
运行中的 exe 被锁。真要做必须「拉起新进程 → 旧进程自杀 → 失败回滚」三段式，
任一段出错就是一个半个坏软件。首版选择让系统安装流程负责替换 ——
失败也不会破坏现有安装。

要上真正的原子更新，前提是把程序拆成 `versions/1.4.2/` + `current` 符号链接，
那是下一个大版本的事。

**资产命名与匹配必须同步**：`updater._asset_pattern()` 按名字找包。
改产物命名时务必同步改它，否则客户端会认为「该版本没有可下载的安装包」。

## 六、跨平台差异

| 能力 | macOS | Windows | 处理方式 |
|---|---|---|---|
| 缩略图 | `sips` | 无 | Windows 直接返回 None，前端显示原图 |
| 通知 | 无 | 无 | 暂未实现 |
| 路径 | `/` | `\` | 一律用 `pathlib.Path`，不拼字符串 |

`sips` 是 macOS 系统自带，不引入 Pillow（省 5 MB 依赖）。
Windows 上缩略图降级是首版的已知限制，已写进 README 与 CHANGELOG。

## 七、已知限制

- macOS 包只做 ad-hoc 签名，未公证，首次打开需右键「打开」
- 无系统托盘，退出靠 Dock 右键或结束进程
- 更新为「下载 + 引导安装」，不是静默自动更新
- 只构建 Apple Silicon 包；Intel 需自行构建
