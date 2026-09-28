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
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
CONFIG_PATH = os.path.join(HERE, "config.json")

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
        self._cached = None        # (base_url, api_key, default_model, probed_at)
        self._models_cache = None  # /v1/models 响应（探测时顺带拉取）
        self._models_at = 0.0

    def probe(self, force=False):
        with self._lock:
            now = time.time()
            if (not force and self._cached
                    and now - self._cached[3] < int(self.cfg.get("probe_recheck_seconds", 60))):
                return self._cached[:3]
            cands = _collect_candidates(self.data_dir)
            if not cands:
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
                self._cached = None
                return None
            # 优先跨候选挑选稳定的 key/model（端口轮换时 key 也变，取探测通过那组的）
            self._cached = (base, key, model, now)
            self._models_cache = self._fetch_models_locked(base, key)
            self._models_at = now
            return base, key, model

    def _fetch_models_locked(self, base, key):
        """上游模型清单。实测该服务没有 /v1/models（404），
        从配置候选里的 models 列表拼出 OpenAI 格式清单。"""
        for cb, ck, cm in _collect_candidates(self.data_dir):
            if cb == base:
                try:
                    req = urllib.request.Request(
                        cb.rstrip("/") + "/models",
                        headers={"Authorization": "Bearer " + ck})
                    with urllib.request.urlopen(req, timeout=4) as resp:
                        return json.loads(resp.read().decode("utf-8", "replace"))
                except Exception:
                    pass
                # 端点 404 → 用配置里的模型列表
                ids = self._model_ids_from_candidates(base)
                if ids is not None:
                    return {"object": "list",
                            "data": [{"id": i, "type": "model"} for i in ids]}
        return None

    def _model_ids_from_candidates(self, base_url):
        for cb, _ck, _cm in _collect_candidates(self.data_dir):
            if cb != base_url:
                continue
            mf = os.path.join(self.data_dir, "openclaw", "state", "agents",
                              "main", "agent", "models.json")
            cfg = _load_json(mf)
            if cfg and cfg.get("providers"):
                for pid, p in cfg["providers"].items():
                    if p.get("baseUrl") == cb and p.get("models"):
                        return [m.get("id") for m in p["models"] if m.get("id")]
            return []  # 匹配到候选但无模型列表
        return None

    def models(self):
        """上游模型列表（缓存 60s）。LobsterAI 不在运行时返回 None。"""
        found = self.probe()
        if not found:
            return None
        now = time.time()
        if self._models_cache is None or now - self._models_at > 60:
            with self._lock:
                self._models_cache = self._fetch_models_locked(found[0], found[1])
                self._models_at = now
        return self._models_cache

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


