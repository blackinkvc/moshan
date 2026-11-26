# sidecar 产物目录

这里放的是给 Tauri 的 `externalBin` 用的单文件 sidecar，命名必须是
`moshan-<target-triple>`（Windows 再加 `.exe`）：

```
moshan-aarch64-apple-darwin
moshan-x86_64-apple-darwin
moshan-x86_64-pc-windows-msvc.exe
moshan-x86_64-unknown-linux-gnu
```

**不要提交这些二进制**：单个 40–80 MB，且能用 `bash scripts/stage-sidecar.sh`
一条命令重建。CI 里由构建步骤现场生成，本地开发缺了它 Rust 侧会退回直接跑
`moshan/launcher.py`（见 `src-tauri/src/sidecar.rs` 的 `program()`）。

已验证的获取方式：

```bash
bash scripts/stage-sidecar.sh          # PyInstaller onefile + 按三元组改名 + ad-hoc 签名
```
