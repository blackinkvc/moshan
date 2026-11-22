#!/bin/bash
# 修复「仓库放在 ExFAT/U盘 上」时 git 报错的问题。
#
# 症状：
#   error: non-monotonic index .git/objects/pack/._pack-xxxx.idx
#   error: packfile .git/objects/pack/._pack-xxxx.pack index not opened
#   fatal: 无法读取对象库 / 任何 git 命令都失败
#
# 原因：ExFAT 不支持扩展属性，macOS 会为带 xattr 的文件生成 "._名字" 边车文件。
#       git 扫描 objects/pack/ 下的 *.idx 时会把 "._pack-xxx.idx" 也当成索引打开，
#       于是一整个仓库都读不动。
#
# 做法：删掉所有 "._*"，再把散装对象重新整理成 pack。
#       删完之后如果立刻又在 pack/ 里出现 "._pack-*"，说明本机一直在生成，
#       这时关掉自动 gc（gc.auto=0），让 git 保持散装对象 —— 散装目录里的 "._"
#       文件名不以 38 位哈希开头，git 会直接忽略，无害。
set -eu

cd "$(dirname "$0")/.."
ROOT="$(pwd)"
[ -d "${ROOT}/.git" ] || { echo "这里不是 git 仓库：${ROOT}"; exit 1; }

echo "==> 清理 .git 内的 AppleDouble 边车文件"
N1=$(find .git -name "._*" 2>/dev/null | wc -l | tr -d ' ')
find .git -name "._*" -delete 2>/dev/null || true
echo "    已删除 ${N1} 个"

echo "==> 清理工作区内的 AppleDouble（.gitignore 已忽略它们，删了更干净）"
N2=$(find . -path ./.git -prune -o -name "._*" -print 2>/dev/null | wc -l | tr -d ' ')
find . -path ./.git -prune -o -name "._*" -print0 2>/dev/null | xargs -0 rm -f 2>/dev/null || true
echo "    已删除 ${N2} 个"

echo "==> 校验对象库"
if git fsck --no-progress >/tmp/moshan-fsck.log 2>&1; then
  echo "    fsck 通过"
else
  echo "    fsck 报错："
  sed 's/^/      /' /tmp/moshan-fsck.log | head -10
  echo "    若仍是 pack 索引相关的报错，删掉 pack 目录后重试："
  echo "      rm -rf .git/objects/pack/* && git fsck"
  exit 1
fi

if [ -d .git/objects/pack ] && ls .git/objects/pack/._pack-*.idx >/dev/null 2>&1; then
  echo "==> 边车文件在 pack 目录里立刻复发，关闭自动 gc 改用散装对象"
  git config gc.auto 0
  git config gc.autoPackLimit 0
  git repack -a -d 2>/dev/null || true
  rm -f .git/objects/pack/._* 2>/dev/null || true
  echo "    已设置 gc.auto=0"
fi

echo "==> 完成。当前状态："
git status -sb | head -3