def list_models():
    """合并上游真实模型清单与 config 映射（去重，真实 ID 优先排前）。"""
    out = []
    seen = set()
    m = UPSTREAM.models()
    if m:
        for x in m.get("data") or []:
            mid = x.get("id")
            if mid and mid not in seen:
                seen.add(mid)
                out.append({"id": mid, "display_name": x.get("display_name") or mid})
    for name, entry in (CONFIG.get("models") or {}).items():
        mid = entry.get("id") if isinstance(entry, dict) else entry
        if name not in seen and mid not in seen and name and mid:
            seen.add(name)
            # 真实模型 ID（name == id）排在 claude-* 映射名前面
            out.append({"id": name, "display_name": name,
                        "_real": name == mid})
    # 稳定排序：真实 ID 在前
    out.sort(key=lambda x: not x.get("_real"))
    for x in out:
        x.pop("_real", None)
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
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        try:
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
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
        except (BrokenPipeError, ConnectionResetError, OSError):
            return False

    def _end_chunked(self):
        try:
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
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
            found = UPSTREAM.probe()
            self._send_json(200, {
                "status": "ok",
                "upstream": found[0] if found else None,
                "upstream_alive": bool(found),
                "default_model": CONFIG["default_model"],
                "models": [m["id"] for m in list_models()],
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
        if CONFIG.get("log_requests"):
            log("-> [openai] model=%s stream=%s msgs=%d" % (
                body.get("model"), client_stream, len(body.get("messages") or [])))
        try:
            resp = self._upstream_with_retry(body)
        except urllib.error.HTTPError as e:
            status, text = upstream_error_text(e)
            self._send_json(status or 502, {"error": {"message": "上游错误(%s): %s" % (status, str(text)[:500]),
                                                      "type": "api_error"}})
            return
        except Exception as e:
            self._send_json(502, {"error": {"message": "无法连接上游: %s" % e,
                                             "type": "api_error"}})
            return
        if client_stream:
            self._pipe_stream(resp)
        else:
            self._pipe_flat(resp)

    def _upstream_with_retry(self, payload):
        """调上游；连接类失败（端口轮换）强制重探测一次再试。"""
        try:
            return call_upstream(payload)
        except urllib.error.HTTPError:
            raise
        except Exception as e:
            log("上游调用失败(%s)，重探测后重试一次" % e)
            return call_upstream(payload, force_probe=True)

    def _pipe_stream(self, resp):
        """把上游 SSE 原样转发（chunked）。"""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            while True:
                block = resp.read1(8192) if hasattr(resp, "read1") else resp.read(8192)
                if not block:
                    break
                if not self._write_chunk(block.decode("utf-8", "replace")):
                    return
        except Exception as e:
            log("透传出错: %s" % e)
        finally:
            try:
                resp.close()
            except Exception:
                pass
            self._end_chunked()

    def _pipe_flat(self, resp):
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
                payload = json.dumps(oa, ensure_ascii=False).encode("utf-8")
                try:
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json; charset=utf-8")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                return
        # 真非流式（未来上游修复后）：原样转发
        try:
            self.send_response(200)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
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

        try:
            resp = self._upstream_with_retry(payload)
        except urllib.error.HTTPError as e:
            status, text = upstream_error_text(e)
            self._send_error_anthropic(
                status or 502, "api_error",
                "上游返回错误(%s): %s" % (status, text[:500]))
            return
        except Exception as e:
            self._send_error_anthropic(502, "api_error", "无法连接上游: %s" % e)
            return

        if stream:
            self._stream_response(resp, model_id, est_in)
        else:
            self._nonstream_response(resp, model_id)

    def _nonstream_response(self, resp, model_id):
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
                self._send_error_anthropic(502, "api_error",
                                           "上游返回空流: %s" % text[:200])
                return
        else:
            try:
                oa = json.loads(text)
            except Exception:
                self._send_error_anthropic(502, "api_error",
                                           "上游返回非 JSON: %s" % text[:300])
                return
        if isinstance(oa, dict) and oa.get("code") not in (0, None) and "choices" not in oa:
            self._send_error_anthropic(502, "api_error",
                                       "上游错误: %s" % str(oa)[:400])
            return
        self._send_json(200, openai_to_anthropic_response(oa, model_id))

    def _stream_response(self, resp, model_id, est_in):
        emitter = AnthropicStreamEmitter(
            model_id, input_tokens_estimate=est_in,
            emit_thinking=bool(CONFIG.get("emit_thinking")))
        self._begin_stream()
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
                    break
                try:
                    chunk = json.loads(data)
                except Exception:
                    continue
                events = emitter.feed(chunk)
                if events and not self._write_chunk(events):
                    return
            tail = emitter.finish_events()
            if tail:
                self._write_chunk(tail)
        except Exception as e:
            log("流式处理出错: %s" % e)
            if not emitter.finished:
                self._write_chunk(emitter.finish_events())
        finally:
            try:
                resp.close()
            except Exception:
                pass
            self._end_chunked()

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

def selftest():
    log("=== 自检 ===")
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

def main():
    if "--selftest" in sys.argv:
        sys.exit(selftest())

    host = CONFIG["listen_host"]
    port = int(CONFIG["listen_port"])

    # 启动前先探测一次上游（失败不阻止启动，方便排查）
    found = UPSTREAM.probe()
    if found:
        log("上游就绪: %s" % found[0])
    else:
        log("警告: 上游暂不可用（LobsterAI 未运行？），代理仍将启动并按需重探测")

    try:
        httpd = ThreadingHTTPServer((host, port), Handler)
    except OSError as e:
        log("无法监听 %s:%s -> %s" % (host, port, e))
        log("端口可能被占用，请修改 config.json 里的 listen_port。")
        sys.exit(1)

    httpd.daemon_threads = True
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
    main()
