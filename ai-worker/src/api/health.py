from __future__ import annotations

import asyncio
import os
from typing import Any

from fastapi import APIRouter, Request

from ..runtime import diagnostics, runtime_summary

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


@router.post("/shutdown")
async def shutdown(request: Request) -> dict[str, str]:
    server = getattr(request.app.state, "server", None)
    if server is not None:
        async def stop_soon() -> None:
            await asyncio.sleep(0.05)
            server.should_exit = True

        asyncio.create_task(stop_soon())
    return {"status": "done"}
