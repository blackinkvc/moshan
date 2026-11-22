#!/usr/bin/env python3
"""给一组图起名字：先把图上的字读出来，再让本机 4B 概括成中文短名。

为什么要分两步
    本机只有 qwen3.5-4b（纯文本）和两个 embedding 模型，没有视觉模型，
    4B 看不见图。所以先借 macOS 自带的 Vision 框架把字读出来（离线、免费、
    中文识别很好），再把文字交给 4B 概括。等以后装了能看图的模型，
    换掉 collect_text 这一层就行，接口不用动。

什么时候不硬凑
    照片、视频截图、纯景物图 OCR 一个字都读不出，这时不编名字，
    原样留着 组-HHMMSS 让人自己在「文件」里改 —— 瞎起名比没名更难找。

用法
    python3 naming.py <目录>          # 打印识别到的字和起名结果，不改任何东西
    python3 naming.py <目录> --apply  # 真改目录名
"""
from __future__ import annotations

import json
import re
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

IMG_EXT = {".jpg", ".jpeg", ".png", ".heic", ".heif", ".webp", ".gif", ".tif", ".tiff", ".bmp"}
OCR_BIN = Path.home() / "Documents" / "cuntu" / "bin" / "pocket_ocr"
OCR_SRC = Path.home() / "Documents" / "cuntu" / "bin" / "ocr.swift"
OLLAMA = "http://127.0.0.1:11434/api/generate"
DEFAULT_MODEL = "qwen3.5-4b-gguf:q4km"

MAX_IMAGES = 8          # 一组里最多读这么多张，够判断主题，又不至于让手机等太久
MAX_CHARS = 1600        # 送进 4B 的文字上限（超了截断，小模型窗口只有 32k）


# —— 一、读字 ————————————————————————————————————————

def ocr_bin() -> Path | None:
    """找到 OCR 小工具；找不到就现场编译一次（首次约 18 秒）。"""
    if OCR_BIN.is_file() and OCR_BIN.stat().st_mode & 0o111:
        return OCR_BIN
    if not OCR_SRC.is_file():
        return None
    try:
        subprocess.run(["swiftc", "-O", "-o", str(OCR_BIN), str(OCR_SRC)],
                       capture_output=True, timeout=240)
    except (OSError, subprocess.SubprocessError):
        return None
    return OCR_BIN if OCR_BIN.is_file() else None


def ocr_images(paths: list[Path], timeout: float = 90) -> dict[str, str]:
    """一次调用读完所有图（模型只加载一次，实测 6 张不到 1 秒）。"""
    exe = ocr_bin()
    if not exe or not paths:
        return {}
    try:
        r = subprocess.run([str(exe)] + [str(p) for p in paths],
                           capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError):
        return {}
    out: dict[str, str] = {}
    for line in (r.stdout or "").splitlines():
        path, _, text = line.partition("\t")
        if path:
            out[path] = text.strip()
    return out


def group_images(d: Path) -> list[Path]:
    try:
        files = [p for p in d.iterdir() if p.is_file() and p.suffix.lower() in IMG_EXT]
    except OSError:
        return []
    # 先读大图：手机截图、信息图通常比缩略图信息多
    files.sort(key=lambda p: -p.stat().st_size if p.exists() else 0)
    return files[:MAX_IMAGES]


def collect_text(d: Path) -> tuple[str, int, int]:
    """返回（拼好的文字, 读了几张, 有多少字）。"""
    files = group_images(d)
    if not files:
        return "", 0, 0
    got = ocr_images(files)
    chunks = [t for t in got.values() if len(t) >= 6]
    text = "\n".join(chunks)
    if len(text) > MAX_CHARS:
        text = text[:MAX_CHARS] + "…"
    return text, len(files), len(text)


# —— 二、起名 ————————————————————————————————————————

PROMPT = """下面是手机上传的一组图片上的文字（OCR 结果，可能有错别字和噪声）。请概括这一组的共同主题，起一个中文名字。

要求：
- 2 到 10 个字
- 不要标点、引号、书名号、序号、表情
- 说清楚「这是什么」，不要出现「截图」「图片」「一组」「文件」这类词
- 只输出名字本身，不要解释，不要换行

文字内容：
{text}

名字："""


