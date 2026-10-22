"""调用 llama-server 的 OpenAI 兼容接口（同 memora `VLM.chat_json`），用标准库 urllib，不新增依赖。

错误分两类，Rust 侧据此决定要不要累加失败次数：
- VlmServerError：连不上、超时、5xx（加载中返回 503）——临时错误，不算这张图的失败；
- VlmOutputError：4xx（例如超出上下文）、重试后 JSON 仍不完整——算一次失败。
"""

from __future__ import annotations

import base64
import json
import socket
import urllib.error
import urllib.request
from typing import Any


class VlmServerError(RuntimeError):
    pass


class VlmOutputError(RuntimeError):
    pass


def chat(
    server_url: str,
    system: str,
    user_text: str,
    jpeg: bytes,
    *,
    schema: dict[str, Any] | None,
    temperature: float,
    max_tokens: int,
    timeout: float,
    model: str = "qwen3.5-9b",
) -> tuple[str, dict[str, Any]]:
    body: dict[str, Any] = {
        "model": model,
        "temperature": temperature,
        "top_p": 0.9,
        "max_tokens": max_tokens,
        "messages": [
            {"role": "system", "content": system},
            {
                "role": "user",
                "content": [
                    {
                        "type": "image_url",
                        "image_url": {"url": "data:image/jpeg;base64," + base64.b64encode(jpeg).decode()},
                    },
                    {"type": "text", "text": user_text},
                ],
            },
        ],
    }
    if schema:
        body["response_format"] = {"type": "json_schema", "json_schema": {"name": "result", "schema": schema}}
    request = urllib.request.Request(
        server_url.rstrip("/") + "/v1/chat/completions",
        data=json.dumps(body, ensure_ascii=False).encode("utf-8"),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            data = json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")[:300]
        if exc.code >= 500:
            raise VlmServerError(f"llama-server {exc.code}: {detail}") from exc
        raise VlmOutputError(f"llama-server {exc.code}: {detail}") from exc
    except (urllib.error.URLError, socket.timeout, TimeoutError, ConnectionError) as exc:
        raise VlmServerError(f"llama-server 不可用：{exc}") from exc
    except (ValueError, UnicodeDecodeError) as exc:
        raise VlmServerError(f"llama-server 返回无法解析：{exc}") from exc
    try:
        content = data["choices"][0]["message"]["content"] or ""
    except (KeyError, IndexError, TypeError) as exc:
        raise VlmOutputError(f"llama-server 返回缺少内容：{str(data)[:200]}") from exc
    return content, data.get("timings") or {}


def chat_json(
    server_url: str,
    system: str,
    user_text: str,
    jpeg: bytes,
    *,
    schema: dict[str, Any],
    temperature: float,
    max_tokens: int,
    timeout: float,
    tries: int = 3,
) -> tuple[dict[str, Any], dict[str, Any], int]:
    """按 Schema 约束输出；偶发截断导致 JSON 不完整时重试（最多再试 tries - 1 次）。
    返回（解析后的对象, 最后一次的 timings, 实际请求次数）。"""
    content = ""
    for attempt in range(1, tries + 1):
        content, timings = chat(
            server_url, system, user_text, jpeg,
            schema=schema, temperature=temperature, max_tokens=max_tokens, timeout=timeout,
        )
        try:
            parsed = json.loads(content)
        except json.JSONDecodeError:
            continue
        if isinstance(parsed, dict):
            return parsed, timings, attempt
    raise VlmOutputError(f"JSON 解析失败：{content[:200]}")


def health(server_url: str, timeout: float = 3) -> bool:
    try:
        with urllib.request.urlopen(server_url.rstrip("/") + "/health", timeout=timeout) as response:
            return response.status == 200
    except (urllib.error.URLError, socket.timeout, TimeoutError, ConnectionError, ValueError):
        return False
