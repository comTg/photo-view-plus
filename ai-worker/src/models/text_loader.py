"""中文文本向量运行时（docs/10 §5.2、A4）：给画面描述编码，用于"描述语义"检索通道。

默认 bge-small-zh-v1.5（512 维）；bge-base-zh-v1.5（768 维）可选。取 [CLS] 向量并 L2 归一化；
查询文本按模型卡加检索指令前缀（manifest 里的 query_prefix），描述文本不加。
同一时间只驻留一个模型；加载失败时进入兜底模式（哈希伪向量，fallback=True，Rust 侧不入库）。
"""

from __future__ import annotations

import os
import sys
import threading

from ..model_registry import model_path, model_spec
from .clip_loader import deterministic_vector, use_fp16

DEFAULT_TEXT_MODEL = "bge-small-zh-v1.5"
# 描述一般 60~150 字，远小于这个上限；查询更短。
MAX_TOKENS = 512

_runtime: "TextRuntime | None" = None
_runtime_lock = threading.Lock()


def default_text_model() -> str:
    return os.environ.get("PVP_TEXT_MODEL", "").strip() or DEFAULT_TEXT_MODEL


class TextRuntime:
    def __init__(self, model_key: str) -> None:
        spec = model_spec(model_key)
        self.model_key = model_key
        self.dims = int(spec.get("dims") or 512)
        self.query_prefix = str(spec.get("query_prefix") or "")
        self.fallback = True
        self.error: str | None = None
        self.device = "cpu"
        self.precision = "fp32"
        self._torch = None
        self._model = None
        self._tokenizer = None
        self._infer_lock = threading.Lock()
        try:
            if spec.get("kind") != "text":
                raise ValueError(f"未知的文本向量模型：{model_key}")
            self._load()
            self.fallback = False
        except Exception as exc:
            self.error = f"{type(exc).__name__}: {exc}"
            print(f"[text] {model_key} 加载失败，进入兜底模式：{self.error}", file=sys.stderr, flush=True)

    def _load(self) -> None:
        import torch  # type: ignore
        from transformers import AutoModel, AutoTokenizer  # type: ignore
        from transformers.utils import logging as transformers_logging  # type: ignore

        transformers_logging.set_verbosity_error()
        path = model_path(self.model_key)
        if not path.exists():
            raise FileNotFoundError(f"模型目录不存在：{path}（pnpm ai:download {self.model_key}）")
        device = "cuda" if torch.cuda.is_available() else "cpu"
        dtype = torch.float16 if use_fp16(device) else torch.float32
        self._tokenizer = AutoTokenizer.from_pretrained(path)
        self._model = AutoModel.from_pretrained(path, torch_dtype=dtype).to(device).eval()
        self._torch = torch
        self.device = device
        self.precision = "fp16" if dtype == torch.float16 else "fp32"

    def embed(self, texts: list[str], query: bool = False) -> list[list[float]]:
        texts = [text.strip() for text in texts]
        if any(not text for text in texts):
            raise ValueError("text is empty")
        if query and self.query_prefix:
            texts = [self.query_prefix + text for text in texts]
        if self.fallback:
            return [deterministic_vector(text.encode("utf-8"), self.dims) for text in texts]
        torch = self._torch
        with self._infer_lock:
            encoded = self._tokenizer(
                texts,
                padding=True,
                truncation=True,
                max_length=MAX_TOKENS,
                return_tensors="pt",
            ).to(self.device)
            with torch.inference_mode():
                cls = self._model(**encoded).last_hidden_state[:, 0].float()
                cls = cls / cls.norm(dim=-1, keepdim=True).clamp_min(1e-12)
            return cls.detach().cpu().tolist()

    def close(self) -> None:
        with self._infer_lock:
            self._model = None
            self._tokenizer = None
            if self._torch is not None and self.device == "cuda":
                try:
                    self._torch.cuda.empty_cache()
                except Exception:
                    pass


def text_runtime(model_key: str | None = None) -> TextRuntime:
    """取指定文本向量模型；不指定时沿用已加载的模型，没有就加载默认模型。换模型会先卸掉旧的。"""
    global _runtime
    with _runtime_lock:
        key = (model_key or "").strip()
        if not key:
            key = _runtime.model_key if _runtime is not None else default_text_model()
        if _runtime is not None and _runtime.model_key == key:
            return _runtime
        if _runtime is not None:
            _runtime.close()
            _runtime = None
        _runtime = TextRuntime(key)
        return _runtime
