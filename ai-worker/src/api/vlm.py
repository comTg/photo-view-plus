"""`POST /vlm/analyze`（docs/10 §4、A6）：一张图一个请求。

请求体是原图字节（Rust 在读闸门许可下读好，读完就释放许可，不占着 NAS 读名额等推理）；
参数走查询串。返回规范化后的分析结果，或带错误类型的失败：
- decode：解码失败（格式不支持、文件损坏）——算失败；
- model：模型输出不合格（4xx、JSON 重试后仍不完整）——算失败；
- server：llama-server 不可用（连不上、超时、5xx）——临时错误，不算这张图的失败。
"""

from __future__ import annotations

import time
from typing import Any

from fastapi import APIRouter, Query, Request
from fastapi.concurrency import run_in_threadpool

from ..vlm import client, images
from ..vlm.normalize import normalize
from ..vlm.prompts import PROMPT_VERSION, SCHEMA, SYSTEM_PROMPT, user_text

router = APIRouter(prefix="/vlm", tags=["vlm"])

TEMPERATURE = 0.2
MAX_TOKENS = 600
REQUEST_TIMEOUT = 240.0


def analyze_bytes(
    data: bytes,
    *,
    server_url: str,
    long_edge: int,
    taken_at: int | None,
    max_tokens: int = MAX_TOKENS,
    timeout: float = REQUEST_TIMEOUT,
) -> dict[str, Any]:
    started = time.perf_counter()
    try:
        jpeg = images.prepare(data, long_edge)
    except Exception as exc:  # noqa: BLE001 - 任何解码异常都按"这张图读不了"处理
        return failure("decode", f"{type(exc).__name__}: {exc}", started)
    prepared = time.perf_counter()
    try:
        raw, timings, requests = client.chat_json(
            server_url,
            SYSTEM_PROMPT,
            user_text(taken_at),
            jpeg,
            schema=SCHEMA,
            temperature=TEMPERATURE,
            max_tokens=max_tokens,
            timeout=timeout,
        )
    except client.VlmServerError as exc:
        return failure("server", str(exc), started)
    except client.VlmOutputError as exc:
        return failure("model", str(exc), started)
    finished = time.perf_counter()
    analysis = normalize(raw)
    return {
        "ok": True,
        "result": analysis.to_dict(),
        "prompt_version": PROMPT_VERSION,
        "prepare_ms": round((prepared - started) * 1000),
        "infer_ms": round((finished - prepared) * 1000),
        "duration_ms": round((finished - started) * 1000),
        "requests": requests,
        "image_bytes": len(jpeg),
        "prompt_tokens": timings.get("prompt_n"),
        "predicted_tokens": timings.get("predicted_n"),
    }


def failure(kind: str, message: str, started: float) -> dict[str, Any]:
    return {
        "ok": False,
        "error_kind": kind,
        "error": message[:500],
        "prompt_version": PROMPT_VERSION,
        "duration_ms": round((time.perf_counter() - started) * 1000),
    }


@router.post("/analyze")
async def analyze(
    request: Request,
    server_url: str = Query(..., description="llama-server 地址，如 http://127.0.0.1:8081"),
    long_edge: int = Query(1024, ge=256, le=2048),
    taken_at: int | None = Query(None, description="拍摄时间（本地时间的 Unix 秒）"),
    max_tokens: int = Query(MAX_TOKENS, ge=64, le=2048),
) -> dict[str, Any]:
    data = await request.body()
    if not data:
        return failure("decode", "请求体为空", time.perf_counter())
    # 解码和推理都是阻塞操作，放线程池；llama-server 有 -np 个槽位，并发请求在那边排队。
    return await run_in_threadpool(
        analyze_bytes,
        data,
        server_url=server_url,
        long_edge=long_edge,
        taken_at=taken_at,
        max_tokens=max_tokens,
    )


@router.get("/prompt")
def prompt() -> dict[str, Any]:
    """当前提示词版本与 Schema（诊断用）。"""
    return {"prompt_version": PROMPT_VERSION, "system": SYSTEM_PROMPT, "schema": SCHEMA}
