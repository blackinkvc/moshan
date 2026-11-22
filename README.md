# 墨山中台 · Moshan

> 把一台电脑变成你自己的中台：文件、传输、分享、对话，全在本机跑，数据不出门。

Moshan 是一个本地优先的个人服务中台。装好之后，它在你的电脑上起一个小服务，
用浏览器打开就是一个工作台：手机能传文件到电脑、电脑能把文件分享给朋友、
一个入口打开本机所有自建服务，还能让它读文件、抓网页、回答问题。

没有账号、没有云端、没有遥测。模型是可选项，不装模型也能正常用文件与传输。

---

## 下载安装

到 [Releases](../../releases) 页下载对应系统的安装包：

| 系统 | 文件 | 说明 |
|---|---|---|
| macOS（Apple 芯片） | `Moshan-*-macos-arm64.dmg` | M1/M2/M3/M4 |
| macOS（Intel） | `Moshan-*-macos-x64.dmg` | Intel Mac |
| Windows 10/11（64 位） | `Moshan-*-windows-x64-Setup.exe` | 一键安装 |

**macOS 首次打开会提示「无法验证开发者」** —— 安装包没有 Apple 付费开发者签名，
这是系统对未签名应用的固定行为，不是文件损坏。解决办法：在「访达」里
**右键点 Moshan → 打开**，或到「系统设置 → 隐私与安全性」点「仍要打开」。只需一次。

安装后会直接启动，浏览器自动打开新手引导。

---

## 首次使用：三步

1. **环境检查** —— 自动检测本机是否已有 Ollama，并显示程序目录、配置目录、资料库目录。
2. **准备模型**（可选）—— 检测到 Ollama 就一键拉取模型（约 2.4 GB，断点续传）；
   没有 Ollama 可以跳过，文件与传输功能完全不受影响。
3. **进入中台** —— 浏览器打开工作台。之后左下角「？」随时能找回这份引导。

---

## 数据在哪里（更新为什么不丢）

这是本项目最重要的一条设计：**程序、状态、资料库三者物理分离**。

| 目录 | macOS | Windows | 装什么 |
|---|---|---|---|
| 程序 | `/Applications/Moshan.app` | `C:\Program Files\Moshan` | 代码与运行时，更新时整体替换 |
| 状态 | `~/Library/Application Support/Moshan` | `%APPDATA%\Moshan` | 配置、令牌、证书、日志、下载缓存 |
| 资料库 | `~/Documents/Moshan` | `%USERPROFILE%\Documents\Moshan` | 你的文件、收件箱、回收站、工作台页面 |

安装包只覆盖「程序」。所以覆盖安装、升级、重装都不会动你的设置和文件 ——
这不是靠代码小心，是目录结构决定的。

想换机器：拷走状态目录与资料库目录即可，程序重装一次就行。

---

## 更新

中台内「设置 → 关于」里有「检查更新」，它会读 GitHub Release 的最新版本：

- 有新版本时显示版本号与更新说明，点一下下载（带 sha256 校验）
- 下载完打开安装包，按系统流程覆盖安装
- 装完重开，配置与文件原样

**校验失败会自动丢弃下载的文件**，不会让你装上一个损坏的包。

命令行也能检查：

```bash
# 源码运行时
python -c "import sys; sys.path.insert(0,'moshan'); import updater; print(updater.check())"
```

---

## 从源码运行

```bash
git clone https://github.com/pamphile0117/moshan.git
cd moshan
python3 -m venv .venv
.venv/bin/pip install -r requirements.txt
.venv/bin/python moshan/launcher.py
```

Windows 把 `.venv/bin/` 换成 `.venv\Scripts\`。

启动器会自动选端口（8321/8323 被占就往后顺延）、建目录、开浏览器。
调试时想关掉浏览器：

```bash
MOSHAN_NO_BROWSER=1 python moshan/launcher.py
```

想把数据和配置放到别处（测试用）：

```bash
MOSHAN_HOME=/tmp/moshan-test python moshan/launcher.py
```

---

## 开发与构建

```
moshan/                 业务代码（平铺模块，import 直接用名字）
  launcher.py         桌面入口：选端口、起服务、开浏览器
  server.py           服务本体（FastAPI）
  paths.py            三层目录约定 —— 改路径先看这里
  welcome.py          新手引导页
  bootstrap.py        环境探测与模型拉取
  updater.py          检查、下载、引导安装
packages/
  moshan.spec           两平台共用的 PyInstaller 配置
  macos/build.sh      产出 Moshan.app 与 dmg
  windows/build.ps1   产出 dist\Moshan 与安装程序
  windows/moshan.iss    Inno Setup 脚本
```

本地构建：

```bash
# macOS
bash packages/macos/build.sh

# Windows
powershell -ExecutionPolicy Bypass -File packages\windows\build.ps1
```

推一个 `v*` 标签，GitHub Actions 会自动在两个平台构建并发 Release：

```bash
git tag v0.2.0 && git push origin v0.2.0
```

---

## 常见问题

**端口被占用怎么办**
启动器会从 8321（HTTPS 主端口）与 8323（本机明文端口）往后自动顺延，
并把实际端口写回配置。所以不需要手动改。

**没有模型能用吗**
能。文件浏览、上传下载、分享、服务聚合都不依赖模型。只有「对话」会降级。

**手机上能用吗**
能，需要 Tailscale 之类的隧道（服务只接受本机与隧道来源，公网直连会被拒绝）。
控制台启动时会打印带令牌的访问地址。

**缩略图在 Windows 上没有**
macOS 用系统自带 `sips` 缩图；Windows 没有这个命令，列表会显示原图。
这是首版的已知降级，不影响功能。

**会不会偷偷上传我的数据**
不会。依赖里没有任何遥测库，出站请求只有两类：访问你自己配的模型端点，
以及你主动点「检查更新」时访问 GitHub API。

---

## 许可

MIT，见 [LICENSE](LICENSE)。
