from __future__ import annotations

import asyncio
from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel

from ..model_registry import download_model, model_inventory

router = APIRouter(prefix="/models", tags=["models"])

_download_tasks: dict[str, dict[str, Any]] = {}


class DownloadRequest(BaseModel):
    model_key: str


@router.get("/status")
def status() -> dict[str, Any]:
    return {"models": model_inventory(), "downloads": _download_tasks}


@router.post("/download")
async def download(payload: DownloadRequest) -> dict[str, Any]:
    key = payload.model_key
    if key in _download_tasks and _download_tasks[key]["status"] == "running":
        return _download_tasks[key]

    task_state = {"model_key": key, "status": "running", "error": None, "path": None}
    _download_tasks[key] = task_state

    async def run() -> None:
        try:
            path = await asyncio.to_thread(download_model, key)
            task_state["status"] = "done"
            task_state["path"] = str(path)
        except Exception as exc:
            task_state["status"] = "failed"
            task_state["error"] = str(exc)

    asyncio.create_task(run())
    return task_state


@router.post("/cancel")
def cancel(payload: DownloadRequest) -> dict[str, Any]:
    state = _download_tasks.get(payload.model_key)
    if state and state["status"] == "running":
        state["status"] = "cancel_requested"
    return state or {"model_key": payload.model_key, "status": "not_running"}
