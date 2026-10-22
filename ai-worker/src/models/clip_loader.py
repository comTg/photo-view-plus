"""图像向量（CLIP 系）运行时：按模型 key 加载，把图片和文本编码到同一个向量空间（docs/10 §5.3、A5）。

- ``chinese-clip-vit-b-16``：Chinese-CLIP（transformers 的 ChineseCLIPModel），中文文本直接检索图片，默认；
- ``clip-vit-b-32``：open_clip ViT-B-32 + OpenAI 权重，只懂英文，仅作回退。

同一时间只驻留一个模型：请求换了模型就先卸掉旧的，释放显存。
模型加载失败时进入兜底模式：返回与内容无关的哈希伪向量，响应带 ``fallback=True``，Rust 侧一律不入库。
"""

from __future__ import annotations

import hashlib
import math
import os
import sys
import threading
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from PIL import Image

from ..model_registry import model_path, model_spec

LEGACY_CLIP_MODEL = "clip-vit-b-32"
DEFAULT_CLIP_MODEL = "chinese-clip-vit-b-16"
# 旧代码按 512 维约定；各模型的真实维度以 runtime.dims 为准。
CLIP_DIMS = 512
# Chinese-CLIP 训练时的文本长度上限（token）。
CHINESE_CLIP_TEXT_LEN = 52
# 读缩略图的线程数：解码 256px 的 WebP 很快，几个线程就能喂饱 GPU。
LOAD_THREADS = 4

_runtime: "ClipRuntime | None" = None
_runtime_lock = threading.Lock()


def default_clip_model() -> str:
    """Rust 启动 worker 时用 PVP_CLIP_MODEL 传入设置里的模型；没传就用默认模型。"""
    return os.environ.get("PVP_CLIP_MODEL", "").strip() or DEFAULT_CLIP_MODEL


def use_fp16(device: str) -> bool:
    """CUDA 上默认半精度推理（显存减半、批次更快）；PVP_CLIP_FP32=1 强制单精度。"""
    return device == "cuda" and os.environ.get("PVP_CLIP_FP32", "") != "1"


class ClipRuntime:
    """一个 CLIP 实现。子类实现 ``_load`` / ``_encode_images`` / ``_encode_texts``。"""

    def __init__(self, model_key: str) -> None:
        self.model_key = model_key
        self.dims = int(model_spec(model_key).get("dims") or CLIP_DIMS)
        self.fallback = True
        self.error: str | None = None
        self.device = "cpu"
        self.precision = "fp32"
        self._torch = None
        self._model = None
        # 同一模型的推理串行：FastAPI 的同步端点跑在线程池里，搜索编码和批量向量化可能同时到达。
        self._infer_lock = threading.Lock()
        try:
            self._load()
            self.fallback = False
        except Exception as exc:
            self.error = f"{type(exc).__name__}: {exc}"
            print(f"[clip] {model_key} 加载失败，进入兜底模式：{self.error}", file=sys.stderr, flush=True)

    def _load(self) -> None:
        raise NotImplementedError

    def _encode_images(self, images: list[Image.Image]) -> list[list[float]]:
        raise NotImplementedError

    def _encode_texts(self, texts: list[str]) -> list[list[float]]:
        raise NotImplementedError

    def embed_images(self, paths: list[Path]) -> list[list[float] | Exception]:
        """逐张返回向量或异常：单张读不出来不影响同批其他图。整批推理失败时异常向上抛。"""
        loaded = load_images(paths)
        out: list[list[float] | Exception] = [img if isinstance(img, Exception) else [] for img in loaded]
        ok = [(index, img) for index, img in enumerate(loaded) if isinstance(img, Image.Image)]
        if not ok:
            return out
        if self.fallback:
            vectors = [image_fallback_vector(img, self.dims) for _, img in ok]
        else:
            with self._infer_lock:
                vectors = self._encode_images([img for _, img in ok])
        for (index, _), vector in zip(ok, vectors):
            out[index] = vector
        return out

    def embed_image(self, path: Path) -> list[float]:
        result = self.embed_images([path])[0]
        if isinstance(result, Exception):
            raise result
        return result

    def encode_text(self, text: str) -> list[float]:
        text = text.strip()
        if not text:
            raise ValueError("text is empty")
        if self.fallback:
            return deterministic_vector(text.encode("utf-8"), self.dims)
        with self._infer_lock:
            return self._encode_texts([text])[0]

    def close(self) -> None:
        """卸载模型、释放显存（换模型时调用）。等在途推理结束再卸。"""
        with self._infer_lock:
            self._model = None
            self._release_extra()
            torch = self._torch
            if torch is not None and self.device == "cuda":
                try:
                    torch.cuda.empty_cache()
                except Exception:
                    pass

    def _release_extra(self) -> None:
        pass

    def _rows(self, features) -> list[list[float]]:
        """L2 归一化后转成 Python 列表（统一在 float32 上做，半精度推理也一样）。"""
        features = features.float()
        features = features / features.norm(dim=-1, keepdim=True).clamp_min(1e-12)
        return features.detach().cpu().tolist()


