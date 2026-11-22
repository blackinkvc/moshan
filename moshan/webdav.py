#!/usr/bin/env python3
"""WebDAV 端点：让 iPhone「文件」App、Mac Finder、Windows 资源管理器
把本机当成一块网络盘直接挂载。

这是 NAS 最有价值的一层 —— 挂上去之后，手机传文件不需要任何自定义界面，
系统文件 App 里拖拽、预览、改名、新建文件夹全部原生可用。

只实现基础 Class 1 + 部分 Class 2（LOCK 返回一个假锁，够 Finder 写入就行）。
"""
from __future__ import annotations

import base64
import binascii
import os
import shutil
import time
from email.utils import formatdate
from pathlib import Path
from urllib.parse import quote, unquote, urlparse

from fastapi import APIRouter, Request
from fastapi.responses import FileResponse, HTMLResponse, Response

import storage

DAV_METHODS = ["OPTIONS", "HEAD", "GET", "PUT", "DELETE", "PROPFIND",
               "PROPPATCH", "MKCOL", "MOVE", "COPY", "LOCK", "UNLOCK"]

XML_HEAD = '<?xml version="1.0" encoding="utf-8"?>\n<D:multistatus xmlns:D="DAV:">'
XML_TAIL = "</D:multistatus>"


def build_router(token: str, root: Path) -> APIRouter:
    router = APIRouter(include_in_schema=False)
    root = root.resolve()

    def authorized(request: Request) -> bool:
        if request.query_params.get("k") == token:
            return True
        raw = request.headers.get("authorization", "")
        if not raw.lower().startswith("basic "):
            return False
        try:
            pair = base64.b64decode(raw.split(" ", 1)[1]).decode("utf-8", "ignore")
        except (binascii.Error, ValueError):
            return False
        _, _, pw = pair.partition(":")
        return pw == token or pair == token

    def need_auth() -> Response:
        return Response(status_code=401,
                        headers={"WWW-Authenticate": 'Basic realm="pocket"'})

    def resolve(rel: str) -> Path | None:
        """把 URL 里的相对路径映射到磁盘，越界返回 None。"""
        rel = unquote(rel or "").lstrip("/")
        p = (root / rel)
        try:
            p = p.resolve()
        except (OSError, RuntimeError):
            return None
        if p != root and root not in p.parents:
            return None
        return p

    def href_for(p: Path) -> str:
        try:
            rel = p.relative_to(root).as_posix()
        except ValueError:
            rel = ""
        h = "/dav/" + (rel if rel and rel != "." else "")
        if p.is_dir() and not h.endswith("/"):
            h += "/"
        return quote(h, safe="/~!*'()@:;,")

    def prop_xml(p: Path) -> str:
        try:
            st = p.stat()
        except OSError:
            return ""
        is_dir = p.is_dir()
        size = 0 if is_dir else st.st_size
        rtype = "<D:collection/>" if is_dir else ""
        ctype = "" if is_dir else f"<D:getcontenttype>{storage.mime_of(p)}</D:getcontenttype>"
        return (
            "<D:response>"
            f"<D:href>{href_for(p)}</D:href>"
            "<D:propstat><D:prop>"
            f"<D:displayname>{_x(p.name)}</D:displayname>"
            f"<D:getcontentlength>{size}</D:getcontentlength>"
            f"<D:getlastmodified>{formatdate(st.st_mtime, usegmt=True)}</D:getlastmodified>"
            f"<D:creationdate>{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(st.st_ctime))}</D:creationdate>"
            f"<D:resourcetype>{rtype}</D:resourcetype>"
            f"{ctype}"
            "<D:supportedlock><D:lockentry><D:lockscope><D:exclusive/></D:lockscope>"
            "<D:locktype><D:write/></D:locktype></D:lockentry></D:supportedlock>"
            "</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>"
            "</D:response>"
        )

    def _x(s: str) -> str:
        return (s.replace("&", "&amp;").replace("<", "&lt;")
                 .replace(">", "&gt;").replace('"', "&quot;"))

    async def handle(request: Request, rel: str) -> Response:
        if not authorized(request):
            return need_auth()
        p = resolve(rel)
        if p is None:
            return Response(status_code=403)
        method = request.method.upper()

        if method == "OPTIONS":
            return Response(status_code=200, headers={
                "DAV": "1, 2", "MS-Author-Via": "DAV",
                "Allow": ", ".join(DAV_METHODS)})

        if method == "PROPFIND":
            if not p.exists():
                return Response(status_code=404)
            depth = request.headers.get("depth", "1").lower()
            parts = [prop_xml(p)]
            if depth != "0" and p.is_dir():
                try:
                    with os.scandir(p) as it:
                        for e in it:
                            if e.name.startswith("."):
                                continue
                            parts.append(prop_xml(Path(e.path)))
                except OSError:
                    pass
            body = XML_HEAD + "".join(parts) + XML_TAIL
            return Response(content=body, status_code=207,
                            media_type='application/xml; charset="utf-8"')

        if method == "PROPPATCH":
            return Response(content=XML_HEAD + prop_xml(p) + XML_TAIL,
                            status_code=207,
                            media_type='application/xml; charset="utf-8"')

        if method in ("LOCK", "UNLOCK"):
            if method == "LOCK":
                lock = (XML_HEAD.replace("multistatus", "prop")
                        + "<D:lockdiscovery><D:activelock>"
                        "<D:locktype><D:write/></D:locktype>"
                        "<D:lockscope><D:exclusive/></D:lockscope><D:depth>infinity</D:depth>"
                        "<D:timeout>Second-3600</D:timeout>"
                        "<D:locktoken><D:href>opaquelocktoken:pocket-lock</D:href></D:locktoken>"
                        "</D:activelock></D:lockdiscovery></D:prop>")
                return Response(content=lock, status_code=200, headers={
                    "Lock-Token": "<opaquelocktoken:pocket-lock>"})
            return Response(status_code=204)

        if method in ("GET", "HEAD"):
            if not p.exists():
                return Response(status_code=404)
            if p.is_dir():
                try:
                    names = sorted(x.name for x in p.iterdir() if not x.name.startswith("."))
                except OSError:
                    names = []
                links = "".join(
                    f'<li><a href="{quote(n)}">{_x(n)}</a></li>' for n in names)
                return HTMLResponse(f"<h3>{_x(p.name)}</h3><ul>{links}</ul>")
            if method == "HEAD":
                st = p.stat()
                return Response(status_code=200, headers={
                    "Content-Length": str(st.st_size),
                    "Content-Type": storage.mime_of(p),
                    "Last-Modified": formatdate(st.st_mtime, usegmt=True),
                    "Accept-Ranges": "bytes"})
            return FileResponse(p, media_type=storage.mime_of(p),
                                filename=p.name if request.query_params.get("dl") else None)

        if method == "PUT":
            if p.is_dir():
                return Response(status_code=405)
            existed = p.exists()
            try:
                p.parent.mkdir(parents=True, exist_ok=True)
                with p.open("wb") as f:
                    async for chunk in request.stream():
                        f.write(chunk)
            except OSError:
                return Response(status_code=500)
            return Response(status_code=204 if existed else 201)

        if method == "MKCOL":
            if p.exists():
                return Response(status_code=405)
            try:
                p.mkdir(parents=True)
            except OSError:
                return Response(status_code=409)
            return Response(status_code=201)

        if method == "DELETE":
            if not p.exists():
                return Response(status_code=404)
            if storage.is_trash(p):
                # 回收站本身不允许通过挂载点删除
                return Response(status_code=403)
            try:
                storage.delete_to_trash([p])
            except Exception:
                return Response(status_code=403)
            return Response(status_code=204)

        if method in ("MOVE", "COPY"):
            dest_raw = request.headers.get("destination", "")
            if not dest_raw:
                return Response(status_code=400)
            dest_url = urlparse(dest_raw).path
            dest_rel = dest_url.split("/dav", 1)[-1] if "/dav" in dest_url else dest_url
            dest = resolve(dest_rel)
            if dest is None:
                return Response(status_code=403)
            if not p.exists():
                return Response(status_code=404)
            overwrite = request.headers.get("overwrite", "T").upper() != "F"
            existed = dest.exists()
            if existed and not overwrite:
                return Response(status_code=412)
            try:
                dest.parent.mkdir(parents=True, exist_ok=True)
                if existed:
                    shutil.rmtree(dest) if dest.is_dir() else dest.unlink()
                if method == "MOVE":
                    shutil.move(str(p), str(dest))
                else:
                    shutil.copytree(p, dest) if p.is_dir() else shutil.copy2(p, dest)
            except OSError:
                return Response(status_code=500)
            return Response(status_code=204 if existed else 201)

        return Response(status_code=405, headers={"Allow": ", ".join(DAV_METHODS)})

    @router.api_route("/dav", methods=DAV_METHODS)
    async def dav_root(request: Request) -> Response:
        return await handle(request, "")

    @router.api_route("/dav/{rel:path}", methods=DAV_METHODS)
    async def dav_any(request: Request, rel: str) -> Response:
        return await handle(request, rel)

    return router
