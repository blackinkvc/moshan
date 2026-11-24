#!/bin/bash
# 让这个仓库用版本库里的钩子（新 clone、换机器、`.git` 被重建之后跑一次）
#
# 为什么不用 .git/hooks：那个目录不进版本库，clone 下来是空的 —— 闸门看着在，
# 其实没人守。放进 .githooks/ 并用 core.hooksPath 指过去，规则就跟着代码走。
#
#   bash scripts/install-hooks.sh
#
# 装完可以用 scripts/check.sh 第 8 节之外的方式自测，例如：
#   printf 'refs/heads/main %s refs/heads/agent/x 0000\n' "$(git rev-parse HEAD)" \
#     | .githooks/pre-push origin url ; echo "应该 exit=0"

set -u
cd "$(dirname "$0")/.." || exit 1

if [ ! -f .githooks/pre-push ]; then
  echo "找不到 .githooks/pre-push，仓库不完整？"
  exit 1
fi

git config core.hooksPath .githooks || exit 1
chmod +x .githooks/* 2>/dev/null

echo "钩子已启用：core.hooksPath=$(git config core.hooksPath)"
echo "  .githooks/pre-push："
echo "    - 任何分支的非快进推送 / 强推 / 删除  → 拒"
echo "    - main / master / tag                 → 需 MOSHAN_PUSH=workbuddy"
echo "    - agent/* 与 workbuddy/*              → 放行"
echo "  每次尝试记 ~/Documents/cuntu/push-guard.log"
