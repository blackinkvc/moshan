#!/bin/bash
# 把「开发目录」的改动同步进交付仓库的 moshan/。
#
# 两类文件，两套处理方式：
#
#   A. 逐字镜像（绝大多数）
#      交付版就是开发版的原样拷贝。内容不同就直接覆盖。
#
#   B. 带补丁（PATCHED 列表里的少数文件）
#      交付版 = 开发版 + 跨平台移植补丁（paths 模块、可配置路径、main() 函数化、
#      平台判断）。直接覆盖会把补丁冲掉，打包版立刻起不来。
#      这类文件用三方合并：
#          基线 = .sync-baseline/<文件>   上次同步时开发目录的样子
#          我方 = moshan/<文件>           基线 + 补丁 + 已合入的改动
#          对方 = 开发目录/<文件>         你这次的新改动
#      结果 = 你的新改动叠加在补丁之上；只有两边改到同一行才报冲突。
#
# 用法：
#   bash scripts/sync-from-dev.sh                 增量同步
#   bash scripts/sync-from-dev.sh --dry-run       只预演
#   bash scripts/sync-from-dev.sh --init-baseline 把当前开发版记为补丁基线
#
# 环境变量：
#   MOSHAN_DEV  开发目录，默认本机 trae 工作区里的 pocket/
#
# 什么时候要跑 --init-baseline：
#   在 PATCHED 里新增文件时；或你确认「开发版此刻已等同于上次同步的状态」
#   （例如刚同步完没再改过）时想重建基线。
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
SRC="${MOSHAN_DEV:-$HOME/.trae-cn/assistant/2026-11-11/local-ai-workspace/work/local-ai-workspace/pocket}"
MIRROR="${ROOT}/moshan"
BASE="${ROOT}/.sync-baseline"
CONFLICT="${ROOT}/.sync-conflicts"

# 交付版里带了手工移植补丁的文件。新增这类文件时必须加进这里，
# 否则会被逐字覆盖，补丁丢失。
PATCHED="server.py ocr_runner.py plan_mcp.py"

MODE="sync"
case "${1:-}" in
  --dry-run) MODE="dry" ;;
  --init-baseline) MODE="init" ;;
  "") ;;
  *) echo "未知参数：$1"; exit 2 ;;
esac

[ -d "${SRC}" ]    || { echo "找不到开发目录：${SRC}"; echo "用 MOSHAN_DEV=/path/to/pocket 指定"; exit 1; }
[ -d "${MIRROR}" ] || { echo "找不到交付目录：${MIRROR}"; exit 1; }
mkdir -p "${BASE}"

is_patched() {
  for p in ${PATCHED}; do [ "$1" = "${p}" ] && return 0; done
  return 1
}

# ---- 收集分发资产：只有 .py 与 ui.html ----
LIST=$(mktemp)
find "${SRC}" -maxdepth 1 -type f \
  \( -name '*.py' -o -name 'ui.html' \) \
  ! -name '*.bak-*' ! -name '*.moved-*' ! -name '._*' \
  | sort > "${LIST}"
TOTAL=$(wc -l < "${LIST}" | tr -d ' ')

echo "开发目录：${SRC}"
echo "交付目录：${MIRROR}"
echo "候选文件：${TOTAL} 个（其中带补丁 $(echo ${PATCHED} | wc -w | tr -d ' ') 个）"

# ---- 只重建补丁文件的基线 ----
if [ "${MODE}" = "init" ]; then
  N=0
  for p in ${PATCHED}; do
    [ -f "${SRC}/${p}" ] || { echo "  开发目录缺 ${p}，跳过"; continue; }
    cp "${SRC}/${p}" "${BASE}/${p}"
    N=$((N + 1))
  done
  rm -f "${LIST}"
  echo "已为 ${N} 个补丁文件建立基线。"
  echo "前提：此刻交付版相对开发版的差异，只有移植补丁本身。"
  exit 0
fi

OKPATCHED=$(mktemp)   # 合并成功、可推进基线的补丁文件
echo
echo "逐个比对："
CHANGED=0; SAME=0; NEW=0; PATCHONLY=0; MERGEDF=0; NOBASE=0; CONF=0

