# Windows 能力对齐：哪些用系统自带，哪些要额外装

写这份东西的原因很简单：**同一份 Moshan，在 macOS 上是「开箱全功能」，在 Windows 上
有几块能力是空的**。空的原因分两类，混在一起谈就会得出「Windows 支持差」的错误结论：

1. **路径找错了**（bug，不是能力缺失）—— 例如 TRAE 数据目录写死成
   `~/Library/Application Support/Trae CN`，Windows 上那三个源永远「没找到」。
2. **平台上真没有对应的系统能力**（能力缺失）—— 例如进程枚举：macOS 有
   `libproc.dylib` 一套 syscall，Windows 没有等价物，得换个办法。

这份文档只管第 2 类。第 1 类见 CHANGELOG 里各版本的「修」。

## 三条原则

1. **能用系统自带，就不引第三方。** Windows 10/11 自带 .NET、WMI、WinRT OCR、
   `tar.exe`、`curl.exe`、`certutil` —— 这些东西不需要用户装任何东西就能用。
2. **第三方依赖必须是「纯附加」**：缺了降级、有则增强，绝不能让服务起不来。
   `hostwork._sample_procs_win` 就是这个写法：没有 psutil 时卡片上写明原因，不是空白。
3. **mac 版不需要的，Windows 版也别为了「整齐」去装。** 按能力缺口装，不按平台对称装。

## 能力对照

| 能力 | macOS 用什么 | Windows 有没有现成的 | 结论 |
|---|---|---|---|
| 进程 / CPU / 内存 / IO 采样 | `libproc.dylib`（ctypes 直连，零依赖） | 没有等价的单次 syscall | **要装 `psutil`**（已落地，见下） |
| 整机内存、开机时长 | `host_statistics` mach 调用 | `GlobalMemoryStatusEx` + `GetTickCount64`（系统自带，`sysinfo.py` 已在用） | 不用装 |
| 磁盘剩余 | `shutil.disk_usage` | 同上，跨平台 | 不用装 |
| 缩略图 / 图片缩放 | `sips`（系统自带） | 没有 `sips`；.NET `System.Drawing` 在系统里 | 优先 .NET（零安装），Pillow 是备选 |
| 截图 / OCR | Vision 框架 + `bin/trae_ocr.swift` | WinRT `Windows.Media.Ocr`（系统自带，需装中文语言包） | 优先 WinRT，`tesseract` 只作兜底 |
| 视频封面 | AVFoundation / `qlmanage` | 没有 | 只能 `ffmpeg`，或干脆跳过 |
| 压缩 / 解包 | `tar`、`ditto` | `tar.exe`（libarchive，系统自带） | 不用装 |
| 自签证书 / 本地 HTTPS | `openssl`（系统自带 LibreSSL） | **没有 `openssl.exe`** | `certutil`（自带）或 Python `cryptography` |
| 文件全文搜索 | `mdfind`（Spotlight） | 没有等价 CLI | 遍历即可，别依赖 Everything |
| 窗口标题采样 | CoreGraphics（`CGWindowListCopyWindowInfo`） | UI Automation 能做，但代价高、还常被权限挡 | 目前不做 |

## 已经落地的第一步：`psutil`

Windows 上「本机工作」的进程卡、CPU 曲线原本是**全空**的 ——
`hostwork.sample_procs()` 整条链走 `libproc`，`_lp()` 一失败 `list_pids()` 就返回空。

- 新增 `hostwork._sample_procs_win()`：用 `psutil` 采，字段形状和 mac 那条完全一致
  （`cpu / mem / dread / dwrite / pids / age / path / owners`），前端一行都不用改。
- `psutil==7.2.2` 进 `requirements.txt`；`packages/moshan.spec` 的 `hiddenimports`
  显式加了它（函数内 import，静态分析抓不到）。
- 缺 psutil 时不崩：返回空列表 + `why` 字段，卡片上写明「缺 psutil」，
  而不是让人对着空白猜。

参考：<https://github.com/giampaolo/psutil>（11.3k stars，跨平台进程/系统库）

## PowerToys 怎么定位

<https://github.com/microsoft/PowerToys>（141k stars）

**不打包、不依赖、不检测。** 它是 GUI 工具集，脚本化调用不稳定，把它当功能前提
等于给用户加一道必装工序。但它有两件事正好补我们的缺口，值得**推荐用户自己装**：

- **Text Extractor（Win+Shift+T）**：就是 WinRT OCR 的现成壳。我们还没做 OCR 时，
  用户可以用它先把字抠出来 —— 比「功能不可用」体面。
- **Image Resizer**：批量缩图，正好顶 `sips` 的批量场景。

也就是说：PowerToys 是「用户侧的便利」，不是「程序侧的依赖」。README 里当一句提示写，
不写进依赖表。

## 后续可选清单（按性价比排序）

| 项目 | 换什么 | 要装东西吗 | 代价 / 风险 |
|---|---|---|---|
| 1. 缩略图 | .NET `System.Drawing`（PowerShell 子进程） | 不用 | 每次 ~200ms，必须加缓存；否则列表页会卡 |
| 2. OCR | WinRT `Windows.Media.Ocr` | 不用（需语言包） | 要写 WinRT 互操作，Python 侧得走 PowerShell 或 `winsdk` |
| 3. 视频封面 | `ffmpeg` | **要**（FFmpeg-Builds） | 单文件 ~80MB，得单独下载；<https://github.com/FFmpeg-Builds/FFmpeg-Builds> |
| 4. 自签证书 | `certutil` 或 `cryptography` | 不用 | 现在缺 `openssl.exe` 时 TLS 降级成 http，能修 |
| 5. 窗口标题 | UI Automation | 不用 | 权限问题多，收益低 |

