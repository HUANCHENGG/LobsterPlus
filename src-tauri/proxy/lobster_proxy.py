#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
LobsterAI 本地 API 稳定代理（fork 自 RaccoonPlus 的 raccoon_proxy.py 转换层）

解决的问题：LobsterAI 桌面客户端在本地开放 OpenAI 兼容代理
（/v1/chat/completions、/v1/models），但端口和 apiKey 随每次重启轮换
（3248→3858→2880→1316…），CC-Switch 里配一次就失效。

原理：本代理监听固定端口（默认 19260），自动探测当前轮换的本地端口/key
并转发。上游端口/key 轮换对调用方彻底透明。

协议能力：
  - POST /v1/chat/completions   OpenAI 格式，纯透传
  - GET  /v1/models             OpenAI 格式，透传 + 缓存
  - POST /v1/messages           Anthropic 格式 → OpenAI 转换（复用 raccoon 转换层）
  - POST /v1/messages/count_tokens
  - GET  /health                健康检查（含上游探测结果）

上游探测（替换原 AuthManager）：
  候选 = openclaw/state/agents/main/agent/models.json 与 openclaw/state/openclaw.json
  两处 baseUrl/apiKey（实测会互相过期）。TCP 探活 + GET /v1/models 验证选活，
  缓存结果；请求失败自动重探测。

