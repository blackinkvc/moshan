#!/bin/bash
# 升版本号。版本号散落在两处，手改必错一次，用这个脚本一次改全。
#
# 用法：bash scripts/bump-version.sh 0.2.0
#
# 会改：
#   VERSION                   根目录的纯版本号文件（CI 与打包脚本读它）
#   moshan/version.py         VERSION = "0.2.0"（Python 线运行时读它）
#   src-tauri/tauri.conf.json 桌面壳的 version（Tauri 用它命名安装包）
#   src-tauri/Cargo.toml      桌面壳 crate 的 version
#   rust/Cargo.toml           workspace.package.version —— Rust 服务端经
#                             `env!("CARGO_PKG_VERSION")` 读它，/api/about 与
#                             更新器报的版本就是这个，漏改=自称上一版
#   CHANGELOG.md              插入 ## [0.2.0] - YYYY-MM-DD 段落（若尚不存在）
#
# 不会做：打 tag、提交。这两步由你确认内容后手动执行，避免误发版。
set -eu

cd "$(dirname "$0")/.."
NEW="${1:-}"
[ -n "${NEW}" ] || { echo "用法：bash scripts/bump-version.sh 0.2.0"; exit 2; }

# 只接受 主.次.补丁 形式，可选 -beta.N 这类后缀
case "${NEW}" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "版本号格式不对：${NEW}（要 0.2.0 这种）"; exit 2 ;;
esac
case "${NEW}" in
  *[!0-9A-Za-z.-]*) echo "版本号里有非法字符：${NEW}"; exit 2 ;;
esac

OLD=$(sed -n 's/^VERSION = "\(.*\)"/\1/p' moshan/version.py | head -1)
[ -n "${OLD}" ] || { echo "读不到 moshan/version.py 里的 VERSION"; exit 1; }
if [ "${OLD}" = "${NEW}" ]; then
  echo "当前已经是 ${NEW}，无需修改"
  exit 0
fi

echo "==> ${OLD} -> ${NEW}"

# VERSION 文件
printf '%s\n' "${NEW}" > VERSION

# version.py，只替换第一处 VERSION 赋值
python3 - "${NEW}" <<'EOF'
import io, re, sys
new = sys.argv[1]
p = "moshan/version.py"
s = io.open(p, encoding="utf-8").read()
s2, n = re.subn(r'(?m)^VERSION = "[^"]*"', 'VERSION = "%s"' % new, s, count=1)
if n != 1:
    sys.exit("version.py 里没找到 VERSION 赋值行")
io.open(p, "w", encoding="utf-8").write(s2)
EOF

# Tauri 壳与 Rust 工作区。这三处原先只有 check.sh 会查，bump 脚本不管 ——
# 每次发版都得手改，漏一次就是「Rust 版自称上一版」，更新器会一直提示更新。
python3 - "${NEW}" <<'EOF'
import io, re, sys
new = sys.argv[1]
targets = [
    ("src-tauri/tauri.conf.json", r'"version"\s*:\s*"[^"]*"', '"version": "%s"' % new),
    ("src-tauri/Cargo.toml", r'(?m)^version = "[^"]*"', 'version = "%s"' % new),
    ("rust/Cargo.toml", r'(?m)^version = "[^"]*"', 'version = "%s"' % new),
]
for path, pat, rep in targets:
    try:
        s = io.open(path, encoding="utf-8").read()
    except FileNotFoundError:
        continue          # 只跑 Python 线时没有 src-tauri/rust，跳过即可
    s2, n = re.subn(pat, rep, s, count=1)
    if n != 1:
        sys.exit("%s 里没找到版本号行" % path)
    io.open(path, "w", encoding="utf-8").write(s2)
EOF

# CHANGELOG，没有该版本段落就插一条
TODAY=$(date +%F)
if grep -q "^## \[${NEW}\]" CHANGELOG.md; then
  echo "    CHANGELOG 已有 ${NEW} 段落，跳过"
else
  python3 - "${NEW}" "${TODAY}" <<'EOF'
import io, sys
new, today = sys.argv[1], sys.argv[2]
p = "CHANGELOG.md"
lines = io.open(p, encoding="utf-8").read().splitlines(True)
block = [
    "## [%s] - %s\n" % (new, today),
    "\n",
    "### 新增\n",
    "- \n",
    "\n",
    "### 修复\n",
    "- \n",
    "\n",
]
# 插在「## [未发布]」整段之后，也就是第一个 "## [" 之前的位置里，
# 具体做法：找到最后一个连续的「未发布」段结束处（下一个 ## 开头），插在它前面。
idx = None
for i, ln in enumerate(lines):
    if ln.startswith("## [") and "未发布" not in ln:
        idx = i
        break
if idx is None:
    idx = len(lines)
lines[idx:idx] = block
io.open(p, "w", encoding="utf-8").write("".join(lines))
EOF
fi

echo
echo "==> 结果"
echo "    VERSION:        $(cat VERSION)"
echo "    version.py:     $(sed -n 's/^VERSION = "\(.*\)"/\1/p' moshan/version.py | head -1)"
echo "    tauri.conf:     $(sed -n 's/.*"version": "\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json 2>/dev/null | head -1)"
echo "    cargo(壳):      $(sed -n 's/^version = "\(.*\)"/\1/p' src-tauri/Cargo.toml 2>/dev/null | head -1)"
echo "    cargo(服务端):  $(sed -n 's/^version = "\(.*\)"/\1/p' rust/Cargo.toml 2>/dev/null | head -1)"
echo "    CHANGELOG:      $(grep -c "^## \[${NEW}\]" CHANGELOG.md) 处 ${NEW} 段落"
echo
echo "接着做：1) 补 CHANGELOG 内容  2) bash scripts/check.sh  3) 提交"
echo "打 tag 与推送请自行确认后执行：git tag -a v${NEW} -m \"Moshan ${NEW}\" && git push origin v${NEW}"
