from __future__ import annotations

import asyncio
import os
from typing import Any

from fastapi import APIRouter, Request
from pydantic import BaseModel, Field

from ..runtime import diagnostics, runtime_summary, set_math_threads

router = APIRouter()


@router.get("/health")
def health() -> dict[str, Any]:
    summary = runtime_summary()
    return {
        "status": "ok",
        "pid": os.getpid(),
        "device": summary.device,
        "compute": summary.compute_capability,
    }


@router.get("/ready")
def ready() -> dict[str, str]:
    return {"status": "ready"}


@router.get("/diagnostics")
def diagnostics_route() -> dict[str, Any]:
    return diagnostics()


class ThrottleRequest(BaseModel):
    threads: int = Field(ge=1, le=64)


@router.post("/runtime/throttle")
def throttle(request: ThrottleRequest) -> dict[str, Any]:
    """资源治理器按负载档位调整数学库线程数（docs/11 §5.3）：使用中 1、短暂离开 2、空闲 4。"""
    return {"threads": set_math_threads(request.threads)}


@router.post("/shutdown")
async def shutdown(request: Request) -> dict[str, str]:
    server = getattr(request.app.state, "server", None)
    if server is not None:
        async def stop_soon() -> None:
            await asyncio.sleep(0.05)
            server.should_exit = True

        asyncio.create_task(stop_soon())
    return {"status": "done"}
