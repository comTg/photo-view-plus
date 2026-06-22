from __future__ import annotations

from functools import lru_cache
from pathlib import Path
from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, Field

router = APIRouter(prefix="/ocr", tags=["ocr"])


class OcrItem(BaseModel):
    id: int
    thumb_path: str = Field(alias="thumbPath")


class OcrRequest(BaseModel):
    items: list[OcrItem] = Field(default_factory=list, max_length=8)


class OcrLine(BaseModel):
    bbox: Any
    content: str
    confidence: float


class OcrResult(BaseModel):
    id: int
    text: str = ""
    lines: list[OcrLine] = Field(default_factory=list)
    error: str | None = None


class OcrResponse(BaseModel):
    items: list[OcrResult]
    model: str
    fallback: bool


class OcrRuntime:
    model_key = "rapidocr"
    fallback = True

    def __init__(self) -> None:
        self.engine = None
        try:
            from rapidocr_onnxruntime import RapidOCR  # type: ignore

            self.engine = RapidOCR()
            self.fallback = False
        except Exception as exc:
            self._error = str(exc)

    def run(self, path: Path) -> tuple[str, list[OcrLine]]:
        if self.engine is None:
            return "", []
        result, _elapsed = self.engine(str(path))
        lines: list[OcrLine] = []
        text_parts: list[str] = []
        for row in result or []:
            if len(row) < 3:
                continue
            bbox, content, confidence = row[0], str(row[1]), float(row[2])
            if content.strip():
                text_parts.append(content.strip())
            lines.append(OcrLine(bbox=bbox, content=content, confidence=confidence))
        return "\n".join(text_parts), lines


@lru_cache(maxsize=1)
def ocr_runtime() -> OcrRuntime:
    return OcrRuntime()


@router.post("/run", response_model=OcrResponse)
def run_ocr(payload: OcrRequest) -> dict[str, Any]:
    runtime = ocr_runtime()
    results: list[OcrResult] = []
    for item in payload.items:
        try:
            text, lines = runtime.run(Path(item.thumb_path))
            results.append(OcrResult(id=item.id, text=text, lines=lines))
        except Exception as exc:
            results.append(OcrResult(id=item.id, error=str(exc)))
    return {"items": results, "model": runtime.model_key, "fallback": runtime.fallback}
