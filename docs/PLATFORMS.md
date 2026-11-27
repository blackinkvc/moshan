# 多端形态：桌面壳、网页版、Android

本文件说明「一套服务，多种外壳」的分工与构建方式。**业务只有一份**：
`rust/moshan-server`（Rust 重写，220/220 条路由）。外壳只决定「谁把窗口开出来、连到哪里」。
`moshan/` 里的 Python 实现保留为逐字节对拍的对照组，不再是运行路径。

---

## 一、五个目标

| 目标 | 形态 | 业务在哪 | 启动方式 |
|---|---|---|---|
| macOS | Tauri 壳（`.app` / `.dmg`） | 本机 sidecar | 双击 → 壳拉起 sidecar → 窗口导航到 `127.0.0.1:<local_port>` |
| Windows | Tauri 壳（NSIS 安装包） | 本机 sidecar | 同上 |
| Linux | Tauri 壳（`.deb` / `.AppImage`） | 本机 sidecar | 同上 |
| 网页版 | 无壳 | 本机服务 | 服务跑起来后浏览器直接开 `http://127.0.0.1:<local_port>` |
| Android | Tauri 壳（APK，**控制器**） | 远端桌面机 | 填地址/端口/令牌 → 加载对方页面 |

关键点：**桌面壳与网页版共用同一个服务进程**。壳启动时先探 `/api/ping`，已经有实例在跑
就直接复用，不再起第二个（否则两个实例会各写各的账，端口还会顺延到别的机器看不懂的号码）。

---

## 二、为什么 Android 是「控制器」

Android 跑不了桌面 sidecar：Linux 二进制依赖 glibc，Android 是 bionic，二者不兼容；
即便交叉编译成功，`launcher.py` 依赖的本机探测（libproc/psutil 采样、openssl 证书生成）
在 Android 沙箱里也不成立。三条路里选最省的一条：

| 方案 | 代价 | 结论 |
|---|---|---|
| Chaquopy 内嵌 CPython | 插件生态（`extplugins.py` 动态 import）基本作废，包体 +50 MB | 不做 |
| 核心重写成 Rust | 见 `MIGRATION-ASSESSMENT.md`：212 路由 + 双平台采样，数周且期间不可用 | 不做 |
| **远端控制器** | 需要桌面机可达（Tailscale 或 `allow_lan`） | **采用** |

因此 APK 装完先显示 `connect.html`：填地址、端口、令牌，探测可达后跳到
`http://<桌面机>:<端口>/dash?k=<令牌>`。手机侧一律明文 HTTP（安卓不信任自签证书），
所以桌面机的 `plain_port`（默认 8322，绑 Tailscale 地址）就是给它的入口。

---

## 三、sidecar 的命名规则（最容易踩的一条）

Tauri 的 `externalBin` 只认「带目标三元组后缀」的文件名，打包时自动去掉后缀：

```
src-tauri/binaries/moshan-aarch64-apple-darwin
src-tauri/binaries/moshan-x86_64-apple-darwin
src-tauri/binaries/moshan-x86_64-pc-windows-msvc.exe
src-tauri/binaries/moshan-x86_64-unknown-linux-gnu
```

名字不对的报错是 `binary not found for target ...`，看起来像配置问题，其实是文件名少了后缀。
生成方式一条命令（会顺带做 macOS 的 ad-hoc 签名，内层没签外层必坏）：

```bash
bash scripts/stage-sidecar.sh
```

**为什么 sidecar 现在是 Rust 单二进制**：`externalBin` 只复制一个可执行文件。服务端全量
Rust 之后，拷进去的就是 `moshan-server` 本身 —— 没有解释器、没有 `_internal/` 目录、
没有首启解包，冷启动是毫秒级。需要对照旧实现时才用 `bash scripts/stage-sidecar.sh --python`
打一次 PyInstaller onefile。

---

## 四、构建

### 本地（macOS，已实测）

```bash
bash scripts/tauri-build.sh            # 完整打包
bash scripts/tauri-build.sh --debug    # 开发构建
```

脚本做三件事：缺 sidecar 就补 → 检测文件系统（exFAT 时把 `target/` 挪到本机盘）→ 调
`cargo tauri build`。

**为什么必须检查文件系统**：仓库常放在 exFAT 移动硬盘上，macOS 会在 `target/`、`capabilities/`
里撒 `._xxx` 元数据文件。tauri 的构建脚本读到这些非 UTF-8 文件会报
`stream did not contain valid UTF-8`，位置指向权限生成，容易误判成配置写错。

### CI

`.github/workflows/tauri.yml`，三个桌面目标跑同一套步骤（装 CLI → 装系统依赖 →
打 sidecar → 服务端冒烟 → 构建 → 壳冒烟），Android 单独一个 job。

