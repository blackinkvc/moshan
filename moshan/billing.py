# -*- coding: utf-8 -*-
"""计费账号 —— 把「按 token 价表推算」换成「直接问供应商要账」。

为什么要这张卡
    百炼和硅基流动都有充值折扣、代金券、赠送额度、限时调价。用价表乘 token
    算出来的钱，跟账单上的实付永远对不齐，差多少全凭猜。与其把折扣一条条
    补进价表（供应商自己也在改），不如把**账号**录进来，直接读官方的余额和
    账单 —— 那才是真金白银。

能读什么（各家公开接口不一样，不假装都一样）
    阿里百炼   要 AccessKey（AK/SK）。BSS OpenAPI 能读账户余额 + 按产品拆的
               月度账单（原价 / 优惠 / 实付三项都有，正好能看出折扣和赠送）。
               只给百炼 API Key 的话，读不了账 —— 那个 key 只能调模型。
    硅基流动   API Key 读 /v1/user/info：总余额 / 充值余额 / 赠送余额分得清。
               官方没有按月账单接口，只能看余额；所以这里记每次拉取的余额，
               用余额差看「这段时间花了多少」。
    DeepSeek   /user/balance：总余额 / 赠送余额 / 充值余额。
    月之暗面   /v1/users/me/balance：可用 / 代金券 / 现金。
    OpenRouter /api/v1/credits：额度与已用（美元）。
    手动录入   上面都没有接口的，自己填余额和本月账单，一样进同一张表。

存哪儿
    ~/Documents/AI项目/_本机工作/计费账号.json（chmod 600）
    密钥只在本机、只进这个文件；给页面的一律打码，只露末四位。
"""
from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from datetime import datetime, timedelta
from pathlib import Path

import paths

OWN_DIR = paths.ensure(paths.data_root() / "_本机工作")
STORE = OWN_DIR / "计费账号.json"

UA = "cuntu-billing/1.0"
TIMEOUT = 12

# 供应商登记表。fields 的顺序就是表单里从上到下的顺序；
# secret=1 的字段给页面时打码，写盘时保留原值（用户不改就不重填）。
PROVIDERS: dict[str, dict] = {
    "bailian": {
        "name": "阿里百炼", "short": "百炼", "vendor": "阿里云",
        "bill": True,
        "fields": [
            {"k": "ak", "label": "AccessKey ID", "ph": "LTAI5t…", "hint": "阿里云控制台 → 右上头像 → AccessKey 管理"},
            {"k": "sk", "label": "AccessKey Secret", "ph": "（只在本机保存）", "secret": 1},
            {"k": "key", "label": "百炼 API Key（可选）", "ph": "sk-…", "secret": 1, "opt": 1,
             "hint": "填了也读不了账，只用来验这个号是不是百炼的"},
        ],
        "note": "AK 需要 AliyunBSSReadOnlyAccess 权限；账单按产品拆，自动挑出「百炼 / 灵积」那一行。",
    },
    "silicon": {
        "name": "硅基流动", "short": "硅基", "vendor": "硅基流动",
        "bill": False,
        "fields": [
            {"k": "key", "label": "API Key", "ph": "sk-…", "secret": 1,
             "hint": "siliconflow.cn → API 密钥"},
        ],
        "note": "官方只给余额（总 / 充值 / 赠送三档），没有按月账单；这里存每次拉取的余额，用余额差看消耗。",
    },
    "deepseek": {
        "name": "DeepSeek", "short": "DS", "vendor": "深度求索",
        "bill": False,
        "fields": [{"k": "key", "label": "API Key", "ph": "sk-…", "secret": 1}],
        "note": "只给余额（总 / 赠送 / 充值），没有账单接口。",
    },
    "moonshot": {
        "name": "月之暗面 Kimi", "short": "Kimi", "vendor": "月之暗面",
        "bill": False,
        "fields": [{"k": "key", "label": "API Key", "ph": "sk-…", "secret": 1}],
        "note": "余额分可用 / 代金券 / 现金三档。",
    },
    "openrouter": {
        "name": "OpenRouter", "short": "OR", "vendor": "OpenRouter",
        "bill": False, "usd": True,
        "fields": [{"k": "key", "label": "API Key", "ph": "sk-or-…", "secret": 1}],
        "note": "美元计。给的是「额度 − 已用」，没有按月账单。",
    },
    "manual": {
        "name": "手动录入", "short": "手录", "vendor": "自定义",
        "bill": True, "offline": True,
        "fields": [
            {"k": "balance", "label": "余额", "ph": "0.00", "num": 1},
            {"k": "month", "label": "本月账单", "ph": "0.00", "num": 1},
            {"k": "cur", "label": "币种", "ph": "CNY", "opt": 1},
        ],
        "note": "没接口的供应商（智谱 / 火山 / 腾讯混元…）用这档：账单页上抄两个数进来，一样能对账。",
    },
}

