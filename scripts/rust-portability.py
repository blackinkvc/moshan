#!/usr/bin/env python3
"""Rust 服务端可移植性审计：找出没被 `#[cfg]` 挡住的平台专有调用。

为什么需要这个脚本
------------------
本机只有 aarch64-apple-darwin 一个 target（Homebrew 装的 rustc，没有 rustup，
装不了交叉 std）；而 `cargo check --target x86_64-pc-windows-msvc` 就算有 std
也过不去 —— ring 的 build.rs 要为本 target 调 cc/nasm，本机没有 Windows 交叉
C 工具链。所以「Windows/Linux 能不能编过」在本地只能靠两件事保证：

  1. `--features sim_windows` / `sim_linux`：把平台分支翻到对面，让对面那份代码
     参与类型检查（这也是为什么平台分支写成 `all(unix, not(feature = "sim_windows"))`
     而不是裸 `unix`）。
  2. 本脚本：反向检查 —— 每处平台专有 API 是否真的落在「对面平台会被编掉」的
     cfg 块里。sim_* 骗得过编译器（mac 上 unix 恒真），骗不过这个静态检查。

真正的三平台编译验证在 CI（`.github/workflows/tauri.yml` 的 matrix 在
macos-latest / windows-latest / ubuntu-22.04 上原生 `cargo build`，并拉起服务
做 `/api/ping` 冒烟）。

用法
----
    python3 scripts/rust-portability.py            # 只报有问题的
    python3 scripts/rust-portability.py -v         # 连安全的一起列出来
    python3 scripts/rust-portability.py --ci       # 有未隔离命中就 exit 1

判定规则
--------
每个命中行，找出真正包住它的所有 `#[cfg]`（含块级属性与紧贴其上的属性），
把它们当「与」求值两次：一次按 `target_os = "windows"`，一次按 `target_os = "linux"`。
  - unix 专有 API（`std::os::unix`、`libc::`）：在 Windows 下必须被编掉
  - windows 专有 API（`std::os::windows`）：在 Linux 下必须被编掉
求值器只认 `all / any / not / ident / key = "value"`；看不懂的表达式一律按
「没隔离」报出来（宁可多报，不可漏报）。
"""

import argparse
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SRC = os.path.join(ROOT, "rust", "moshan-server", "src")

PATTERNS = [
    (re.compile(r"\bstd::os::unix\b"), "unix", "std::os::unix::*"),
    (re.compile(r"\blibc::"), "unix", "libc::*"),
    (re.compile(r"\bstd::os::windows\b"), "windows", "std::os::windows::*"),
]

# 纯声明行：注释、文档、extern 链接声明本身
SKIP_LINE = re.compile(r"^\s*(//|///|//!|#\[link)")
EXTERN_DECL = re.compile(r'^\s*(pub\s+)?(unsafe\s+)?extern\s+"[A-Za-z0-9_-]+"\s*\{')

UNIX_OSES = {"macos", "linux", "freebsd", "openbsd", "netbsd", "android", "ios", "dragonfly"}
KNOWN_OSES = UNIX_OSES | {"windows", "fuchsia", "emscripten", "wasi"}


class CfgError(Exception):
    pass


def _tokenize(s):
    toks, i = [], 0
    while i < len(s):
        c = s[i]
        if c in " \t\n":
            i += 1
        elif c == ",":
            toks.append(",")
            i += 1
        elif c in "()":
            toks.append(c)
            i += 1
        elif s.startswith('"', i):
            j = s.index('"', i + 1)
            toks.append(("str", s[i + 1:j]))
            i = j + 1
        elif c == "=":
            toks.append("=")
            i += 1
        elif c == "&":  # 多个属性拼起来的 &&，按 all 处理
            toks.append("&")
            i += 1
        else:
            m = re.match(r"[A-Za-z_][A-Za-z0-9_]*", s[i:])
            if not m:
                raise CfgError(f"看不懂的字符 {c!r} in {s!r}")
            toks.append(("id", m.group(0)))
            i += len(m.group(0))
    return toks


class Parser:
    def __init__(self, toks):
        self.t = toks
        self.i = 0

    def peek(self):
        return self.t[self.i] if self.i < len(self.t) else None

    def next(self):
        v = self.peek()
        self.i += 1
        return v

    def expr(self):
        tok = self.next()
        if tok is None:
            raise CfgError("表达式提前结束")
        if tok == "(":
            e = self.expr()
            if self.next() != ")":
                raise CfgError("括号不配对")
            return e
        if isinstance(tok, tuple) and tok[0] == "id":
            name = tok[1]
            if self.peek() == "(":
                self.next()
                args = []
                while self.peek() != ")":
                    args.append(self.expr())
                    if self.peek() == ",":
                        self.next()
                    elif self.peek() != ")":
                        raise CfgError("参数列表不合法")
                self.next()
                if name == "all":
                    return ("all", args)
                if name == "any":
                    return ("any", args)
                if name == "not":
                    return ("not", args)
                raise CfgError(f"未知函数 {name}")
            if self.peek() == "=":
                self.next()
                v = self.next()
                if not (isinstance(v, tuple) and v[0] == "str"):
                    raise CfgError("key 的值不是字符串")
                return ("kv", name, v[1])
            return ("id", name)
        raise CfgError(f"意外的 token {tok!r}")

    def parse(self):
        e = self.expr()
        while self.peek() == "&":
            self.next()
            e = ("all", [e, self.expr()])
        if self.peek() is not None:
            raise CfgError("多余的 token")
        return e