冒烟判据与 `build.yml` 一致：**进程起来不算数，要拿到 `/api/ping` 的 200**。
打 sidecar 那一步已经在 Linux/Windows 上 `cargo build` 过服务端，紧接着的「服务端冒烟」
直接跑这个二进制（`MOSHAN_HOME` 指到临时目录、`MOSHAN_RS_PORT=18400`、关调度），
把「能编译」推进到「能应答」。它和壳的冒烟分开：服务端挂了不该被壳的 GUI 问题掩盖。
Linux runner 没有显示服务，GUI 起不来，所以壳那一侧只验证两件事：`ldd` 无 `not found`
（壳的依赖完整）+ sidecar 能应答。GUI 由 macOS / Windows 两个 job 覆盖。

---

## 五、三平台差异表（服务端行为）

macOS 以外没有 `open` / `sips` / `unzip` 这些系统命令，服务端把这几类动作收敛到了两个
crate 级模块：`src/sysopen.rs`（打开路径/网址）与 `src/imaging.rs`（缩略图/格式转换），
解压打包走 `zip` crate。下表是**当前代码的真实行为**，不是计划：

| 动作 | macOS | Linux | Windows | 降级 / 说明 |
|---|---|---|---|---|
| 打开目录或文件（`/api/inbox/open`、`/api/ledger/open`、`/api/report/open`、`/api/reveal`） | `open <路径>` | `xdg-open <路径>` | `cmd /C start "" <路径>` | Linux 没装 `xdg-open` 时起进程就失败：`/api/ledger/open` 回 `{"ok":false,"error":…}`，`/api/inbox/open` 静默（与 Python 的返回形状一致） |
| 在文件管理器里**定位选中**某个文件（`sysopen::reveal_path`） | `open -R <路径>` | `xdg-open <父目录>` | `explorer /select,<路径>` | Linux 没有 `-R` 等价物，只能打开父目录；当前没有路由调用它（Python 也没有「定位到某文件」的接口） |
| 用默认浏览器打开网址（`MOSHAN_OPEN=1` 时自举打开 `/welcome`） | `open <url>` | `xdg-open <url>` | `cmd /C start "" <url>` | Windows 的 `start` 第一个引号参数是窗口标题，代码固定传空串，否则带空格的 URL 会被当标题吞掉 |
| 缩略图（`/api/thumb`、`/api/ledger/img`、`/api/browse?inline=1`、账本换图 `replace_image`） | `sips -Z`（与 Python 逐字节一致） | `magick` → `convert` → `ffmpeg` 依次探测 | 同 Linux（`magick.exe` / `convert.exe` / `ffmpeg.exe`） | 四个入口现在都收敛到 `storage::make_thumb` → `imaging`；三个外挂都没有就返回失败，调用方退回原图。**Python 在非 mac 上直接 `return None`**，这里是比 Python 好，不是必须 —— 判「Linux 有缩略图」前先确认机器上真装了这三个之一 |
| heic/heif → jpg（图集入库） | `sips -s format jpeg` | 跳过这张图 | 跳过这张图 | Python 的 `linkbox.SIPS` 也写死 `/usr/bin/sips`，失败路径相同；跳过不会让整个图集失败 |
| 打包下载（`/api/zip`） | 自写 zlib FFI，与 `zipfile` **逐字节一致** | `zip` crate | `zip` crate | 非 mac 的容器头（version made by 等）可能与 `zipfile` 不同；**条目名、顺序、内容一致**，解压结果相同 |
| 推送包解包（`/api/update/push`） | `zip` crate | `zip` crate | `zip` crate | 三平台同一份实现；zip-slip（`../`）与后缀白名单在解包前逐条判定，与 Python `pushupd._safe_name` 同口径 |
| 证书 / 日志 / 版本目录 | `~/Library/Application Support/Moshan/{certs,logs,versions}` | `$XDG_STATE_HOME/moshan/…`（缺省 `~/.local/state/moshan/…`） | `%APPDATA%\Moshan\…` | 由 `paths.rs` 与 `boot.rs` 建目录，位置与 `moshan/paths.py` 一致 |
| 进程存活 / 终止（心跳、灰测取消） | `kill(pid, 0)` / `SIGTERM` | 同 macOS | `OpenProcess` / `TerminateProcess` | 原来散在几个文件里直接调 `libc::kill`，Windows 编不过；现在统一在 `hostwork::platform::{proc_alive, terminate}` |
| 「这个文件能不能跑」（`which`、找 CLI） | `mode & 0o111` | 同 macOS | 扩展名在 `PATHEXT` 里 | `platform::is_exec_file`；`shutil.which()` 的对位实现 |
| `chmod +x`（生成的脚本、解包出来的二进制） | 真改权限位 | 同 macOS | 空实现 | `platform::add_exec_bits` |
| 负载（`os.getloadavg`） | 有 | 有 | **没有这个口径**，返回 `null` | `platform::load_avg()`。返回 `None` 而不是 `[0,0,0]`：0 看起来像「机器很闲」，与「没这个指标」是两件事 |
| 文件 inode（增量扫判「换文件了没」） | `st_ino` | `st_ino` | NTFS 文件索引号（`file_index()`） | `platform::file_inode`；`sim_windows` 在 mac 上取不到 Windows 的 `MetadataExt`，退化成 0（每次当新文件重扫，只影响缓存命中率） |
| 开机时刻 / 物理内存 / CPU 计数 | `sysctl` + `mach` | `/proc` + `sysconf` | `GetTickCount64` + `GetSystemInfo` | 都在 `hostwork::platform` 里按 `target_os` 分派；`sim_linux` / `sim_windows` 两个 feature 用 `all(unix, not(feature = "sim_windows"))` 这种写法把分支翻到对面，好在 mac 上过类型 |
| 数据根 | `~/Documents/Moshan` | 同 macOS | 同 macOS | 可用 `MOSHAN_HOME` 或 `config.json` 的 `library_dir` 覆盖；**位置与格式都不许动**，否则用户已有数据会「消失」 |
| 自启动 / 服务安装 | 不做 | 不做 | 不做 | 服务端不写 launchd plist、systemd unit 或计划任务；自启由安装包 / 桌面壳负责（mac `.app`、Windows NSIS、Linux `.deb`/AppImage 的桌面项）。代码里对 `LaunchAgents` 的读取只用于「某个外部插件有没有注册」，不是安装逻辑 |