# 供应商 → cherry_usage 里的 vendor_id，用来跟本地估算对账
EST_KEY = {"bailian": "dashscope", "silicon": "silicon"}


# ---------------------------------------------------------------- 存

def _read() -> dict:
    try:
        d = json.loads(STORE.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {"accounts": []}
    if not isinstance(d, dict):
        return {"accounts": []}
    if not isinstance(d.get("accounts"), list):
        d["accounts"] = []
    return d


def _write(d: dict) -> None:
    OWN_DIR.mkdir(parents=True, exist_ok=True)
    tmp = STORE.with_suffix(".json.tmp")
    tmp.write_text(json.dumps(d, ensure_ascii=False, indent=1), encoding="utf-8")
    try:
        os.chmod(tmp, 0o600)          # 里面有 AccessKey，别让同机别的账号读到
    except OSError:
        pass
    tmp.replace(STORE)


def mask(v: str) -> str:
    s = str(v or "")
    if not s:
        return ""
    if len(s) <= 8:
        return "•" * max(2, len(s) - 2) + s[-2:]
    return s[:4] + "…" + s[-4:]


def _num(v) -> float | None:
    if v is None or v == "":
        return None
    try:
        return round(float(v), 2)
    except (TypeError, ValueError):
        return None


def _public(acc: dict) -> dict:
    """给页面的那一份：密钥全打码，历史只留近 60 条。"""
    prov = PROVIDERS.get(acc.get("provider") or "", {})
    f = {}
    for spec in prov.get("fields", []):
        k = spec["k"]
        v = (acc.get("fields") or {}).get(k)
        if spec.get("secret"):
            f[k] = mask(v) if v else ""
        else:
            f[k] = v if v is not None else ""
    return {
        "id": acc.get("id") or "",
        "provider": acc.get("provider") or "",
        "provider_name": prov.get("name") or acc.get("provider") or "?",
        # 徽标只有一个字，用 short（百炼 / 硅基 / DS …）比全名首字好认
        "provider_short": prov.get("short") or (prov.get("name") or "?")[:1],
        "name": acc.get("name") or "",
        "note": acc.get("note") or "",
        "fields": f,
        "filled": {s["k"]: bool((acc.get("fields") or {}).get(s["k"])) for s in prov.get("fields", [])},
        "last": acc.get("last") or {},
        "hist": (acc.get("hist") or [])[-60:],
        "can_pull": bool(prov),
        "bill": bool(prov.get("bill")),
        "usd": bool(prov.get("usd")),
    }


def providers_payload() -> dict:
    return {"ok": True, "items": [
        {"id": k, "name": v["name"], "vendor": v["vendor"], "bill": bool(v.get("bill")),
         "offline": bool(v.get("offline")), "usd": bool(v.get("usd")),
         "fields": v["fields"], "note": v["note"]}
        for k, v in PROVIDERS.items()]}


# ---------------------------------------------------------------- 改

def save_account(body: dict) -> dict:
    """新建或改一个账号。id 为空就是新建。

    密钥字段：前端回来的是打码串（含「…」或「•」）就表示「没改」，保留原值。
    这样用户只改名字、不动密钥时不会被打码串覆盖成废值。
    """
    d = _read()
    accs = d["accounts"]
    pid = str(body.get("id") or "").strip()
    prov = str(body.get("provider") or "").strip()
    if prov not in PROVIDERS:
        return {"ok": False, "error": "供应商不认识"}
    cur = next((a for a in accs if a.get("id") == pid), None)
    if cur is None:
        prov_spec = PROVIDERS[prov]
        name = str(body.get("name") or "").strip() or prov_spec["name"]
        if any(a.get("name") == name and a.get("provider") == prov for a in accs):
            name = name + " 2"
        cur = {"id": uuid.uuid4().hex[:8], "provider": prov, "name": name,
               "note": "", "fields": {}, "last": {}, "hist": []}
        accs.append(cur)
    if cur.get("provider") != prov:
        cur["provider"] = prov
        cur["last"] = {}                     # 换了供应商，上次的账不作数
    if str(body.get("name") or "").strip():
        cur["name"] = str(body["name"]).strip()[:40]
    if "note" in body:
        cur["note"] = str(body.get("note") or "").strip()[:120]

    specs = {s["k"]: s for s in PROVIDERS[prov]["fields"]}
    nf = dict(cur.get("fields") or {})
    for k, v in (body.get("fields") or {}).items():
        if k not in specs:
            continue
        v = str(v or "").strip()
        if specs[k].get("secret") and ("…" in v or "•" in v):
            continue                          # 打码串 = 没改
        nf[k] = v
    cur["fields"] = nf

    miss = [s["label"] for s in specs.values()
            if not s.get("opt") and not s.get("num") and not nf.get(s["k"])]
    if miss:
        # 不写盘：存一条拉不到账的账号，比不存更让人困惑（页面上一行红字，还占着位置）
        return {"ok": False, "error": "还缺 " + "、".join(miss)}
    _write(d)
    return {"ok": True, "account": _public(cur)}


def del_account(aid: str) -> dict:
    d = _read()
    n = len(d["accounts"])
    d["accounts"] = [a for a in d["accounts"] if a.get("id") != aid]
    _write(d)
    return {"ok": True, "removed": n - len(d["accounts"])}


# ---------------------------------------------------------------- 网络

def _http(url: str, headers: dict | None = None, data: bytes | None = None) -> tuple[int, str]:
    req = urllib.request.Request(url, data=data, headers={"User-Agent": UA, **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=TIMEOUT) as r:
            return r.status, r.read(200000).decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read(20000).decode("utf-8", "replace")
    except Exception as e:                     # noqa: BLE001  连不上/超时/被墙
        return 0, f"{type(e).__name__}: {e}"


def _jget(url: str, key: str = "") -> tuple[dict | None, str]:
    code, txt = _http(url, {"Authorization": "Bearer " + key} if key else {})
    if code == 0:
        return None, txt
    try:
        return json.loads(txt), ""
    except ValueError:
        return None, f"HTTP {code}：{txt[:120]}"


def _err_of(obj: dict) -> str:
    """各家错误文案的字段名都不一样，挨个看。"""
    if not obj:
        return ""
    e = obj.get("error")
    if isinstance(e, dict):
        return str(e.get("message") or e.get("code") or "")
    if isinstance(e, str):
        return e
    for k in ("message", "msg", "Message", "detail", "code"):
        if obj.get(k):
            return str(obj[k])
    return ""


# ---- 阿里云 BSS：RPC 签名 v1（HMAC-SHA1），business.aliyuncs.com 实测可用
def _pe(s) -> str:
    return (urllib.parse.quote(str(s), safe="")
            .replace("+", "%20").replace("*", "%2A").replace("%7E", "~"))


def _ali(ak: str, sk: str, action: str, extra: dict | None = None) -> tuple[dict | None, str]:
    p = {"Action": action, "Format": "JSON", "Version": "2017-12-14",
         "AccessKeyId": ak, "SignatureMethod": "HMAC-SHA1",
         "SignatureVersion": "1.0", "SignatureNonce": uuid.uuid4().hex,
         "Timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    p.update(extra or {})
    q = "&".join(f"{_pe(k)}={_pe(p[k])}" for k in sorted(p))
    sts = "GET&%2F&" + _pe(q)
    sig = base64.b64encode(hmac.new((sk + "&").encode(), sts.encode("utf-8"),
                                   hashlib.sha1).digest()).decode()
    url = "https://business.aliyuncs.com/?" + q + "&Signature=" + _pe(sig)
    obj, err = _jget(url)
    if err:
        return None, err
    if obj and obj.get("Code"):
        return None, f"{obj.get('Code')}：{obj.get('Message') or ''}"
    return obj, ""


def _f(v) -> float:
    try:
        return float(v or 0)
    except (TypeError, ValueError):
        return 0.0


def _bill_month(ak: str, sk: str, ym: str) -> tuple[dict | None, str]:
    """取某一个账期（YYYY-MM）的账单，按产品拆。返回 (汇总, 错误)。"""
    bill, berr = _ali(ak, sk, "QueryAccountBill",
                      {"BillingCycle": ym, "IsGroupByProduct": "true",
                       "Granularity": "MONTHLY", "PageNum": "1", "PageSize": "300"})
    if berr:
        return None, berr
    bd = (bill or {}).get("Data") or {}
    items = ((bd.get("Items") or {}).get("Item") or [])
    if isinstance(items, dict):
        items = [items]
    gross = net = disc = 0.0
    rows = []
    for it in items:
        g = _f(it.get("PretaxGrossAmount"))
        d_ = (_f(it.get("DeductedByCoupons")) + _f(it.get("InvoiceDiscount"))
              + _f(it.get("DeductedByPrepaidCard")) + _f(it.get("DeductedByCashCoupons")))
        n = _f(it.get("PaymentAmount")) or max(g - d_, 0.0)
        nm = str(it.get("ProductName") or it.get("ProductCode") or "?")
        gross += g
        disc += d_
        net += n
        rows.append({"name": nm, "gross": round(g, 2), "disc": round(d_, 2),
                     "net": round(n, 2), "code": it.get("ProductCode") or ""})
    rows.sort(key=lambda r: -r["net"])
    # 百炼这一行单独拎出来：产品名历史上叫过「灵积」「大模型服务平台百炼」
    hit = [r for r in rows if any(w in r["name"] for w in ("百炼", "灵积", "DashScope", "dashscope"))]
    scoped = rows
    if hit:
        scoped = hit
    return {"ym": bd.get("BillingCycle") or ym,
            "net": round(sum(r["net"] for r in scoped), 2),
            "gross": round(sum(r["gross"] for r in scoped), 2),
            "disc": round(sum(r["disc"] for r in scoped), 2),
            "acct_net": round(net, 2),          # 整个阿里云账号的账单，不只百炼
            "scoped": "百炼" if hit else "全账号",
            "items": rows[:8]}, ""


def pull_bailian(a: dict) -> dict:
    f = a.get("fields") or {}
    ak, sk = (f.get("ak") or "").strip(), (f.get("sk") or "").strip()
    if not (ak and sk):
        return {"ok": False, "err": "要 AccessKey（AK + SK）。只用百炼 API Key 读不了账 —— 那个 key 只能调模型。"}
    bal, err = _ali(ak, sk, "QueryAccountBalance")
    if err:
        return {"ok": False, "err": err}
    data = (bal or {}).get("Data") or {}
    out = {"ok": True,
           "balance": _num(data.get("AvailableAmount")),
           "cash": _num(data.get("AvailableCashAmount")),
           "gift": None,
           "credit": _num(data.get("CreditAmount")),
           "currency": data.get("Currency") or "CNY",
           "items": []}
    # 账单：最近四个账期各取一次。只取本月的话，月初进来永远是 0，
    # 「上个月到底花了多少」反而看不到 —— 而账单常常是事后才看。
    # 读不到不算失败：余额已经拿到了，多半是 AK 少了 BSS 只读权限，
    # 页面照常显示余额，账单那栏说明原因。
    now = datetime.now()
    yms, _d = [], now
    for _ in range(4):
        yms.append(_d.strftime("%Y-%m"))
        _d = _d.replace(day=1) - timedelta(days=1)
    got, first_err = [], ""
    for ym in yms:
        r, berr = _bill_month(ak, sk, ym)
        if berr:
            first_err = first_err or berr
            continue
        if r:
            got.append(r)
    if first_err and not got:
        out["bill_err"] = first_err
        return out
    out["months"] = got
    cur = next((r for r in got if r["ym"] == yms[0]), None)
    paid = next((r for r in got if (r.get("net") or 0) > 0), None)
    out.update({"month": (cur or {}).get("net"), "month_ym": (cur or {}).get("ym") or yms[0],
                "month_gross": (cur or {}).get("gross"),
                "month_disc": (cur or {}).get("disc"),
                "acct_net": (cur or {}).get("acct_net"),
                "scoped": (cur or {}).get("scoped"),
                "items": (cur or {}).get("items") or []})
    if paid and paid["ym"] != yms[0]:
        out["paid"] = paid                     # 「最近一个真出账的月份」，卡片上要写出来
    if first_err:
        out["bill_err"] = first_err
    return out


def pull_silicon(a: dict) -> dict:
    key = ((a.get("fields") or {}).get("key") or "").strip()
    if not key:
        return {"ok": False, "err": "缺 API Key"}
    obj, err = _jget("https://api.siliconflow.cn/v1/user/info", key)
    if err:
        return {"ok": False, "err": err}
    if not obj or obj.get("code") not in (20000, 0, None) or obj.get("status") is False:
        return {"ok": False, "err": _err_of(obj) or "接口没认这个 key"}
    d = obj.get("data") or {}
    return {"ok": True, "currency": "CNY",
            "balance": _num(d.get("totalBalance")),
            "gift": _num(d.get("balance")),            # 赠送余额
            "cash": _num(d.get("chargeBalance")),      # 充值余额
            "who": d.get("name") or "", "items": []}


def pull_deepseek(a: dict) -> dict:
    key = ((a.get("fields") or {}).get("key") or "").strip()
    if not key:
        return {"ok": False, "err": "缺 API Key"}
    obj, err = _jget("https://api.deepseek.com/user/balance", key)
    if err:
        return {"ok": False, "err": err}
    if not obj or obj.get("error"):
        return {"ok": False, "err": _err_of(obj) or "接口没认这个 key"}
    bi = (obj.get("balance_infos") or [{}])
    d = bi[0] if bi else {}
    return {"ok": True, "currency": d.get("currency") or "CNY",
            "balance": _num(d.get("total_balance")),
            "gift": _num(d.get("granted_balance")),
            "cash": _num(d.get("topped_up_balance")),
            "alive": bool(obj.get("is_available")), "items": []}


def pull_moonshot(a: dict) -> dict:
    key = ((a.get("fields") or {}).get("key") or "").strip()
    if not key:
        return {"ok": False, "err": "缺 API Key"}
    obj, err = _jget("https://api.moonshot.cn/v1/users/me/balance", key)
    if err:
        return {"ok": False, "err": err}
    if not obj or obj.get("error"):
        return {"ok": False, "err": _err_of(obj) or "接口没认这个 key"}
    d = obj.get("data") or {}
    return {"ok": True, "currency": "CNY",
            "balance": _num(d.get("available_balance")),
            "gift": _num(d.get("voucher_balance")),
            "cash": _num(d.get("cash_balance")), "items": []}


def pull_openrouter(a: dict) -> dict:
    key = ((a.get("fields") or {}).get("key") or "").strip()
    if not key:
        return {"ok": False, "err": "缺 API Key"}
    obj, err = _jget("https://openrouter.ai/api/v1/credits", key)
    if err:
        return {"ok": False, "err": err}
    if not obj or obj.get("error"):
        return {"ok": False, "err": _err_of(obj) or "接口没认这个 key"}
    d = obj.get("data") or {}
    cr, us = _num(d.get("total_credits")), _num(d.get("total_usage"))
    return {"ok": True, "currency": "USD", "usd": True,
            "balance": round((cr or 0) - (us or 0), 2),
            "spent": us, "credit": cr, "items": []}


def pull_manual(a: dict) -> dict:
    f = a.get("fields") or {}
    cur = (f.get("cur") or "CNY").strip().upper()[:4]
    return {"ok": True, "currency": cur, "manual": True,
            "balance": _num(f.get("balance")),
            "month": _num(f.get("month")),
            "month_ym": datetime.now().strftime("%Y-%m"), "items": []}


PULLERS = {"bailian": pull_bailian, "silicon": pull_silicon, "deepseek": pull_deepseek,
           "moonshot": pull_moonshot, "openrouter": pull_openrouter,
           "manual": pull_manual}


def pull(aid: str = "", ids: list | None = None) -> dict:
    """拉一次账。aid / ids 都为空就拉全部（手动录入那种不联网，只是回填原值）。

    ids 是为了「只拉过期的几个」：页面上有一个 6 小时没拉，不该顺手把另外十个
    也拉一遍 —— 每个账号都是一次真实出网请求，有的还带配额。
    """
    d = _read()
    want = {str(x) for x in (ids or []) if x}
    todo = [a for a in d["accounts"]
            if (not aid and not want) or a.get("id") == aid or a.get("id") in want]
    if (aid or want) and not todo:
        return {"ok": False, "error": "没这个账号"}
    done = []
    for a in todo:
        prov = a.get("provider") or ""
        fn = PULLERS.get(prov)
        t0 = time.time()
        if not fn:
            res = {"ok": False, "err": "这个供应商还没有取数实现"}
        else:
            try:
                res = fn(a)
            except Exception as e:             # noqa: BLE001  单个账号炸了不许影响别的
                res = {"ok": False, "err": f"{type(e).__name__}: {e}"}
        res["at"] = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        res["ms"] = int((time.time() - t0) * 1000)
        res["provider"] = prov
        # 失败也记一笔（只是不记余额），页面上要看得出「上次什么时候试过、为什么不行」
        a["last"] = res
        if res.get("ok") and res.get("balance") is not None:
            hist = a.get("hist") or []
            hist.append({"t": int(time.time()), "b": res.get("balance"),
                         "m": res.get("month")})
            a["hist"] = hist[-120:]
        done.append({"id": a.get("id"), **res})
    _write(d)
    return {"ok": True, "items": done}


# ---------------------------------------------------------------- 读

def _delta(a: dict) -> dict:
    """余额历史里能读出什么：上次差多少、这个月掉了多少。

    充值会让余额上涨，所以「本月掉了多少」只有在没有充值的前提下才等于花掉的钱；
    页面照实写「余额变化」，不硬说是消费。
    """
    hist = a.get("hist") or []
    out: dict = {}
    if len(hist) >= 2:
        prev = hist[-2]
        out["d_prev"] = round((hist[-1].get("b") or 0) - (prev.get("b") or 0), 2)
        out["t_prev"] = prev.get("t")
    if hist:
        ym = datetime.now().strftime("%Y-%m")
        month = [h for h in hist if datetime.fromtimestamp(h.get("t") or 0).strftime("%Y-%m") == ym]
        base = month[0] if month else hist[0]
        out["d_month"] = round((hist[-1].get("b") or 0) - (base.get("b") or 0), 2)
        out["t_month"] = base.get("t")
        out["n"] = len(hist)
    return out


def view(est: dict | None = None) -> dict:
    """页面用的整份：账号清单 + 对账（官方账单 vs 本地按 token 估算）。"""
    d = _read()
    est = est or {}
    rows = []
    for a in d["accounts"]:
        p = _public(a)
        p["delta"] = _delta(a)
        key = EST_KEY.get(p["provider"])
        e = est.get(key) if key else None
        last = a.get("last") or {}
        if e and (e.get("by_ym") or {}):
            # 用「最近一个真出账的月份」去对本地估算：本月常常还没出账（0），
            # 拿 0 去比会得出一个假的「估算高了 100%」。
            months = last.get("months") or []
            paid = last.get("paid") or next(
                (m for m in months if (m.get("net") or 0) > 0), None)
            if paid:
                ym = paid.get("ym") or ""
                base = (e["by_ym"] or {}).get(ym)
                p["est"] = {"cny": round(float((base or {}).get("cny") or 0), 2),
                            "calls": (base or {}).get("calls") or 0,
                            "ym": ym, "known": base is not None}
                if base is not None:
                    p["gap"] = round(float(paid.get("net") or 0) - p["est"]["cny"], 2)
        rows.append(p)
    # 手动录入那档的余额要参与合计，但币种不同不能相加 —— 只合计人民币
    cny = [r for r in rows if not r.get("usd")]
    return {
        "ok": True,
        "accounts": rows,
        "store": str(STORE),
        "n": len(rows),
        "total_balance": round(sum((r.get("last") or {}).get("balance") or 0
                                   for r in cny
                                   if (r.get("last") or {}).get("ok")), 2),
        "total_month": round(sum(float((r.get("last") or {}).get("month") or 0)
                                 for r in cny
                                 if (r.get("last") or {}).get("ok")), 2),
        "at": datetime.now().strftime("%H:%M:%S"),
    }


if __name__ == "__main__":
    import sys
    cmd = (sys.argv[1] if len(sys.argv) > 1 else "view")
    if cmd == "view":
        print(json.dumps(view(), ensure_ascii=False, indent=1))
    elif cmd == "pull":
        print(json.dumps(pull(sys.argv[2] if len(sys.argv) > 2 else ""),
                         ensure_ascii=False, indent=1))
    elif cmd == "providers":
        print(json.dumps(providers_payload(), ensure_ascii=False, indent=1))