while IFS= read -r f; do
  name=$(basename "${f}")
  dst="${MIRROR}/${name}"

  if is_patched "${name}"; then
    base="${BASE}/${name}"
    if [ ! -f "${base}" ]; then
      NOBASE=$((NOBASE + 1))
      printf "  [缺基线] %-18s 这是补丁文件但没基线，需先 --init-baseline 或人工处理\n" "${name}"
      continue
    fi
    if cmp -s "${f}" "${base}"; then
      SAME=$((SAME + 1))   # 开发版自上次同步未变
      echo "${name}" >> "${OKPATCHED}"
      continue
    fi
    tmp=$(mktemp)
    if git merge-file -p --quiet "${dst}" "${base}" "${f}" > "${tmp}" 2>/dev/null; then
      if cmp -s "${tmp}" "${dst}"; then
        PATCHONLY=$((PATCHONLY + 1))
        printf "  [补丁不变] %-18s 开发版有改动，但落点与补丁无交集\n" "${name}"
      else
        MERGEDF=$((MERGEDF + 1))
        printf "  [合并] %-18s 新改动已叠加到补丁之上\n" "${name}"
        [ "${MODE}" = "dry" ] || cp "${tmp}" "${dst}"
      fi
      echo "${name}" >> "${OKPATCHED}"
    else
      CONF=$((CONF + 1))
      mkdir -p "${CONFLICT}"
      cp "${tmp}" "${CONFLICT}/${name}.merged"
      printf "  [冲突] %-18s 两边改到同一处，结果见 .sync-conflicts/%s.merged\n" "${name}" "${name}"
    fi
    rm -f "${tmp}"
    continue
  fi

  # ---- 逐字镜像 ----
  if [ ! -f "${dst}" ]; then
    NEW=$((NEW + 1))
    printf "  [新增] %-18s\n" "${name}"
    [ "${MODE}" = "dry" ] || cp "${f}" "${dst}"
  elif cmp -s "${f}" "${dst}"; then
    SAME=$((SAME + 1))
  else
    CHANGED=$((CHANGED + 1))
    printf "  [更新] %-18s\n" "${name}"
    [ "${MODE}" = "dry" ] || cp "${f}" "${dst}"
  fi
done < "${LIST}"

rm -f "${LIST}"

echo
echo "小结：更新 ${CHANGED} / 合并 ${MERGEDF} / 新增 ${NEW} / 补丁不变 ${PATCHONLY} / 相同 ${SAME} / 缺基线 ${NOBASE} / 冲突 ${CONF}"

if [ "${MODE}" = "dry" ]; then
  rm -f "${OKPATCHED}"
  echo "（预演模式，未改动任何文件）"
  exit 0
fi

# ---- 推进补丁基线：只对合并成功的推进，冲突的留到下次 ----
BASED=0
while IFS= read -r name; do
  [ -f "${SRC}/${name}" ] || continue
  cp "${SRC}/${name}" "${BASE}/${name}"
  BASED=$((BASED + 1))
done < "${OKPATCHED}"
rm -f "${OKPATCHED}"
[ "${BASED}" -gt 0 ] && echo "补丁基线已推进 ${BASED} 个文件"

if [ "${CHANGED}" -eq 0 ] && [ "${MERGEDF}" -eq 0 ] && [ "${NEW}" -eq 0 ] && [ "${CONF}" -eq 0 ]; then
  echo "开发目录没有需要同步的改动。"
  exit 0
fi

echo
echo "==> 语法检查"
if python3 -m compileall -q -x '(^|/)\._' "${MIRROR}" >/dev/null 2>&1; then
  echo "    通过"
else
  echo "    有语法错误，先修好再提交："
  python3 -m compileall -q -x '(^|/)\._' "${MIRROR}" 2>&1 | head -10
  exit 1
fi
find "${MIRROR}" -name "__pycache__" -type d -exec rm -rf {} + 2>/dev/null || true

echo
echo "==> 交付目录的改动"
git -C "${ROOT}" status --short -- moshan | head -30

if [ "${CONF}" -gt 0 ]; then
  echo
  echo "有 ${CONF} 个文件冲突，未自动合并。处理完再跑一次本脚本推进基线。"
  exit 1
fi

echo
echo "同步完成。后续："
echo "  git diff moshan/                      看具体改了什么"
echo "  bash scripts/check.sh                 自检"
echo "  需发版时 bash scripts/bump-version.sh X.Y.Z"
echo "  git add -A && git commit -m \"feat: ...\""