仅使用 Python 标准库，无第三方依赖。兼容 Python 3.8+。
"""

import base64
import json
import os
import re
import socket
import sqlite3
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG_PATH = os.path.join(HERE, "config.json")
USAGE_DB_PATH = os.path.join(HERE, "usage.sqlite")
USAGE_SCHEMA_VERSION = 2
# 代理脚本版本：Rust 侧启动时用它判断磁盘上的脚本是否比正在运行的进程新。
# 升级安装后旧进程仍在跑，必须能识别出来并重启，否则改再多次也不生效
# （v0.3.3 实测：另一台机器升级后 CC-Switch 拿到的仍是旧的 5 个模型）。
PROXY_SCRIPT_VERSION = "0.3.4"

# 客户端在响应写完之前主动断开连接时会抛出的异常。
# 注意三者是「兄弟」而非父子关系：ConnectionAbortedError 既不是
# BrokenPipeError 也不是 ConnectionResetError，只捕获后两者会漏掉它，
# Windows 上客户端超时/取消时抛的正是 ConnectionAbortedError
# [WinError 10053]，于是 socketserver 会打印一整段吓人的 traceback，
# 而实际上代理没坏、只是对端先挂了。三者共同父类是 ConnectionError。
CLIENT_GONE_ERRORS = (BrokenPipeError, ConnectionResetError, ConnectionAbortedError)

DEFAULT_CONFIG = {
    "listen_host": "127.0.0.1",
    "listen_port": 19260,
    # LobsterAI 数据目录（探测上游配置用）
    "lobster_data_dir": os.path.join(
        os.path.expandvars("%APPDATA%" if os.name == "nt" else "~/.config"),
        "LobsterAI"),
    "default_model": "deepseek-flash",
    "emit_thinking": False,
    "max_tokens_cap": 100000,
    "request_timeout": 330,
    "log_requests": True,
    "probe_recheck_seconds": 60,
    # 代理自身鉴权：空 = 不校验（仅监听 127.0.0.1）
    "api_key": "",
    "models": {},
}


def log(msg):
    sys.stdout.write("[%s] %s\n" % (time.strftime("%H:%M:%S"), msg))
    sys.stdout.flush()


def load_config():
    cfg = dict(DEFAULT_CONFIG)
    if os.path.exists(CONFIG_PATH):
        try:
            with open(CONFIG_PATH, "r", encoding="utf-8") as f:
                user = json.load(f)
            cfg.update(user)
        except Exception as e:
            log("警告: 读取 config.json 失败，使用默认配置 (%s)" % e)
    d = cfg.get("lobster_data_dir")
    if d:
        cfg["lobster_data_dir"] = os.path.expanduser(os.path.expandvars(str(d)))
    env_port = os.environ.get("LOBSTER_PROXY_PORT")
    if env_port:
        try:
            cfg["listen_port"] = int(env_port)
        except ValueError:
            pass
    return cfg


CONFIG = load_config()


# --------------------------------------------------------------------------
# 用量统计（仅保存按日聚合，不保存请求内容或密钥）
# --------------------------------------------------------------------------

class UsageStore(object):
    """SQLite 按日聚合统计；每次写入使用独立连接和单条 UPSERT 短事务。"""

    COLUMNS = ("day", "protocol", "model", "input", "output", "request",
               "success", "error", "accurate", "estimated", "missing")

    def __init__(self, path=USAGE_DB_PATH):
        self.path = path
        self._init_lock = threading.Lock()
        self._ready = False

    def _connect(self):
        conn = sqlite3.connect(self.path, timeout=5.0)
        conn.execute("PRAGMA busy_timeout=5000")
        return conn

    @staticmethod
    def _is_busy(exc):
        text = str(exc).lower()
        return "locked" in text or "busy" in text

    def _retry(self, operation):
        for attempt in range(4):
            try:
                return operation()
            except sqlite3.OperationalError as e:
                if not self._is_busy(e) or attempt == 3:
                    raise
                time.sleep(0.05 * (attempt + 1))

    @staticmethod
    def _table_exists(conn, name):
        return conn.execute(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)
        ).fetchone() is not None

    def _validate_schema(self, conn):
        rows = conn.execute("PRAGMA table_info(usage_daily)").fetchall()
        columns = tuple(row[1] for row in rows)
        primary = tuple(row[1] for row in sorted(rows, key=lambda row: row[5]) if row[5])
        if columns != self.COLUMNS or primary != ("day", "protocol", "model"):
            raise RuntimeError("usage_daily schema 不兼容: columns=%r pk=%r" %
                               (columns, primary))

    def _initialize_once(self):
        conn = self._connect()
        try:
            conn.execute("PRAGMA journal_mode=WAL")
            conn.execute("BEGIN IMMEDIATE")
            conn.execute("""
                CREATE TABLE IF NOT EXISTS usage_daily (
                    day TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    model TEXT NOT NULL,
                    input INTEGER NOT NULL DEFAULT 0,
                    output INTEGER NOT NULL DEFAULT 0,
                    request INTEGER NOT NULL DEFAULT 0,
                    success INTEGER NOT NULL DEFAULT 0,
                    error INTEGER NOT NULL DEFAULT 0,
                    accurate INTEGER NOT NULL DEFAULT 0,
                    estimated INTEGER NOT NULL DEFAULT 0,
                    missing INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY (day, protocol, model)
                )
            """)
            self._validate_schema(conn)
            if self._table_exists(conn, "daily_usage"):
                conn.execute("""
                    INSERT INTO usage_daily
                        (day, protocol, model, input, output, request, success,
                         error, accurate, estimated, missing)
                    SELECT day, protocol, model,
                           SUM(input_tokens), SUM(output_tokens), SUM(requests),
                           SUM(successes), SUM(failures),
                           SUM(CASE WHEN source = 'upstream' THEN requests ELSE 0 END),
                           SUM(CASE WHEN source = 'estimated' THEN requests ELSE 0 END),
                           SUM(CASE WHEN source NOT IN ('upstream', 'estimated')
                                    THEN requests ELSE 0 END)
                    FROM daily_usage GROUP BY day, protocol, model
                    ON CONFLICT(day, protocol, model) DO UPDATE SET
                        input = input + excluded.input,
                        output = output + excluded.output,
                        request = request + excluded.request,
                        success = success + excluded.success,
                        error = error + excluded.error,
                        accurate = accurate + excluded.accurate,
                        estimated = estimated + excluded.estimated,
                        missing = missing + excluded.missing
                """)
                conn.execute("DROP TABLE daily_usage")
            conn.execute("PRAGMA user_version=%d" % USAGE_SCHEMA_VERSION)
            conn.commit()
        except Exception:
            conn.rollback()
            raise
        finally:
            conn.close()

    def initialize(self):
        if self._ready:
            return
        with self._init_lock:
            if self._ready:
                return
            self._retry(self._initialize_once)
            self._ready = True

    def record(self, protocol, model, success, input_tokens, output_tokens, source,
               day=None):
        self.initialize()
        day = day or time.strftime("%Y-%m-%d", time.localtime())
        source = source if source in ("accurate", "estimated", "missing") else "missing"
        source_counts = [1 if source == name else 0
                         for name in ("accurate", "estimated", "missing")]
        values = (day, str(protocol or "unknown"), str(model or "unknown"),
                  max(0, int(input_tokens or 0)), max(0, int(output_tokens or 0)),
                  1, 1 if success else 0, 0 if success else 1) + tuple(source_counts)

        def write_once():
            conn = self._connect()
            try:
                conn.execute("""
                    INSERT INTO usage_daily
                        (day, protocol, model, input, output, request, success,
                         error, accurate, estimated, missing)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(day, protocol, model) DO UPDATE SET
                        input = input + excluded.input,
                        output = output + excluded.output,
                        request = request + excluded.request,
                        success = success + excluded.success,
                        error = error + excluded.error,
                        accurate = accurate + excluded.accurate,
                        estimated = estimated + excluded.estimated,
                        missing = missing + excluded.missing
                """, values)
                conn.commit()
            finally:
                conn.close()

        self._retry(write_once)


USAGE = UsageStore()


def _safe_record_usage(protocol, model, success, input_tokens=0, output_tokens=0,
                       source="estimated"):
    """统计失败不得影响转发。"""
    try:
        USAGE.record(protocol, model, success, input_tokens, output_tokens, source)
    except Exception as e:
        log("警告: 用量统计写入失败 (%s)" % e)


def _usage_values(usage, fallback_input=0, fallback_output=0):
    """提取 OpenAI usage；任一 token 字段缺失时整体按估算标记。"""
    usage = usage if isinstance(usage, dict) else {}
    has_exact = (usage.get("prompt_tokens") is not None
                 and usage.get("completion_tokens") is not None)
    if has_exact:
        return (max(0, int(usage.get("prompt_tokens") or 0)),
                max(0, int(usage.get("completion_tokens") or 0)), "accurate")
    return (max(0, int(fallback_input or 0)),
            max(0, int(fallback_output or 0)), "estimated")


# --------------------------------------------------------------------------
# 上游探测（稳定器核心，替换 raccoon 的 AuthManager）
# --------------------------------------------------------------------------

def _load_json(path):
    try:
        with open(path, encoding="utf-8", errors="ignore") as f:
            return json.load(f)
    except Exception:
        return None


def _pick_model(p):
    models = p.get("models") or []
    if not models:
        return None
    # 默认挑列表里第一个带 id 的（deepseek-flash 系优先）
    for m in models:
        if m.get("id") == "deepseek-flash":
            return "deepseek-flash"
    return models[0].get("id")


def _model_entries_from_provider(p):
    """提取 provider 的模型目录（真实 ID + 展示名）。

    上游真实 ID 才是可调用的键；name（如 DeepSeek-V4.1-Flash）只用于展示。
    CC-Switch 的模型选择器只读 id，但其他客户端会展示 display_name。
    """
    out = []
    for m in p.get("models") or []:
        if not isinstance(m, dict) or not m.get("id"):
            continue
        mid = str(m["id"])
        out.append({
            "id": mid,
            "display_name": str(m.get("name") or m.get("display_name") or mid),
        })
    return out


def _collect_model_catalogs(data_dir):
    """从 models.json 与 openclaw.json 收集 (base_url, [模型目录])。

    与 _collect_candidates 不同，这里不要求 apiKey：模型目录本身与鉴权无关，
    即使某一处 baseUrl 已过期，模型清单仍然有效（端口轮换不动模型列表）。
    """
    out = []
    mf = os.path.join(data_dir, "openclaw", "state", "agents", "main", "agent", "models.json")
    cfg = _load_json(mf)
    if cfg and cfg.get("providers"):
        for pid, p in cfg["providers"].items():
            if "lobsterai" not in pid.lower() and p.get("api") != "lobsterai-model-compat":
                continue
            entries = _model_entries_from_provider(p)
            if p.get("baseUrl") and entries:
                out.append((p["baseUrl"], entries))
    oc = _load_json(os.path.join(data_dir, "openclaw", "state", "openclaw.json"))
    if oc:
        for pid, p in ((oc.get("models", {}) or {}).get("providers", {}) or {}).items():
            if "lobsterai" not in pid.lower():
                continue
            entries = _model_entries_from_provider(p)
            if p.get("baseUrl") and entries:
                out.append((p["baseUrl"], entries))
    return out


def _server_model_catalog(data_dir, timeout=6):
    """从 LobsterAI 服务端 `GET /api/models/available` 拉权威模型目录。

    实测这是客户端自己用的唯一权威来源（app.asar 的
    `[Auth:getModels] requesting available models at ...`）：返回
    `{"code":0,"data":[{modelId, modelName, contextWindow, accessible, ...}]}`。
    本地 openclaw 配置只是它的投影，可能缺失或过期（另一台机器实测为空，
    代理只能退回 config.json 的映射名）。

    鉴权用 live `lobsterai.sqlite` 的 kv.auth_tokens.accessToken。只读打开
    （`mode=ro`），绝不在客户端运行时写 live 库——数据库有活跃 WAL，
    并发写会损坏数据。

    失败一律返回 None（离线/未登录/接口变更都不影响代理转发）。
    """
    try:
        import sqlite3
        db = os.path.join(data_dir, "lobsterai.sqlite")
        if not os.path.isfile(db):
            return None
        uri = "file:" + urllib.request.pathname2url(db) + "?mode=ro"
        conn = sqlite3.connect(uri, uri=True, timeout=3)
        try:
            row = conn.execute(
                "SELECT value FROM kv WHERE key='auth_tokens'").fetchone()
        finally:
            conn.close()
        if not row:
            return None
        token = (json.loads(row[0]) or {}).get("accessToken")
        if not token:
            return None
        # testMode 用 inner 域名；可用 config.json 的 server_base_url 覆盖
        base = str(CONFIG.get("server_base_url") or "").strip()
        if not base:
            base = "https://lobsterai-server.youdao.com"
        req = urllib.request.Request(
            base + "/api/models/available",
            headers={"Authorization": "Bearer " + token,
                     "Accept": "application/json"})
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            body = json.loads(resp.read().decode("utf-8", "replace"))
        if not isinstance(body, dict) or body.get("code") != 0:
            return None
        entries = []
        for m in body.get("data") or []:
            if not isinstance(m, dict):
                continue
            mid = str(m.get("modelId") or "").strip()
            if not mid:
                continue
            entries.append({
                "id": mid,
                "display_name": str(m.get("modelName") or mid),
                "context_window": m.get("contextWindow"),
                "accessible": m.get("accessible"),
                "restriction": m.get("restrictionHint"),
            })
        return entries or None
    except Exception as e:
        log("警告: 服务端模型目录获取失败 (%s)" % e)
        return None


def _collect_candidates(data_dir):
    """从 models.json 与 openclaw.json 收集 (base_url, api_key, default_model) 候选。
    两处配置在端口轮换后都可能过期（任一来源都不可全信）。"""
    cands = []
    mf = os.path.join(data_dir, "openclaw", "state", "agents", "main", "agent", "models.json")
    cfg = _load_json(mf)
    if cfg and cfg.get("providers"):
        for pid, p in cfg["providers"].items():
            if "lobsterai" in pid.lower() or p.get("api") == "lobsterai-model-compat":
                if p.get("baseUrl") and p.get("apiKey"):
                    cands.append((p["baseUrl"], p["apiKey"], _pick_model(p)))
    oc = _load_json(os.path.join(data_dir, "openclaw", "state", "openclaw.json"))
    if oc:
        for pid, p in ((oc.get("models", {}) or {}).get("providers", {}) or {}).items():
            if "lobsterai" in pid.lower() and p.get("baseUrl") and p.get("apiKey"):
                cands.append((p["baseUrl"], p["apiKey"], _pick_model(p)))
    # 去重（保序）
    seen, out = set(), []
    for c in cands:
        k = (c[0], c[1])
        if k not in seen:
            seen.add(k)
            out.append(c)
    return out


def _port_open(base_url, timeout=1.5):
    try:
        u = urllib.parse.urlparse(base_url)
        with socket.create_connection((u.hostname, u.port), timeout=timeout):
            return True
    except Exception:
        return False


def _probe_upstream(base_url, api_key):
    """TCP 通 + 最小 chat 请求可用 → 认定候选有效。
    实测上游（lobsterai-model-compat）没有 /v1/models 端点（404），
    真实端点只有 /v1/chat/completions（错 key 也会回 200+data 流），
    所以探活改用「端口可达 + chat 端点存在」判定。"""
    if not _port_open(base_url):
        return None
    url = base_url.rstrip("/")
    if not url.endswith("/v1"):
        url = url + "/v1"
    body = json.dumps({
        "model": "deepseek-flash",
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": False,
    }).encode("utf-8")
    req = urllib.request.Request(
        url + "/chat/completions",
        data=body,
        headers={"Authorization": "Bearer " + api_key,
                 "Content-Type": "application/json"},
        method="POST")
    try:
        with urllib.request.urlopen(req, timeout=4) as _resp:
            # 能拿到 HTTP 200 响应即认为端点活着（读首块即弃）
            _resp.read(256)
            return True
    except urllib.error.HTTPError:
        # 4xx/5xx 也证明 chat 端点存在（服务活着、只是拒绝请求）
        return True
    except Exception:
        return None


class UpstreamManager(object):
    """探测并缓存当前 LobsterAI 本地代理的 (base_url, api_key)。
    缓存有效期 probe_recheck_seconds；请求失败可 force_probe 立即重探测。"""

    def __init__(self, cfg):
        self.cfg = cfg
        self.data_dir = cfg["lobster_data_dir"]
        self._lock = threading.Lock()
        # _probe_lock 串行化探测（网络动作），_lock 只保护字段读写。
        # 两者分开：probe 最坏要 1.5s×候选 + 4s chat 请求，持 _lock 期间
        # /health 的 peek() 会一起阻塞（踩坑 10 的同类问题）。
        self._probe_lock = threading.Lock()
        self._cached = None        # (base_url, api_key, default_model, probed_at)
        self._models_cache = None  # /v1/models 响应（探测时顺带拉取）
        self._models_at = 0.0
        # 服务端权威目录单独缓存：它要走 HTTPS，最坏 6s，绝不能持 _lock 拉取
        self._server_catalog = None
        self._server_at = 0.0
        self._server_lock = threading.Lock()
        self._server_refreshing = False

    def probe(self, force=False):
        with self._lock:
            now = time.time()
            if (not force and self._cached
                    and now - self._cached[3] < int(self.cfg.get("probe_recheck_seconds", 60))):
                return self._cached[:3]
        # 双重检查：等锁期间别的线程可能已经探测完
        with self._probe_lock:
            with self._lock:
                now = time.time()
                if (not force and self._cached
                        and now - self._cached[3]
                        < int(self.cfg.get("probe_recheck_seconds", 60))):
                    return self._cached[:3]
            cands = _collect_candidates(self.data_dir)
            if not cands:
                with self._lock:
                    self._cached = None
                return None
            base, key, model = None, None, None
            for cb, ck, cm in cands:
                if _probe_upstream(cb, ck):
                    base, key, model = cb, ck, cm
                    break
            if base is None:
                # 全部候选验证失败：退回第一个端口可达的（可能 /models 校验太严）
                for cb, ck, cm in cands:
                    if _port_open(cb):
                        base, key, model = cb, ck, cm
                        break
            if base is None:
                with self._lock:
                    self._cached = None
                return None
            # 优先跨候选挑选稳定的 key/model（端口轮换时 key 也变，取探测通过那组的）
            with self._lock:
                self._cached = (base, key, model, now)
            entries = self._fetch_models(base, key)
            with self._lock:
                self._models_cache = entries
                self._models_at = now
            return base, key, model

    # -- 服务端权威目录（独立缓存，永不持 _lock 做网络） -------------------

    def _refresh_server_catalog(self):
        try:
            entries = _server_model_catalog(self.data_dir)
            if entries:
                with self._server_lock:
                    self._server_catalog = entries
                    self._server_at = time.time()
                # 立即用新目录重建模型缓存，而不是只置 None 等懒加载：
                # /health 走 peek() 只读缓存，置 None 会让它在下次
                # /v1/models 之前一直返回空/仅 config 的残缺清单。
                base = None
                with self._lock:
                    if self._cached:
                        base = self._cached[0]
                if base:
                    rebuilt = self._catalog_model_list(base)
                    with self._lock:
                        self._models_cache = rebuilt
                        self._models_at = time.time()
                log("服务端模型目录: %d 个模型" % len(entries))
        except Exception as e:
            log("警告: 服务端模型目录刷新失败 (%s)" % e)
        finally:
            with self._server_lock:
                self._server_refreshing = False

    def server_catalog(self, timeout=0, force=False):
        """取服务端目录。

        timeout<=0：只读缓存，过期时后台异步刷新（毫秒级返回，给请求路径用）。
        timeout>0 ：在锁外等待刷新完成（最多 timeout 秒）；force 时即使有
                    旧值也重新拉取（后台刷新线程用）。
        """
        with self._server_lock:
            current = self._server_catalog
            fresh = (current is not None
                     and time.time() - self._server_at < 600)
            if fresh and not force:
                return current
            already = self._server_refreshing
            if timeout <= 0:
                if not already:
                    self._server_refreshing = True
                spawn = not already
            else:
                spawn = not already
                if spawn:
                    self._server_refreshing = True
        if timeout <= 0:
            if spawn:
                threading.Thread(target=self._refresh_server_catalog,
                                 name="server-model-catalog", daemon=True).start()
            with self._server_lock:
                return self._server_catalog
        if spawn:
            # 在锁外做网络；内部会自行清 _server_refreshing
            self._refresh_server_catalog()
        else:
            deadline = time.time() + max(0.0, float(timeout))
            while time.time() < deadline:
                with self._server_lock:
                    if not self._server_refreshing:
                        break
                time.sleep(0.05)
        with self._server_lock:
            return self._server_catalog

    def _catalog_model_list(self, base_url):
        """由当前目录拼 OpenAI 格式清单（不访问上游端点，纯本地）。"""
        entries = self._catalog_entries(base_url)
        if not entries:
            return None
        return {"object": "list",
                "data": [{"id": e["id"], "type": "model",
                          "display_name": e["display_name"]} for e in entries]}

    def _fetch_models(self, base, key):
        """上游模型清单（无锁版本，调用方须在 _probe_lock 内）。

        优先级：上游 `/models`（实测 404，仅防未来变化）→ 服务端
        `/api/models/available`（权威，带真实显示名）→ 本地 openclaw 配置。
        绝不返回 config.json 里的 claude-* 映射别名——那些是协议转换用的
        虚拟名，不是可提供的模型。
        """
        upstream = None
        try:
            req = urllib.request.Request(
                base.rstrip("/") + "/models",
                headers={"Authorization": "Bearer " + key})
            with urllib.request.urlopen(req, timeout=4) as resp:
                upstream = json.loads(resp.read().decode("utf-8", "replace"))
        except Exception:
            upstream = None
        if isinstance(upstream, dict) and (upstream.get("data") or upstream.get("models")):
            return upstream
        local = self._catalog_model_list(base)
        if local:
            return local
        if isinstance(upstream, dict):
            return upstream
        return None

    def _catalog_entries(self, base_url):
        """模型目录：服务端权威目录优先，本地 openclaw 配置兜底（按 id 去重）。

        服务端 `/api/models/available` 是客户端自己的来源，模型全集与
        `accessible` 标记都在这里；本地 openclaw 只是它的投影，可能缺失
        （另一台机器实测为空）或过期。两者都拿不到时才返回空（调用方再
        回退 config 映射，仅用于 /health 展示）。
        """
        out, seen = [], set()
        for e in self.server_catalog() or []:
            if e["id"] in seen:
                continue
            seen.add(e["id"])
            out.append(e)
        catalogs = _collect_model_catalogs(self.data_dir)
        # 先取 baseUrl 与当前探测结果相同的目录（最可信），再补充另一处
        ordered = sorted(catalogs, key=lambda c: c[0] != base_url)
        for _cb, entries in ordered:
            for e in entries:
                if e["id"] in seen:
                    continue
                seen.add(e["id"])
                out.append(e)
        return out

    def models(self):
        """上游模型列表（缓存 60s）。LobsterAI 不在运行时返回 None。"""
        found = self.probe()
        if not found:
            return None
        now = time.time()
        with self._lock:
            cached = self._models_cache
            stale = cached is None or now - self._models_at > 60
        if not stale:
            return cached
        # 刷新也要串行化，避免多线程同时打上游
        with self._probe_lock:
            with self._lock:
                cached = self._models_cache
                stale = cached is None or time.time() - self._models_at > 60
            if not stale:
                return cached
            entries = self._fetch_models(found[0], found[1])
            with self._lock:
                self._models_cache = entries
                self._models_at = time.time()
            return entries

    def peek(self):
        """只读当前缓存，绝不触发网络探测。

        给 /health 用：健康检查必须是毫秒级返回。若走 probe()，
        上游不在时会依次做 TCP 探活（1.5s/候选）+ 最小 chat 请求（4s），
        远超 GUI 端 1.5s 的健康检查超时——GUI 判定代理挂了并关闭连接，
        代理这边就报 WinError 10053。返回 (base, alive, models) 或 None。
        """
        with self._lock:
            if not self._cached:
                return None
            base = self._cached[0]
            m = self._models_cache
        return base, m

    def upstream_model_ids(self):
        m = self.models()
        if not m:
            return []
        return [x.get("id") for x in (m.get("data") or []) if x.get("id")]


UPSTREAM = UpstreamManager(CONFIG)


# --------------------------------------------------------------------------
# 模型名映射
# --------------------------------------------------------------------------

def resolve_model(name):
    """把调用方传来的模型名映射到上游真实模型 ID。
    规则：上游存在的 ID 直通；claude-* 前缀走 config models 映射；兜底 default。"""
    if not name:
        return CONFIG["default_model"]
    raw = str(name).strip()
    clean = re.sub(r"\[[^\]]*\]$", "", raw).strip()

    models = CONFIG.get("models") or {}
    for candidate in (raw, clean):
        if candidate in models:
            entry = models[candidate]
            if isinstance(entry, dict):
                return entry.get("id") or CONFIG["default_model"]
            return str(entry)

    # 上游实际存在的模型 ID 直通（探测结果优先，config 声明的兜底）
    known = set(UPSTREAM.upstream_model_ids())
    for entry in models.values():
        mid = entry.get("id") if isinstance(entry, dict) else entry
        if mid:
            known.add(mid)
    if clean in known:
        return clean

    return CONFIG["default_model"]


def model_limits(model_id):
    models = CONFIG.get("models") or {}
    for entry in models.values():
        if isinstance(entry, dict) and entry.get("id") == model_id:
            return (entry.get("context_window") or 1000000,
                    entry.get("max_tokens") or CONFIG["max_tokens_cap"])
    return (1000000, CONFIG["max_tokens_cap"])


def _config_real_models():
    """config.json 里 name == id 的映射项（真实上游 ID），不含 claude-* 别名。

    别名（claude-opus-4-8 → glm-5.3）只是协议转换用的虚拟名，不是可提供的
    模型；把它们列进 /v1/models 会让 CC-Switch 的模型选择器出现一堆不可用
    项（另一台机器只剩 5 个兜底映射时尤其明显）。
    """
    out = []
    for name, entry in (CONFIG.get("models") or {}).items():
        mid = entry.get("id") if isinstance(entry, dict) else entry
        if name and mid and name == mid:
            out.append(mid)
    return out


def list_models():
    """合并上游真实模型清单与 config 真实 ID（去重，真实模型优先）。"""
    out = []
    seen = set()
    m = UPSTREAM.models()
    if m:
        for x in m.get("data") or []:
            mid = x.get("id")
            if mid and mid not in seen:
                seen.add(mid)
                out.append({"id": mid, "display_name": x.get("display_name") or mid})
    for mid in _config_real_models():
        if mid not in seen:
            seen.add(mid)
            out.append({"id": mid, "display_name": mid})
    return out


def _models_from_cache(m):
    """由（可能为 None 的）模型缓存拼出 /health 的模型名列表，不触发网络。"""
    out = []
    seen = set()
    for x in (m or {}).get("data") or []:
        mid = x.get("id")
        if mid and mid not in seen:
            seen.add(mid)
            out.append(mid)
    for mid in _config_real_models():
        if mid not in seen:
            seen.add(mid)
            out.append(mid)
    return out


# --------------------------------------------------------------------------
# 请求转换：Anthropic -> OpenAI（自 raccoon_proxy.py 移植，实战验证过的转换层）
# --------------------------------------------------------------------------

def _blocks_to_text(content):
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for b in content:
        if isinstance(b, str):
            parts.append(b)
        elif isinstance(b, dict) and b.get("type") == "text":
            parts.append(b.get("text") or "")
    return "".join(parts)


def _anthropic_image_to_openai(block):
    src = block.get("source") or {}
    stype = src.get("type")
    if stype == "base64":
        media = src.get("media_type") or "image/png"
        return {"type": "image_url",
                "image_url": {"url": "data:%s;base64,%s" % (media, src.get("data") or "")}}
    if stype == "url":
        return {"type": "image_url", "image_url": {"url": src.get("url") or ""}}
    return None


def _content_to_openai_parts(content):
    if isinstance(content, str):
        return ([{"type": "text", "text": content}], False)
    if not isinstance(content, list):
        return ([], False)
    parts = []
    has_non_text = False
    for b in content:
        if isinstance(b, str):
            parts.append({"type": "text", "text": b})
            continue
        if not isinstance(b, dict):
            continue
        bt = b.get("type")
        if bt == "text":
            parts.append({"type": "text", "text": b.get("text") or ""})
        elif bt == "image":
            img = _anthropic_image_to_openai(b)
            if img:
                parts.append(img)
                has_non_text = True
    return (parts, has_non_text)


def anthropic_to_openai_request(body):
    """把 Anthropic Messages 请求体转成 OpenAI Chat Completions 请求体。"""
    model_id = resolve_model(body.get("model"))
    ctx, model_max = model_limits(model_id)

    messages = []

    sys_content = body.get("system")
    if sys_content:
        text = _blocks_to_text(sys_content)
        if text:
            messages.append({"role": "system", "content": text})

    for msg in body.get("messages") or []:
        if not isinstance(msg, dict):
            continue
        role = msg.get("role")
        content = msg.get("content")

        if role == "assistant":
            text = ""
            tool_calls = []
            if isinstance(content, list):
                for b in content:
                    if not isinstance(b, dict):
                        continue
                    bt = b.get("type")
                    if bt == "text":
                        text += b.get("text") or ""
                    elif bt == "tool_use":
                        tool_calls.append({
                            "id": b.get("id") or ("call_" + uuid.uuid4().hex[:16]),
                            "type": "function",
                            "function": {
                                "name": b.get("name") or "",
                                "arguments": json.dumps(b.get("input") or {},
                                                        ensure_ascii=False),
                            },
                        })
            elif isinstance(content, str):
                text = content

            out = {"role": "assistant", "content": text}
            if tool_calls:
                out["tool_calls"] = tool_calls
            messages.append(out)
            continue

        if isinstance(content, str):
            messages.append({"role": "user", "content": content})
            continue

        if not isinstance(content, list):
            continue

        for b in content:
            if isinstance(b, dict) and b.get("type") == "tool_result":
                tr = b.get("content")
                if isinstance(tr, list):
                    tr_text = _blocks_to_text(tr)
                    for sub in tr:
                        if isinstance(sub, dict) and sub.get("type") == "image":
                            tr_text += "\n[图片内容]"
                else:
                    tr_text = tr if isinstance(tr, str) else json.dumps(tr, ensure_ascii=False)
                if b.get("is_error"):
                    tr_text = "ERROR: " + tr_text
                messages.append({
                    "role": "tool",
                    "tool_call_id": b.get("tool_use_id") or "",
                    "content": tr_text or "",
                })

        parts, has_non_text = _content_to_openai_parts(content)
        parts = [p for p in parts if not (p.get("type") == "text" and not p.get("text"))]
        if parts:
            if has_non_text:
                messages.append({"role": "user", "content": parts})
            else:
                messages.append({
                    "role": "user",
                    "content": "".join(p.get("text", "") for p in parts),
                })

    max_tokens = body.get("max_tokens")
    try:
        max_tokens = int(max_tokens)
    except (TypeError, ValueError):
        max_tokens = min(8192, model_max)
    max_tokens = max(1, min(max_tokens, model_max, CONFIG["max_tokens_cap"]))

    out = {
        "model": model_id,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": bool(body.get("stream")),
    }

    for key in ("temperature", "top_p"):
        if body.get(key) is not None:
            out[key] = body[key]

    if body.get("stop_sequences"):
        out["stop"] = body["stop_sequences"]

    tools = body.get("tools")
    if tools:
        conv = []
        for t in tools:
            if not isinstance(t, dict):
                continue
            name = t.get("name")
            if not name:
                continue
            fn = {"name": name,
                  "parameters": t.get("input_schema") or {"type": "object", "properties": {}}}
            if t.get("description"):
                fn["description"] = t["description"]
            conv.append({"type": "function", "function": fn})
        if conv:
            out["tools"] = conv

    tc = body.get("tool_choice")
    if isinstance(tc, dict):
        ttype = tc.get("type")
        if ttype == "auto":
            out["tool_choice"] = "auto"
        elif ttype == "any":
            out["tool_choice"] = "required"
        elif ttype == "none":
            out["tool_choice"] = "none"
        elif ttype == "tool" and tc.get("name"):
            out["tool_choice"] = {"type": "function",
                                  "function": {"name": tc["name"]}}

    if out["stream"]:
        out["stream_options"] = {"include_usage": True}

    return out, model_id


# --------------------------------------------------------------------------
# 响应转换：OpenAI -> Anthropic（自 raccoon_proxy.py 移植）
# --------------------------------------------------------------------------

FINISH_MAP = {
    "stop": "end_turn",
    "length": "max_tokens",
    "tool_calls": "tool_use",
    "function_call": "tool_use",
    "content_filter": "end_turn",
}


def map_finish(reason):
    return FINISH_MAP.get(reason or "", "end_turn")


def _parse_arguments(raw):
    if raw is None or raw == "":
        return {}
    if isinstance(raw, dict):
        return raw
    try:
        val = json.loads(raw)
        return val if isinstance(val, dict) else {"value": val}
    except Exception:
        return {}


def _estimate_tokens(obj):
    if obj is None:
        return 0
    if isinstance(obj, str):
        return max(1, len(obj) // 4)
    if isinstance(obj, (int, float, bool)):
        return 1
    if isinstance(obj, list):
        return sum(_estimate_tokens(x) for x in obj)
    if isinstance(obj, dict):
        return sum(_estimate_tokens(v) for v in obj.values())
    return 0


def openai_to_anthropic_response(oa, model_id):
    choices = oa.get("choices") or []
    choice = choices[0] if choices else {}
    msg = choice.get("message") or {}
    finish = choice.get("finish_reason")

    content = []
    text = msg.get("content")
    if isinstance(text, str) and text:
        content.append({"type": "text", "text": text})

    for tc in msg.get("tool_calls") or []:
        fn = tc.get("function") or {}
        content.append({
            "type": "tool_use",
            "id": tc.get("id") or ("toolu_" + uuid.uuid4().hex[:20]),
            "name": fn.get("name") or "",
            "input": _parse_arguments(fn.get("arguments")),
        })

    if not content:
        content.append({"type": "text", "text": ""})

    if not finish and any(c["type"] == "tool_use" for c in content):
        finish = "tool_calls"

    usage = oa.get("usage") or {}
    return {
        "id": "msg_" + uuid.uuid4().hex[:24],
        "type": "message",
        "role": "assistant",
        "model": model_id,
        "content": content,
        "stop_reason": map_finish(finish),
        "stop_sequence": None,
        "usage": {
            "input_tokens": usage.get("prompt_tokens") or 0,
            "output_tokens": usage.get("completion_tokens") or 0,
        },
    }


# --------------------------------------------------------------------------
# 上游调用
# --------------------------------------------------------------------------

def call_upstream(payload, force_probe=False):
    """调用上游 chat/completions，返回 HTTPResponse（流式需自行读取并关闭）。"""
    found = UPSTREAM.probe(force=force_probe)
    if not found:
        raise RuntimeError("LobsterAI 本地服务不可用（客户端未运行或端口未探测到）")
    base, key, _model = found
    url = base.rstrip("/") + "/chat/completions"
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    headers = {
        "Authorization": "Bearer " + key,
        "Content-Type": "application/json",
        "Accept": "text/event-stream" if payload.get("stream") else "application/json",
    }
    req = urllib.request.Request(url, data=body, headers=headers, method="POST")
    return urllib.request.urlopen(req, timeout=CONFIG["request_timeout"])


def upstream_error_text(exc):
    if isinstance(exc, urllib.error.HTTPError):
        try:
            raw = exc.read().decode("utf-8", "replace")
        except Exception:
            raw = ""
        return exc.code, raw
    return None, str(exc)


def iter_sse_lines(resp):
    reader = getattr(resp, "read1", None) or resp.read
    buf = b""
    while True:
        try:
            block = reader(8192)
        except (socket.timeout, TimeoutError):
            break
        if not block:
            break
        buf += block
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            yield line.decode("utf-8", "replace").rstrip("\r")
    if buf:
        yield buf.decode("utf-8", "replace").rstrip("\r")


# --------------------------------------------------------------------------
# 流式转换：OpenAI SSE -> Anthropic SSE（自 raccoon_proxy.py 移植）
# --------------------------------------------------------------------------

class AnthropicStreamEmitter(object):
    def __init__(self, model_id, input_tokens_estimate=0, emit_thinking=False):
        self.model_id = model_id
        self.msg_id = "msg_" + uuid.uuid4().hex[:24]
        self.input_tokens = input_tokens_estimate
        self.emit_thinking = emit_thinking
        self.started = False
        self.finished = False
        self.next_index = 0
        self.open_index = None
        self.open_kind = None
        self.tool_slots = {}
        self.tool_ids = {}
        self.tool_names = {}
        self.finish_reason = None
        self.output_tokens = 0
        self.usage_received = False

    def _ev(self, event, data):
        return "event: %s\ndata: %s\n\n" % (event, json.dumps(data, ensure_ascii=False))

    def start_events(self):
        self.started = True
        return self._ev("message_start", {
            "type": "message_start",
            "message": {
                "id": self.msg_id,
                "type": "message",
                "role": "assistant",
                "model": self.model_id,
                "content": [],
                "stop_reason": None,
                "stop_sequence": None,
                "usage": {"input_tokens": self.input_tokens, "output_tokens": 0},
            },
        })

    def _open_block(self, kind, extra=None):
        idx = self.next_index
        self.next_index += 1
        self.open_index = idx
        self.open_kind = kind
        if kind == "text":
            block = {"type": "text", "text": ""}
        elif kind == "thinking":
            block = {"type": "thinking", "thinking": ""}
        else:
            block = {"type": "tool_use",
                     "id": (extra or {}).get("id") or ("toolu_" + uuid.uuid4().hex[:20]),
                     "name": (extra or {}).get("name") or "",
                     "input": {}}
        return self._ev("content_block_start",
                        {"type": "content_block_start", "index": idx,
                         "content_block": block})

    def _close_block(self):
        if self.open_index is None:
            return ""
        ev = self._ev("content_block_stop",
                      {"type": "content_block_stop", "index": self.open_index})
        self.open_index = None
        self.open_kind = None
        return ev

    def feed(self, chunk):
        out = []
        if not self.started:
            out.append(self.start_events())

        choices = chunk.get("choices") or []
        if not choices:
            usage = chunk.get("usage")
            if usage:
                self._absorb_usage(usage)
            return "".join(out)

        choice = choices[0]
        delta = choice.get("delta") or {}
        if choice.get("finish_reason"):
            self.finish_reason = choice["finish_reason"]

        reasoning = delta.get("reasoning_content")
        if reasoning and self.emit_thinking:
            if self.open_kind != "thinking":
                out.append(self._close_block())
                out.append(self._open_block("thinking"))
            out.append(self._ev("content_block_delta", {
                "type": "content_block_delta", "index": self.open_index,
                "delta": {"type": "thinking_delta", "thinking": reasoning}}))

        text = delta.get("content")
        if text:
            if self.open_kind != "text":
                out.append(self._close_block())
                out.append(self._open_block("text"))
            out.append(self._ev("content_block_delta", {
                "type": "content_block_delta", "index": self.open_index,
                "delta": {"type": "text_delta", "text": text}}))

        for tc in delta.get("tool_calls") or []:
            t_index = tc.get("index", 0)
            fn = tc.get("function") or {}

            if t_index not in self.tool_slots:
                out.append(self._close_block())
                tid = tc.get("id") or ("toolu_" + uuid.uuid4().hex[:20])
                name = fn.get("name") or ""
                self.tool_ids[t_index] = tid
                self.tool_names[t_index] = name
                out.append(self._open_block("tool_use", {"id": tid, "name": name}))
                self.tool_slots[t_index] = self.open_index
            else:
                if fn.get("name") and not self.tool_names.get(t_index):
                    self.tool_names[t_index] = fn["name"]
                if tc.get("id") and not self.tool_ids.get(t_index):
                    self.tool_ids[t_index] = tc["id"]

            args = fn.get("arguments")
            if args:
                out.append(self._ev("content_block_delta", {
                    "type": "content_block_delta",
                    "index": self.tool_slots[t_index],
                    "delta": {"type": "input_json_delta", "partial_json": args}}))

        usage = chunk.get("usage")
        if usage:
            self._absorb_usage(usage)

        return "".join(out)

    def _absorb_usage(self, usage):
        if (usage.get("prompt_tokens") is not None
                and usage.get("completion_tokens") is not None):
            self.usage_received = True
        if usage.get("completion_tokens"):
            self.output_tokens = usage["completion_tokens"]
        if usage.get("prompt_tokens"):
            self.input_tokens = usage["prompt_tokens"]

    def finish_events(self):
        if self.finished:
            return ""
        out = []
        if not self.started:
            out.append(self.start_events())
        out.append(self._close_block())

        reason = self.finish_reason
        if not reason and self.tool_slots:
            reason = "tool_calls"

        out.append(self._ev("message_delta", {
            "type": "message_delta",
            "delta": {"stop_reason": map_finish(reason), "stop_sequence": None},
            "usage": {"input_tokens": self.input_tokens,
                      "output_tokens": self.output_tokens},
        }))
        out.append(self._ev("message_stop", {"type": "message_stop"}))
        self.finished = True
        return "".join(out)


# --------------------------------------------------------------------------
# HTTP 处理
# --------------------------------------------------------------------------

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "LobsterPlusProxy/1.0"

    def log_message(self, fmt, *args):
        if CONFIG.get("log_requests"):
            sys.stdout.write("[%s] %s\n" % (time.strftime("%H:%M:%S"), fmt % args))
            sys.stdout.flush()

    def handle_one_request(self):
        """客户端在请求处理中途断开时，不要把整段 traceback 打进日志。

        socketserver 的默认实现会在 process_request_thread 里捕获异常后
        调用 handle_error() 打印完整堆栈。对「对端先挂」这类情况（用户取消、
        GUI 健康检查 1.5s 超时后关连接、curl 被 Ctrl+C）完全属于正常现象，
        却会在日志里制造看似严重的报错。这里把这类异常吞掉并记一行简报。
        """
        try:
            BaseHTTPRequestHandler.handle_one_request(self)
        except CLIENT_GONE_ERRORS as e:
            self.close_connection = True
            if CONFIG.get("log_requests"):
                sys.stdout.write("[%s] 客户端提前断开（%s），忽略\n"
                                 % (time.strftime("%H:%M:%S"), type(e).__name__))
                sys.stdout.flush()

    def handle_error(self, request, client_address):
        """兜底：连接类异常不打印 traceback（见 handle_one_request 注释）。"""
        import traceback
        exc = sys.exc_info()[1]
        if isinstance(exc, CLIENT_GONE_ERRORS):
            self.close_connection = True
            return
        # 其他异常保持原样，方便排查真实故障
        traceback.print_exc()

    # -- 工具方法 ---------------------------------------------------------

    def _check_api_key(self):
        """config 配了 api_key 时校验 Bearer（仅本地回环监听，轻量鉴权）。"""
        want = str(CONFIG.get("api_key") or "")
        if not want:
            return True
        got = self.headers.get("Authorization") or ""
        return got == "Bearer " + want

    def _read_body(self):
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        if length <= 0:
            return b""
        return self.rfile.read(length)

    def _send_json(self, status, obj):
        data = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except CLIENT_GONE_ERRORS:
            # 客户端已断开（例如 GUI 健康检查超时后主动关闭连接，
            # Windows 上表现为 ConnectionAbortedError/WinError 10053）。
            # 这不是代理故障，无需打印 traceback。
            pass

    def _send_error_anthropic(self, status, etype, message):
        self._send_json(status, {"type": "error",
                                 "error": {"type": etype, "message": message}})

    def _begin_stream(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

    def _write_chunk(self, text):
        data = text.encode("utf-8")
        if not data:
            return True
        try:
            self.wfile.write(("%X\r\n" % len(data)).encode("ascii"))
            self.wfile.write(data)
            self.wfile.write(b"\r\n")
            self.wfile.flush()
            return True
        except CLIENT_GONE_ERRORS:
            return False

    def _end_chunked(self):
        try:
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except CLIENT_GONE_ERRORS:
            pass

    # -- 路由（归一化 /v1 前缀） -------------------------------------------

    @staticmethod
    def _norm(path):
        p = path.split("?")[0].rstrip("/")
        if p.startswith("/v1/"):
            p = p[3:]
        elif p == "/v1":
            p = ""
        return p or "/"

    def do_GET(self):
        path = self._norm(self.path)
        if path in ("/health", "/healthz", "/"):
            # 只用缓存，不做网络探测：本接口被 GUI 以 1.5s 超时轮询，
            # 一旦在这里同步探上游（最坏 1.5s×候选数 + 4s），必然超时，
            # 客户端先断 → ConnectionAbortedError 刷屏。缓存由启动探测
            # 与后台刷新线程维护（见 start_probe_refresher）。
            snap = UPSTREAM.peek()
            base, models_cache = (snap if snap else (None, None))
            self._send_json(200, {
                "status": "ok",
                "upstream": base,
                "upstream_alive": bool(base),
                "default_model": CONFIG["default_model"],
                "models": _models_from_cache(models_cache),
                # Rust 侧靠它判断正在运行的进程是否还是旧脚本（升级后自动重启）
                "script_version": PROXY_SCRIPT_VERSION,
            })
            return
        if path in ("/models",):
            m = UPSTREAM.models()
            if m is None:
                self._send_json(503, {"error": {
                    "message": "LobsterAI 本地服务不可用（客户端未运行）"}})
                return
            # 透传上游清单 + config 映射合并
            self._send_json(200, {
                "object": "list",
                "data": [{"id": m["id"], "type": "model",
                          "display_name": m["display_name"]} for m in list_models()],
            })
            return
        self._send_error_anthropic(404, "not_found_error", "未知路径: %s" % self.path)

    def do_POST(self):
        path = self._norm(self.path)
        if not self._check_api_key():
            self._send_error_anthropic(401, "authentication_error",
                                       "代理 api_key 不匹配（检查 config.json / CC-Switch 配置）")
            return
        if path in ("/messages",):
            self._handle_messages()
            return
        if path in ("/messages/count_tokens",):
            self._handle_count_tokens()
            return
        if path in ("/chat/completions",):
            self._handle_chat_completions()
            return
        self._send_error_anthropic(404, "not_found_error", "未知路径: %s" % self.path)

    # -- POST /v1/chat/completions（OpenAI 纯透传） ------------------------

    def _handle_chat_completions(self):
        raw = self._read_body()
        try:
            body = json.loads(raw.decode("utf-8"))
        except Exception as e:
            self._send_json(400, {"error": {"message": "请求体不是合法 JSON: %s" % e,
                                             "type": "invalid_request_error"}})
            return
        # 模型名归一化（claude-* → 上游真实 ID）
        try:
            body["model"] = resolve_model(body.get("model"))
        except Exception:
            pass
        # 实测上游对 stream:false 也强制回 SSE；对客户端的非流式请求，
        # 强制走流式再聚合（保持请求语义）。客户端要流式则照常透传。
        client_stream = bool(body.get("stream"))
        body["stream"] = True
        body["stream_options"] = {"include_usage": True}
        model_id = body.get("model") or "unknown"
        est_in = _estimate_tokens(body.get("messages")) + \
            _estimate_tokens(body.get("tools"))
        if CONFIG.get("log_requests"):
            log("-> [openai] model=%s stream=%s msgs=%d" % (
                body.get("model"), client_stream, len(body.get("messages") or [])))
        try:
            resp = self._upstream_with_retry(body)
        except urllib.error.HTTPError as e:
            status, text = upstream_error_text(e)
            _safe_record_usage("openai", model_id, False, est_in, 0, "estimated")
            self._send_json(status or 502, {"error": {"message": "上游错误(%s): %s" % (status, str(text)[:500]),
                                                      "type": "api_error"}})
            return
        except Exception as e:
            _safe_record_usage("openai", model_id, False, est_in, 0, "estimated")
            self._send_json(502, {"error": {"message": "无法连接上游: %s" % e,
                                             "type": "api_error"}})
            return
        if client_stream:
            self._pipe_stream(resp, model_id, est_in)
        else:
            self._pipe_flat(resp, model_id, est_in)

    def _upstream_with_retry(self, payload):
        """调上游；连接类失败（端口轮换）强制重探测一次再试。"""
        try:
            return call_upstream(payload)
        except urllib.error.HTTPError:
            raise
        except Exception as e:
            log("上游调用失败(%s)，重探测后重试一次" % e)
            return call_upstream(payload, force_probe=True)

    def _pipe_stream(self, resp, model_id, est_in):
        """把上游 SSE 原样转发（chunked），旁路解析 usage 后记账。"""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        usage = {}
        output_parts = []
        success = True
        valid_response = False
        normal_end = False
        sse_buf = ""
        try:
            while True:
                block = resp.read1(8192) if hasattr(resp, "read1") else resp.read(8192)
                if not block:
                    break
                text = block.decode("utf-8", "replace")
                sse_buf += text
                lines = sse_buf.split("\n")
                sse_buf = lines.pop()
                for line in lines:
                    if line.strip() == "data: [DONE]":
                        normal_end = True
                    chunk = self._parse_sse_chunk(line)
                    if chunk:
                        if self._openai_business_error(chunk):
                            success = False
                        if chunk.get("usage"):
                            usage = chunk["usage"]
                        choices = chunk.get("choices") or []
                        if choices:
                            valid_response = True
                            if choices[0].get("finish_reason") is not None:
                                normal_end = True
                            delta = choices[0].get("delta") or {}
                            output_parts.append(delta.get("content") or "")
                            output_parts.append(delta.get("reasoning_content") or "")
                            output_parts.extend(json.dumps(tc, ensure_ascii=False)
                                                for tc in delta.get("tool_calls") or [])
                if not self._write_chunk(text):
                    success = False
                    break
        except Exception as e:
            success = False
            log("透传出错: %s" % e)
        finally:
            try:
                resp.close()
            except Exception:
                pass
            self._end_chunked()
            success = success and valid_response and normal_end
            est_out = _estimate_tokens("".join(output_parts))
            inp, out, source = _usage_values(usage, est_in, est_out)
            _safe_record_usage("openai", model_id, success, inp, out, source)

    @staticmethod
    def _openai_business_error(obj):
        if not isinstance(obj, dict):
            return True
        if obj.get("error"):
            return True
        return obj.get("code") not in (None, 0, "0") and not obj.get("choices")

    @staticmethod
    def _parse_sse_chunk(line):
        line = line.strip()
        if not line.startswith("data:"):
            return None
        data = line[5:].strip()
        if not data or data == "[DONE]":
            return None
        try:
            chunk = json.loads(data)
            return chunk if isinstance(chunk, dict) else None
        except Exception:
            return None

    def _pipe_flat(self, resp, model_id, est_in):
        """非流式转发。实测上游（lobsterai-model-compat）对 stream:false 也
        强制返回 SSE 流（content-type: text/event-stream），所以这里统一按
        SSE 读入并聚合成标准 OpenAI 非流式响应。"""
        try:
            data = resp.read()
        finally:
            try:
                resp.close()
            except Exception:
                pass
        text = data.decode("utf-8", "replace")
        ctype = ""
        try:
            ctype = (resp.headers.get("Content-Type") or "").lower()
        except Exception:
            pass
        if "text/event-stream" in ctype or text.lstrip().startswith("data:"):
            oa = self._aggregate_sse(text)
            if oa is not None:
                business_error = self._openai_business_error(oa)
                usage = oa.get("usage") or {}
                message = ((oa.get("choices") or [{}])[0].get("message") or {})
                est_out = _estimate_tokens(message)
                inp, out, source = _usage_values(usage, est_in, est_out)
                _safe_record_usage("openai", model_id, not business_error,
                                   inp, out, source)
                payload = json.dumps(oa, ensure_ascii=False).encode("utf-8")
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json; charset=utf-8")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except CLIENT_GONE_ERRORS:
                    pass
                return
        # 真非流式（未来上游修复后）：解析统计后原样转发
        try:
            oa = json.loads(text)
            usage = oa.get("usage") or {}
            message = ((oa.get("choices") or [{}])[0].get("message") or {})
            est_out = _estimate_tokens(message)
            inp, out, source = _usage_values(usage, est_in, est_out)
            _safe_record_usage("openai", model_id,
                               not self._openai_business_error(oa), inp, out, source)
        except Exception:
            _safe_record_usage("openai", model_id, False, est_in, 0, "estimated")
        try:
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except CLIENT_GONE_ERRORS:
            pass

    @staticmethod
    def _aggregate_sse(text):
        """把上游 SSE 分片聚合成一个 OpenAI chat.completion 对象。"""
        content_parts = []
        reasoning_parts = []
        tool_calls = {}   # index -> {"id":..., "name":..., "arguments": str}
        finish_reason = None
        usage = {}
        model_id = None
        for line in text.splitlines():
            line = line.strip()
            if not line or line.startswith(":") or not line.startswith("data:"):
                continue
            chunk_str = line[5:].strip()
            if not chunk_str or chunk_str == "[DONE]":
                continue
            try:
                chunk = json.loads(chunk_str)
            except Exception:
                continue
            model_id = model_id or chunk.get("model")
            if chunk.get("usage"):
                usage = chunk["usage"]
            choices = chunk.get("choices") or []
            if not choices:
                continue
            ch = choices[0]
            if ch.get("finish_reason"):
                finish_reason = ch["finish_reason"]
            delta = ch.get("delta") or {}
            if delta.get("content"):
                content_parts.append(delta["content"])
            if delta.get("reasoning_content"):
                reasoning_parts.append(delta["reasoning_content"])
            for tc in delta.get("tool_calls") or []:
                idx = tc.get("index", 0)
                slot = tool_calls.setdefault(idx, {"id": None, "name": None, "arguments": ""})
                if tc.get("id") and not slot["id"]:
                    slot["id"] = tc["id"]
                fn = tc.get("function") or {}
                if fn.get("name") and not slot["name"]:
                    slot["name"] = fn["name"]
                if fn.get("arguments"):
                    slot["arguments"] += fn["arguments"]
        if not content_parts and not tool_calls and not reasoning_parts:
            return None
        message = {"role": "assistant", "content": "".join(content_parts)}
        if reasoning_parts:
            message["reasoning_content"] = "".join(reasoning_parts)
        if tool_calls:
            message["tool_calls"] = [
                {"id": slot["id"] or ("call_%d" % i),
                 "type": "function",
                 "function": {"name": slot["name"] or "",
                              "arguments": slot["arguments"]}}
                for i, slot in sorted(tool_calls.items())
            ]
            if not finish_reason:
                finish_reason = "tool_calls"
        return {
            "id": "chatcmpl-" + uuid.uuid4().hex[:24],
            "object": "chat.completion",
            "created": int(time.time()),
            "model": model_id or "lobsterai",
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": finish_reason or "stop",
            }],
            "usage": usage or {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
        }

    # -- POST /v1/messages（Anthropic → OpenAI 转换） -----------------------

    def _handle_messages(self):
        raw = self._read_body()
        try:
            body = json.loads(raw.decode("utf-8"))
        except Exception as e:
            self._send_error_anthropic(400, "invalid_request_error",
                                       "请求体不是合法 JSON: %s" % e)
            return

        stream = bool(body.get("stream"))

        try:
            payload, model_id = anthropic_to_openai_request(body)
        except Exception as e:
            self._send_error_anthropic(400, "invalid_request_error",
                                       "请求转换失败: %s" % e)
            return

        est_in = _estimate_tokens(payload.get("messages")) + \
            _estimate_tokens(payload.get("tools"))

        if CONFIG.get("log_requests"):
            log("-> [anthropic] %s stream=%s msgs=%d tools=%d max_tokens=%s" % (
                model_id, stream, len(payload.get("messages") or []),
                len(payload.get("tools") or []), payload.get("max_tokens")))

        # 实测上游对 stream:false 也强制回 SSE：非流式请求也走流式再聚合
        payload["stream"] = True
        payload["stream_options"] = {"include_usage": True}

        try:
            resp = self._upstream_with_retry(payload)
        except urllib.error.HTTPError as e:
            status, text = upstream_error_text(e)
            _safe_record_usage("anthropic", model_id, False, est_in, 0, "estimated")
            self._send_error_anthropic(
                status or 502, "api_error",
                "上游返回错误(%s): %s" % (status, text[:500]))
            return
        except Exception as e:
            _safe_record_usage("anthropic", model_id, False, est_in, 0, "estimated")
            self._send_error_anthropic(502, "api_error", "无法连接上游: %s" % e)
            return

        if stream:
            self._stream_response(resp, model_id, est_in)
        else:
            self._nonstream_response(resp, model_id, est_in)

    def _nonstream_response(self, resp, model_id, est_in):
        """Anthropic 非流式。实测上游对 stream:false 也回 SSE → 按 SSE 聚合
        成完整 OpenAI 响应再转 Anthropic。"""
        try:
            data = resp.read()
        finally:
            try:
                resp.close()
            except Exception:
                pass
        text = data.decode("utf-8", "replace")
        if text.lstrip().startswith("data:"):
            # 上游强制 SSE：聚合成完整对象
            oa = self._aggregate_sse(text)
            if oa is None:
                _safe_record_usage("anthropic", model_id, False, est_in, 0,
                                   "estimated")
                self._send_error_anthropic(502, "api_error",
                                           "上游返回空流: %s" % text[:200])
                return
        else:
            try:
                oa = json.loads(text)
            except Exception:
                _safe_record_usage("anthropic", model_id, False, est_in, 0,
                                   "estimated")
                self._send_error_anthropic(502, "api_error",
                                           "上游返回非 JSON: %s" % text[:300])
                return
        if self._openai_business_error(oa):
            _safe_record_usage("anthropic", model_id, False, est_in, 0,
                               "estimated")
            self._send_error_anthropic(502, "api_error",
                                       "上游错误: %s" % str(oa)[:400])
            return
        converted = openai_to_anthropic_response(oa, model_id)
        est_out = _estimate_tokens(converted.get("content"))
        inp, out, source = _usage_values(oa.get("usage"), est_in, est_out)
        _safe_record_usage("anthropic", model_id, True, inp, out, source)
        self._send_json(200, converted)

    def _stream_response(self, resp, model_id, est_in):
        emitter = AnthropicStreamEmitter(
            model_id, input_tokens_estimate=est_in,
            emit_thinking=bool(CONFIG.get("emit_thinking")))
        self._begin_stream()
        success = True
        valid_response = False
        normal_end = False
        try:
            for line in iter_sse_lines(resp):
                if not line or line.startswith(":"):
                    continue
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if not data:
                    continue
                if data == "[DONE]":
                    normal_end = True
                    break
                try:
                    chunk = json.loads(data)
                except Exception:
                    success = False
                    continue
                if self._openai_business_error(chunk):
                    success = False
                choices = chunk.get("choices") or []
                if choices:
                    valid_response = True
                    if choices[0].get("finish_reason") is not None:
                        normal_end = True
                events = emitter.feed(chunk)
                if events and not self._write_chunk(events):
                    success = False
                    return
            tail = emitter.finish_events()
            if tail and not self._write_chunk(tail):
                success = False
        except Exception as e:
            success = False
            log("流式处理出错: %s" % e)
            if not emitter.finished:
                self._write_chunk(emitter.finish_events())
        finally:
            try:
                resp.close()
            except Exception:
                pass
            self._end_chunked()
            success = success and valid_response and normal_end
            source = "accurate" if emitter.usage_received else "estimated"
            _safe_record_usage("anthropic", model_id, success,
                               emitter.input_tokens or est_in,
                               emitter.output_tokens, source)

    # -- POST /v1/messages/count_tokens ------------------------------------

    def _handle_count_tokens(self):
        raw = self._read_body()
        try:
            body = json.loads(raw.decode("utf-8"))
        except Exception:
            body = {}
        total = _estimate_tokens(body.get("system")) + \
            _estimate_tokens(body.get("messages")) + \
            _estimate_tokens(body.get("tools"))
        self._send_json(200, {"input_tokens": max(1, total)})


# --------------------------------------------------------------------------
# 自检
# --------------------------------------------------------------------------

def usage_storage_selftest():
    """不访问真实上游，验证最终 schema、v1 迁移、聚合及并发写入。"""
    fd, path = tempfile.mkstemp(prefix="lobster-usage-test-", suffix=".sqlite")
    os.close(fd)
    try:
        conn = sqlite3.connect(path)
        try:
            conn.execute("""
                CREATE TABLE daily_usage (
                    day TEXT NOT NULL, protocol TEXT NOT NULL, model TEXT NOT NULL,
                    source TEXT NOT NULL, requests INTEGER NOT NULL,
                    successes INTEGER NOT NULL, failures INTEGER NOT NULL,
                    input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                    PRIMARY KEY (day, protocol, model, source)
                )
            """)
            conn.executemany("INSERT INTO daily_usage VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", [
                ("2000-01-01", "openai", "test-model", "upstream", 2, 2, 0, 6, 4),
                ("2000-01-01", "openai", "test-model", "estimated", 1, 0, 1, 1, 0),
            ])
            conn.execute("PRAGMA user_version=1")
            conn.commit()
        finally:
            conn.close()

        store = UsageStore(path)
        store.initialize()
        store.initialize()
        threads = []
        errors = []

        def record_test_row():
            try:
                store.record("openai", "test-model", True, 3, 2, "accurate",
                             day="2000-01-01")
            except Exception as e:
                errors.append(e)

        for _i in range(8):
            thread = threading.Thread(target=record_test_row)
            threads.append(thread)
            thread.start()
        for thread in threads:
            thread.join()
        if errors:
            raise errors[0]
        store.record("openai", "test-model", False, 1, 0, "missing",
                     day="2000-01-01")
        conn = sqlite3.connect(path)
        try:
            version = conn.execute("PRAGMA user_version").fetchone()[0]
            mode = conn.execute("PRAGMA journal_mode").fetchone()[0]
            columns = tuple(row[1] for row in conn.execute(
                "PRAGMA table_info(usage_daily)").fetchall())
            legacy = conn.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='daily_usage'"
            ).fetchone()
            row = conn.execute("""
                SELECT input, output, request, success, error,
                       accurate, estimated, missing
                FROM usage_daily WHERE day=? AND protocol=? AND model=?
            """, ("2000-01-01", "openai", "test-model")).fetchone()
        finally:
            conn.close()
        expected = (32, 20, 12, 10, 2, 10, 1, 1)
        if (version != USAGE_SCHEMA_VERSION or mode.lower() != "wal"
                or columns != UsageStore.COLUMNS or legacy is not None
                or row != expected):
            raise AssertionError(
                "统计存储结果不符: version=%s mode=%s columns=%r legacy=%r row=%r" %
                (version, mode, columns, legacy, row))
        log("统计存储自检通过（最终 schema/v1 迁移/WAL/聚合/失败/并发）")
        return 0
    except Exception as e:
        log("统计存储自检失败: %s" % e)
        return 1
    finally:
        for suffix in ("", "-wal", "-shm"):
            try:
                os.remove(path + suffix)
            except OSError:
                pass


def model_catalog_selftest():
    """离线自检：模型目录提取/合并 + 真实模型过滤。

    回归「CC-Switch 获取模型不完整 / 模型名不可用」：
    - 旧实现只读 models.json 的 providers[*].models（可能缺失或过期），
      匹配不到 baseUrl 时返回空清单 → 只剩 config 里 5 个映射名；
    - 更早的 list_models 会把 config 的 claude-* 映射别名当模型返回，
      那些是协议转换用的虚拟名，不是可提供的模型。
    不访问网络：_fetch_models 的端点用 127.0.0.1:1 触发连接拒绝，且
    服务端目录缓存显式置空以禁用后台拉取。
    """
    log("=== 模型目录自检 ===")
    tmp = tempfile.mkdtemp(prefix="lobster-proxy-catalog-")
    orig_catalog = UPSTREAM._server_catalog
    orig_at = UPSTREAM._server_at
    orig_models_cfg = CONFIG.get("models")
    # 源码目录没有 config.json，模型映射为空；自检自造一份，保证可重复
    CONFIG["models"] = {
        "deepseek-flash": {"id": "deepseek-flash", "context_window": 1000000},
        "glm-5.3": {"id": "glm-5.3", "context_window": 1000000},
        "claude-opus-4-8": {"id": "glm-5.3", "context_window": 1000000},
        "claude-haiku-4-5": {"id": "deepseek-flash", "context_window": 1000000},
    }
    try:
        state = os.path.join(tmp, "openclaw", "state")
        agent = os.path.join(state, "agents", "main", "agent")
        os.makedirs(agent, exist_ok=True)
        # models.json：只有 2 个模型（模拟过期/不完整）
        with open(os.path.join(agent, "models.json"), "w", encoding="utf-8") as f:
            json.dump({"providers": {"lobsterai-server": {
                "baseUrl": "http://127.0.0.1:1111/v1",
                "apiKey": "old-key",
                "models": [{"id": "deepseek-flash", "name": "DeepSeek-Flash"},
                           {"id": "glm-5.3", "name": "GLM-5.3"}],
            }}}, f)
        # openclaw.json：完整 3 个模型，baseUrl 与新端口一致（无 apiKey 也要收集目录）
        with open(os.path.join(state, "openclaw.json"), "w", encoding="utf-8") as f:
            json.dump({"models": {"providers": {"lobsterai-server": {
                "baseUrl": "http://127.0.0.1:2222/v1",
                "models": [{"id": "deepseek-flash", "name": "DeepSeek-V4.1-Flash"},
                           {"id": "kimi-k3.1-pro", "name": "Kimi-K3.1-Pro"},
                           {"id": "MiniMax-M3", "name": "MiniMax-M3"}],
            }}}}, f)

        entries = _model_entries_from_provider({
            "models": [{"id": "a", "name": "A"}, {"id": ""}, "junk",
                       {"id": "b"}]})
        if entries != [{"id": "a", "display_name": "A"},
                       {"id": "b", "display_name": "b"}]:
            raise AssertionError("provider 模型解析不符：%r" % (entries,))

        catalogs = _collect_model_catalogs(tmp)
        if len(catalogs) != 2:
            raise AssertionError("应收集两处配置的目录，实际 %r" % (catalogs,))

        # 断言 config 里确实配了别名（否则下面的过滤用例没有意义）
        aliases = [n for n, e in (CONFIG.get("models") or {}).items()
                   if isinstance(e, dict) and e.get("id") and n != e.get("id")]
        if not aliases:
            raise AssertionError("预期 config 里有 claude-* 别名映射，实际没有")

        mgr = UpstreamManager(dict(CONFIG, lobster_data_dir=tmp))
        merged = mgr._catalog_entries("http://127.0.0.1:2222/v1")
        ids = [e["id"] for e in merged]
        if ids != ["deepseek-flash", "kimi-k3.1-pro", "MiniMax-M3", "glm-5.3"]:
            raise AssertionError("目录合并顺序/去重不符：%r" % (ids,))
        if merged[0]["display_name"] != "DeepSeek-V4.1-Flash":
            raise AssertionError("匹配 baseUrl 的目录应优先：%r" % (merged[0],))

        # 上游 /models 不可达时必须回退到配置目录，而不是空清单
        mgr._server_catalog = []   # 禁用服务端目录（离线）
        got = mgr._fetch_models("http://127.0.0.1:1/v1", "k")
        got_ids = [x.get("id") for x in (got or {}).get("data") or []]
        # 该 baseUrl 与两处配置都不匹配：顺序不保证，但必须一条不少
        if sorted(got_ids) != sorted(ids):
            raise AssertionError("_fetch_models 回退不符：%r" % (got_ids,))

        # 服务端目录优先于本地配置，且 config 别名不得出现在模型清单里
        mgr._server_catalog = [
            {"id": "server-a", "display_name": "Server A"},
            {"id": "deepseek-flash", "display_name": "DeepSeek-V4.1-Flash"},
        ]
        with mgr._server_lock:
            mgr._server_at = time.time()
        merged2 = [e["id"] for e in mgr._catalog_entries("http://127.0.0.1:2222/v1")]
        if merged2[0] != "server-a" or "deepseek-flash" not in merged2:
            raise AssertionError("服务端目录应优先：%r" % (merged2,))

        # _config_real_models 只保留 name == id 的真实 ID
        real = _config_real_models()
        if any(a in real for a in aliases):
            raise AssertionError("config 别名不应进真实模型清单：%r" % (real,))
        log("模型目录自检通过（两处合并/展示名/服务端优先/别名过滤）")
        return 0
    except Exception as e:
        log("模型目录自检失败: %s" % e)
        return 1
    finally:
        CONFIG["models"] = orig_models_cfg if orig_models_cfg is not None else {}
        UPSTREAM._server_catalog = orig_catalog
        UPSTREAM._server_at = orig_at
        import shutil
        shutil.rmtree(tmp, ignore_errors=True)


def resilience_selftest():
    """离线自检：客户端断连容错 / 健康检查不阻塞 / 缓存快照。

    不访问网络，可在任意机器跑（--selftest-storage 会一并调用）。
    """
    log("=== 连接容错自检 ===")
    try:
        # 1) 三个「对端已断开」异常都必须被 CLIENT_GONE_ERRORS 覆盖
        for exc in (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            if not issubclass(exc, CLIENT_GONE_ERRORS):
                raise AssertionError("CLIENT_GONE_ERRORS 未覆盖 %s" % exc.__name__)
        # 10053 (WSAECONNABORTED) 必须映射到被捕获的类型
        e = ConnectionAbortedError(10053, "客户端已断开")
        if not isinstance(e, CLIENT_GONE_ERRORS):
            raise AssertionError("WinError 10053 未被捕获")
        if isinstance(e, (BrokenPipeError, ConnectionResetError)):
            raise AssertionError("预期前提不成立：Aborted 不应该是 Pipe/Reset 的子类")

        # 2) /health 用的 peek() 在无缓存时必须立即返回 None，且绝不发起网络请求
        saved = (UPSTREAM._cached, UPSTREAM._models_cache)
        calls = {"n": 0}
        orig_port_open = globals()["_port_open"]

        def trap(*a, **kw):
            calls["n"] += 1
            raise AssertionError("peek() 触发了网络探测（%s）" % (a,))

        try:
            UPSTREAM._cached = None
            UPSTREAM._models_cache = None
            globals()["_port_open"] = trap
            t0 = time.time()
            snap = UPSTREAM.peek()
            cost = time.time() - t0
            if snap is not None:
                raise AssertionError("空缓存时 peek() 应返回 None，实际 %r" % (snap,))
            if calls["n"]:
                raise AssertionError("peek() 不应触发网络探测")
            if cost > 0.05:
                raise AssertionError("peek() 太慢：%.3fs，健康检查会超时" % cost)
        finally:
            globals()["_port_open"] = orig_port_open
            UPSTREAM._cached, UPSTREAM._models_cache = saved

        # 3) 有缓存时 peek() 原样返回，且 _models_from_cache 含上游模型
        UPSTREAM._cached = ("http://127.0.0.1:1/v1", "k", "m", time.time())
        UPSTREAM._models_cache = {"data": [{"id": "upstream-only-model"}]}
        try:
            snap = UPSTREAM.peek()
            if not snap or snap[0] != "http://127.0.0.1:1/v1":
                raise AssertionError("peek() 未返回缓存的上游地址：%r" % (snap,))
            ids = _models_from_cache(snap[1])
            if "upstream-only-model" not in ids:
                raise AssertionError("上游模型丢失：%r" % (ids,))
            # config 里 name == id 的真实模型必须一并出现；claude-* 映射别名
            # 绝不能出现在模型清单里（它们是协议转换用的虚拟名，不是可提供
            # 的模型——这正是「模型名疑似不是可提供的模型」的根因）。
            for mid in _config_real_models():
                if mid not in ids:
                    raise AssertionError("config 真实模型 %r 未合并：%r" % (mid, ids))
            for name, entry in (CONFIG.get("models") or {}).items():
                mid = entry.get("id") if isinstance(entry, dict) else entry
                if name != mid and name in ids:
                    raise AssertionError("config 别名 %r 不应出现在模型清单：%r"
                                         % (name, ids))
        finally:
            UPSTREAM._cached, UPSTREAM._models_cache = saved

        log("连接容错自检通过（10053 捕获/健康检查零阻塞/缓存合并）")
        return 0
    except Exception as e:
        log("连接容错自检失败: %s" % e)
        return 1


def selftest():
    log("=== 自检 ===")
    if usage_storage_selftest():
        return 1
    if resilience_selftest():
        return 1
    found = UPSTREAM.probe(force=True)
    if not found:
        log("上游不可用：LobsterAI 未运行或本地端口探测失败")
        return 1
    base, key, model = found
    log("上游: %s (key 长度 %d)" % (base, len(key or "")))
    ids = UPSTREAM.upstream_model_ids()
    log("上游模型 %d 个: %s" % (len(ids), ", ".join(ids[:8])))

    payload, mid = anthropic_to_openai_request({
        "model": CONFIG["default_model"],
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "say hi in 3 words"}],
    })
    try:
        resp = call_upstream(payload)
        oa = json.loads(resp.read().decode("utf-8"))
        resp.close()
        ans = ((oa.get("choices") or [{}])[0].get("message") or {}).get("content")
        log("真实调用成功 (model=%s): %s" % (mid, ans))
    except Exception as e:
        st, tx = upstream_error_text(e)
        log("真实调用失败 (%s): %s" % (st, tx[:300]))
        return 1
    log("=== 自检通过 ===")
    return 0


# --------------------------------------------------------------------------
# 入口
# --------------------------------------------------------------------------

def start_probe_refresher():
    """后台定期刷新上游探测缓存，让 /health 永远只读缓存、毫秒级返回。

    间隔取 probe_recheck_seconds（默认 60s）。探测失败不退出线程：
    用户可能稍后才启动 LobsterAI，下次循环会再试。
    """
    interval = max(5, int(CONFIG.get("probe_recheck_seconds", 60)))

    def loop():
        while True:
            try:
                found = UPSTREAM.probe(force=True)
                if found:
                    log("上游探测刷新: %s" % found[0])
                    # 服务端模型目录走 HTTPS，必须在 _lock 之外补
                    UPSTREAM.server_catalog(timeout=8)
            except Exception as e:
                log("上游探测刷新失败: %s" % e)
            time.sleep(interval)

    t = threading.Thread(target=loop, name="upstream-probe-refresher")
    t.daemon = True
    t.start()
    return t


def main():
    if "--selftest-storage" in sys.argv:
        try:
            success = (usage_storage_selftest() == 0
                       and resilience_selftest() == 0
                       and model_catalog_selftest() == 0)
        except BaseException as e:
            log("存储/容错自检失败（未处理异常）: %s" % e)
            success = False
        return 0 if success else 1
    if "--selftest" in sys.argv:
        try:
            success = selftest() == 0
        except BaseException as e:
            log("自检失败（未处理异常）: %s" % e)
            success = False
        return 0 if success else 1

    host = CONFIG["listen_host"]
    port = int(CONFIG["listen_port"])

    # 启动前先探测一次上游（失败不阻止启动，方便排查）
    found = UPSTREAM.probe()
    if found:
        log("上游就绪: %s" % found[0])
        # 服务端目录要在首次 /v1/models 之前就绪；它在锁外，最多 8s
        UPSTREAM.server_catalog(timeout=8)
    else:
        log("警告: 上游暂不可用（LobsterAI 未运行？），代理仍将启动并按需重探测")

    try:
        httpd = ThreadingHTTPServer((host, port), Handler)
    except OSError as e:
        log("无法监听 %s:%s -> %s" % (host, port, e))
        log("端口可能被占用，请修改 config.json 里的 listen_port。")
        sys.exit(1)

    httpd.daemon_threads = True
    start_probe_refresher()
    log("LobsterPlus 稳定代理已启动: http://%s:%d" % (host, port))
    log("  OpenAI   : http://%s:%d/v1/chat/completions" % (host, port))
    log("  Anthropic: http://%s:%d/v1/messages" % (host, port))
    log("  健康检查 : http://%s:%d/health" % (host, port))
    log("CC-Switch 里把 Base URL 指到上面的地址即可（端口永不漂移）。按 Ctrl+C 停止。")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        log("正在停止...")
    finally:
        httpd.server_close()


if __name__ == "__main__":
    sys.exit(main())