**判断依据**：前两项是「零安装就能补上的缺口」，值得先做；第 3 项为了视频封面
多背 80MB 不划算，除非真的有视频墙需求；第 4 项影响的是 HTTPS，安全相关，可以做。

## 别做的事

- 不要把 `ffmpeg` / `tesseract` / `openssl` 塞进安装包 —— 安装包会从 15MB 涨到 100MB+，
  而三个都是「少数场景才用」的东西。
- 不要为了「Windows 也能用 sips 那套命令」去写一层 shell 兼容层 —— 命令能模拟，
  语义模拟不了（`sips -Z` 的等比缩放规则、EXIF 方向处理都不一样）。
- 不要在启动时探测 PowerToys / ffmpeg 并据此改 UI —— 用户随时会装/卸，
  探测结果只会变成「昨天能用的按钮今天灰了」。

---

## 调研补记（2026-11-24）

上一节是按「有没有等价能力」排的。这一节是查过实际体积与可行性之后的结论，
数字都来自 PyPI 与 GitHub 的实时查询，不是估的。

### ① 缩略图：三个方案，选 Pillow

| 方案 | 成本 | 机型/格式覆盖 | 问题 |
|---|---|---|---|
| **Pillow**（`python-pillow/Pillow` ★14k） | win wheel **7.0 MB** | jpg/png/webp/gif/bmp/tiff | 不认 HEIC（要再加 `pillow-heif` 6.3 MB） |
| .NET `System.Drawing`（PowerShell 调） | 系统自带 | 同上 | **每张图约 200 ms**，大头是启动 PowerShell 进程，不是解码 |
| Shell `IShellItemImageFactory` | 系统自带 | 连视频/PDF/Office 都能出图，走资源管理器缓存 | 要写 C# 互操作；拿到位图还得再编码成 PNG |

结论：**选 Pillow**。`sips` 在 mac 上之所以好用，是因为它一次进程干一批活；
换成 PowerShell 就是每张图起一次进程，列表页滚出 50 张就是 10 秒白屏。缓存
仍然要做（键 = 路径 + mtime + 尺寸），那是为了不重复解码，不是为了绕过启动开销。

顺带一处：`packages/moshan.spec` 的 `EXCLUDES` 里现在有 `"PIL"` —— 真要上 Pillow，
得先把这行去掉，否则打进去也会被排除掉。

### ② OCR：TRAE 侧栏不该用 OCR

这一条原来的假设是错的。查完 TRAE 的本地数据后确认：

- `ModularData/ai-agent/database.db` 是**加密库**（文件头不是 SQLite 的 `SQLite format 3`，
  是随机字节），读不出用量；`state.vscdb` 里也没有 token 字段；
- renderer 日志里能搜到 `token`，但全是 JWT / ByteCloudToken 这类鉴权串，**没有用量**。

所以「OCR 侧栏」不是偷懒，是当时唯一能拿到那个数字的本地途径。真要替换，
按性价比排：

| 方案 | 成本 | 准确性 |
|---|---|---|
| **UI Automation（`pywinauto` ★6.5k / `uiautomation` ★3.6k）** | 一个 pip 包 | 读控件文本，**精确**、无需识别 |
| 自述账 `/api/trae/note`（已建好） | 零 | 精确，但依赖 TRAE 主动调用 |
| WinRT `Windows.Media.Ocr` | 系统自带（要中文 OCR 语言包） | 图像识别，有错字风险 |
| RapidOCR（`RapidAI/RapidOCR` ★8.6k） | 14.2 MB + onnxruntime 16.3 MB | 同上 |

结论：**TRAE 侧栏改走 UIA 读控件，OCR 只留给真正的图像**（扫描件、图里的字）。
UIA 读的是 Electron 的 accessibility tree，Chromium 在检测到 UIA 客户端时会打开它。

### ③ 视频封面：`imageio-ffmpeg`，不是静态 ffmpeg

静态 ffmpeg.exe 约 80 MB。`imageio/imageio-ffmpeg`（★306）的 Windows wheel
**37.1 MB**，里面就是那个二进制，**pip 装得下来**，不用让用户自己去官网找。

结论：媒体墙本来就在用 ffmpeg（mac 上也是，`media.frame_thumb` 直接调 `FFMPEG`），
所以这不是「Windows 多一个依赖」，是「媒体墙的依赖在 Windows 上要单独铺」。
按需装：媒体墙不装，就不下这 37 MB。

Shell 缩略图对 **mkv** 无效（Windows 没有 mkv 的缩略图处理器，装了 Icaros 之类才有），
而 mkv 恰恰是这类媒体库的主格式 —— 所以视频封面没法靠系统 API 糊过去。

### ④ 证书：0.3.1 已落地，openssl.exe 不再是前提

`cryptography` 在进程内签证书，生成 0.03 s（openssl 子进程约 0.3 s）。0.3.1 起
Windows 上没有 openssl 也有 HTTPS；`vendor/openssl/` 只作为可选钩子留着。
