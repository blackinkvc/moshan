#!/bin/bash
# macOS 构建脚本：源码 -> Moshan.app -> dmg
#
# 用法：packages/macos/build.sh [--no-dmg]
# 前置：本机有 python3（3.10+）。脚本自己建独立构建环境，不碰你的运行环境。
#
# 为什么构建环境不在仓库目录里：
#   如果仓库放在 exFAT 移动硬盘上（很多人的选择），pip 会在「升级/替换包」时
#   把文件 rename 到系统临时目录，而 exFAT 与 APFS 之间不允许跨设备 rename，
#   直接报 EXDEV: cross-device link not permitted。所以构建环境一律放在本机
#   临时目录（APFS），只把最终产物拷回仓库 dist/。
set -eu

cd "$(dirname "$0")/../.."
ROOT="$(pwd)"
SRC="${ROOT}/moshan"
DIST="${ROOT}/dist"
# 构建工作区放到本机盘（TMPDIR 在 macOS 上是 /var/folders/...，属于 APFS）
WORK="${TMPDIR:-/tmp}/moshan-build"
BUILD_VENV="${WORK}/venv"
DIST_TMP="${WORK}/dist"
VERSION=$(sed -n 's/^VERSION = "\(.*\)"/\1/p' "${SRC}/version.py" | head -1)
[ -n "${VERSION}" ] || { echo "读不到版本号"; exit 1; }

ARCH=$(uname -m)
case "${ARCH}" in
  arm64) PLAT="macos-arm64" ;;
  x86_64) PLAT="macos-x64" ;;
  *) PLAT="macos-${ARCH}" ;;
esac

echo "==> 版本 ${VERSION} / ${PLAT}"
echo "==> 构建工作区 ${WORK}"

# PyPI 源：默认走清华镜像（国内直连官方源经常几分钟装不上一个包）。
# 在海外或想用官方源，构建前 export MOSHAN_PIP_INDEX=https://pypi.org/simple
PIP_INDEX="${MOSHAN_PIP_INDEX:-https://pypi.tuna.tsinghua.edu.cn/simple}"

rm -rf "${WORK}"
mkdir -p "${WORK}" "${DIST_TMP}"

# ---- 1. 构建环境 ----
echo "==> 创建独立构建环境"
python3 -m venv "${BUILD_VENV}"
"${BUILD_VENV}/bin/pip" install -q --upgrade pip -i "${PIP_INDEX}"
echo "==> 安装依赖（源：${PIP_INDEX}）"
"${BUILD_VENV}/bin/pip" install -q -i "${PIP_INDEX}" -r "${ROOT}/requirements.txt"
"${BUILD_VENV}/bin/pip" install -q -i "${PIP_INDEX}" "pyinstaller>=6.6"

# ---- 2. 打包 ----
echo "==> PyInstaller 打包"
"${BUILD_VENV}/bin/pyinstaller" --noconfirm --clean \
  --distpath "${DIST_TMP}" --workpath "${WORK}/build" "${ROOT}/packages/moshan.spec"

APP_TMP="${DIST_TMP}/Moshan.app"
[ -d "${APP_TMP}" ] || { echo "打包失败：没有生成 ${APP_TMP}"; exit 1; }

# ---- 3. ad-hoc 签名 ----
# 没有开发者证书时，ad-hoc 签名至少让包内文件签名一致，
# 用户首次打开仍需右键「打开」，这一点写进 README，不假装能绕过。
echo "==> ad-hoc 签名"
codesign --force --deep --sign - "${APP_TMP}" 2>/dev/null || echo "   （签名失败，不影响运行，但 Gatekeeper 提示会更硬）"

# ---- 4. 拷回仓库 ----
echo "==> 拷回 dist/"
mkdir -p "${DIST}"
# 用 ditto 而不是 rm+cp：
#   * 旧产物在 exFAT 上时，rm/cp 会撞「跨设备 rename 不允许」与「批量删除」两道坑；
#   * ditto 是 macOS 原生拷贝工具，会正确合并 .app 的结构、资源分支与签名。
# 同名文件会被覆盖，结构一致时不会残留旧内容。
ditto "${APP_TMP}" "${DIST}/Moshan.app"
echo "==> 产物体积"
du -sh "${DIST}/Moshan.app"

# ---- 5. dmg ----
if [ "${1:-}" != "--no-dmg" ]; then
  DMG_DIR="${WORK}"
  DMG="${DMG_DIR}/Moshan-${VERSION}-${PLAT}.dmg"
  echo "==> 生成 dmg"
  STAGE=$(mktemp -d)
  cp -R "${APP_TMP}" "${STAGE}/"
  ln -s /Applications "${STAGE}/Applications"
  hdiutil create -volname "Moshan ${VERSION}" -srcfolder "${STAGE}" \
    -ov -format UDZO "${DMG}" >/dev/null
  rm -rf "${STAGE}"
  cp "${DMG}" "${DIST}/"
  echo "==> dmg: ${DIST}/$(basename "${DMG}")  ($(du -h "${DMG}" | cut -f1))"
  shasum -a 256 "${DMG}" | awk '{print "    sha256:", $1}'
fi

echo "==> 完成（构建工作区保留在 ${WORK}，可删）"
