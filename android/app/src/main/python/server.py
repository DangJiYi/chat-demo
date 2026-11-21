#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""dsh-chat 的本地服务：只用标准库，不需要 pip 装任何东西。

做四件事：
  1. 把 www/ 里的静态页发出去，顺便把发往 api.deepseek.com 的请求代理掉（Key 只在服务端）；
  2. 会话、附件、导入暂存都落在 data/ 目录，浏览器里不留副本；
  3. 算今日/本月花费、查余额、判峰谷；
  4. 局域网增量同步（每 N 秒互拉一次）、超期会话归档、历史全文搜索。

用法:
  python server.py                  # 默认 8790 / 8791，数据放 ./data，自动开窗口
  python server.py --no-window      # 只跑服务
  python server.py --port 8890 --sync-port 8891 --data D:/tmp/x
"""

import argparse
import gzip
import hashlib
import http.client
import json
import os
import re
import shutil
import socket
import socketserver
import ssl
import sys
import threading
import time
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

VERSION = "0.2.0"
BEIJING = timezone(timedelta(hours=8))
UPSTREAM = "api.deepseek.com"
SENTINEL = "__server__"

# 打包成 exe 后：资源在 sys._MEIPASS，数据目录放在 exe 同级
FROZEN = bool(getattr(sys, "frozen", False))
BASE_DIR = os.path.dirname(sys.executable) if FROZEN else os.path.dirname(os.path.abspath(__file__))
HERE = BASE_DIR
WWW = os.path.join(getattr(sys, "_MEIPASS", BASE_DIR), "www")

# 峰谷与价目（人民币 / 1M tokens），口径同 dsh-cost-meter
PEAK_HOURS = ((9, 12), (14, 18))
HOLIDAYS = {
    "2026-01-01", "2026-02-16", "2026-02-17", "2026-02-18", "2026-02-19",
    "2026-02-20", "2026-02-21", "2026-02-22",
    "2026-04-04", "2026-04-05", "2026-04-06",
    "2026-05-01", "2026-05-02", "2026-05-03", "2026-05-04", "2026-05-05",
    "2026-06-19", "2026-06-20", "2026-06-21",
    "2026-09-25", "2026-09-26", "2026-09-27",
    "2026-10-01", "2026-10-02", "2026-10-03", "2026-10-04",
    "2026-10-05", "2026-10-06", "2026-10-07",
}
PRICES = {
    "deepseek-flash": (0.04, 2.00, 8.00),      # (缓存命中, 未命中, 输出)
    "deepseek-v4-pro": (0.30, 9.00, 27.00),
}

MAX_SESSION = 32 << 20
MAX_FILE = 128 << 20
LOCK = threading.RLock()

# 拉取时向前回看的窗口。
#
# 前端只在“发问”时推进 updatedAt，回答完成时不再更新这个字段
# （chat-demo-main/src/services/chat/store.ts: streamReply 结束时只 persist，不碰 updatedAt）。
# 于是对端可能先拉到“只有问题、回答 pending”的快照，之后回答完成、内容变了但时间戳没变，
# 按时间戳过滤就永远不会再发送 —— 表现为“同步后只看到问题，拿不到回答”。
# 用回看窗口 + 幂等合并解决，不依赖前端改时间戳。
SYNC_LOOKBACK_MS = 15 * 60 * 1000

# app.log 大小上限；超过就只保留后半部分，避免手机占用无限增长
LOG_MAX_BYTES = 1 << 20

# UDP 自动发现：不用手填对端 IP。
# 一台广播，另一台应答，列表里点一下即完成配对（两边都配好）。
# 只在局域网用，与同步端口 8791（本来就无鉴权）同属一个信任模型。
DISCOVERY_PORT = 8792
DISCOVER_MAGIC = b"DSH-CHAT-DISCOVER/1"
DISCOVER_REPLY = b"DSH-CHAT-HERE/1"


# ---------------------------------------------------------------- 小工具

def now_ms() -> int:
    return int(time.time() * 1000)


def bj(ms: int) -> datetime:
    return datetime.fromtimestamp(ms / 1000, BEIJING)


def atomic_write(path: str, data: bytes) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)


def load_json(path: str, default):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except (OSError, ValueError):
        return default


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def is_peak(ms: int) -> bool:
    t = bj(ms)
    if t.strftime("%Y-%m-%d") in HOLIDAYS:
        return False
    if t.weekday() >= 5:          # 周六周日全天谷价（调休上班日也不特殊处理）
        return False
    return any(a <= t.hour < b for a, b in PEAK_HOURS)


def next_boundary(ms: int) -> int:
    cur = is_peak(ms)
    t = ms - ms % 60000 + 60000
    for _ in range(8 * 24 * 60):
        if is_peak(t) != cur:
            return t
        t += 60000
    return t


def prev_boundary(ms: int) -> int:
    cur = is_peak(ms)
    t = ms - ms % 60000
    for _ in range(7 * 24 * 60):
        p = t - 60000
        if is_peak(p) != cur:
            return t
        t = p
    return t


def price_for(model: str):
    return PRICES.get(model or "", PRICES["deepseek-flash"])


def cost_of(model: str, ms: int, hit: int, miss: int, out: int) -> float:
    ph, pm, po = price_for(model)
    factor = 1.0 if is_peak(ms) else 0.5
    return (hit * ph + miss * pm + out * po) * factor / 1e6


def offpeak_cost(model: str, hit: int, miss: int, out: int) -> float:
    ph, pm, po = price_for(model)
    return (hit * ph + miss * pm + out * po) * 0.5 / 1e6


# ---------------------------------------------------------------- 存储

class Store:
    """会话/附件/归档的落盘层。索引是可重建的缓存，坏了就扫目录。"""

    def __init__(self, data_dir: str):
        self.dir = data_dir
        for sub in ("sessions", "meta", "media", "staged/sessions", "staged/files", "archive"):
            os.makedirs(os.path.join(data_dir, sub), exist_ok=True)
        self.index_path = os.path.join(data_dir, "meta", "index.json")
        self.idx = load_json(self.index_path, None)
        if not isinstance(self.idx, dict):
            self.rebuild()
        self.idx.setdefault("archiveDeleted", [])

    def p(self, *parts) -> str:
        return os.path.join(self.dir, *parts)

    def save_index(self) -> None:
        self.idx["cursor"] = now_ms()
        atomic_write(self.index_path, json.dumps(self.idx, ensure_ascii=False, indent=1).encode())

    def rebuild(self) -> None:
        self.idx = {"sessions": {}, "files": {}, "archive": [], "archiveDeleted": [], "cursor": 0}
        for name in os.listdir(self.p("sessions")):
            if not name.endswith(".json"):
                continue
            try:
                with open(self.p("sessions", name), "rb") as f:
                    lite = parse_session(f.read())
                if lite:
                    self.idx["sessions"][lite["meta"]["id"]] = lite["meta"]
            except (OSError, ValueError):
                continue
        self.save_index()

    # --- 会话 ---
    def save_session(self, sid: str, raw: bytes) -> None:
        lite = parse_session(raw)
        if not lite:
            raise ValueError("会话 JSON 无法解析")
        if lite["meta"]["id"] != sid:
            raise ValueError("会话 id 不一致")
        atomic_write(self.p("sessions", sid + ".json"), raw)
        self.idx["sessions"][sid] = lite["meta"]
        self.save_index()

    def save_remote(self, sid: str, raw: bytes, deleted: bool = False) -> None:
        """写入对端同步来的会话，墓碑要靠 deleted 标记，不能走普通保存路径。"""
        lite = parse_session(raw)
        if not lite:
            raise ValueError("对端会话无法解析")
        atomic_write(self.p("sessions", sid + ".json"), raw)
        meta = lite["meta"]
        meta["deleted"] = bool(deleted)
        self.idx["sessions"][sid] = meta
        self.save_index()

    def load_session(self, sid: str):
        try:
            with open(self.p("sessions", sid + ".json"), "rb") as f:
                return f.read()
        except OSError:
            return None

    def rename(self, sid: str, title: str) -> None:
        raw = self.load_session(sid)
        if raw is None:
            return
        doc = json.loads(raw.decode("utf-8"))
        doc["title"] = title
        atomic_write(self.p("sessions", sid + ".json"),
                     json.dumps(doc, ensure_ascii=False).encode())
        meta = self.idx["sessions"].get(sid)
        if meta:
            meta["title"] = title
        self.save_index()

    def delete(self, sid: str) -> None:
        meta = self.idx["sessions"].get(sid)
        if not meta:
            return
        meta["deleted"] = True
        meta["updatedAt"] = now_ms()
        used = set()
        for other_id, other in self.idx["sessions"].items():
            if other_id == sid or other.get("deleted"):
                continue
            used.update(other.get("fileIds") or [])
        for fid in meta.get("fileIds") or []:
            if fid in used:
                continue
            fm = self.idx["files"].pop(fid, None)
            if fm:
                try:
                    os.remove(self.p("media", fm["sha"]))
                except OSError:
                    pass
        self.save_index()

    def summaries(self):
        return sorted(
            (m for m in self.idx["sessions"].values() if not m.get("deleted")),
            key=lambda m: m.get("updatedAt", 0), reverse=True)

    def deleted_summaries(self):
        return sorted((m for m in self.idx["sessions"].values() if m.get("deleted")),
                      key=lambda m: m.get("updatedAt", 0), reverse=True)

    # --- 附件 ---
    def put_file(self, fid: str, data: bytes, mime: str, name: str, image: bool) -> dict:
        digest = sha256(data)
        atomic_write(self.p("media", digest), data)
        fm = {"id": fid, "sha": digest, "name": name, "size": len(data),
              "mime": mime, "isImage": image, "createdAt": now_ms()}
        self.idx["files"][fid] = fm
        self.save_index()
        return fm

    def get_file(self, fid: str):
        fm = self.idx["files"].get(fid)
        if not fm:
            return None, None
        try:
            with open(self.p("media", fm["sha"]), "rb") as f:
                return fm, f.read()
        except OSError:
            return fm, None

    def files(self):
        return list(self.idx["files"].values())

    def replace_file(self, fid: str, data: bytes) -> str:
        fm = self.idx["files"].get(fid)
        if not fm:
            return ""
        old = fm["sha"]
        digest = sha256(data)
        if digest == old:
            return digest
        atomic_write(self.p("media", digest), data)
        fm["sha"] = digest
        fm["size"] = len(data)
        self.save_index()
        if not any(f["sha"] == old for f in self.idx["files"].values()):
            try:
                os.remove(self.p("media", old))
            except OSError:
                pass
        return digest

    # --- 导入暂存 ---
    def clear_staging(self) -> None:
        for sub in ("sessions", "files"):
            d = self.p("staged", sub)
            for name in os.listdir(d):
                try:
                    os.remove(os.path.join(d, name))
                except OSError:
                    pass
        try:
            os.remove(self.p("staged", "files.json"))
        except OSError:
            pass

    def add_staging_session(self, sid: str, raw: bytes) -> None:
        atomic_write(self.p("staged", "sessions", sid + ".json"), raw)

    def add_staging_file(self, fid: str, data: bytes, meta: dict) -> None:
        atomic_write(self.p("staged", "files", fid), data)
        metas = load_json(self.p("staged", "files.json"), {})
        metas[fid] = meta
        atomic_write(self.p("staged", "files.json"),
                     json.dumps(metas, ensure_ascii=False).encode())

    def update_staging_types(self, types: dict) -> None:
        d = self.p("staged", "sessions")
        for name in os.listdir(d):
            if not name.endswith(".json"):
                continue
            path = os.path.join(d, name)
            doc = load_json(path, None)
            if not isinstance(doc, dict):
                continue
            changed = False
            for msg in doc.get("messages") or []:
                if not isinstance(msg, dict) or msg.get("role") != "user":
                    continue
                for f in msg.get("files") or []:
                    if not isinstance(f, dict):
                        continue
                    want = types.get(f.get("id"), "")
                    if f.get("mimeType") != want:
                        f["mimeType"] = want
                        changed = True
            if changed:
                atomic_write(path, json.dumps(doc, ensure_ascii=False).encode())

    def commit_staging(self):
        for name in os.listdir(self.p("sessions")):
            try:
                os.remove(self.p("sessions", name))
            except OSError:
                pass
        self.idx["sessions"] = {}
        self.idx["files"] = {}
        out = []
        d = self.p("staged", "sessions")
        for name in sorted(os.listdir(d)):
            if not name.endswith(".json"):
                continue
            with open(os.path.join(d, name), "rb") as f:
                raw = f.read()
            lite = parse_session(raw)
            if not lite:
                continue
            atomic_write(self.p("sessions", lite["meta"]["id"] + ".json"), raw)
            self.idx["sessions"][lite["meta"]["id"]] = lite["meta"]
            out.append(lite["meta"])
        metas = load_json(self.p("staged", "files.json"), {})
        for fid, meta in metas.items():
            try:
                with open(self.p("staged", "files", fid), "rb") as f:
                    data = f.read()
            except OSError:
                continue
            digest = sha256(data)
            atomic_write(self.p("media", digest), data)
            self.idx["files"][fid] = {"id": fid, "sha": digest, "name": meta.get("name", ""),
                                      "size": len(data), "mime": meta.get("mime", ""),
                                      "isImage": meta.get("isImage", False), "createdAt": now_ms()}
        self.save_index()
        self.clear_staging()
        return out

    # --- 归档 ---
    def mark_archived(self, metas) -> None:
        for m in metas:
            self.idx["sessions"].pop(m["id"], None)
            try:
                os.remove(self.p("sessions", m["id"] + ".json"))
            except OSError:
                pass
            existing = [a for a in self.idx["archive"] if a["id"] != m["id"]]
            existing.append(m)
            self.idx["archive"] = existing
        self.save_index()

    def archived_month(self, sid: str):
        for a in self.idx["archive"]:
            if a["id"] == sid:
                return a["month"]
        return None


def parse_session(raw: bytes):
    try:
        doc = json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        return None
    if not isinstance(doc, dict) or not doc.get("id"):
        return None
    file_ids, seen = [], set()
    for msg in doc.get("messages") or []:
        if not isinstance(msg, dict) or msg.get("role") != "user":
            continue
        for f in msg.get("files") or []:
            fid = f.get("id") if isinstance(f, dict) else None
            if fid and fid not in seen:
                seen.add(fid)
                file_ids.append(fid)
    meta = {
        "id": doc["id"],
        "title": doc.get("title", ""),
        "createdAt": int(doc.get("createdAt") or 0),
        "updatedAt": int(doc.get("updatedAt") or 0),
        "fileIds": file_ids,
    }
    return {"meta": meta, "fileIds": file_ids}


# ---------------------------------------------------------------- 状态

class App:
    def __init__(self, data_dir: str, ui_port: int, sync_port: int):
        self.data_dir = data_dir
        self.ui_port = ui_port
        self.sync_port = sync_port
        self.started = time.time()
        self.store = Store(data_dir)
        self.config = self.load_config()
        self.key = self.load_key()
        self.balance_cache = {"at": 0, "data": None}
        self.sync_state = {"lastOk": 0, "lastTry": 0, "lastErr": "", "pulled": 0,
                           "cursor": 0, "peer": "", "enabled": False}
        # 待确认的配对请求（像蓝牙那样：对方请求，本机点接受才生效）
        self.pair_requests = []
        self.log_lock = threading.Lock()
    # --- 配置 / Key ---
    def config_path(self):
        return os.path.join(self.data_dir, "meta", "config.json")

    def load_config(self) -> dict:
        cfg = load_json(self.config_path(), {}) or {}
        cfg.setdefault("peer", "")
        cfg.setdefault("intervalSec", 3)
        cfg.setdefault("archiveDays", 7)
        cfg.setdefault("imageDays", 14)
        cfg.setdefault("deviceName", socket.gethostname())
        cfg.setdefault("syncPort", self.sync_port)
        if not 1024 <= int(cfg["syncPort"]) <= 65535:
            cfg["syncPort"] = self.sync_port
        if cfg["intervalSec"] not in (3, 5, 10):
            cfg["intervalSec"] = 3
        if not 7 <= cfg["archiveDays"] <= 14:
            cfg["archiveDays"] = 7
        # 0 表示"不按天数过滤"，用于"立即压缩"；负数回落到默认
        if cfg["imageDays"] < 0:
            cfg["imageDays"] = 14
        return cfg

    def save_config(self, patch: dict) -> dict:
        cfg = self.load_config()
        cfg.update({k: patch[k] for k in patch if k in cfg})
        atomic_write(self.config_path(), json.dumps(cfg, ensure_ascii=False, indent=1).encode())
        self.config = cfg
        return cfg

    def key_path(self):
        return os.path.join(self.data_dir, "meta", "secret.json")

    def load_key(self) -> str:
        return (load_json(self.key_path(), {}) or {}).get("apiKey", "").strip()

    def save_key(self, key: str) -> None:
        self.key = key.strip()
        atomic_write(self.key_path(), json.dumps({"apiKey": self.key}).encode())
        self.balance_cache = {"at": 0, "data": None}

    def hint(self) -> str:
        return "…" + self.key[-4:] if len(self.key) > 4 else ""

    def log(self, msg: str) -> None:
        line = time.strftime("%H:%M:%S ") + msg
        path = os.path.join(self.data_dir, "app.log")
        with self.log_lock:
            print(line, flush=True)
            try:
                with open(path, "a", encoding="utf-8") as f:
                    f.write(line + "\n")
                # 日志只增不减会一直吃空间；超过 1 MB 就只留最近一半。
                if os.path.getsize(path) > LOG_MAX_BYTES:
                    self._rotate_log(path)
            except OSError:
                pass

    @staticmethod
    def _rotate_log(path: str) -> None:
        """保留文件后半部分。宁可丢老日志，也不要无限增长。"""
        try:
            with open(path, "rb") as f:
                data = f.read()
            keep = data[len(data) // 2:]
            # 从下一行开头起，避免保留半行
            cut = keep.find(b"\n")
            if cut >= 0:
                keep = keep[cut + 1:]
            atomic_write(path, keep)
        except OSError:
            pass


# ---------------------------------------------------------------- HTTP 基础设施

def json_bytes(obj) -> bytes:
    return json.dumps(obj, ensure_ascii=False).encode("utf-8")


class QuietServer(ThreadingHTTPServer):
    daemon_threads = True

    def server_bind(self):
        """跳过 HTTPServer.server_bind 里的 socket.getfqdn(host)。

        标准实现会做一次反查：self.server_name = socket.getfqdn(host)。
        在 Android 上这一步可能触发 DNS 查询并阻塞几十秒，
        表现就是“服务迟迟不监听” —— 外层只能看到启动超时。
        server_name 只用于 CGI 等场景，我们不需要，直接用地址即可。
        """
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name = host
        self.server_port = port

    def handle_error(self, request, client_address):
        # 浏览器关页面/关闭时断开是常态，不用打堆栈
        import traceback
        exc = sys.exc_info()[1]
        if isinstance(exc, (ConnectionResetError, ConnectionAbortedError, BrokenPipeError)):
            return
        traceback.print_exc()


class QuietServer6(QuietServer):
    """监听 IPv6 回环。

    安卓上 WebView 用 http://localhost:PORT 访问时，localhost 可能解析到 ::1；
    Android 的 network-security-config 只接受主机名、不接受 IP，
    所以安卓端必须用 localhost 才能让明文策略生效，也就必须同时监听 ::1。
    """
    address_family = socket.AF_INET6


class Base(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    app: App = None  # 由子类注入

    def log_message(self, fmt, *args):  # 关掉默认的每请求日志
        pass

    # --- 响应 ---
    def send_json(self, code: int, obj) -> None:
        body = json_bytes(obj)
        self.send_response(code)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_bytes(self, code: int, body: bytes, ctype: str, cache: str = "no-store") -> None:
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Cache-Control", cache)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def read_body(self, limit: int) -> bytes:
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length > limit:
            raise ValueError("请求体过大")
        return self.rfile.read(length) if length else b""

    def query(self) -> dict:
        return dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(self.path).query))

    # --- 路由 ---
    def do_GET(self):
        self.route("GET")

    def do_PUT(self):
        self.route("PUT")

    def do_POST(self):
        self.route("POST")

    def do_DELETE(self):
        self.route("DELETE")

    def route(self, method: str):
        path = urllib.parse.urlsplit(self.path).path
        try:
            if self.dispatch(method, path):
                return
            self.send_json(404, {"error": "not found"})
        except BrokenPipeError:
            pass
        except Exception as exc:  # 兜底，别让一个请求把线程弄挂
            self.app.log(f"{method} {path} 失败: {exc!r}")
            try:
                self.send_json(500, {"error": str(exc)})
            except Exception:
                pass

    def dispatch(self, method: str, path: str) -> bool:
        raise NotImplementedError


# ---------------------------------------------------------------- UI + 代理

class UIServer(Base):
    def dispatch(self, method: str, path: str) -> bool:
        if path.startswith("/local/"):
            return self.local_api(method, path[len("/local"):])
        if path.startswith("/models") or path.startswith("/anthropic/") or path.startswith("/v1/"):
            if method != "GET" and method != "POST":
                return False
            self.proxy(path)
            return True
        if method != "GET":
            return False
        self.serve_static(path)
        return True

    # --- 静态文件 ---
    def serve_static(self, path: str) -> None:
        rel = path.lstrip("/") or "index.html"
        root = os.path.abspath(WWW)
        full = os.path.abspath(os.path.join(root, rel))
        try:
            inside = os.path.commonpath([full, root]) == root
        except ValueError:
            inside = False
        if not inside:
            self.send_json(403, {"error": "forbidden"})
            return
        if os.path.isdir(full):
            full = os.path.join(full, "index.html")
        if not os.path.isfile(full):
            if os.path.splitext(rel)[1]:
                self.send_json(404, {"error": "not found"})
                return
            full = os.path.join(WWW, "index.html")
            if not os.path.isfile(full):
                self.send_json(500, {"error": "www/index.html 缺失"})
                return
        ctype = {
            ".html": "text/html; charset=utf-8",
            ".js": "text/javascript; charset=utf-8",
            ".css": "text/css; charset=utf-8",
            ".json": "application/json; charset=utf-8",
            ".svg": "image/svg+xml",
            ".png": "image/png",
            ".jpg": "image/jpeg",
            ".woff2": "font/woff2",
            ".ttf": "font/ttf",
        }.get(os.path.splitext(full)[1].lower(), "application/octet-stream")
        cache = "public, max-age=31536000, immutable" if rel.startswith("static/") else "no-cache"
        with open(full, "rb") as f:
            self.send_bytes(200, f.read(), ctype, cache)

    # --- 反向代理（SSE 需要边收边发） ---
    def proxy(self, path: str) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None

        inbound_key = (self.headers.get("x-api-key") or "").strip()
        auth = (self.headers.get("Authorization") or "").strip()
        key = self.app.key

        # 先把来自浏览器的真实凭据筛出来（哨兵值和空值都不算凭据）。
        in_key = "" if inbound_key == SENTINEL else inbound_key
        in_auth = "" if auth == "Bearer " + SENTINEL else auth

        if in_key or in_auth:
            # 前端带了真凭据就用它。这一支很关键：
            # verifyApiKey() 只带 Authorization、不带 x-api-key，
            # 若按“任一为空就用服务端 Key”判定，验证新 Key 时会被旧 Key 顶替，
            # 结果永远验证不过 —— 用户想换 Key 也换不掉。
            out_key, out_auth = in_key, in_auth
        else:
            # 没带凭据（或只有哨兵）：用服务端保存的 Key。
            if not key:
                # 直接把 "__server__" 发给上游会得到一个语焉不详的 401
                # （“API Key 无效或已被删除”），让人以为 Key 失效了，实际是没配置。
                self.send_json(400, {
                    "error": "服务端尚未配置 API Key",
                    "detail": "请先在设置里填入 Key（会保存在本机 data/meta/secret.json）",
                    "code": "no_server_key",
                })
                return
            out_key, out_auth = key, "Bearer " + key

        conn = http.client.HTTPSConnection(UPSTREAM, timeout=600,
                                           context=ssl.create_default_context())
        headers = {
            "Host": UPSTREAM,
            "x-api-key": out_key,
            "Authorization": out_auth,
            "Content-Type": self.headers.get("Content-Type") or "application/json",
            "Accept": self.headers.get("Accept") or "application/json",
        }
        if self.headers.get("anthropic-beta"):
            headers["anthropic-beta"] = self.headers["anthropic-beta"]
        conn.request(self.command, path, body=body, headers=headers)
        resp = conn.getresponse()

        # 用 HTTP/1.0 + Connection: close 表示"读到 EOF 为止"，这样 SSE 能立刻逐块落地
        self.close_connection = True
        self.wfile.write(f"HTTP/1.0 {resp.status} {resp.reason}\r\n".encode())
        for name, value in resp.getheaders():
            low = name.lower()
            if low in ("transfer-encoding", "connection", "content-length"):
                continue
            self.wfile.write(f"{name}: {value}\r\n".encode())
        self.wfile.write(b"Connection: close\r\n\r\n")
        while True:
            chunk = resp.read(4096)
            if not chunk:
                break
            self.wfile.write(chunk)
            self.wfile.flush()
        conn.close()

    # --- /local/* ---
    def local_api(self, method: str, path: str) -> bool:
        app = self.app
        store = app.store

        if path == "/status" and method == "GET":
            self.send_json(200, {"version": VERSION, "dataDir": app.data_dir,
                                 "port": app.ui_port, "syncPort": app.sync_port,
                                 "uptimeSec": int(time.time() - app.started),
                                 "platform": sys.platform,
                                 "localIps": local_ips(),
                                 "deviceName": app.config.get("deviceName", "")})
            return True
        if path == "/shutdown" and method == "POST":
            self.send_json(200, {"ok": True})
            threading.Timer(0.2, lambda: os._exit(0)).start()
            return True

        if path == "/db/summaries" and method == "GET":
            self.send_json(200, [{"id": m["id"], "title": m.get("title", ""),
                                  "createdAt": m.get("createdAt", 0),
                                  "updatedAt": m.get("updatedAt", 0)} for m in store.summaries()])
            return True

        m = re.match(r"^/db/session/([^/]+)(/rename)?$", path)
        if m:
            sid = urllib.parse.unquote(m.group(1))
            if m.group(2) and method == "POST":
                payload = json.loads(self.read_body(1 << 20) or b"{}")
                with LOCK:
                    store.rename(sid, payload.get("title", ""))
                self.send_bytes(204, b"", "text/plain")
                return True
            if method == "GET":
                raw = store.load_session(sid)
                if raw is None:
                    self.send_json(404, {"error": "not found"})
                else:
                    self.send_bytes(200, raw, "application/json; charset=utf-8")
                return True
            if method == "PUT":
                body = self.read_body(MAX_SESSION)
                with LOCK:
                    store.save_session(sid, body)
                self.send_bytes(204, b"", "text/plain")
                return True
            if method == "DELETE":
                with LOCK:
                    store.delete(sid)
                self.send_bytes(204, b"", "text/plain")
                return True

        m = re.match(r"^/db/file/([^/]+)$", path)
        if m:
            fid = urllib.parse.unquote(m.group(1))
            if method == "PUT":
                body = self.read_body(MAX_FILE)
                mime = self.headers.get("Content-Type") or "application/octet-stream"
                name = urllib.parse.unquote(self.headers.get("X-File-Name") or "")
                image = self.headers.get("X-Image") == "1" or mime.startswith("image/")
                with LOCK:
                    store.put_file(fid, body, mime, name, image)
                self.send_bytes(204, b"", "text/plain")
                return True
            if method == "GET":
                fm, data = store.get_file(fid)
                if not fm or data is None:
                    self.send_json(404, {"error": "not found"})
                else:
                    self.send_bytes(200, data, fm.get("mime") or "application/octet-stream",
                                    "public, max-age=31536000, immutable")
                return True

        if path == "/db/staging/clear" and method == "POST":
            with LOCK:
                store.clear_staging()
            self.send_bytes(204, b"", "text/plain")
            return True

        m = re.match(r"^/db/staging/session/([^/]+)$", path)
        if m and method == "PUT":
            body = self.read_body(MAX_SESSION)
            with LOCK:
                store.add_staging_session(urllib.parse.unquote(m.group(1)), body)
            self.send_bytes(204, b"", "text/plain")
            return True

        m = re.match(r"^/db/staging/file/([^/]+)$", path)
        if m and method == "PUT":
            body = self.read_body(MAX_FILE)
            mime = self.headers.get("Content-Type") or "application/octet-stream"
            meta = {"name": urllib.parse.unquote(self.headers.get("X-File-Name") or ""),
                    "size": len(body), "mime": mime,
                    "isImage": self.headers.get("X-Image") == "1" or mime.startswith("image/")}
            with LOCK:
                store.add_staging_file(urllib.parse.unquote(m.group(1)), body, meta)
            self.send_bytes(204, b"", "text/plain")
            return True

        if path == "/db/staging/filetypes" and method == "POST":
            payload = json.loads(self.read_body(8 << 20) or b"{}")
            with LOCK:
                store.update_staging_types(payload.get("types") or {})
            self.send_bytes(204, b"", "text/plain")
            return True

        if path == "/db/staging/commit" and method == "POST":
            with LOCK:
                metas = store.commit_staging()
            self.send_json(200, [{"id": m["id"], "title": m.get("title", ""),
                                  "createdAt": m.get("createdAt", 0),
                                  "updatedAt": m.get("updatedAt", 0)} for m in metas])
            return True

        if path == "/search" and method == "GET":
            self.send_json(200, app_search(app, self.query().get("q", ""),
                                           int(self.query().get("limit", 50) or 50)))
            return True

        if path == "/usage" and method == "GET":
            self.send_json(200, app_usage(app, self.query().get("session", "")))
            return True

        if path == "/balance" and method == "GET":
            self.send_json(200, app_balance(app))
            return True

        if path == "/apikey":
            if method == "GET":
                self.send_json(200, {"configured": bool(app.key), "hint": app.hint()})
                return True
            if method == "PUT":
                payload = json.loads(self.read_body(1 << 16) or b"{}")
                key = (payload.get("apiKey") or "").strip()
                if not key:
                    self.send_json(400, {"error": "apiKey 不能为空"})
                    return True
                app.save_key(key)
                self.send_json(200, {"configured": True, "hint": app.hint()})
                return True
            if method == "DELETE":
                app.save_key("")
                self.send_bytes(204, b"", "text/plain")
                return True

        if path == "/config":
            if method == "GET":
                self.send_json(200, app.config)
                return True
            if method == "PUT":
                patch = json.loads(self.read_body(1 << 16) or b"{}")
                self.send_json(200, app.save_config(patch))
                return True

        if path == "/sync/status" and method == "GET":
            self.send_json(200, app.sync_state)
            return True

        if path == "/sync/now" and method == "POST":
            if not app.config.get("peer"):
                self.send_json(400, {"error": "还没填对端地址"})
            else:
                threading.Thread(target=pull_once, args=(app,), daemon=True).start()
                self.send_json(200, {"started": True})
            return True

        if path == "/discover" and method == "GET":
            # 广播最多 2 秒；这里直接同步等，前端会看到短暂 loading
            self.send_json(200, {"peers": discover_peers(app),
                                 "self": {"name": app.config.get("deviceName", ""),
                                          "ips": local_ips(), "port": app.sync_port}})
            return True

        if path == "/pair" and method == "POST":
            """发起配对：本机记下对端，并向对端的**同步端口**发送配对请求。"""
            payload = json.loads(self.read_body(1 << 16) or b"{}")
            ip = str(payload.get("ip") or "").strip()
            port = int(payload.get("port") or 8791)
            if not ip:
                self.send_json(400, {"error": "缺少 ip"})
                return True
            result = {"peer": f"{ip}:{port}", "reverse": "skipped"}
            app.save_config({"peer": f"{ip}:{port}"})
            # 反向：请对端也指向本机。对端只会入待确认队列，不会直接生效。
            mine = (local_ips() or [""])[0]
            if mine:
                body = json.dumps({"ip": mine, "port": app.sync_port,
                                   "name": app.config.get("deviceName", "")}).encode()
                try:
                    # 注意：请求对端的**同步端口**（对端不知道我们的 UI 端口）
                    # urllib.request 在模块顶部导入：若在这里 import，
                    # urllib 会成为 local_api 的局部名，使前面所有 urllib.parse.unquote
                    # 抛 UnboundLocalError（"cannot access local variable 'urllib'"）。
                    req = urllib.request.Request(
                        f"http://{ip}:{port}/peer",
                        data=body, method="POST",
                        headers={"Content-Type": "application/json"})
                    with urllib.request.urlopen(req, timeout=6) as resp:
                        resp.read()
                    result["reverse"] = "pending"
                except Exception as exc:  # noqa: BLE001
                    result["reverse"] = f"failed: {exc}"
            # 立即拉一次，本机马上能看到数据
            threading.Thread(target=pull_once, args=(app,), daemon=True).start()
            self.send_json(200, result)
            return True

        if path == "/pair/requests" and method == "GET":
            with LOCK:
                self.send_json(200, list(app.pair_requests))
            return True

        if path == "/pair/accept" and method == "POST":
            """本机确认接受配对：才真正把 peer 写进配置。"""
            payload = json.loads(self.read_body(1 << 16) or b"{}")
            ip = str(payload.get("ip") or "").strip()
            port = int(payload.get("port") or 8791)
            with LOCK:
                app.pair_requests = [r for r in app.pair_requests
                                     if not (r["ip"] == ip and r["port"] == port)]
            if not ip:
                self.send_json(400, {"error": "缺少 ip"})
                return True
            app.save_config({"peer": f"{ip}:{port}"})
            app.log(f"已接受配对：{ip}:{port}")
            threading.Thread(target=pull_once, args=(app,), daemon=True).start()
            self.send_json(200, {"peer": f"{ip}:{port}"})
            return True

        if path == "/pair/reject" and method == "POST":
            payload = json.loads(self.read_body(1 << 16) or b"{}")
            ip = str(payload.get("ip") or "").strip()
            port = int(payload.get("port") or 8791)
            with LOCK:
                app.pair_requests = [r for r in app.pair_requests
                                     if not (r["ip"] == ip and r["port"] == port)]
            self.send_json(200, {"ok": True})
            return True

        if path == "/archived" and method == "GET":
            deleted = set(app.store.idx.get("archiveDeleted") or [])
            items = [a for a in app.store.idx["archive"] if a["id"] not in deleted]
            self.send_json(200, sorted(items, key=lambda a: a.get("updatedAt", 0), reverse=True))
            return True

        if path == "/archive/now" and method == "POST":
            # 用户点“立即整理”时才做去重：这是显式动作，不在后台自动删东西。
            freed = 0
            try:
                freed = compact_archive(app).get("savedBytes", 0)
            except Exception as exc:  # noqa: BLE001
                app.log(f"归档去重失败: {exc!r}")
            n = archive_once(app)
            self.send_json(200, {"archived": n, "dedupSavedBytes": freed})
            return True

        if path == "/orphans" and method == "GET":
            # 只统计不删除，给界面显示“未使用附件占多少”
            self.send_json(200, orphan_media(app))
            return True

        if path == "/delete" and method == "POST":
            payload = json.loads(self.read_body(1 << 20) or b"{}")
            ids = [str(i) for i in (payload.get("ids") or []) if i]
            done = 0
            with LOCK:
                for sid in ids:
                    if app.store.archived_month(sid):
                        app.store.idx["archive"] = [a for a in app.store.idx["archive"] if a["id"] != sid]
                        tomb = app.store.idx.setdefault("archiveDeleted", [])
                        if sid not in tomb:
                            tomb.append(sid)
                        done += 1
                    elif app.store.idx["sessions"].get(sid):
                        app.store.delete(sid)
                        done += 1
                app.store.save_index()
            self.send_json(200, {"deleted": done})
            return True

        m = re.match(r"^/restore/([^/]+)$", path)
        if m and method == "POST":
            sid = urllib.parse.unquote(m.group(1))
            ok, msg = restore_session(app, sid)
            self.send_json(200 if ok else 404, {"restored": sid} if ok else {"error": msg})
            return True

        if path == "/storage" and method == "GET":
            self.send_json(200, storage_info(app))
            return True

        if path == "/images/now" and method == "POST":
            self.send_json(200, compact_images(app))
            return True

        return False


def dir_size(path: str) -> int:
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    return total


def storage_info(app: App) -> dict:
    return {
        "activeSessions": len(app.store.summaries()),
        "archivedSessions": len(app.store.idx["archive"]),
        "activeBytes": dir_size(os.path.join(app.data_dir, "sessions")) + dir_size(os.path.join(app.data_dir, "meta")),
        "archiveBytes": dir_size(os.path.join(app.data_dir, "archive")),
        "mediaBytes": dir_size(os.path.join(app.data_dir, "media")),
        # 前端静态资源在私有目录里有一份解压副本（安卓读不了 APK 内的 assets），
        # 这份不算“你的数据”，但在系统设置里会计入用户数据，所以单独报出来。
        "assetsBytes": dir_size(WWW),
        "logBytes": os.path.getsize(os.path.join(app.data_dir, "app.log")) \
                     if os.path.isfile(os.path.join(app.data_dir, "app.log")) else 0,
    }


# ---------------------------------------------------------------- 同步端口

class SyncServer(Base):
    def dispatch(self, method: str, path: str) -> bool:
        app = self.app
        if path == "/health" and method == "GET":
            self.send_json(200, {"ok": True, "version": VERSION, "platform": sys.platform})
            return True
        if path == "/sync/since" and method == "GET":
            since = int(self.query().get("since", 0) or 0)
            sessions = []
            for meta in app.store.summaries():
                if meta.get("updatedAt", 0) <= since:
                    continue
                raw = app.store.load_session(meta["id"])
                if raw is not None:
                    sessions.append(json.loads(raw.decode("utf-8")))
            for meta in app.store.deleted_summaries():
                if meta.get("updatedAt", 0) <= since:
                    continue
                raw = app.store.load_session(meta["id"])
                doc = json.loads(raw.decode("utf-8")) if raw else {
                    "id": meta["id"], "title": meta.get("title", ""),
                    "createdAt": meta.get("createdAt", 0), "messages": []}
                doc["deleted"] = True
                doc["updatedAt"] = meta.get("updatedAt", 0)
                sessions.append(doc)
            files = [f for f in app.store.files() if f.get("createdAt", 0) > since]
            self.send_json(200, {"now": now_ms(), "sessions": sessions, "files": files})
            return True
        if path in ("/peer", "/local/peer") and method == "POST":
            # 配对请求：**不直接改本机配置**，只入待确认队列。
            # 对方只知道我们的同步端口 8791，不知道 UI 端口，所以必须在这里受理。
            # 无鉴权地改配置 = 任人把你指向别人的服务器；改成请求-确认后，
            # 实际生效要本机用户在界面上点「接受」（与蓝牙配对一致）。
            payload = json.loads(self.read_body(1 << 16) or b"{}")
            ip = str(payload.get("ip") or "").strip()
            port = int(payload.get("port") or 8791)
            if not ip:
                self.send_json(400, {"error": "缺少 ip"})
                return True
            name = str(payload.get("name") or ip)[:64]
            with LOCK:
                app.pair_requests = [r for r in app.pair_requests
                                     if not (r["ip"] == ip and r["port"] == port)]
                app.pair_requests.append({"ip": ip, "port": port, "name": name,
                                          "at": now_ms()})
            app.log(f"收到配对请求：{name} ({ip}:{port})，等待确认")
            self.send_json(200, {"pending": True})
            return True
        if path == "/sync/merge" and method == "POST":
            payload = json.loads(self.read_body(512 << 20) or b"{}")
            accepted = 0
            with LOCK:
                for doc in payload.get("sessions") or []:
                    if merge_remote(app, doc):
                        accepted += 1
            self.send_json(200, {"accepted": accepted, "missing": []})
            return True
        if path.startswith("/file/") and method == "GET":
            digest = os.path.basename(path)
            full = os.path.join(app.data_dir, "media", digest)
            if not os.path.isfile(full):
                self.send_json(404, {"error": "not found"})
                return True
            with open(full, "rb") as f:
                self.send_bytes(200, f.read(), "application/octet-stream")
            return True
        return False


def peer_url(peer: str) -> str:
    peer = (peer or "").strip()
    if not peer:
        return ""
    if not peer.startswith(("http://", "https://")):
        peer = "http://" + peer
    rest = peer.split("://", 1)[1]
    if ":" not in rest:
        peer += ":8791"
    return peer.rstrip("/")


# 助手消息的终态；用户消息没有 status，也算定稿
_FINAL_STATUS = (None, "done", "stopped", "error")


def _is_final(msg: dict) -> bool:
    return msg.get("status") in _FINAL_STATUS


def _more_complete(cur: dict, new: dict) -> bool:
    """new 是否比 cur 更完整。已定稿胜过未定稿，否则比内容块多少。"""
    if not _is_final(cur) and _is_final(new):
        return True
    if _is_final(cur) and not _is_final(new):
        return False
    a = cur.get("blocks")
    b = new.get("blocks")
    return (len(b) if isinstance(b, list) else 0) > (len(a) if isinstance(a, list) else 0)


def merge_remote(app: App, remote: dict) -> bool:
    """把对端会话合入本地。返回是否真的改了本地。

    不能简单地“时间戳旧就丢弃”：回答完成不会推进 updatedAt，
    对端那份可能时间戳一样、内容却多了一条回复。
    """
    rid = remote.get("id")
    if not rid:
        return False
    local_raw = app.store.load_session(rid)
    if local_raw is None:
        app.store.save_remote(rid, json.dumps(remote, ensure_ascii=False).encode(),
                              bool(remote.get("deleted")))
        return True
    local = json.loads(local_raw.decode("utf-8"))

    local_del = bool(local.get("deleted"))
    remote_del = bool(remote.get("deleted"))
    # 墓碑优先：删掉的会话不能被旧副本复活
    if remote_del and not local_del:
        app.store.save_remote(rid, json.dumps(remote, ensure_ascii=False).encode(), True)
        return True
    if local_del and not remote_del:
        return False

    lt = int(local.get("updatedAt") or 0)
    rt = int(remote.get("updatedAt") or 0)
    if rt > lt:
        # 对端确实更新，整份替换
        app.store.save_remote(rid, json.dumps(remote, ensure_ascii=False).encode(), False)
        return True

    # rt <= lt：逐条按完整度合并，不能整份丢弃
    remote_by_id = {}
    appended = []
    for msg in remote.get("messages") or []:
        if not isinstance(msg, dict):
            continue
        mid = msg.get("id")
        if not mid:
            continue
        remote_by_id[mid] = msg

    changed = False
    out = []
    seen = set()
    for msg in local.get("messages") or []:
        mid = msg.get("id") if isinstance(msg, dict) else None
        if mid and mid in remote_by_id:
            seen.add(mid)
            if _more_complete(msg, remote_by_id[mid]):
                msg = remote_by_id[mid]
                changed = True
        out.append(msg)
    for msg in remote.get("messages") or []:
        if not isinstance(msg, dict):
            continue
        mid = msg.get("id")
        if mid and mid not in seen:
            out.append(remote_by_id[mid])
            appended.append(mid)
            seen.add(mid)
            changed = True

    merged = dict(local)
    merged["messages"] = out
    # 树结构字段本地缺就从对端补，否则新追加的消息可能接不上
    for key in ("rootMessageId", "selectedChildId"):
        if merged.get(key) in (None, "") and remote.get(key) not in (None, ""):
            merged[key] = remote[key]
            changed = True
    if not merged.get("title") and remote.get("title"):
        merged["title"] = remote["title"]
        changed = True

    if not changed:
        return False
    # 刻意不推进 updatedAt：靠回看窗口重新拉取，合并幂等，不会来回覆盖
    app.store.save_remote(rid, json.dumps(merged, ensure_ascii=False).encode(), False)
    return True


def pull_once(app: App) -> None:
    cfg = app.config
    base = peer_url(cfg.get("peer", ""))
    state = app.sync_state
    state["lastTry"] = now_ms()
    state["peer"] = cfg.get("peer", "")
    state["enabled"] = bool(base)
    if not base:
        return
    try:
        # 回看一段窗口：回答完成不会推进 updatedAt，只靠时间戳过滤会漏掉
        # “问题已同步、回答后才落盘”的会话（详见 SYNC_LOOKBACK_MS 注释）。
        # 合并是幂等的，重复拉到相同内容不会产生写入。
        since = max(0, int(state["cursor"]) - SYNC_LOOKBACK_MS)
        data = http_get_json(f"{base}/sync/since?since={since}")
        pulled = 0
        with LOCK:
            for doc in data.get("sessions") or []:
                if merge_remote(app, doc):
                    pulled += 1
        for fm in data.get("files") or []:
            digest = fm.get("sha")
            if not digest:
                continue
            if os.path.isfile(os.path.join(app.data_dir, "media", digest)):
                continue
            fetch_media(app, base, digest)
        state["lastOk"] = now_ms()
        state["lastErr"] = ""
        state["cursor"] = int(data.get("now") or 0)
        state["pulled"] += pulled
    except Exception as exc:
        state["lastErr"] = str(exc)


def http_get_json(url: str, timeout: int = 8):
    import urllib.request
    req = urllib.request.Request(url, headers={"Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def fetch_media(app: App, base: str, digest: str) -> None:
    import urllib.request
    try:
        with urllib.request.urlopen(f"{base}/file/{digest}", timeout=30) as resp:
            data = resp.read(256 << 20)
        atomic_write(os.path.join(app.data_dir, "media", digest), data)
    except Exception:
        return


def sync_loop(app: App) -> None:
    while True:
        cfg = app.config
        interval = cfg.get("intervalSec", 3)
        if cfg.get("peer"):
            pull_once(app)
            time.sleep(interval)
        else:
            time.sleep(2)


# ---------------------------------------------------------------- 归档 / 图片 / 搜索 / 计费

def compact_archive(app: App) -> dict:
    """把归档里重复的会话条目合并掉（每个会话只保留最新的一份）。

    归档是“追加一个 gzip member”写进去的，所以同一个会话被恢复后再次归档、
    或者同步把对端旧副本又拉回来，就会在同一个文件里出现多份。
    每份都是完整会话，重复就是白占空间。

    只删“同一个会话的旧副本”，不会丢掉任何会话，也不会动附件原件。
    """
    archive_dir = os.path.join(app.data_dir, "archive")
    if not os.path.isdir(archive_dir):
        return {"files": 0, "dropped": 0, "savedBytes": 0}
    total_files = dropped = saved = 0
    for name in sorted(os.listdir(archive_dir)):
        if not name.endswith(".jsonl.gz"):
            continue
        path = os.path.join(archive_dir, name)
        latest = {}          # sid -> (archivedAt, doc)
        original = 0
        try:
            before = os.path.getsize(path)
            with gzip.open(path, "rt", encoding="utf-8") as f:
                for line in f:
                    original += 1
                    try:
                        env = json.loads(line)
                    except ValueError:
                        continue
                    doc = env.get("session") or {}
                    sid = doc.get("id")
                    if not sid:
                        continue
                    at = int(env.get("archivedAt") or 0)
                    if sid not in latest or at >= latest[sid][0]:
                        latest[sid] = (at, doc)
        except (OSError, EOFError):
            continue
        total_files += 1
        if not latest or original <= len(latest):
            continue                      # 没有重复，不动它

        lines, md = [], []
        for sid, (at, doc) in latest.items():
            lines.append(json.dumps({"archivedAt": at, "session": doc},
                                    ensure_ascii=False) + "\n")
            msgs = doc.get("messages") or []
            last_user = ""
            for msg in reversed(msgs):
                if isinstance(msg, dict) and msg.get("role") == "user" and msg.get("content"):
                    last_user = str(msg["content"]).replace("\n", " ")[:160]
                    break
            md.append(f"## {doc.get('title', '')}\n\n- id: `{sid}`\n"
                      f"- 最后活动: {bj(int(doc.get('updatedAt') or at)).strftime('%Y-%m-%d %H:%M')}\n"
                      f"- 消息数: {len(msgs)}\n- 最后提问: {last_user}\n\n")
        try:
            atomic_write(path, gzip.compress("".join(lines).encode("utf-8")))
            atomic_write(path[:-len(".jsonl.gz")] + ".md", "".join(md).encode("utf-8"))
            saved += max(0, before - os.path.getsize(path))
            dropped += max(0, original - len(latest))
        except OSError:
            continue
    if dropped or saved:
        app.log(f"归档去重：移除 {dropped} 条重复，释放 {saved // 1024} KB")
    return {"files": total_files, "dropped": dropped, "savedBytes": saved}


def orphan_media(app: App) -> dict:
    """统计“没有任何会话引用”的附件原件，**只统计不删除**。

    删除归档会话时旧逻辑只写了墓碑，没动 media，那些图片会永久留在磁盘上。
    这里把活跃会话 + 归档内容过一遍算出孤儿，大小报给界面让用户自己决定。
    """
    referenced = set()

    def collect_files(doc):
        for msg in doc.get("messages") or []:
            if not isinstance(msg, dict):
                continue
            for f in msg.get("files") or []:
                if isinstance(f, dict) and f.get("id"):
                    referenced.add(f["id"])

    for meta in app.store.summaries():
        raw = app.store.load_session(meta["id"])
        if not raw:
            continue
        try:
            collect_files(json.loads(raw.decode("utf-8")))
        except ValueError:
            continue

    archive_dir = os.path.join(app.data_dir, "archive")
    if os.path.isdir(archive_dir):
        for name in sorted(os.listdir(archive_dir)):
            if not name.endswith(".jsonl.gz"):
                continue
            try:
                with gzip.open(os.path.join(archive_dir, name), "rt", encoding="utf-8") as f:
                    for line in f:
                        try:
                            env = json.loads(line)
                        except ValueError:
                            continue
                        collect_files(env.get("session") or {})
            except (OSError, EOFError):
                continue

    count = size = 0
    for fid, fm in app.store.idx["files"].items():
        if fid in referenced:
            continue
        sha = fm.get("sha")
        if sha and any(o.get("sha") == sha for o in app.store.idx["files"].values()):
            continue
        try:
            size += os.path.getsize(os.path.join(app.data_dir, "media", sha or ""))
            count += 1
        except OSError:
            pass
    return {"count": count, "bytes": size}


def archive_once(app: App) -> int:
    days = max(1, int(app.config.get("archiveDays", 7)))
    cutoff = now_ms() - days * 24 * 3600 * 1000
    todo = []
    for meta in app.store.summaries():
        if meta.get("updatedAt", 0) > cutoff:
            continue
        raw = app.store.load_session(meta["id"])
        if raw is None:
            continue
        text = raw.decode("utf-8", "ignore")
        if '"status":"pending"' in text or '"status":"streaming"' in text or '"status": "pending"' in text or '"status": "streaming"' in text:
            continue
        todo.append((meta, raw))
    if not todo:
        return 0

    by_month = {}
    for meta, raw in todo:
        month = bj(meta.get("updatedAt", now_ms())).strftime("%Y-%m")
        by_month.setdefault(month, []).append((meta, raw))

    total = 0
    for month, items in by_month.items():
        gz_path = os.path.join(app.data_dir, "archive", f"archive_{month}.jsonl.gz")
        md_path = os.path.join(app.data_dir, "archive", f"archive_{month}.md")
        lines, md, metas = [], [], []
        for meta, raw in items:
            doc = json.loads(raw.decode("utf-8"))
            envelope = {"archivedAt": now_ms(), "session": doc}
            lines.append(json.dumps(envelope, ensure_ascii=False) + "\n")
            msgs = doc.get("messages") or []
            last_user = ""
            for msg in reversed(msgs):
                if isinstance(msg, dict) and msg.get("role") == "user" and msg.get("content"):
                    last_user = str(msg["content"]).replace("\n", " ")[:160]
                    break
            md.append(f"## {doc.get('title', '')}\n\n- id: `{doc.get('id')}`\n"
                      f"- 最后活动: {bj(meta.get('updatedAt', 0)).strftime('%Y-%m-%d %H:%M')}\n"
                      f"- 消息数: {len(msgs)}\n- 最后提问: {last_user}\n\n")
            metas.append({"id": doc.get("id"), "month": month, "title": doc.get("title", ""),
                          "updatedAt": meta.get("updatedAt", 0), "msgCount": len(msgs)})
        # 每次追加一个新 gzip member，读的时候会自动接续
        with open(gz_path, "ab") as f:
            f.write(gzip.compress("".join(lines).encode("utf-8")))
        with open(md_path, "a", encoding="utf-8") as f:
            f.write("".join(md))
        app.store.mark_archived(metas)
        total += len(metas)
    return total


def restore_session(app: App, sid: str):
    month = app.store.archived_month(sid)
    if not month:
        return False, "归档里没有这条会话"
    path = os.path.join(app.data_dir, "archive", f"archive_{month}.jsonl.gz")
    try:
        with gzip.open(path, "rt", encoding="utf-8") as f:
            for line in f:
                try:
                    env = json.loads(line)
                except ValueError:
                    continue
                doc = env.get("session") or {}
                if doc.get("id") == sid:
                    with LOCK:
                        app.store.save_session(sid, json.dumps(doc, ensure_ascii=False).encode())
                        app.store.idx["archive"] = [a for a in app.store.idx["archive"] if a["id"] != sid]
                        app.store.save_index()
                    return True, ""
    except (OSError, EOFError):
        return False, "归档文件打不开"
    return False, "归档文件里没找到"


def compact_images(app: App) -> dict:
    """标准库解不了 JPEG/PNG 图像，能做的是用 zlib 重新压缩 PNG 的 IDAT。"""
    import zlib
    days = int(app.config.get("imageDays", 14))
    cutoff = now_ms() - days * 24 * 3600 * 1000 if days > 0 else now_ms()
    out = {"scanned": 0, "shrunk": 0, "savedBytes": 0, "skipped": 0}
    for fm in app.store.files():
        if not fm.get("isImage") or fm.get("createdAt", 0) > cutoff:
            continue
        out["scanned"] += 1
        try:
            with open(os.path.join(app.data_dir, "media", fm["sha"]), "rb") as f:
                data = f.read()
        except OSError:
            out["skipped"] += 1
            continue
        if not data.startswith(b"\x89PNG\r\n\x1a\n"):
            out["skipped"] += 1        # JPEG 没有编码器，原样保留
            continue
        new = recompress_png(data, zlib)
        if new is None or len(new) >= len(data):
            out["skipped"] += 1
            continue
        app.store.replace_file(fm["id"], new)
        out["shrunk"] += 1
        out["savedBytes"] += len(data) - len(new)
    return out


def recompress_png(data: bytes, zlib):
    """把 IDAT 解压后以 level 9 重新压缩，像素完全不变。"""
    try:
        out, pos = bytearray(data[:8]), 8
        while pos < len(data):
            length = int.from_bytes(data[pos:pos + 4], "big")
            ctype = data[pos + 4:pos + 8]
            payload = data[pos + 8:pos + 8 + length]
            pos += 12 + length
            if ctype == b"IDAT":
                payload = zlib.compress(zlib.decompress(payload), 9)
            out += len(payload).to_bytes(4, "big") + ctype + payload
            out += (zlib.crc32(ctype + payload) & 0xFFFFFFFF).to_bytes(4, "big")
        return bytes(out)
    except Exception:
        return None


def app_search(app: App, query: str, limit: int = 50) -> dict:
    started = time.time()
    hits, scanned = [], 0
    needle = (query or "").strip().lower()
    if not needle:
        return {"query": query, "hits": [], "scanned": 0, "tookMs": 0}
    limit = max(1, min(500, limit or 50))

    def collect(doc, archived):
        nonlocal scanned
        scanned += 1
        for msg in doc.get("messages") or []:
            if not isinstance(msg, dict):
                continue
            parts = []
            if msg.get("content"):
                parts.append(str(msg["content"]))
            for blk in msg.get("blocks") or []:
                if isinstance(blk, dict):
                    if blk.get("text"):
                        parts.append(str(blk["text"]))
                    if blk.get("thinking"):
                        parts.append(str(blk["thinking"]))
            for text in parts:
                idx = text.lower().find(needle)
                if idx < 0:
                    continue
                snippet, offset = snippet_around(text, idx, len(needle))
                hits.append({"sessionId": doc.get("id"), "title": doc.get("title", ""),
                             "messageId": msg.get("id"), "role": msg.get("role"),
                             "ts": msg.get("createdAt") or 0, "snippet": snippet,
                             "offset": offset, "length": len(needle), "archived": archived})
                break

    for meta in app.store.summaries():
        raw = app.store.load_session(meta["id"])
        if raw:
            try:
                collect(json.loads(raw.decode("utf-8")), False)
            except ValueError:
                continue

    active = {m["id"] for m in app.store.summaries()}
    active.update(app.store.idx.get("archiveDeleted") or [])
    archive_dir = os.path.join(app.data_dir, "archive")
    if os.path.isdir(archive_dir):
        for name in sorted(os.listdir(archive_dir)):
            if not name.endswith(".jsonl.gz"):
                continue
            try:
                with gzip.open(os.path.join(archive_dir, name), "rt", encoding="utf-8") as f:
                    for line in f:
                        try:
                            env = json.loads(line)
                        except ValueError:
                            continue
                        doc = env.get("session") or {}
                        if doc.get("id") in active:
                            continue
                        collect(doc, True)
            except (OSError, EOFError):
                continue

    hits.sort(key=lambda h: h["ts"], reverse=True)
    return {"query": query, "hits": hits[:limit], "scanned": scanned,
            "tookMs": int((time.time() - started) * 1000)}


def snippet_around(text: str, byte_ish_index: int, length: int):
    """按字符切（Python 的 find 是字符偏移，正好）。"""
    radius = 40
    start = max(0, byte_ish_index - radius)
    end = min(len(text), byte_ish_index + length + radius)
    snippet = text[start:end]
    if start > 0:
        snippet = "…" + snippet
    if end < len(text):
        snippet += "…"
    offset = byte_ish_index - start + (1 if start > 0 else 0)
    return snippet, offset


def app_usage(app: App, session_id: str = "") -> dict:
    today_key = bj(now_ms()).strftime("%Y-%m-%d")
    month_key = bj(now_ms()).strftime("%Y-%m")

    def empty():
        return {"calls": 0, "hitTokens": 0, "missTokens": 0, "outTokens": 0,
                "cost": 0.0, "cacheSaved": 0.0, "offPeakSaving": 0.0}

    session, today, month = empty(), empty(), empty()
    for meta in app.store.summaries():
        raw = app.store.load_session(meta["id"])
        if not raw:
            continue
        try:
            doc = json.loads(raw.decode("utf-8"))
        except ValueError:
            continue
        for msg in doc.get("messages") or []:
            usage = (msg or {}).get("usage")
            if not isinstance(usage, dict) or msg.get("role") != "assistant":
                continue
            hit = int(usage.get("cache_read_input_tokens") or 0)
            miss = int(usage.get("input_tokens") or 0) + int(usage.get("cache_creation_input_tokens") or 0)
            out = int(usage.get("output_tokens") or 0)
            if hit + miss + out == 0:
                continue
            model = msg.get("model") or "deepseek-flash"
            ts = int(msg.get("createdAt") or now_ms())
            cost = cost_of(model, ts, hit, miss, out)
            ph, pm, _ = price_for(model)
            factor = 1.0 if is_peak(ts) else 0.5
            item = {"calls": 1, "hitTokens": hit, "missTokens": miss, "outTokens": out,
                    "cost": cost, "cacheSaved": hit * (pm - ph) * factor / 1e6,
                    "offPeakSaving": max(0.0, cost - offpeak_cost(model, hit, miss, out))}
            for bucket, ok in ((session, doc.get("id") == session_id),
                               (today, bj(ts).strftime("%Y-%m-%d") == today_key),
                               (month, bj(ts).strftime("%Y-%m") == month_key)):
                if ok:
                    for k, v in item.items():
                        bucket[k] += v

    peak = is_peak(now_ms())
    return {"now": {"peak": peak, "label": "峰时" if peak else "闲时",
                    "nextLabel": "闲时" if peak else "峰时",
                    "until": next_boundary(now_ms()), "since": prev_boundary(now_ms())},
            "session": session, "today": today, "month": month, "currency": "CNY"}


def app_balance(app: App) -> dict:
    if not app.key:
        return {"configured": False}
    cached = app.balance_cache
    if cached["data"] and time.time() - cached["at"] < 60:
        return cached["data"]
    import urllib.request
    req = urllib.request.Request("https://api.deepseek.com/user/balance",
                                 headers={"Authorization": "Bearer " + app.key,
                                          "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=8) as resp:
            raw = json.loads(resp.read().decode("utf-8"))
    except Exception as exc:
        body = getattr(exc, "read", lambda: b"")()
        return {"configured": True, "error": (body.decode("utf-8", "ignore")
                                              if body else str(exc))[:300]}
    infos = raw.get("balance_infos") or []
    pick = next((i for i in infos if i.get("currency") == "CNY"), infos[0] if infos else {})
    data = {"configured": True, "available": raw.get("is_available"),
            "currency": pick.get("currency"), "total": pick.get("total_balance"),
            "granted": pick.get("granted_balance"), "toppedUp": pick.get("topped_up_balance"),
            "at": now_ms()}
    app.balance_cache = {"at": time.time(), "data": data}
    return data


# ---------------------------------------------------------------- 启动

def open_window(url: str) -> None:
    """用系统默认浏览器打开页面。

    走 explorer.exe：它固定在普通权限下运行，能避免从提权进程里把浏览器也拉成管理员实例
    （那会弹"现有实例正在以提升的权限运行"）。explorer 不可用时再退回 ShellExecute。

    subprocess / webbrowser 在这里才导入：安卓壳用 --no-window，永远走不到这里，
    放在模块顶部只会增加 "导入 server.py 就失败" 的面。
    """
    import subprocess
    try:
        subprocess.Popen(["explorer.exe", url])
        return
    except OSError as exc:
        print(f"explorer 打开失败({exc})，改用系统默认方式")
    try:
        import webbrowser
        webbrowser.open(url)
    except Exception as exc:  # noqa: BLE001
        print(f"打开浏览器失败({exc})，请手动访问 {url}")


def local_ips() -> list:
    """本机在局域网里的 IPv4 地址（不含回环）。

    用 UDP connect 取出口地址：不会真的发包，只是让系统选出默认网卡。
    安卓上需要 INTERNET 权限（已有），且不依赖 DNS。
    """
    ips = []
    try:
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        probe.settimeout(0.3)
        try:
            # 任意公网地址均可；目的地址不合法也无妨，只为了拿到出口网卡
            probe.connect(("223.5.5.5", 80))
            ip = probe.getsockname()[0]
            if ip and not ip.startswith("127."):
                ips.append(ip)
        finally:
            probe.close()
    except OSError:
        pass
    if not ips:
        # 没网时的退路：用主机名解析
        try:
            for info in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET):
                ip = info[4][0]
                if ip and not ip.startswith("127.") and ip not in ips:
                    ips.append(ip)
        except OSError:
            pass
    return ips


def make_handler(app: App, kind: str):
    cls = UIServer if kind == "ui" else SyncServer
    return type(f"Handler_{kind}", (cls,), {"app": app})


# ---------------------------------------------------------------- 自动发现


def _broadcast_targets() -> list:
    """要广播的目标地址：受限广播 + 按本机网卡推算的子网广播。

    只发 255.255.255.255 在部分安卓设备上会被丢弃，补上子网广播更可靠。
    """
    targets = ["255.255.255.255"]
    for ip in local_ips():
        parts = ip.split(".")
        if len(parts) == 4:
            targets.append(".".join(parts[:3] + ["255"]))
    return targets


def discover_peers(app: App, timeout: float = 2.0) -> list:
    """广播询问局域网里还有谁，收集应答。

    返回 [{"name", "ip", "port"}]，已去重。
    """
    found = {}
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
        sock.settimeout(0.4)
        payload = DISCOVER_MAGIC + b" " + json.dumps(
            {"name": app.config.get("deviceName", socket.gethostname()),
             "port": app.sync_port}).encode()
        for target in _broadcast_targets():
            try:
                sock.sendto(payload, (target, DISCOVERY_PORT))
            except OSError:
                continue
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                data, addr = sock.recvfrom(2048)
            except socket.timeout:
                continue
            except OSError:
                break
            if not data.startswith(DISCOVER_REPLY):
                continue
            try:
                info = json.loads(data.split(b" ", 1)[1].decode("utf-8"))
            except (ValueError, IndexError):
                continue
            port = int(info.get("port") or 8791)
            ip = addr[0]
            name = info.get("name") or ip
            # 同一台机器会从多个网卡各回一次，按设备名去重，
            # 否则列表里会出现两条同名设备（一个有线一个无线）。
            key = name if name != ip else f"{ip}:{port}"
            found.setdefault(key, {"name": name, "ip": ip, "port": port})
    finally:
        sock.close()
    return list(found.values())


def discovery_server(app: App) -> None:
    """后台线程：监听发现广播并应答。绑定失败不致命（比如端口被占）。"""
    try:
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("", DISCOVERY_PORT))
        sock.settimeout(2.0)
    except OSError as exc:
        app.log(f"发现端口 {DISCOVERY_PORT} 不可用: {exc}")
        return
    app.log(f"自动发现已就绪 (UDP {DISCOVERY_PORT})")
    name = app.config.get("deviceName") or socket.gethostname()
    reply = DISCOVER_REPLY + b" " + json.dumps(
        {"name": name, "port": app.sync_port}).encode()
    while True:
        try:
            data, addr = sock.recvfrom(2048)
        except socket.timeout:
            continue
        except OSError:
            break
        if not data.startswith(DISCOVER_MAGIC):
            continue
        try:
            sock.sendto(reply, addr)
        except OSError:
            continue


def tray_icon_path() -> str:
    """app.ico 的位置：打包后在 _MEIPASS，源码运行时在 server/ 里。"""
    return os.path.join(getattr(sys, "_MEIPASS", BASE_DIR), "app.ico")


def start_tray(url: str, data_dir: str, app) -> object:
    """Windows 上启动托盘图标。失败返回 None，调用方退回“直接开浏览器”。"""
    if sys.platform != "win32":
        return None
    try:
        from tray import TrayIcon, hide_console
    except Exception as exc:  # noqa: BLE001
        app.log(f"托盘模块不可用: {exc!r}")
        return None

    # 打包成窗口程序后不该再有控制台；即便有，也藏起来
    if FROZEN:
        try:
            hide_console()
        except Exception:  # noqa: BLE001
            pass

    icon = tray_icon_path()
    tray = TrayIcon(
        f"dsh-chat {VERSION}",
        icon if os.path.isfile(icon) else None,
        on_open=lambda: open_window(url),
        on_data=lambda: open_window(data_dir),
        on_exit=lambda: os._exit(0),
    )
    tray.start()
    # 等图标真正建好再返回：失败时宁可退回开浏览器，也不要留个看不见的程序
    for _ in range(40):
        if tray.ok or tray.error is not None:
            break
        time.sleep(0.05)
    if not tray.ok:
        app.log(f"托盘创建失败: {tray.error!r}")
        return None
    app.log("托盘图标已就绪")
    return tray


def run_android(data_dir: str, www_dir: str, port: int = 8790) -> None:
    """安卓壳的入口：显式参数，**不经过 sys.argv**。

    为什么不设 sys.argv：Chaquopy 把 Java 的 String[] 转成 jarray，
    它能索引、能取长度，但不是真正的 Python list。
    argparse 内部会调 list.remove()，jarray 没有这个方法，直接抛
    AttributeError: 'jarray' object has no attribute 'remove'。

    这里收普通字符串（Java String -> Python str 是最基本的转换），
    在 Python 侧拼成 list 再交给 main()。
    """
    main([
        "--no-window",
        "--data", str(data_dir),
        "--www", str(www_dir),
        "--port", str(port),
    ])


def main(argv=None) -> None:
    # 安卓上静态目录在 APK 里是只读的，由宿主先解压到私有目录再传进来
    global WWW
    # 打包成窗口程序（console=False）时 sys.stdout/stderr 是 None，print 会直接抛异常
    if sys.stdout is None:
        sys.stdout = open(os.devnull, "w", encoding="utf-8")
    if sys.stderr is None:
        sys.stderr = open(os.devnull, "w", encoding="utf-8")
    if hasattr(sys.stdout, "reconfigure"):
        try:
            sys.stdout.reconfigure(encoding="utf-8", errors="replace")
            sys.stderr.reconfigure(encoding="utf-8", errors="replace")
        except Exception:
            pass
    parser = argparse.ArgumentParser(description="dsh-chat 本地服务")
    parser.add_argument("--port", type=int, default=8790)
    parser.add_argument("--sync-port", type=int, default=8791)
    parser.add_argument("--data", default=os.path.join(HERE, "data"))
    parser.add_argument("--www", default=WWW)
    parser.add_argument("--no-window", action="store_true")
    args = parser.parse_args(argv)

    # 同步端口允许在界面上改（存在 config.json 里）。命令行显式给了 --sync-port 就以命令行为准，
    # 否则默认值 8791 表示“没指定”，沿用配置里的。
    if args.sync_port == 8791:
        pre = load_json(os.path.join(os.path.abspath(args.data), "meta", "config.json"), {}) or {}
        saved = pre.get("syncPort")
        if isinstance(saved, int) and 1024 <= saved <= 65535:
            args.sync_port = saved

    WWW = os.path.abspath(args.www)
    app = App(os.path.abspath(args.data), args.port, args.sync_port)

    # UI 服务同时监听 IPv4 与 IPv6 回环：
    # 安卓上 WebView 用 http://localhost 访问，localhost 可能解析到 ::1。
    handler = make_handler(app, "ui")
    ui_servers = []
    bind_errors = []
    for host, cls in (("127.0.0.1", QuietServer), ("::1", QuietServer6)):
        try:
            ui_servers.append(cls((host, args.port), handler))
            app.log(f"bind ok: {host}:{args.port}")
        except OSError as exc:
            bind_errors.append(f"{host}: {exc!r}")
            app.log(f"bind fail: {host}:{args.port} -> {exc!r}")
    if not ui_servers:
        # 不能静默 return：外层（安卓壳）只能看到“启动超时”，完全查不出原因
        detail = "; ".join(bind_errors) or "unknown"
        msg = f"无法监听端口 {args.port}（{detail}）"
        app.log("[fatal] " + msg)
        raise RuntimeError(msg)
    ui = ui_servers[0]
    for extra in ui_servers[1:]:
        threading.Thread(target=extra.serve_forever, daemon=True).start()
    app.log(f"UI 服务监听: {', '.join(s.server_address[0] for s in ui_servers)}:{args.port}")
    try:
        sync = QuietServer(("0.0.0.0", args.sync_port), make_handler(app, "sync"))
        threading.Thread(target=sync.serve_forever, daemon=True).start()
    except OSError as exc:
        app.log(f"同步端口 {args.sync_port} 不可用: {exc}")

    threading.Thread(target=sync_loop, args=(app,), daemon=True).start()
    threading.Thread(target=discovery_server, args=(app,), daemon=True).start()
    threading.Thread(target=lambda: (time.sleep(30), archive_once(app)), daemon=True).start()

    url = f"http://127.0.0.1:{args.port}/"
    app.log(f"dsh-chat {VERSION} 就绪 {url} (数据目录 {app.data_dir})")
    app.log(f"静态目录 {WWW} (index.html {'存在' if os.path.isfile(os.path.join(WWW, 'index.html')) else '缺失'})")
    if not args.no_window:
        tray = start_tray(url, app.data_dir, app)
        if tray is not None:
            # 首次使用还没填 Key 时自动开一次，否则用户对着托盘图标不知从哪进
            if not app.key:
                threading.Timer(0.3, lambda: open_window(url)).start()
        else:
            threading.Timer(0.6, lambda: open_window(url)).start()

    try:
        ui.serve_forever()
    except KeyboardInterrupt:
        app.log("正在关闭…")


def _crash_hook(exc_type, exc, tb):
    """未捕获异常时写 crash.log 并停住窗口，避免"闪退看不到原因"。"""
    import traceback

    text = "".join(traceback.format_exception(exc_type, exc, tb))
    try:
        target = os.path.join(BASE_DIR, "crash.log")
        with open(target, "a", encoding="utf-8") as f:
            f.write(time.strftime("%Y-%m-%d %H:%M:%S ") + "\n" + text + "\n")
        print(f"\n程序出错，详情已写入：{target}")
    except OSError:
        pass
    print(text)
    # 窗口模式下没有 stdin，input() 会抛 RuntimeError
    try:
        if sys.stdin is not None and getattr(sys.stdin, "isatty", lambda: False)():
            input("按回车退出…")
    except (EOFError, RuntimeError):
        pass


if __name__ == "__main__":
    sys.excepthook = _crash_hook
    try:
        main()
    except SystemExit:
        raise
    except BaseException:
        _crash_hook(*sys.exc_info())
        sys.exit(1)
