#!/bin/bash
# 本地自检 —— 推代码前跑一遍，和 CI 的检查项对齐。
# 用法：scripts/check.sh
set -eu
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
FAIL=0

# 用哪个解释器跑检查。默认 python3，但 2、3 两节要 fastapi / certifi ——
# 系统 python 没装就会一片「No module named 'fastapi'」，看起来像代码坏了，
# 其实是解释器不对。所以允许外部指定：MOSHAN_PY=/path/to/venv/bin/python scripts/check.sh
PY="${MOSHAN_PY:-python3}"
DEPS_OK=1
"${PY}" -c "import fastapi, certifi" >/dev/null 2>&1 || DEPS_OK=0

say() { printf '%s\n' "$*"; }
ok()  { printf '  [通过] %s\n' "$*"; }
bad() { printf '  [失败] %s\n' "$*"; FAIL=1; }

say "== 1. 语法 =="
# -x 排除 ._xxxx.py：仓库放在 exFAT/网络盘上时，macOS 会生成 AppleDouble 元数据文件，
# 它们是二进制、含 null 字节，compileall 会误报「source code cannot contain null bytes」。
if "${PY}" -m compileall -q -x '(^|/)\._' moshan >/dev/null 2>&1; then
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
if ( cd moshan && HOME="${SMOKE_HOME}" MOSHAN_HOME="${SMOKE_HOME}/Moshan" PYTHONPATH=. "${PY}" - <<'PY'
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
def _paths(rs):
    """路由枚举：新版 FastAPI 的 include_router 会包一层 _IncludedRouter（没有 .path），
    这里递归取出来，免得依赖升级后自检假红。"""
    for r in rs:
        p = getattr(r, "path", "")
        if p:
            yield p
        sub = getattr(r, "routes", None)
        if sub:
            yield from _paths(sub)


routes = set(_paths(server.app.routes))
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
if ( cd moshan && PYTHONPATH=. "${PY}" - <<'PY' >/dev/null 2>&1
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
if ( cd moshan && PYTHONPATH=. "${PY}" - <<'PY' >/dev/null 2>&1
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
# 两个例外，都是「处理输入里的路径」而不是「把家目录写死」：
#   ocr_runner.py —— 调 sips 时的 macOS 路径
#   hostwork.py   —— 把采集到的 /Users/xxx/proj 裁成 proj（Windows 上还有 /home/）
ALLOW_HOME_FILES="-e ocr_runner.py -e hostwork.py"
if grep -rIl "/Users/" moshan/*.py 2>/dev/null | grep -v ${ALLOW_HOME_FILES} | grep -q .; then
  bad "代码里仍有写死的家目录："
  grep -rIn "/Users/" moshan/*.py 2>/dev/null | grep -v ${ALLOW_HOME_FILES} | head -5
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
# Tauri 的 version 是静态字符串，不会跟着 VERSION 走。四处不一致的后果很隐蔽：
# 安装包文件名、Gatekeeper 显示、应用内更新比对全按不同数字走，
# 用户会收到「已经是最新」但其实装的是旧壳。
V1=$(sed -n 's/^VERSION = "\(.*\)"/\1/p' moshan/version.py | head -1)
V2=$(head -1 VERSION 2>/dev/null | tr -d '[:space:]')
V3=$(sed -n 's/.*"version": "\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json 2>/dev/null | head -1)
V4=$(sed -n 's/^version = "\([^"]*\)"/\1/p' src-tauri/Cargo.toml 2>/dev/null | head -1)
if [ -n "$V1" ] && [ "$V1" = "$V2" ] && [ "$V1" = "$V3" ] && [ "$V1" = "$V4" ]; then
  ok "version.py / VERSION / tauri.conf.json / Cargo.toml 一致：$V1"
else
  bad "版本号不一致：version.py=$V1 / VERSION=$V2 / tauri.conf.json=$V3 / Cargo.toml=$V4"
fi

say "== 7. 自述通道（任何 agent 都能报）=="
# 这条通道有三段，少一段 agent 报的账就落不到卡片上：
#   写（POST /api/agent/note，任意 agent）→ 存（_本机工作/Agent自述.jsonl）
#   → 读（dash 折成卡片行，按 agent 分组）
# 说明书里的接口写错端点（曾经写成 /api/agent/event，那条进总线、卡片读不到），
# 装到用户机器上表现就是「明明报了，工作台却空着」—— 只能用断言钉住。
# 另外别把接口写回只认 Trae：用户以后装 Codex / 自建 harness 也要能报。
if ( cd moshan && "${PY}" - <<'PY'
import sys
import agent_link
t = agent_link.prompt("trae", "http://127.0.0.1:8323")
问题 = []
if "/api/agent/note" not in t:
    问题.append("说明书没让 agent 报 /api/agent/note")
if '"agent"' not in t:
    问题.append("说明书没让 agent 带 agent 字段（卡片没法分组）")
for k in ('"in"', '"out"', '"calls"'):
    if k not in t:
        问题.append(f"说明书缺用量字段 {k}")
if not callable(getattr(agent_link, "auto_install", None)):
    问题.append("没有 auto_install（升级后不会自动投放说明书）")
ids = {a["id"] for a in agent_link.AGENTS}
for must in ("trae", "codex", "claude", "workbuddy"):
    if must not in ids:
        问题.append(f"AGENTS 里没有 {must}（认不出这家就投不了说明书）")
for a in agent_link.AGENTS:
    if not a.get("skill"):
        问题.append(f"{a['id']} 缺 skill 路径")
if 问题:
    print("；".join(问题)); sys.exit(1)
PY
); then ok "说明书走通用 /api/agent/note，认得出 $(cd moshan && "${PY}" -c 'import agent_link;print(len(agent_link.AGENTS))') 家 agent"; else bad "自述通道不完整（别把接口写回 /api/agent/event，也别只认 Trae）"; fi

if grep -q '"/api/agent/note"' moshan/server.py && \
   grep -q '"/api/trae/note"' moshan/server.py && \
   grep -q 'Agent自述.jsonl' moshan/server.py && \
   grep -q 'Agent自述.jsonl' moshan/trae_work.py; then
  ok "接口两代并存（新 /api/agent/note + 老 /api/trae/note），都落 Agent自述.jsonl"
else
  bad "自述接口或落盘路径不对（老路径要保留，否则已装出去的说明书报不上来）"
fi

if grep -q "def _trae_self_items" moshan/dash.py && \
   grep -q '_trae_self_items(since_ms)' moshan/dash.py && \
   grep -q 'Agent自述.jsonl' moshan/dash.py; then
  ok "工作台会把 Agent自述.jsonl 折成卡片行（按 agent 分组）"
else
  bad "dashboard 没读 Agent自述.jsonl（agent 报了也看不见）"
fi

say "== 8. 远端脚本编码与工作目录 =="
# PowerShell 5.1 把无 BOM 的 .ps1/.cmd 当 GBK 读：末尾汉字的尾字节会吃掉换行，
# 把**下一行**并进注释。踩过一次：Set-Location 静默失效，PyInstaller 在
# C:\Windows\System32 里拒绝运行，日志里只少一行。所以这类文件只允许
# 「纯 ASCII」或「带 UTF-8 BOM」，中间态一律判失败。
BADFILES=$("${PY}" - <<'PY'
import pathlib
bad = []
for p in pathlib.Path('.').rglob('*'):
    if p.suffix.lower() not in ('.ps1', '.cmd', '.bat'):
        continue
    parts = set(p.parts)
    if parts & {'.git', 'dist', 'build', '.venv-build', 'node_modules'}:
        continue
    b = p.read_bytes()
    if not b or b[:3] == b'\xef\xbb\xbf':
        continue
    if all(x < 128 for x in b):
        continue
    bad.append(str(p))
print(' '.join(bad))
PY
)
if [ -n "$BADFILES" ]; then
  bad "含非 ASCII 又没 BOM（PS 5.1 会读坏，命令会静默失效）：$BADFILES"
else
  ok "远端脚本要么纯 ASCII、要么带 BOM"
fi
if grep -q 'Set-Location \$Root' packages/windows/build.ps1 2>/dev/null; then
  ok "build.ps1 会先切到仓库根（PyInstaller 不在 System32 里跑）"
else
  bad "build.ps1 没有 Set-Location，远端后台构建会直接失败"
fi

printf '\n'
if [ "${DEPS_OK}" -eq 0 ]; then
  say "提示：当前解释器（${PY}）缺 fastapi / certifi，第 2、3 节的失败是环境问题不是代码问题。"
  say "      换一个装好依赖的解释器再跑：MOSHAN_PY=/path/to/venv/bin/python scripts/check.sh"
  printf '\n'
fi
if [ "$FAIL" -eq 0 ]; then say "全部通过，可以提交。"; else say "有失败项，修完再来。"; fi
exit "$FAIL"