def evaluate(node, target_os, features):
    kind = node[0]
    if kind == "all":
        return all(evaluate(a, target_os, features) for a in node[1])
    if kind == "any":
        return any(evaluate(a, target_os, features) for a in node[1])
    if kind == "not":
        return not evaluate(node[1][0], target_os, features)
    if kind == "kv":
        key, val = node[1], node[2]
        if key == "target_os":
            return target_os == val
        if key == "target_family":
            return (target_os == "windows") == (val == "windows")
        if key == "feature":
            return val in features
        if key == "target_pointer_width":
            return val == "64"
        raise CfgError(f"不认识的 cfg 键 {key}")
    if kind == "id":
        name = node[1]
        if name == "unix":
            return target_os in UNIX_OSES
        if name == "windows":
            return target_os == "windows"
        if name == "test":
            return False
        raise CfgError(f"不认识的 cfg 开关 {name}")
    raise CfgError(f"不认识的节点 {kind}")


def excluded_on(cfg_text, target_os):
    """这段 cfg 在 target_os 上会不会把代码编掉。

    返回 True / False / None（None = 表达式看不懂，调用方按「没隔离」处理）。
    """
    try:
        node = Parser(_tokenize(cfg_text)).parse()
        # 真实构建里 sim_* 是关的 —— 按关掉来求值，正好等价于「这台机器上真编」
        return not evaluate(node, target_os, features=set())
    except CfgError:
        return None


def extract_cfgs(line):
    """把一行里所有 `#[cfg(...)]` 抠出来（支持同行跟着代码、支持嵌套括号）。"""
    out, rest, i = [], line, 0
    while True:
        j = rest.find("#[cfg(", i)
        if j < 0:
            out.append(rest[i:]) if False else None
            break
        start = j + len("#[cfg(")
        depth, k = 1, start
        while k < len(rest) and depth:
            if rest[k] == "(":
                depth += 1
            elif rest[k] == ")":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        if depth:
            break
        body = rest[start:k]
        # 扣掉属性本身，其后的 `]` 也一起
        end = k + 1
        if end < len(rest) and rest[end] == "]":
            end += 1
        out.append(body)
        rest = rest[:j] + rest[end:]
        i = j
    return out, rest


def logical_lines(text):
    """把跨行的 `#[cfg(all(\\n ... \\n))]` 合成一行，同时记住起始行号。

    rustfmt 会把长 cfg 拆成多行；不合并的话属性识别不到，整块会被误判成「无 cfg」。
    """
    raw = text.split("\n")
    out, i = [], 0
    while i < len(raw):
        start, line = i + 1, raw[i]
        while "#[cfg(" in line and line.count("(") > line.count(")"):
            i += 1
            if i >= len(raw):
                break
            line += " " + raw[i].strip()
        out.append((start, line))
        i += 1
    return out


def scan(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        lines = logical_lines(f.read())

    hits = []
    stack = []          # [(cfg_text_or_None, ...)]，每项是那层花括号的 cfg（已「与」起来）
    depth = 0
    pending = []        # 紧贴下面的属性（含同行属性里的那部分）
    extern_depth = None

    for idx, line in lines:
        bodies, rest = extract_cfgs(line)
        code = rest.strip()
        if bodies:
            pending.extend(bodies)
        if not code or code.startswith("//"):
            continue

        opens, closes = code.count("{"), code.count("}")
        cfg_here = " && ".join(pending) if pending else None
        for _ in range(opens):
            depth += 1
            stack.append(cfg_here)

        if not SKIP_LINE.match(code):
            enclosing = [c for c in stack if c]
            for pat, plat, desc in PATTERNS:
                if pat.search(code):
                    if extern_depth is not None and depth >= extern_depth:
                        break
                    hits.append((idx, plat, desc, enclosing, code))
                    break

        if EXTERN_DECL.match(code):
            extern_depth = depth

        for _ in range(closes):
            if stack:
                stack.pop()
            depth = max(0, depth - 1)
            if extern_depth is not None and depth < extern_depth:
                extern_depth = None

        pending = []

    return hits


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-v", "--verbose", action="store_true", help="连安全的也列出来")
    ap.add_argument("--ci", action="store_true", help="有未隔离命中就退出 1")
    args = ap.parse_args()

    if not os.path.isdir(SRC):
        print(f"找不到源码目录：{SRC}", file=sys.stderr)
        return 2

    files = []
    for dirpath, _dirs, names in os.walk(SRC):
        for n in sorted(names):
            if n.endswith(".rs"):
                files.append(os.path.join(dirpath, n))

    total, bad, unknown = 0, [], 0
    for path in sorted(files):
        rel = os.path.relpath(path, ROOT)
        for (lineno, plat, desc, cfgs, code) in scan(path):
            total += 1
            # unix 专有：要求「在 Windows 上会被编掉」；windows 专有要求「在 Linux 上会被编掉」
            target = "windows" if plat == "unix" else "linux"
            verdicts = [excluded_on(c, target) for c in cfgs]
            if any(v is True for v in verdicts):
                if args.verbose:
                    print(f"  ok  {rel}:{lineno}  [{plat}] {desc}  ← {' | '.join(cfgs)}")
                continue
            if any(v is None for v in verdicts):
                unknown += 1
            bad.append((rel, lineno, plat, desc, cfgs, code))

    for rel, lineno, plat, desc, cfgs, code in bad:
        where = " | ".join(cfgs) if cfgs else "（无 cfg）"
        print(f"{rel}:{lineno}: 未隔离的 {plat} 专有调用 {desc}")
        print(f"    {code[:110]}")
        print(f"    所在 cfg：{where}")

    print()
    print(f"扫描 {len(files)} 个文件，{total} 处平台专有 API，"
          f"已隔离 {total - len(bad)}，未隔离 {len(bad)}"
          + (f"（其中 {unknown} 处的 cfg 看不懂，按未隔离计）" if unknown else ""))
    if args.ci and bad:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
