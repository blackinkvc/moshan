# 这个仓库的 git 规矩（所有工具都读）

维护（改代码、提交、推送、发版）由 WorkBuddy 窗口做。你可以改文件，但 git 操作按下面来。

## 不要碰

- **不要 commit 到 main，不要 push main，不要 tag。** main 上了服务端 Ruleset（必须走 PR），
  直推会被 GitHub 拒；本地 `.git/hooks/pre-push` 也会先拦一次。
- **不要 `git push --force` / `-f` / `--force-with-lease`、`git reset --hard`、
  `git checkout -- .`、`git clean -fd`、`git add -A`。** 这个仓库有多个窗口并行写同一份
  工作目录，这些命令会抹掉别人还没推的提交，而且不会报错。
- 不要删钩子、不要 `--no-verify`、不要 `MOSHAN_FORCE=1`。

## 你要推的时候

```bash
git push origin HEAD:refs/heads/agent/<你的名字>
```

然后告诉用户「已推到 agent/xxx」。`feature/*`、`trae/*` 这类分支名会被闸门拦。

## 被拒的时候

只做一件事：

```bash
git fetch origin && git rebase origin/main
```

冲突了就停下报告，**不要「保留我的版本」**。

## 需要独立目录时

`bash ~/Documents/cuntu/new-worktree.sh <任务名>`（建 `/Volumes/AI仓库/worktrees/<任务名>`）。
不要在主目录 `git checkout -b`——切分支会把别人未提交的改动带走。

完整规则见仓库根目录 `AGENTS.md` 与 `CONTRIBUTING.md`。
