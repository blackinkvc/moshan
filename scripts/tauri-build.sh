#!/bin/bash
# 本地构建 Tauri 桌面壳（macOS 用）。
#
# 为什么需要这层包装：本仓库可能放在 exFAT 移动硬盘上，而 cargo 的 target/ 目录里
# 会被 macOS 撒上 `._xxx` 元数据文件。tauri 的构建脚本会去读 target/ 下自己生成的
# permissions/*.toml，读到 AppleDouble 就报「stream did not contain valid UTF-8」，
# 报错位置看着像配置写错了，实际是文件系统问题。
# 所以只要检测到非 APFS/HFS，就把 target 指到本机盘。
#
# 用法：
#   bash scripts/tauri-build.sh              # 完整打包（dmg / nsis / deb，按当前系统）
#   bash scripts/tauri-build.sh --debug      # 开发构建
#   bash scripts/tauri-build.sh --no-bundle  # 只编二进制，不打安装包
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"

TRIPLE=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
[ -n "${TRIPLE}" ] || { echo "读不到 target triple：请先装 Rust"; exit 1; }

# 1) sidecar 缺了就补
EXT=""
case "${TRIPLE}" in *windows*) EXT=".exe" ;; esac
if [ ! -f "src-tauri/binaries/moshan-${TRIPLE}${EXT}" ]; then
  echo "==> 缺少 sidecar，先构建"
  bash scripts/stage-sidecar.sh
fi

# 2) 非 APFS/HFS 时把 target 挪到本机盘
if [ "$(uname -s)" = "Darwin" ]; then
  FSTYPE=$(stat -f %T "$ROOT" 2>/dev/null || echo unknown)
  case "${FSTYPE}" in
    apfs|hfs) : ;;
    *)
      export CARGO_TARGET_DIR="${MOSHAN_TAURI_TARGET:-$HOME/Library/Caches/moshan-tauri-target}"
      echo "==> 仓库位于 ${FSTYPE}，target 改到 ${CARGO_TARGET_DIR}"
      ;;
  esac
  # 仍可能有余留的 AppleDouble，清一遍再编
  find src-tauri -name "._*" -delete >/dev/null 2>&1 || true
fi

cd src-tauri
exec cargo tauri build "$@"
