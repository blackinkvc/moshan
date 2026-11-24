# 这个仓库由 WorkBuddy 维护

维护工作（改代码、提交、推送、打 tag、发版）统一由 WorkBuddy 窗口做，其它工具/窗口
只是使用者。所以：

- **不要 `git push`，不要 `git tag`。** `.git/hooks/pre-push` 会拦住不带
  `MOSHAN_PUSH=workbuddy` 的 `main` / tag 推送；被拦时不要绕过（别删钩子、别 `--no-verify`）。
- **不要 `git reset --hard`、不要手跑 `git gc`**（仓库在 ExFAT 卷上，见 `CONTRIBUTING.md`）。
- 需要动代码时，改完把结论交给用户，由 WorkBuddy 统一提交与发布。

细节见仓库根目录 `CONTRIBUTING.md`「推送与维护」。
