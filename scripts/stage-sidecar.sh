#!/bin/bash
# 把 Python 服务打成 Tauri sidecar，并按 target triple 命名放进 src-tauri/binaries/。
#
# 为什么名字必须带三元组：Tauri 构建时按 `binaries/moshan-<triple>` 找文件，
# 复制进 app 时去掉后缀。名字不对的表现是 `cargo tauri build` 报
# "binary not found for target ..."。
#
# 用法：
#   bash scripts/stage-sidecar.sh            # 构建 + 安装
#   bash scripts/stage-sidecar.sh --reuse    # 不重建，只把 dist/sidecar 里的产物再拷一次
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
REUSE=0
[ "${1:-}" = "--reuse" ] && REUSE=1

TRIPLE=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
[ -n "${TRIPLE}" ] || { echo "读不到 target triple：请先安装 Rust（rustup）"; exit 1; }

OUT_DIR="${ROOT}/src-tauri/binaries"
mkdir -p "${OUT_DIR}"

if [ "${REUSE}" -eq 0 ]; then
  echo "==> PyInstaller onefile（约 1–3 分钟）"
  python3 -m PyInstaller --noconfirm --clean \
    --distpath "${ROOT}/dist/sidecar" \
    --workpath "${ROOT}/build/sidecar" \
    packages/moshan-sidecar.spec
fi

SRC_BIN="${ROOT}/dist/sidecar/moshan"
[ -f "${SRC_BIN}" ] || SRC_BIN="${ROOT}/dist/sidecar/moshan.exe"
[ -f "${SRC_BIN}" ] || { echo "没找到 sidecar 产物（dist/sidecar/moshan）"; exit 1; }

# macOS 上必须先给内层二进制做 ad-hoc 签名：外层 app 签名会递归校验子组件，
# 内层没签或签坏，整包在用户机器上就是「已损坏，移到废纸篓」。
if [ "$(uname -s)" = "Darwin" ]; then
  codesign --force --sign - "${SRC_BIN}"
  codesign --verify --strict "${SRC_BIN}"
fi

EXT=""
case "${TRIPLE}" in *windows*) EXT=".exe" ;; esac
DEST="${OUT_DIR}/moshan-${TRIPLE}${EXT}"
cp "${SRC_BIN}" "${DEST}"
chmod +x "${DEST}"

# exFAT / 网络盘会在文件旁撒 ._xxx，codesign 会把它当子组件去签然后整包失败。
find "${OUT_DIR}" -name "._*" -delete >/dev/null 2>&1 || true

ls -lh "${DEST}"
echo "==> 已就位：src-tauri/binaries/$(basename "${DEST}")"
echo "    下一步：cd src-tauri && cargo tauri build（或 cargo tauri dev）"
