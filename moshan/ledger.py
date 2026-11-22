"""记账：丢一张订单截图进来，认字、裁出产品图、攒成一件商品一张卡。

目录（<数据根>/12-记账/）：
    账本.json     真源，一件商品一条
    图/<id>.jpg   裁出来的产品图（卡片和抽屉用这个）
    原图/<id>*    原始截图，留着追溯，认错了还能回去看
    _回收站/      删除的先进这儿

为什么单开一个目录：04-存图 有自己的 _索引.json，08-阅读 有 _已读.json，
这两个列表页会把目录里的图当成「收藏」扫出来。记账的图混进去就成了脏数据。

裁图和 OCR 都在 ~/Documents/cuntu/bin/ledger_shot（Swift，Vision）里做：
服务这个 venv 里没有 Pillow，也不该为了一件事往里装。
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import time
import uuid
from datetime import datetime
from pathlib import Path

DIR_NAME = "12-记账"
SHOT_TOOL = Path.home() / "Documents/cuntu/bin/ledger_shot"
OCR_TOOL = Path.home() / "Documents/cuntu/bin/ocrvision"

CATEGORIES = ["数码", "家电", "家居", "日用", "服饰", "书籍", "食品", "出行", "其他"]

# 平台识别：截图里出现这些词就认成这家。顺序＝优先级（天猫含「淘宝」字样时别认成淘宝）。
PLATFORMS = [
    ("京东", ["京东", "JD.COM", "京东商城"]),
    ("天猫", ["天猫", "tmall"]),
    ("淘宝", ["淘宝", "taobao"]),
    ("拼多多", ["拼多多", "pinduoduo"]),
    ("闲鱼", ["闲鱼", "咸鱼"]),
    ("得物", ["得物", "poizon"]),
    ("抖音", ["抖音", "抖店"]),
    ("小红书", ["小红书"]),
    ("苏宁", ["苏宁"]),
    ("唯品会", ["唯品会"]),
    ("Apple", ["Apple Store", "apple.com.cn", "苹果官网"]),
    ("小米", ["小米商城", "小米有品", "mi.com"]),
    ("华为", ["华为商城", "vmall"]),
    ("亚马逊", ["亚马逊", "amazon"]),
]

_PRICE_WORDS = ("实付款", "实付", "应付", "合计", "总价", "订单金额", "付款金额", "成交价", "商品总价")
_BUY_WORDS = ("下单时间", "购买时间", "创建时间", "成交时间", "付款时间", "支付时间", "订单时间")
_NOISE = ("订单详情", "订单", "等待收货", "待收货", "待发货", "已完成", "交易成功", "退款",
          "查看物流", "申请退款", "联系客服", "再次购买", "评价", "确认收货", "全部", "首页",
          "我的", "购物车", "实付款", "实付", "合计", "优惠", "运费", "店铺", "订单编号",
          "支付方式", "运单号", "收货地址", "备注", "发票", "客服")

_DATE_RE = re.compile(r"(20\d{2})\s*[-/.年]\s*(\d{1,2})\s*[-/.月]\s*(\d{1,2})")
_NUM_RE = re.compile(r"(?:[¥￥]\s*)?(\d{1,3}(?:,\d{3})*(?:\.\d{1,2})?|\d+(?:\.\d{1,2})?)")
_ORDER_RE = re.compile(r"订单编号\D{0,4}(\d{10,})")


# ---------- 路径 ----------

def _root(cfg: dict) -> Path:
    roots = cfg.get("allowed_roots") or []
    return Path(roots[0]) if roots else Path.home() / "Documents/AI项目"


def ledger_dir(cfg: dict) -> Path:
    return _root(cfg) / DIR_NAME


def _ensure(cfg: dict) -> Path:
    d = ledger_dir(cfg)
    for sub in ("图", "原图", "_回收站"):
        (d / sub).mkdir(parents=True, exist_ok=True)
    return d


def book_path(cfg: dict) -> Path:
    return ledger_dir(cfg) / "账本.json"


# ---------- 读写 ----------

def load(cfg: dict) -> dict:
    d = _load_json(book_path(cfg), {}) or {}
    items = d.get("items") if isinstance(d.get("items"), list) else []
    return {"items": [i for i in items if isinstance(i, dict) and i.get("id")],
            "updated_at": d.get("updated_at") or ""}


def _load_json(p: Path, default):
    try:
        with p.open("r", encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


def _atomic_write(p: Path, text: str) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    tmp = p.with_name(p.name + f".{os.getpid()}.{uuid.uuid4().hex[:6]}.tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(text)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, p)


def save(cfg: dict, data: dict) -> None:
    data["updated_at"] = datetime.now().strftime("%Y-%m-%dT%H:%M:%S")
    _atomic_write(book_path(cfg), json.dumps(data, ensure_ascii=False, indent=1))


def get_item(cfg: dict, iid: str) -> dict | None:
    return next((i for i in load(cfg)["items"] if i["id"] == iid), None)


# ---------- 解析：从 OCR 文本里猜字段 ----------
# 规则优先，模型不参与。订单页的字段是固定词 + 数字，正则比 4B 稳，也不会算错日期。

def _clean(s: str) -> str:
    return re.sub(r"\s+", " ", (s or "").strip())


def _money(s: str) -> float | None:
    m = _NUM_RE.search((s or "").replace(" ", ""))
    if not m:
        return None
    try:
        v = float(m.group(1).replace(",", ""))
    except ValueError:
        return None
    return v if 0 < v < 10_000_000 else None


def parse_fields(text: str, lines: list[dict] | None = None) -> dict:
    """返回 {name, platform, bought_at, price, category, order_no, shop, conf}。
    conf 说明哪些字段是猜的，前端会把它标成「待确认」。"""
    raw = text or ""
    # 留着每行的纵向位置：标题折成两行时得能判断「是不是紧接着的那一行」
    seq: list[tuple[str, float, float]] = []
    for l in (lines or []):
        t = _clean(l.get("t", ""))
        if t:
            seq.append((t, float(l.get("y") or 0), float(l.get("h") or 0)))
    if not seq:
        seq = [(_clean(x), 0.0, 0.0) for x in raw.split("\n") if _clean(x)]
    rows = [s[0] for s in seq]

    out: dict = {"name": "", "platform": "", "bought_at": "", "price": None,
                 "category": "", "order_no": "", "shop": "", "conf": []}

    # 平台
    low = raw.lower()
    for name, keys in PLATFORMS:
        if any(k.lower() in low for k in keys):
            out["platform"] = name
            break

    # 价格：先找「实付款 ¥x」这类明确字段，找不到就取出现过的最大金额
    price = None
    for r in rows:
        if any(w in r for w in _PRICE_WORDS):
            v = _money(r)
            if v:
                price = v
                break
    if price is None:
        nums = [v for v in (_money(r) for r in rows if "¥" in r or "￥" in r) if v]
        if nums:
            price = max(nums)
    out["price"] = price
    if price is None:
        out["conf"].append("price")

    # 购买时间：优先「下单时间」那行，其次全文第一个像日期的
    bought = ""
    for r in rows:
        if any(w in r for w in _BUY_WORDS):
            m = _DATE_RE.search(r)
            if m:
                bought = _fmt_date(m)
                break
    if not bought:
        m = _DATE_RE.search(raw)
        if m:
            bought = _fmt_date(m)
    if bought:
        out["bought_at"] = bought
    else:
        out["conf"].append("bought_at")

    # 订单号 / 店铺
    m = _ORDER_RE.search(raw.replace(" ", ""))
    if m:
        out["order_no"] = m.group(1)
    for idx, r in enumerate(rows):
        if "店铺" in r:
            seg = re.split(r"店铺[：:]?", r, maxsplit=1)
            tail = _clean(seg[1]) if len(seg) > 1 else ""
            # OCR 常把「… 店铺」和店名断成两行，第二行开头就是店名
            if not tail and idx + 1 < len(rows):
                tail = re.split(r"运单号|订单号|备注", rows[idx + 1])[0]
            if tail:
                out["shop"] = _clean(tail)[:40]
            break

    # 商品名：最像标题的那行 —— 够长、有中文、不含价格/日期/固定字段
    cand = []
    for r in rows:
        if len(r) < 6 or len(r) > 120:
            continue
        if any(w in r for w in _NOISE):
            continue
        if _DATE_RE.search(r) or "¥" in r or "￥" in r:
            continue
        if re.fullmatch(r"[\d\s.,:：%\-]+", r):
            continue
        if not re.search(r"[\u4e00-\u9fa5A-Za-z]", r):
            continue
        # 有中文也有字母数字、字数多，最像商品标题
        cand.append(r)
    if cand:
        name = max(cand, key=lambda s: len(s))
        i = rows.index(name)
        # 标题折行的常见形态：上一行断在半截（「…声波震」），下一行接着（「动 白色」）。
        # 只接紧挨着的那一行，最多接一次，接完还得是像标题的样子。
        if i + 1 < len(rows):
            nxt, ny, nh = seq[i + 1]
            cur = seq[i]
            gap = ny - (cur[1] + cur[2]) if cur[2] else 0
            tight = gap <= max(10.0, cur[2] * 0.9) if cur[2] else True
            ok_next = (tight and 4 <= len(nxt) <= 40
                       and not any(w in nxt for w in _NOISE)
                       and not _DATE_RE.search(nxt) and "¥" not in nxt and "￥" not in nxt)
            if ok_next and len(name) + len(nxt) + 1 <= 60:
                name = name + " " + nxt
        out["name"] = name[:60]
    else:
        out["conf"].append("name")

    out["category"] = guess_category(out["name"])
    if not out["category"]:
        out["conf"].append("category")
    return out


def _fmt_date(m: re.Match) -> str:
    y, mo, d = m.group(1), int(m.group(2)), int(m.group(3))
    if not (1 <= mo <= 12 and 1 <= d <= 31):
        return ""
    return f"{y}-{mo:02d}-{d:02d}"


_CAT_WORDS = {
    "数码": ("键盘", "鼠标", "耳机", "相机", "镜头", "手机", "电脑", "显示器", "平板", "硬盘",
             "固态", "路由", "音箱", "充电", "线材", "iPad", "iPhone", "Mac", "显卡", "内存",
             "手表", "手柄", "投影", "录音"),
    "家电": ("冰箱", "洗衣机", "空调", "电饭", "微波炉", "吸尘", "扫地", "风扇", "取暖", "净化",
             "热水壶", "电磁炉", "烤箱"),
    "家居": ("椅子", "书桌", "桌子", "床垫", "台灯", "衣柜", "收纳", "被子", "枕头", "炒锅",
             "餐具", "地毯", "窗帘"),
    "日用": ("牙刷", "牙膏", "纸巾", "洗衣", "清洁", "洗发", "沐浴", "厨房", "垃圾袋", "保鲜"),
    "服饰": ("外套", "羽绒", "T恤", "衬衫", "长裤", "短裤", "运动鞋", "跑鞋", "帽子", "袜子",
             "卫衣", "毛衣"),
    "书籍": ("全集", "文集", "译本", "出版社", "画集", "小说"),
    "食品": ("零食", "咖啡豆", "挂耳", "牛奶", "坚果", "巧克力", "面包", "大米", "茶叶"),
    "出行": ("机票", "火车票", "高铁", "酒店", "打车", "门票", "签证"),
}


def guess_category(name: str) -> str:
    for cat, words in _CAT_WORDS.items():
        if any(w.lower() in (name or "").lower() for w in words):
            return cat
    return ""


# ---------- 收一张截图 ----------

def _run_shot(src: Path, dst: Path) -> dict:
    """跑 Swift 工具：OCR + 裁图。失败也要给结果（宁可没图，不能丢这条记录）。"""
    if not SHOT_TOOL.exists():
        return {"ok": False, "text": _ocr_only(src), "lines": [], "crop_src": "none"}
    try:
        r = subprocess.run([str(SHOT_TOOL), str(src), str(dst)],
                           capture_output=True, timeout=60)
        data = json.loads((r.stdout or b"{}").decode("utf-8", "replace") or "{}")
        if not isinstance(data, dict):
            raise ValueError("输出不是 JSON")
        if not data.get("ok") and not data.get("text"):
            data["text"] = _ocr_only(src)
        return data
    except (OSError, subprocess.SubprocessError, ValueError):
        return {"ok": False, "text": _ocr_only(src), "lines": [], "crop_src": "none"}


def _ocr_only(src: Path) -> str:
    if not OCR_TOOL.exists():
        return ""
    try:
        r = subprocess.run([str(OCR_TOOL), str(src)], capture_output=True, timeout=45)
        line = (r.stdout or b"").decode("utf-8", "replace").strip()
        return line.split("\t", 1)[1] if "\t" in line else line
    except (OSError, subprocess.SubprocessError):
        return ""


def ingest(cfg: dict, raw: bytes, filename: str = "", price: float | None = None) -> dict:
    """存原图 → OCR → 裁产品图 → 落一条记录。返回 {ok, item, guess}。"""
    if not raw:
        return {"ok": False, "error": "空文件"}
    if len(raw) > 30 * 1024 * 1024:
        return {"ok": False, "error": "截图超过 30MB"}
    d = _ensure(cfg)
    day = datetime.now().strftime("%Y%m%d")
    iid = f"p-{day}-{uuid.uuid4().hex[:4]}"
    ext = Path(filename or "").suffix.lower()
    if ext not in (".png", ".jpg", ".jpeg", ".heic", ".webp"):
        ext = ".png"
    src = d / "原图" / f"{iid}{ext}"
    src.write_bytes(raw)

    shot = _run_shot(src, d / "图" / f"{iid}.jpg")
    fields = parse_fields(shot.get("text") or "", shot.get("lines") or [])

    has_crop = (shot.get("crop_src") not in (None, "", "none")
                and (d / "图" / f"{iid}.jpg").exists())
    now = datetime.now().strftime("%Y-%m-%dT%H:%M:%S")
    item = {
        "id": iid,
        "name": fields["name"] or "（待命名）",
        "platform": fields["platform"] or "",
        "bought_at": fields["bought_at"] or datetime.now().strftime("%Y-%m-%d"),
        "price": price if price is not None else (fields["price"] or 0),
        "category": fields["category"] or "其他",
        "shop": fields["shop"],
        "order_no": fields["order_no"],
        "note": "",
        "status": "在用",
        "sold_at": "",
        "sold_price": None,
        "sold_to": "",
        "image": f"{DIR_NAME}/图/{iid}.jpg" if has_crop else "",
        "raw": f"{DIR_NAME}/原图/{src.name}",
        "crop_src": shot.get("crop_src") or "none",
        "ocr": (shot.get("text") or "")[:4000],
        "conf": fields["conf"],
        "created_at": now,
        "updated_at": now,
    }
    book = load(cfg)
    book["items"].insert(0, item)
    save(cfg, book)
    return {"ok": True, "item": item,
            "guess": {k: fields[k] for k in ("name", "platform", "bought_at", "price", "category")},
            "conf": fields["conf"], "crop_src": item["crop_src"]}


# ---------- 改 / 卖 / 删 ----------

# image / raw 只有服务端会写（换图时），放进来是为了让「删记录」能连新图一起收走
EDITABLE = ("name", "platform", "bought_at", "price", "category", "shop", "order_no",
            "note", "status", "sold_at", "sold_price", "sold_to",
            "image", "raw", "crop_src")


def update(cfg: dict, iid: str, patch: dict) -> dict:
    book = load(cfg)
    item = next((i for i in book["items"] if i["id"] == iid), None)
    if not item:
        return {"ok": False, "error": "没有这条记录"}
    for k in EDITABLE:
        if k not in patch:
            continue
        v = patch[k]
        if k in ("price", "sold_price"):
            if v in ("", None):
                item[k] = None if k == "sold_price" else 0
            else:
                try:
                    item[k] = round(float(str(v).replace("¥", "").replace(",", "")), 2)
                except ValueError:
                    return {"ok": False, "error": f"{k} 不是数字"}
        elif k == "status":
            item[k] = "已卖" if str(v) in ("已卖", "sold", "1", "true") else "在用"
        else:
            item[k] = str(v)[:4000 if k == "note" else 200]
    # 状态跟卖出信息互相对齐，避免「已卖但没价钱」的记录混进统计
    if item.get("status") == "已卖":
        if not item.get("sold_at"):
            item["sold_at"] = datetime.now().strftime("%Y-%m-%d")
        if item.get("sold_price") is None:
            item["sold_price"] = 0
    item["updated_at"] = datetime.now().strftime("%Y-%m-%dT%H:%M:%S")
    save(cfg, book)
    return {"ok": True, "item": item}


def remove(cfg: dict, iid: str) -> dict:
    """删除：记录从账本移走，图和原图一起挪进 _回收站（还能捡回来）。"""
    book = load(cfg)
    item = next((i for i in book["items"] if i["id"] == iid), None)
    if not item:
        return {"ok": False, "error": "没有这条记录"}
    d = _ensure(cfg)
    # 括号别省：Path / str + str 会先算出 Path 再做加法，报 TypeError
    bin_ = d / "_回收站" / (datetime.now().strftime("%Y%m%d-%H%M%S") + "-" + iid)
    bin_.mkdir(parents=True, exist_ok=True)
    for key in ("image", "raw"):
        rel = item.get(key) or ""
        if not rel:
            continue
        p = _root(cfg) / rel
        if p.exists():
            try:
                shutil.move(str(p), str(bin_ / p.name))
            except OSError:
                pass
    _atomic_write(bin_ / "item.json", json.dumps(item, ensure_ascii=False, indent=1))
    book["items"] = [i for i in book["items"] if i["id"] != iid]
    save(cfg, book)
    return {"ok": True, "trash": str(bin_)}


def replace_image(cfg: dict, iid: str, raw: bytes, filename: str = "") -> dict:
    """手动换产品图：直接把这张图当产品图（不再裁），原图另存一份。"""
    item = get_item(cfg, iid)
    if not item:
        return {"ok": False, "error": "没有这条记录"}
    if not raw:
        return {"ok": False, "error": "空文件"}
    d = _ensure(cfg)
    ext = Path(filename or "").suffix.lower() or ".png"
    if ext not in (".png", ".jpg", ".jpeg", ".webp", ".heic"):
        ext = ".png"
    tmp = d / "原图" / f"{iid}-换图{ext}"
    tmp.write_bytes(raw)
    # HEIC 之类 sips 先转 JPEG，卡片的 <img> 才认
    dest = d / "图" / f"{iid}.jpg"
    ok = False
    for argv in (["sips", "-Z", "1000", "-s", "format", "jpeg", str(tmp), "--out", str(dest)],
                 ["sips", "-Z", "1000", str(tmp), "--out", str(dest)]):
        try:
            r = subprocess.run(argv, capture_output=True, timeout=30)
            if r.returncode == 0 and dest.exists():
                ok = True
                break
        except (OSError, subprocess.SubprocessError):
            continue
    if not ok:
        shutil.copyfile(tmp, dest)
    return update(cfg, iid, {"image": f"{DIR_NAME}/图/{iid}.jpg",
                             "raw": f"{DIR_NAME}/原图/{tmp.name}", "crop_src": "manual"})


# ---------- 汇总 ----------

def summary(items: list[dict]) -> dict:
    spent = 0.0
    recovered = 0.0
    owned_value = 0.0
    owned = sold = 0
    pnl = 0.0
    for i in items:
        price = float(i.get("price") or 0)
        spent += price
        if i.get("status") == "已卖":
            sold += 1
            sp = float(i.get("sold_price") or 0)
            recovered += sp
            pnl += sp - price
        else:
            owned += 1
            owned_value += price
    return {
        "total": len(items), "owned": owned, "sold": sold,
        "spent": round(spent, 2), "recovered": round(recovered, 2),
        "net": round(spent - recovered, 2), "owned_value": round(owned_value, 2),
        "pnl": round(pnl, 2),
    }


def payload(cfg: dict) -> dict:
    """给工作台卡片用：摘要 + 每件商品的卡片字段（不含 OCR 全文，省流量）。"""
    book = load(cfg)
    items = []
    for i in book["items"]:
        items.append({
            "id": i["id"], "name": i.get("name") or "", "platform": i.get("platform") or "",
            "bought_at": i.get("bought_at") or "", "price": i.get("price") or 0,
            "category": i.get("category") or "其他", "status": i.get("status") or "在用",
            "sold_at": i.get("sold_at") or "", "sold_price": i.get("sold_price"),
            "sold_to": i.get("sold_to") or "",
            "image": i.get("image") or "", "crop_src": i.get("crop_src") or "none",
            "note": (i.get("note") or "")[:400], "conf": i.get("conf") or [],
            "days": _days(i), "pnl": _pnl(i),
        })
    return {"summary": summary(book["items"]), "items": items,
            "dir": str(ledger_dir(cfg)), "updated_at": book["updated_at"]}


def _days(i: dict) -> int:
    try:
        a = datetime.strptime(i.get("bought_at") or "", "%Y-%m-%d")
        b = datetime.strptime(i.get("sold_at") or "", "%Y-%m-%d") if i.get("status") == "已卖" else datetime.now()
        return max(0, (b - a).days)
    except ValueError:
        return 0


def _pnl(i: dict) -> float | None:
    if i.get("status") != "已卖":
        return None
    return round(float(i.get("sold_price") or 0) - float(i.get("price") or 0), 2)


def full_item(cfg: dict, iid: str) -> dict:
    """抽屉用：整条，含 OCR 原文。"""
    i = get_item(cfg, iid)
    if not i:
        return {"ok": False, "error": "没有这条记录"}
    return {"ok": True, "item": i, "ocr": i.get("ocr") or ""}


def image_file(cfg: dict, iid: str, raw: bool = False) -> Path | None:
    """按 id 找图。路由不直接把路径交给 /api/raw：相对路径在服务端 CWD 下解析会越界，
    而且账本里留相对路径，以后挪数据目录也不至于全断。"""
    i = get_item(cfg, iid)
    if not i:
        return None
    rel = (i.get("raw") if raw else i.get("image")) or ""
    if not rel:
        return None
    p = _root(cfg) / rel
    return p if p.is_file() else None
