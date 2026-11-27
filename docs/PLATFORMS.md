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
打 sidecar → 构建 → 冒烟），Android 单独一个 job。

冒烟判据与 `build.yml` 一致：**进程起来不算数，要拿到 `/api/ping` 的 200**。
Linux runner 没有显示服务，GUI 起不来，所以只验证两件事：`ldd` 无 `not found`（壳的依赖完整）
+ sidecar 能应答。GUI 由 macOS / Windows 两个 job 覆盖。

---

## 五、已验证 / 未验证

| 项 | 状态 | 说明 |
|---|---|---|
| macOS 壳编译 | ✅ `cargo check` 通过 | 在 APFS 构建目录下 |
| macOS 壳端到端（起服务、导航、签名） | ⚠️ 待跑 | 需要完整 `tauri build` + 真实 sidecar |
| Windows 壳 | ⚠️ 仅配置正确 | 本机无法运行验证，由 CI 构建并冒烟 |
| Linux 壳 | ⚠️ 仅配置正确 | 本机无 Linux，CI 只验依赖与 sidecar |
| Android APK | ⚠️ 未构建过 | 本机无 Android SDK；CI 里 `tauri android init` 生成工程后再打 |
| 网页版 | ✅ 未受影响 | 与改造前完全一致，没有改任何路由或端口逻辑 |

---

## 六、许可证落地

| 项 | 位置 |
|---|---|
| MIT 全文 | `LICENSE-MIT`（由原 `LICENSE` 改名，内容未改） |
| Apache-2.0 全文 | `LICENSE-APACHE`（取自 cargo registry 的标准文本，含 APPENDIX） |
| 包元数据 | `src-tauri/Cargo.toml` 的 `license = "MIT OR Apache-2.0"` |
| 安装包展示 | `tauri.conf.json` 的 `bundle.licenseFile`，并把两份全文打进 `bundle.resources` |
| 新增 Rust 源码 | 每个文件头两行：`SPDX-License-Identifier: MIT OR Apache-2.0` + 职责/不做 |
| README / 关于页 | 徽章与结尾改为 `MIT OR Apache-2.0`，链接分别指向两份全文 |
