from __future__ import annotations

from pathlib import Path
from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, Field

from ..models.tagger_loader import tagger_runtime

router = APIRouter(prefix="/tagger", tags=["tagger"])


class TagItem(BaseModel):
    id: int
    thumb_path: str = Field(alias="thumbPath")


class TagRequest(BaseModel):
    items: list[TagItem] = Field(default_factory=list, max_length=16)


class TagScore(BaseModel):
    name: str
    score: float
    category: str | None = None


class TagResult(BaseModel):
    id: int
    tags: list[TagScore] = Field(default_factory=list)
    error: str | None = None


class TagResponse(BaseModel):
    items: list[TagResult]
    model: str
    fallback: bool


@router.post("/run", response_model=TagResponse)
def run_tagger(payload: TagRequest) -> dict[str, Any]:
    runtime = tagger_runtime()
    results: list[TagResult] = []
    for item in payload.items:
        try:
            tags = runtime.tags_for_image(Path(item.thumb_path))
            results.append(TagResult(id=item.id, tags=[TagScore(**tag) for tag in tags]))
        except Exception as exc:
            results.append(TagResult(id=item.id, error=str(exc)))
    return {"items": results, "model": runtime.model_key, "fallback": runtime.fallback}
