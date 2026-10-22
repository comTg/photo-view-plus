from __future__ import annotations

from pathlib import Path
from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, Field

from ..models.clip_loader import clip_runtime

router = APIRouter(prefix="/clip", tags=["clip"])


class EmbedItem(BaseModel):
    id: int
    thumb_path: str = Field(alias="thumbPath")


class EmbedRequest(BaseModel):
    items: list[EmbedItem] = Field(default_factory=list, max_length=64)
    # 图像向量模型 key（设置项 aiClipModel）；不传时沿用已加载的模型或默认模型。
    model: str | None = None


class EmbedResult(BaseModel):
    id: int
    embedding: list[float] | None = None
    error: str | None = None


class EmbedResponse(BaseModel):
    items: list[EmbedResult]
    model: str
    dims: int
    fallback: bool


class TextRequest(BaseModel):
    text: str
    model: str | None = None


class TextResponse(BaseModel):
    embedding: list[float]
    model: str
    dims: int
    fallback: bool


@router.post("/embed", response_model=EmbedResponse)
def embed_images(payload: EmbedRequest) -> dict[str, Any]:
    runtime = clip_runtime(payload.model)
    results: list[EmbedResult] = []
    try:
        vectors = runtime.embed_images([Path(item.thumb_path) for item in payload.items])
    except Exception as exc:  # 整批推理失败（如显存不足）：每张都记失败，由 Rust 侧按次数重试
        message = f"{type(exc).__name__}: {exc}"
        vectors = [RuntimeError(message) for _ in payload.items]
    for item, vector in zip(payload.items, vectors):
        if isinstance(vector, Exception):
            results.append(EmbedResult(id=item.id, error=str(vector)))
        else:
            results.append(EmbedResult(id=item.id, embedding=vector))
    return {"items": results, "model": runtime.model_key, "dims": runtime.dims, "fallback": runtime.fallback}


@router.post("/encode_text", response_model=TextResponse)
def encode_text(payload: TextRequest) -> dict[str, Any]:
    runtime = clip_runtime(payload.model)
    return {
        "embedding": runtime.encode_text(payload.text),
        "model": runtime.model_key,
        "dims": runtime.dims,
        "fallback": runtime.fallback,
    }
