# 本仓库协作规则（给本机所有 Agent 窗口）

本机只有一份工作目录，多个窗口共用。改代码前先认这三条。

## 1. 不切分支

`git checkout -b` / `git switch` 会把别的窗口未提交的改动一起带走。**只在 main 上工作。**

## 2. 做完的活推到自己的分支，不要推 main

```bash
git add -A && git commit -m "类型(范围): 说人话的说明"
git push origin HEAD:refs/heads/agent/trae
```

推完向用户报一句「已在 agent/<名字>，等合并」。

**不要** `git push origin main`，推不上去：`.git/hooks/pre-push` 会拒掉 main 和 tag，
只放行 `agent/*`。被拒时不要绕过（别 `--no-verify`、别删钩子），改推自己的分支。

## 3. main 的合并与发版归主力窗口（WorkBuddy）

合并、打 tag、推送 main 都由它做。别的窗口不要 `git tag`、不要动 `VERSION`。

## 提交前

`bash scripts/check.sh` 全绿再提交。跑不动就在提交说明里写清没跑的原因。
