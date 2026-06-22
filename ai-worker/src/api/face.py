from __future__ import annotations

from functools import lru_cache
from pathlib import Path
from typing import Any

from fastapi import APIRouter
from pydantic import BaseModel, Field

router = APIRouter(prefix="/face", tags=["face"])


class FaceItem(BaseModel):
    id: int
    thumb_path: str = Field(alias="thumbPath")


class FaceRequest(BaseModel):
    items: list[FaceItem] = Field(default_factory=list, max_length=8)


class FaceBox(BaseModel):
    x: float
    y: float
    w: float
    h: float


class FaceDetection(BaseModel):
    bbox: FaceBox
    confidence: float
    embedding: list[float] | None = None


class FaceResult(BaseModel):
    id: int
    faces: list[FaceDetection] = Field(default_factory=list)
    error: str | None = None


class FaceResponse(BaseModel):
    items: list[FaceResult]
    model: str
    fallback: bool


class FaceRuntime:
    model_key = "insightface-buffalo-l"
    fallback = True

    def __init__(self) -> None:
        self.app = None
        try:
            import onnxruntime as ort  # type: ignore
            from insightface.app import FaceAnalysis  # type: ignore

            providers = ort.get_available_providers()
            provider = "CUDAExecutionProvider" if "CUDAExecutionProvider" in providers else "CPUExecutionProvider"
            ctx_id = 0 if provider == "CUDAExecutionProvider" else -1
            self.app = FaceAnalysis(name="buffalo_l", providers=[provider])
            self.app.prepare(ctx_id=ctx_id, det_size=(640, 640))
            self.fallback = False
        except Exception as exc:
            self._error = str(exc)

    def detect(self, path: Path) -> list[FaceDetection]:
        if self.app is None:
            return []
        import numpy as np  # type: ignore
        from PIL import Image

        image = Image.open(path).convert("RGB")
        arr = np.asarray(image)
        faces = self.app.get(arr)
        detections: list[FaceDetection] = []
        for face in faces:
            x1, y1, x2, y2 = [float(v) for v in face.bbox]
            embedding = getattr(face, "embedding", None)
            vector = None
            if embedding is not None:
                vector = [float(v) for v in embedding.tolist()]
            detections.append(
                FaceDetection(
                    bbox=FaceBox(x=x1, y=y1, w=max(0.0, x2 - x1), h=max(0.0, y2 - y1)),
                    confidence=float(getattr(face, "det_score", 0.0)),
                    embedding=vector,
                )
            )
        return detections


@lru_cache(maxsize=1)
def face_runtime() -> FaceRuntime:
    return FaceRuntime()


@router.post("/detect", response_model=FaceResponse)
def detect_faces(payload: FaceRequest) -> dict[str, Any]:
    runtime = face_runtime()
    results: list[FaceResult] = []
    for item in payload.items:
        try:
            results.append(FaceResult(id=item.id, faces=runtime.detect(Path(item.thumb_path))))
        except Exception as exc:
            results.append(FaceResult(id=item.id, error=str(exc)))
    return {"items": results, "model": runtime.model_key, "fallback": runtime.fallback}
