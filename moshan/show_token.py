#!/usr/bin/env python3
"""打印口袋服务的访问地址和令牌（复制到手机浏览器就能用）。"""
from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

BASE = Path(__file__).resolve().parent
sys.path.insert(0, str(BASE))


def main() -> int:
    cfg_path = BASE / "config.json"
    if not cfg_path.exists():
        print("还没有配置，先运行一次 server.py 生成。")
        return 1
    cfg = json.loads(cfg_path.read_text(encoding="utf-8"))
    token = cfg["token"]
    port = cfg.get("port", 8321)
    scheme = "https" if cfg.get("tls", True) else "http"

    try:
        import tls as tlsmod
        lan, ts = tlsmod.lan_ip(), tlsmod.tailscale_ip()
        ts_name = tlsmod.tailscale_name()
        cert_dir = Path(cfg.get("tls_dir", str(BASE / "certs")))
        trusted = (cert_dir / "tailscale.crt").exists()
    except Exception:  # noqa: BLE001 - 只是打印脚本，不该因此崩
        lan = ts = ts_name = None
        cert_dir, trusted = BASE / "certs", False

    print("口袋服务（全程加密）\n" + "=" * 46)
    if lan:
        print(f"同一 WiFi：{scheme}://{lan}:{port}/?k={token}")
    else:
        print("没拿到局域网地址，检查是否连着 WiFi。")
    if ts:
        print(f"外网(Tailscale)：{scheme}://{ts}:{port}/?k={token}")
        if ts_name:
            print(f"               {scheme}://{ts_name}/?k={token}")
    else:
        print("Tailscale 尚未就绪：装好登录后这里会多一行 100.x 外网地址。")

    print(f"\n令牌：{token}")
    print(f"收件箱：{cfg.get('inbox_dir')}")
    print(f"回收站：{cfg.get('trash_dir')}（保留 {cfg.get('trash_days', 30)} 天）")

    print("\n证书\n" + "-" * 46)
    crt = cert_dir / ("tailscale.crt" if trusted else "server.crt")
    if crt.exists():
        print(f"{crt}")
        if trusted:
            print("Tailscale 签发的公信证书，任何设备都不会提示不安全。")
        else:
            print("自签证书。iPhone 首次访问点「显示详细信息 → 访问此网站」；")
            print("想彻底去掉提示：把这个 .crt 隔空投送到手机 → 设置里安装描述文件 →")
            print("关于本机 → 证书信任设置 → 打开完全信任。")
    else:
        print("还没有证书，服务启动时会自动生成。")

    print("\nWebDAV\n" + "-" * 46)
    if cfg.get("dav_enabled"):
        print(f"已开启：{scheme}://{ts or lan}:{port}/dav（密码用令牌）")
    else:
        print("已关闭（明文 Basic 认证不可接受，且 iOS 文件 App 不支持 WebDAV）。")
        print("要用就改 config.json：\"dav_enabled\": true")
    return 0


if __name__ == "__main__":
    sys.exit(main())