def ask_4b(prompt: str, model: str = DEFAULT_MODEL, timeout: float = 90) -> str:
    body = json.dumps({
        "model": model, "prompt": prompt, "stream": False, "think": False,
        "options": {"temperature": 0.2, "num_predict": 32, "top_p": 0.9},
    }).encode()
    req = urllib.request.Request(OLLAMA, data=body,
                                 headers={"Content-Type": "application/json"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))  # 本机地址别走代理
    with opener.open(req, timeout=timeout) as r:
        data = json.loads(r.read())
    return (data.get("response") or "").strip()


BAD_CHARS = re.compile(r'[\\/:*?"<>|\n\r\t]+')
WRAP = re.compile(r'^[\s「」『』“”"\'‘’《》【】\[\]()（）]*|[\s「」『』“”"\'‘’《》【】\[\]()（）]*$')


def clean_name(raw: str) -> str:
    """把模型输出收拾成一个能当目录名的短句。"""
    line = ""
    for ln in (raw or "").splitlines():
        if ln.strip():
            line = ln.strip()
            break
    line = WRAP.sub("", line)
    line = line.split("：")[0].split(":")[0] if "：" in line or ":" in line else line
    line = BAD_CHARS.sub(" ", line)
    line = re.sub(r"\s+", " ", line).strip(" .。、,，;；")
    # 模型偶尔会把要求复述出来，砍掉常见的开头词
    for pre in ("名字是", "名字", "答案", "标题"):
        if line.startswith(pre):
            line = line[len(pre):].lstrip("：: 是")
    if len(line) > 12:            # 2-10 个字的要求守不住时，硬截到 12
        line = line[:12].rstrip(" 的之与和")
    return line


def _uniq(directory: Path, name: str) -> Path:
    target = directory / name
    if not target.exists():
        return target
    for i in range(2, 40):
        cand = directory / f"{name} ({i})"
        if not cand.exists():
            return cand
    return directory / f"{name} ({int(time.time())})"


def name_folder(d: Path, model: str | None = None, force: bool = False) -> dict:
    """读图 → 起名 → （可选）改目录名。返回结果，永不抛异常给调用方。"""
    t0 = time.time()
    d = Path(d)
    res = {"ok": False, "renamed": False, "name": d.name, "path": str(d),
           "images": 0, "chars": 0, "ms": 0, "reason": ""}

    if not d.is_dir():
        res["reason"] = "目录不在了"
        return res
    if not force and not d.name.startswith("组-"):
        res["reason"] = "这组已经有名字了"
        return res

    text, n_img, n_chars = collect_text(d)
    res.update(images=n_img, chars=n_chars)
    if n_img == 0:
        res["reason"] = "这组里没有图片"
        res["ms"] = int((time.time() - t0) * 1000)
        return res
    if n_chars < 12:
        res["reason"] = "图里没读到字（照片或视频截图），留着原名"
        res["ms"] = int((time.time() - t0) * 1000)
        return res

    try:
        raw = ask_4b(PROMPT.format(text=text), model=model or DEFAULT_MODEL)
    except (urllib.error.URLError, OSError, ValueError) as e:
        res["reason"] = f"4B 没应答：{str(e)[:60]}"
        res["ms"] = int((time.time() - t0) * 1000)
        return res

    name = clean_name(raw)
    if len(name) < 2:
        res["reason"] = f"4B 没给出像样的名字（原文：{raw[:24]}）"
        res["ms"] = int((time.time() - t0) * 1000)
        return res

    res["name"] = name
    res["ok"] = True
    target = _uniq(d.parent, name)
    try:
        d.rename(target)
        res["renamed"] = True
        res["path"] = str(target)
    except OSError as e:
        res["reason"] = f"改名失败：{str(e)[:60]}"
    res["ms"] = int((time.time() - t0) * 1000)
    return res


if __name__ == "__main__":
    import sys
    if len(sys.argv) < 2:
        print(__doc__)
        raise SystemExit(2)
    target = Path(sys.argv[1]).expanduser()
    apply = "--apply" in sys.argv
    if not apply:
        txt, n, c = collect_text(target)
        print(f"读了 {n} 张，{c} 字")
        print("—— OCR 原文 ——")
        print(txt[:800] or "（没有读到字）")
        print("—— 起名 ——")
        if c >= 12:
            raw = ask_4b(PROMPT.format(text=txt))
            print("原始输出:", repr(raw))
            print("收拾后:", clean_name(raw))
        print("（加 --apply 才会真的改目录名）")
    else:
        print(json.dumps(name_folder(target, force=True), ensure_ascii=False, indent=1))