---

## 六、已验证 / 未验证

| 项 | 状态 | 说明 |
|---|---|---|
| macOS 壳编译 | ✅ `cargo check` 通过 | 在 APFS 构建目录下 |
| macOS 壳端到端（起服务、导航、签名） | ⚠️ 待跑 | 需要完整 `tauri build` + 真实 sidecar |
| Windows 壳 | ⚠️ 仅配置正确 | 本机无法运行验证，由 CI 构建并冒烟 |
| Linux 壳 | ⚠️ 仅配置正确 | 本机无 Linux，CI 只验依赖与 sidecar |
| Android APK | ⚠️ 未构建过 | 本机无 Android SDK；CI 里 `tauri android init` 生成工程后再打 |
| 网页版 | ✅ 未受影响 | 与改造前完全一致，没有改任何路由或端口逻辑 |
| 三平台「打开」/ 缩略图 / 解压 / 进程 | ⚠️ 仅 macOS 实跑 | macOS 上逐个调过（`open` 打点、缩略图字节、zip 逐字节）；Linux/Windows 分支按源码实现，由 CI 冒烟（`cargo build` + `/api/ping` 200）覆盖，未在真机点过 UI |

### 本地怎么验「另外两个平台能编」

本机只有一个 target（Homebrew 的 rustc，没有 rustup），交叉 `cargo check --target x86_64-pc-windows-msvc`
也过不去：`ring` 的 build.rs 要为该 target 调 cc/nasm，本机没有 Windows 交叉 C 工具链。
所以「Windows/Linux 能编过」拆成两条本地可查的判据：

```bash
cd rust
# 1) 把平台分支翻到对面，让对面那份代码参与类型检查
CARGO_TARGET_DIR=/tmp/rs-lin cargo check --features sim_linux   --message-format short
CARGO_TARGET_DIR=/tmp/rs-win cargo check --features sim_windows --message-format short

# 2) 反向静态检查：每处平台专有 API 是否真落在「对面平台会被编掉」的 cfg 块里
python3 scripts/rust-portability.py            # 加 --ci 则命中即退出 1
```

第二条是必要的：`sim_windows` 在 mac 上 `unix` 恒真，裸 `#[cfg(unix)]` 里的
`std::os::unix` / `libc::` 照样能编过 —— 编译器看不出问题，静态检查能。当前基线是
`74 处平台专有 API，全部已隔离，未隔离 0`。真正的三平台编译在 CI 的 matrix 上用原生
`cargo build` 完成。

---

## 七、许可证落地

| 项 | 位置 |
|---|---|
| MIT 全文 | `LICENSE-MIT`（由原 `LICENSE` 改名，内容未改） |
| Apache-2.0 全文 | `LICENSE-APACHE`（取自 cargo registry 的标准文本，含 APPENDIX） |
| 包元数据 | `src-tauri/Cargo.toml` 的 `license = "MIT OR Apache-2.0"` |
| 安装包展示 | `tauri.conf.json` 的 `bundle.licenseFile`，并把两份全文打进 `bundle.resources` |
| 新增 Rust 源码 | 每个文件头两行：`SPDX-License-Identifier: MIT OR Apache-2.0` + 职责/不做 |
| README / 关于页 | 徽章与结尾改为 `MIT OR Apache-2.0`，链接分别指向两份全文 |
