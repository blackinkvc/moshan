#!/bin/bash
# 把 Rust 服务端（moshan-server）按 target triple 命名放进 src-tauri/binaries/。
#
# 为什么名字必须带三元组：Tauri 构建时按 `binaries/moshan-<triple>` 找文件，
# 复制进 app 时去掉后缀。名字不对的表现是 `cargo tauri build` 报
# "binary not found for target ..."。
#
# 为什么不再走 PyInstaller：服务端已经全量 Rust 重写（220/220 条路由），
# 桌面壳里再塞一个 Python 解释器只会让安装包大几十兆、启动慢十几秒，
# 而且两边的行为还要各自维护。旧的 Python 打包路径保留成 `--python` 兜底，
# 只在需要对照旧实现时用。
#
# 用法：
#   bash scripts/stage-sidecar.sh              # 构建 Rust 服务 + 安装
#   bash scripts/stage-sidecar.sh --reuse      # 不重建，只把已有产物再拷一次
#   bash scripts/stage-sidecar.sh --python     # 兜底：打 Python onefile（旧路径）
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
MODE="rust"
[ "${1:-}" = "--reuse" ] && MODE="reuse"
[ "${1:-}" = "--python" ] && MODE="python"

TRIPLE=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
[ -n "${TRIPLE}" ] || { echo "读不到 target triple：请先安装 Rust（rustup）"; exit 1; }

OUT_DIR="${ROOT}/src-tauri/binaries"
mkdir -p "${OUT_DIR}"

EXT=""
case "${TRIPLE}" in *windows*) EXT=".exe" ;; esac
DEST="${OUT_DIR}/moshan-${TRIPLE}${EXT}"

if [ "${MODE}" = "python" ]; then
  echo "==> 兜底：PyInstaller onefile（旧路径）"
  python3 -m PyInstaller --noconfirm --clean \
    --distpath "${ROOT}/dist/sidecar" \
    --workpath "${ROOT}/build/sidecar" \
    packages/moshan-sidecar.spec
  SRC_BIN="${ROOT}/dist/sidecar/moshan"
  [ -f "${SRC_BIN}" ] || SRC_BIN="${ROOT}/dist/sidecar/moshan.exe"
  [ -f "${SRC_BIN}" ] || { echo "没找到 sidecar 产物（dist/sidecar/moshan）"; exit 1; }
else
  if [ "${MODE}" = "rust" ]; then
    echo "==> cargo build --release（全部泳道默认开启）"
    # 目标目录固定放 APFS（/tmp）：仓库所在的 exFAT 会撒 ._* 文件，
    # tauri 的权限生成读到非 UTF-8 会 panic。
    CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/moshan-rs-target}" \
      cargo build --release --manifest-path "${ROOT}/rust/Cargo.toml"
  fi
  SRC_BIN="${CARGO_TARGET_DIR:-/tmp/moshan-rs-target}/release/moshan-server"
  [ -f "${SRC_BIN}" ] || SRC_BIN="${ROOT}/rust/target/release/moshan-server"
  [ -f "${SRC_BIN}" ] || { echo "没找到 moshan-server 产物，先跑一次 bash scripts/stage-sidecar.sh"; exit 1; }
fi

# macOS 上必须先给内层二进制做 ad-hoc 签名：外层 app 签名会递归校验子组件，
# 内层没签或签坏，整包在用户机器上就是「已损坏，移到废纸篓」。
if [ "$(uname -s)" = "Darwin" ]; then
  codesign --force --sign - "${SRC_BIN}"
  codesign --verify --strict "${SRC_BIN}"
fi

cp "${SRC_BIN}" "${DEST}"
chmod +x "${DEST}"

# exFAT / 网络盘会在文件旁撒 ._xxx，codesign 会把它当子组件去签然后整包失败。
find "${OUT_DIR}" -name "._*" -delete >/dev/null 2>&1 || true

ls -lh "${DEST}"
echo "==> 已就位：src-tauri/binaries/$(basename "${DEST}")"
echo "    下一步：cd src-tauri && cargo tauri build（或 cargo tauri dev）"
