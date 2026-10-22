from __future__ import annotations

from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, Field

from ..models.text_loader import text_runtime

router = APIRouter(prefix="/text", tags=["text"])


class TextEmbedRequest(BaseModel):
    texts: list[str] = Field(default_factory=list, max_length=256)
    # 文本向量模型 key（设置项 aiTextModel）；不传时沿用已加载的模型或默认模型。
    model: str | None = None
    # 查询文本：按模型卡加检索指令前缀；描述（被检索的文本）不加。
    query: bool = False


class TextEmbedResponse(BaseModel):
    embeddings: list[list[float]]
    model: str
    dims: int
    fallback: bool


@router.post("/embed", response_model=TextEmbedResponse)
def embed_texts(payload: TextEmbedRequest) -> dict[str, Any]:
    runtime = text_runtime(payload.model)
    embeddings = runtime.embed(payload.texts, query=payload.query) if payload.texts else []
    return {
        "embeddings": embeddings,
        "model": runtime.model_key,
        "dims": runtime.dims,
        "fallback": runtime.fallback,
    }