class OpenAiClipRuntime(ClipRuntime):
    """open_clip ViT-B-32 + OpenAI 权重（旧模型，保持单精度，向量与历史数据一致）。"""

    def _load(self) -> None:
        import open_clip  # type: ignore
        import torch  # type: ignore

        device = "cuda" if torch.cuda.is_available() else "cpu"
        model, _, preprocess = open_clip.create_model_and_transforms("ViT-B-32", pretrained="openai", device=device)
        model.eval()
        self._torch = torch
        self._model = model
        self._preprocess = preprocess
        self._tokenizer = open_clip.get_tokenizer("ViT-B-32")
        self.device = device

    def _encode_images(self, images: list[Image.Image]) -> list[list[float]]:
        torch = self._torch
        tensor = torch.stack([self._preprocess(img) for img in images]).to(self.device)
        with torch.inference_mode():
            return self._rows(self._model.encode_image(tensor))

    def _encode_texts(self, texts: list[str]) -> list[list[float]]:
        torch = self._torch
        tokens = self._tokenizer(texts).to(self.device)
        with torch.inference_mode():
            return self._rows(self._model.encode_text(tokens))


class ChineseClipRuntime(ClipRuntime):
    """Chinese-CLIP（OFA-Sys），本地目录加载，CUDA 上半精度。"""

    def _load(self) -> None:
        import torch  # type: ignore
        from transformers import ChineseCLIPModel, ChineseCLIPProcessor  # type: ignore
        from transformers.utils import logging as transformers_logging  # type: ignore

        transformers_logging.set_verbosity_error()
        path = model_path(self.model_key)
        if not path.exists():
            raise FileNotFoundError(f"模型目录不存在：{path}（pnpm ai:download {self.model_key}）")
        device = "cuda" if torch.cuda.is_available() else "cpu"
        dtype = torch.float16 if use_fp16(device) else torch.float32
        model = ChineseCLIPModel.from_pretrained(path, torch_dtype=dtype).to(device).eval()
        self._processor = ChineseCLIPProcessor.from_pretrained(path)
        self._torch = torch
        self._model = model
        self._dtype = dtype
        self.device = device
        self.precision = "fp16" if dtype == torch.float16 else "fp32"

    def _release_extra(self) -> None:
        self._processor = None

    def _encode_images(self, images: list[Image.Image]) -> list[list[float]]:
        torch = self._torch
        pixels = self._processor(images=images, return_tensors="pt")["pixel_values"]
        pixels = pixels.to(self.device, dtype=self._dtype)
        with torch.inference_mode():
            # 不用 get_image_features：当前 transformers 版本里它取到的 pooled 输出为空；
            # 按 Chinese-CLIP 的定义取 [CLS]（pooler_output 已过 post_layernorm）再过投影层，与评测原型一致。
            out = self._model.vision_model(pixel_values=pixels)
            pooled = out.pooler_output if out.pooler_output is not None else out.last_hidden_state[:, 0, :]
            return self._rows(self._model.visual_projection(pooled))

    def _encode_texts(self, texts: list[str]) -> list[list[float]]:
        torch = self._torch
        inputs = self._processor.tokenizer(
            texts,
            padding=True,
            truncation=True,
            max_length=CHINESE_CLIP_TEXT_LEN,
            return_tensors="pt",
        ).to(self.device)
        with torch.inference_mode():
            out = self._model.text_model(**inputs)
            return self._rows(self._model.text_projection(out.last_hidden_state[:, 0, :]))


class UnknownClipRuntime(ClipRuntime):
    def _load(self) -> None:
        raise ValueError(f"未知的图像向量模型：{self.model_key}")


RUNTIME_CLASSES: dict[str, type[ClipRuntime]] = {
    "open_clip": OpenAiClipRuntime,
    "chinese_clip": ChineseClipRuntime,
}


def clip_runtime(model_key: str | None = None) -> ClipRuntime:
    """取指定模型的运行时；不指定时沿用已加载的模型，没有就加载默认模型。换模型会先卸掉旧的。"""
    global _runtime
    with _runtime_lock:
        key = (model_key or "").strip()
        if not key:
            key = _runtime.model_key if _runtime is not None else default_clip_model()
        if _runtime is not None and _runtime.model_key == key:
            return _runtime
        if _runtime is not None:
            _runtime.close()
            _runtime = None
        spec = model_spec(key)
        runtime_class = RUNTIME_CLASSES.get(str(spec.get("format")), UnknownClipRuntime)
        if spec.get("kind") not in (None, "clip"):
            runtime_class = UnknownClipRuntime
        _runtime = runtime_class(key)
        return _runtime


def loaded_clip_runtime() -> ClipRuntime | None:
    return _runtime


def load_images(paths: list[Path]) -> list[Image.Image | Exception]:
    if len(paths) <= 1:
        return [_open_rgb(path) for path in paths]
    with ThreadPoolExecutor(max_workers=min(LOAD_THREADS, len(paths))) as pool:
        return list(pool.map(_open_rgb, paths))


def _open_rgb(path: Path) -> Image.Image | Exception:
    try:
        with Image.open(path) as image:
            return image.convert("RGB")
    except Exception as exc:
        return exc


def image_fallback_vector(image: Image.Image, dims: int) -> list[float]:
    small = image.resize((32, 32))
    digest = hashlib.blake2b(small.tobytes(), digest_size=64).digest()
    return deterministic_vector(digest, dims)


def deterministic_vector(seed: bytes, dims: int = CLIP_DIMS) -> list[float]:
    values: list[float] = []
    counter = 0
    while len(values) < dims:
        digest = hashlib.blake2b(seed + counter.to_bytes(4, "little"), digest_size=64).digest()
        for index in range(0, len(digest), 4):
            raw = int.from_bytes(digest[index : index + 4], "little")
            values.append((raw / 0xFFFF_FFFF) * 2.0 - 1.0)
            if len(values) == dims:
                break
        counter += 1
    norm = math.sqrt(sum(v * v for v in values)) or 1.0
    return [v / norm for v in values]
