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
    items: list[EmbedItem] = Field(default_factory=list, max_length=32)


class EmbedResult(BaseModel):
    id: int
    embedding: list[float] | None = None
    error: str | None = None


class EmbedResponse(BaseModel):
    items: list[EmbedResult]
    model: str
    fallback: bool


class TextRequest(BaseModel):
    text: str


class TextResponse(BaseModel):
    embedding: list[float]
    model: str
    fallback: bool


@router.post("/embed", response_model=EmbedResponse)
def embed_images(payload: EmbedRequest) -> dict[str, Any]:
    runtime = clip_runtime()
    results: list[EmbedResult] = []
    for item in payload.items:
        try:
            vector = runtime.embed_image(Path(item.thumb_path))
            results.append(EmbedResult(id=item.id, embedding=vector))
        except Exception as exc:
            results.append(EmbedResult(id=item.id, error=str(exc)))
    return {"items": results, "model": runtime.model_key, "fallback": runtime.fallback}


@router.post("/encode_text", response_model=TextResponse)
def encode_text(payload: TextRequest) -> dict[str, Any]:
    runtime = clip_runtime()
    return {
        "embedding": runtime.encode_text(payload.text),
        "model": runtime.model_key,
        "fallback": runtime.fallback,
    }
