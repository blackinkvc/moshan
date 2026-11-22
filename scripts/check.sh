#!/bin/bash
# 本地自检 —— 推代码前跑一遍，和 CI 的检查项对齐。
# 用法：scripts/check.sh
set -eu
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
FAIL=0

say() { printf '%s\n' "$*"; }
ok()  { printf '  [通过] %s\n' "$*"; }
bad() { printf '  [失败] %s\n' "$*"; FAIL=1; }

say "== 1. 语法 =="
# -x 排除 ._xxxx.py：仓库放在 exFAT/网络盘上时，macOS 会生成 AppleDouble 元数据文件，
# 它们是二进制、含 null 字节，compileall 会误报「source code cannot contain null bytes」。
if python3 -m compileall -q -x '(^|/)\._' moshan >/dev/null 2>&1; then
  ok "所有 .py 语法正确"
else
  bad "存在语法错误"
fi
find moshan -name "__pycache__" -type d -exec rm -rf {} + 2>/dev/null || true
# BSD 的 find -delete 会把删掉的文件名打印到 stdout，重定向掉免得混进检查结果
find . -name "._*" -not -path "./dist/*" -delete >/dev/null 2>&1 || true

say "== 2. 导入冒烟（模拟全新用户）=="
# 必须同时满足两个条件，否则这类问题本机永远看不出来：
#   1) 逐个导入 moshan/ 下的**全部**模块，不是挑几个。手挑的清单漏过 article，
#      而 article 在 server 的 import 链上（server -> linkbox -> article）。
#   2) HOME 指向空目录。开发机上有 ~/Documents/cuntu，article.py 会从那里拿到
#      textfmt，于是本机全绿、CI 全红 —— 2026-11-23 的 textfmt.py 就是这么漏的。
SMOKE_HOME="${TMPDIR:-/tmp}/moshan-smoke"
rm -rf "${SMOKE_HOME}"; mkdir -p "${SMOKE_HOME}"
MODN=$(ls moshan/*.py | wc -l | tr -d ' ')
if ( cd moshan && HOME="${SMOKE_HOME}" MOSHAN_HOME="${SMOKE_HOME}/Moshan" PYTHONPATH=. python3 - <<'PY'
import importlib, pathlib, sys
bad = []
for p in sorted(pathlib.Path(".").glob("*.py")):
    m = p.stem
    if m.startswith((".", "__")):
        continue
    try:
        importlib.import_module(m)
    except Exception as e:                             # noqa: BLE001
        bad.append(f"{m}: {type(e).__name__}: {e}")
if bad:
    print("导入失败的模块：")
    for b in bad:
        print("  " + b)
    sys.exit(1)

import paths, server, version
routes = {r.path for r in server.app.routes if getattr(r, "path", "")}
for need in ("/welcome", "/welcome/status", "/welcome/pull", "/welcome/finish", "/about"):
    assert need in routes, f"缺少路由 {need}"
d = paths.describe()
assert d["state_dir"] != d["library_dir"], "状态目录与资料库不应是同一个"
assert d["app_dir"] != d["state_dir"], "程序目录与状态目录不应是同一个"
assert paths.data_root() == paths.library_dir(), "数据根应落在资料库"
assert version.is_newer("0.2.0", "0.1.0"), "版本比较坏了"
assert not version.is_newer("0.1.0", "0.1.0")
assert not version.is_newer("0.1.0-beta.1", "0.1.0"), "预发布不应比正式版新"
PY
); then ok "${MODN} 个模块全部可导入（空 HOME），路由 / 目录 / 版本正常"
else bad "有模块导入失败（依赖了仓库外的文件？见上）"; fi

# 更新器的资产名匹配：曾经把 (?i) 写在正则中间，Python 直接抛 PatternError，
# 「检查更新」对所有人都是 500，而单元测试没覆盖到。这里钉死一条断言。
if ( cd moshan && PYTHONPATH=. python3 - <<'PY' >/dev/null 2>&1
import updater
arm = updater._asset_pattern("macos-arm64")
win = updater._asset_pattern("windows-x64")
assert arm.search("Moshan-0.1.0-macos-arm64.dmg")
assert win.search("Moshan-0.1.0-windows-x64-Setup.exe")
assert not arm.search("Moshan-0.1.0-macos-x64.dmg")      # 架构不能串
assert not win.search("Moshan-0.1.0-windows-x64-Setup.exe.sha256")
PY
); then ok "更新资产匹配规则正常"; else bad "updater 资产正则失效（检查 (?i) 的写法）"; fi

say "== 3. 网络信任链（安装版能不能联网的前提）=="
# 这条是本机唯一能提前发现「打包后 https 全废」的地方：
# 打包版用自己的 OpenSSL，默认证书路径在别人机器上不存在，源码态因为系统有
# /etc/ssl/cert.pem 而完全看不出问题。所以断言两件事——
#   1) certifi 真的装上了（它会被打进包）；
#   2) netca.install() 选中它，且默认上下文里真的装进了根证书。
if ( cd moshan && PYTHONPATH=. python3 - <<'PY' >/dev/null 2>&1
import pathlib
import certifi

p = pathlib.Path(certifi.where())
assert p.is_file(), f"certifi 证书文件不存在：{p}"
assert p.stat().st_size > 50_000, "certifi 证书文件过小，可能是残缺的"

import netca

s = netca.stats()
assert s["cafile"], "netca 找不到任何 CA 证书（打包版将无法访问 https）"
assert pathlib.Path(s["cafile"]).resolve() == p.resolve(), \
    f"CA 没指向 certifi：{s['cafile']}（应走随包分发的证书）"
assert s["x509"] > 50, f"默认上下文只装进 {s['x509']} 张根证书"
PY
); then ok "CA 走 certifi，默认上下文已装载根证书"
else bad "https 信任链有问题（装版会 CERTIFICATE_VERIFY_FAILED，先 pip install certifi）"; fi

say "== 4. 硬编码路径 =="
if grep -rIl "/Users/" moshan/*.py 2>/dev/null | grep -v "ocr_runner.py" | grep -q .; then
  bad "代码里仍有写死的家目录："
  grep -rIn "/Users/" moshan/*.py 2>/dev/null | grep -v "ocr_runner.py" | head -5
else
  ok "没有写死的家目录"
fi

say "== 5. 密钥与运行时数据 =="
if [ -f moshan/config.json ] || [ -f config.json ]; then
  bad "仓库里出现了 config.json（应在状态目录）"
else
  ok "没有配置文件入库"
fi
if grep -RIl --exclude-dir=.git --exclude-dir=dist \
     -e '-----BEGIN [A-Z ]*PRIVATE KEY-----' . >/dev/null 2>&1; then
  bad "疑似私钥入库"
else
  ok "没有私钥文件"
fi

say "== 6. 版本号一致性 =="
V1=$(sed -n 's/^VERSION = "\(.*\)"/\1/p' moshan/version.py | head -1)
V2=$(head -1 VERSION 2>/dev/null | tr -d '[:space:]')
if [ -n "$V1" ] && [ "$V1" = "$V2" ]; then
  ok "version.py 与 VERSION 一致：$V1"
else
  bad "版本号不一致：version.py=$V1 / VERSION=$V2"
fi

printf '\n'
if [ "$FAIL" -eq 0 ]; then say "全部通过，可以提交。"; else say "有失败项，修完再来。"; fi
exit "$FAIL"
