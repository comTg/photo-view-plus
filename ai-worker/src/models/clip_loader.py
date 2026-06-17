from __future__ import annotations

import hashlib
import math
import threading
from pathlib import Path

from PIL import Image

CLIP_DIMS = 512

_runtime: "ClipRuntime | None" = None
_runtime_lock = threading.Lock()


class ClipRuntime:
    model_key = "clip-vit-b-32"

    def __init__(self) -> None:
        self.fallback = True
        self._model = None
        self._preprocess = None
        self._tokenizer = None
        self._torch = None
        self._device = "cpu"
        self._load_open_clip()

    def _load_open_clip(self) -> None:
        try:
            import open_clip  # type: ignore
            import torch  # type: ignore

            device = "cuda" if torch.cuda.is_available() else "cpu"
            model, _, preprocess = open_clip.create_model_and_transforms(
                "ViT-B-32",
                pretrained="openai",
                device=device,
            )
            model.eval()
            self._model = model
            self._preprocess = preprocess
            self._tokenizer = open_clip.get_tokenizer("ViT-B-32")
            self._torch = torch
            self._device = device
            self.fallback = False
        except Exception:
            self.fallback = True

    def embed_image(self, path: Path) -> list[float]:
        if not path.exists():
            raise FileNotFoundError(str(path))
        if not self.fallback and self._model is not None:
            return self._embed_image_open_clip(path)
        with Image.open(path) as image:
            image = image.convert("RGB").resize((32, 32))
            digest = hashlib.blake2b(image.tobytes(), digest_size=64).digest()
        return deterministic_vector(digest)

    def encode_text(self, text: str) -> list[float]:
        text = text.strip()
        if not text:
            raise ValueError("text is empty")
        if not self.fallback and self._model is not None:
            return self._encode_text_open_clip(text)
        return deterministic_vector(text.encode("utf-8"))

    def _embed_image_open_clip(self, path: Path) -> list[float]:
        torch = self._torch
        assert torch is not None
        assert self._preprocess is not None
        with Image.open(path) as image:
            tensor = self._preprocess(image.convert("RGB")).unsqueeze(0).to(self._device)
        with torch.no_grad():
            features = self._model.encode_image(tensor)
            features = features / features.norm(dim=-1, keepdim=True)
        return [float(v) for v in features.squeeze(0).detach().cpu().tolist()]

    def _encode_text_open_clip(self, text: str) -> list[float]:
        torch = self._torch
        assert torch is not None
        assert self._tokenizer is not None
        tokens = self._tokenizer([text]).to(self._device)
        with torch.no_grad():
            features = self._model.encode_text(tokens)
            features = features / features.norm(dim=-1, keepdim=True)
        return [float(v) for v in features.squeeze(0).detach().cpu().tolist()]


def clip_runtime() -> ClipRuntime:
    global _runtime
    with _runtime_lock:
        if _runtime is None:
            _runtime = ClipRuntime()
        return _runtime


def deterministic_vector(seed: bytes) -> list[float]:
    values: list[float] = []
    counter = 0
    while len(values) < CLIP_DIMS:
        digest = hashlib.blake2b(seed + counter.to_bytes(4, "little"), digest_size=64).digest()
        for index in range(0, len(digest), 4):
            raw = int.from_bytes(digest[index : index + 4], "little")
            values.append((raw / 0xFFFF_FFFF) * 2.0 - 1.0)
            if len(values) == CLIP_DIMS:
                break
        counter += 1
    norm = math.sqrt(sum(v * v for v in values)) or 1.0
    return [v / norm for v in values]
