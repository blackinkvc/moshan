# 更新日志

格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [未发布]

### 计划
- Windows 上补缩略图（用 PowerShell + System.Drawing 替代 macOS 的 sips）
- 应用内托盘图标与「退出」菜单
- 真正的原子更新（versions/ + current 符号链接，替换正在运行的程序）

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

### 已知限制
- macOS 安装包未做 Apple 公证，首次打开需右键「打开」
- Windows 上缩略图功能降级为显示原图
- 无系统托盘，退出需在 Dock 上 Cmd+Q（macOS）或结束进程（Windows）
- 模型依赖 Ollama；自定义 OpenAI 兼容端点需手工填配置
