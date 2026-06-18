from __future__ import annotations

import importlib
import io
import os
import sys
import warnings
from contextlib import redirect_stdout
from pathlib import Path

from PIL import Image, ImageStat

from ..model_registry import model_path
from .clip_loader import clip_runtime

# RAM-plus 输入分辨率与最多保留标签数。
RAM_IMAGE_SIZE = 384
MAX_TAGS = 24
BERT_TOKENIZER_DIRNAME = "bert-base-uncased"
BERT_TOKENIZER_REQUIRED_FILES = ("vocab.txt",)

# (英文 prompt, 中文展示名, category)
# 仅在 RAM-plus 不可用时作为兜底：CLIP 文本编码器对英文 prompt 更准，所以打分用英文，
# 展示给用户的 name 用中文。
TAG_CANDIDATES: list[tuple[str, str, str]] = [
    ("sunset", "日落", "scene"),
    ("beach", "海滩", "scene"),
    ("mountain", "山", "scene"),
    ("city", "城市", "scene"),
    ("forest", "森林", "scene"),
    ("food", "食物", "object"),
    ("portrait", "人像", "person"),
    ("animal", "动物", "object"),
    ("screenshot", "截图", "type"),
    ("document", "文档", "type"),
    ("night", "夜景", "scene"),
    ("sky", "天空", "scene"),
    ("water", "水", "scene"),
    ("flower", "花", "object"),
    ("vehicle", "车辆", "object"),
]

_runtime: "TaggerRuntime | None" = None


class TaggerRuntime:
    """优先用 RAM-plus（Recognize Anything Plus）做多标签识别、原生中文输出；
    模型库或权重缺失时，退回 CLIP 零样本 + 颜色启发式的占位实现。"""

    def __init__(self) -> None:
        self._ram = None
        self._transform = None
        self._infer = None
        self._torch = None
        self._device = "cpu"
        self._load_ram_plus()

    @property
    def model_key(self) -> str:
        return "ram-plus" if self._ram is not None else "clip-zero-shot"

    @property
    def fallback(self) -> bool:
        # RAM-plus 可用即非 fallback；否则取决于 CLIP 是否为真模型。
        if self._ram is not None:
            return False
        return clip_runtime().fallback

    def _load_ram_plus(self) -> None:
        try:
            import torch  # type: ignore

            _patch_transformers_for_ram()
            _quiet_ram_import_warnings()
            _patch_ram_tokenizer()
            from ram import get_transform, inference_ram  # type: ignore
            from ram.models import ram_plus  # type: ignore
        except Exception as exc:
            # 未装 tagger extra / recognize-anything 时这里报 ImportError（预期，退回占位）；
            # 其它 import 错误也打印出来，便于排查（之前静默，定位困难）。
            print(f"[tagger] RAM 库不可用，使用占位标签：{exc}", file=sys.stderr, flush=True)
            return
        checkpoint = _find_ram_plus_checkpoint()
        if checkpoint is None:
            print(
                "[tagger] 未找到 RAM-plus 权重，暂用占位中文标签；"
                "运行 pnpm ai:download ram-plus 下载后重启 worker",
                file=sys.stderr,
                flush=True,
            )
            return
        try:
            device = "cuda" if torch.cuda.is_available() else "cpu"
            with redirect_stdout(io.StringIO()):
                model = ram_plus(
                    pretrained=str(checkpoint),
                    image_size=RAM_IMAGE_SIZE,
                    vit="swin_l",
                )
            model = model.eval().to(device)
        except Exception as exc:
            # 加载失败（显存/权重损坏/库版本不匹配等）→ 退回占位实现，不拖垮 worker。
            print(f"[tagger] RAM-plus 加载失败，退回占位标签：{exc}", file=sys.stderr, flush=True)
            return
        self._torch = torch
        self._transform = get_transform(image_size=RAM_IMAGE_SIZE)
        self._infer = inference_ram
        self._ram = model
        self._device = device
        print(f"[tagger] RAM-plus 已加载（device={device}, ckpt={checkpoint.name}）", file=sys.stderr, flush=True)

    def tags_for_image(self, path: Path) -> list[dict[str, float | str | None]]:
        if self._ram is not None:
            return self._ram_tags(path)
        return self._fallback_tags(path)

    def _ram_tags(self, path: Path) -> list[dict[str, float | str | None]]:
        assert self._torch is not None and self._transform is not None and self._infer is not None
        with Image.open(path) as image:
            tensor = self._transform(image.convert("RGB")).unsqueeze(0).to(self._device)
        with self._torch.no_grad():
            result = self._infer(tensor, self._ram)
        # inference_ram 返回 (英文标签串, 中文标签串)，用 " | " 分隔；取中文。
        chinese = result[1] if isinstance(result, (tuple, list)) and len(result) > 1 else result
        names = _split_tags(str(chinese))
        # RAM 不给每标签分数（阈值过滤后的命中集），统一给较高分；按命中顺序略降以保持稳定排序。
        out: list[dict[str, float | str | None]] = []
        for index, name in enumerate(names[:MAX_TAGS]):
            out.append({"name": name, "score": round(max(0.5, 0.95 - index * 0.01), 3), "category": None})
        return out

    def _fallback_tags(self, path: Path) -> list[dict[str, float | str | None]]:
        with Image.open(path) as image:
            image = image.convert("RGB").resize((64, 64))
            stat = ImageStat.Stat(image)
            width, height = image.size
        tags = self._visual_tags(stat.mean, width, height)
        if not clip_runtime().fallback:
            tags.extend(self._clip_tags(path))
        dedup: dict[str, dict[str, float | str | None]] = {}
        for tag in tags:
            name = str(tag["name"])
            prior = dedup.get(name)
            if prior is None or float(tag["score"]) > float(prior["score"]):
                dedup[name] = tag
        return sorted(dedup.values(), key=lambda item: float(item["score"]), reverse=True)[:12]

    def _visual_tags(self, mean: list[float], width: int, height: int) -> list[dict[str, float | str | None]]:
        red, green, blue = mean
        tags: list[dict[str, float | str | None]] = []
        if blue > red * 1.08 and blue > green * 1.02:
            tags.append({"name": "蓝色", "score": 0.82, "category": "color"})
            tags.append({"name": "天空", "score": 0.66, "category": "scene"})
        if red > blue * 1.08 and red > green:
            tags.append({"name": "暖色", "score": 0.76, "category": "color"})
            tags.append({"name": "日落", "score": 0.58, "category": "scene"})
        if green > red * 1.04 and green > blue * 1.04:
            tags.append({"name": "绿色", "score": 0.78, "category": "color"})
            tags.append({"name": "森林", "score": 0.58, "category": "scene"})
        if abs(width - height) < 8:
            tags.append({"name": "方形", "score": 0.5, "category": "shape"})
        if not tags:
            tags.append({"name": "照片", "score": 0.6, "category": "type"})
        return tags

    def _clip_tags(self, path: Path) -> list[dict[str, float | str | None]]:
        clip = clip_runtime()
        image_vec = clip.embed_image(path)
        scored: list[dict[str, float | str | None]] = []
        for prompt, name, category in TAG_CANDIDATES:
            text_vec = clip.encode_text(f"a photo of {prompt}")
            score = sum(a * b for a, b in zip(image_vec, text_vec))
            scored.append({"name": name, "score": max(0.0, min(1.0, (score + 1.0) / 2.0)), "category": category})
        return sorted(scored, key=lambda item: float(item["score"]), reverse=True)[:8]


def _split_tags(raw: str) -> list[str]:
    parts = [part.strip() for part in raw.replace("｜", "|").replace("，", "|").split("|")]
    seen: set[str] = set()
    ordered: list[str] = []
    for part in parts:
        if part and part not in seen:
            seen.add(part)
            ordered.append(part)
    return ordered


def _patch_transformers_for_ram() -> None:
    """RAM 的 bert.py 仍从 transformers.modeling_utils 导入若干已迁移到 pytorch_utils 的函数，
    新版 transformers 不再 re-export。把它们补回 modeling_utils，使 RAM 在新版 transformers 下可 import。"""
    try:
        import transformers.modeling_utils as mu
        import transformers.pytorch_utils as pu
    except Exception:
        return
    for name in (
        "apply_chunking_to_forward",
        "find_pruneable_heads_and_indices",
        "prune_linear_layer",
    ):
        if not hasattr(mu, name) and hasattr(pu, name):
            setattr(mu, name, getattr(pu, name))


def _patch_ram_tokenizer() -> None:
    """Make RAM use a local BERT tokenizer and avoid HuggingFace HEAD requests at inference time."""
    try:
        from transformers import BertTokenizer
    except Exception:
        return

    tokenizer_dir = _find_bert_tokenizer_dir()

    def init_tokenizer(text_encoder_type: str = BERT_TOKENIZER_DIRNAME):
        source = (
            str(tokenizer_dir)
            if text_encoder_type == BERT_TOKENIZER_DIRNAME and tokenizer_dir
            else text_encoder_type
        )
        tokenizer = BertTokenizer.from_pretrained(source, local_files_only=True)
        tokenizer.add_special_tokens({"bos_token": "[DEC]"})
        tokenizer.add_special_tokens({"additional_special_tokens": ["[ENC]"]})
        tokenizer.enc_token_id = tokenizer.additional_special_tokens_ids[0]
        return tokenizer

    # RAM imports init_tokenizer with `from .utils import *`, so patch both the source module and
    # the already-imported module globals that RAM_plus/RAM call during model construction.
    for module_name in ("ram.models.utils", "ram.models.ram_plus", "ram.models.ram"):
        try:
            module = importlib.import_module(module_name)
            setattr(module, "init_tokenizer", init_tokenizer)
        except Exception:
            continue


def _find_bert_tokenizer_dir() -> Path | None:
    env_dir = os.environ.get("PVP_BERT_TOKENIZER_DIR")
    candidates: list[Path] = []
    if env_dir:
        candidates.append(Path(env_dir))

    ram_dir = model_path("ram-plus")
    candidates.extend(
        [
            ram_dir / BERT_TOKENIZER_DIRNAME,
            model_path(BERT_TOKENIZER_DIRNAME),
        ]
    )
    candidates.extend(_hf_snapshot_dirs(BERT_TOKENIZER_DIRNAME))

    for candidate in candidates:
        if _has_bert_tokenizer_files(candidate):
            return candidate
    return None


def _quiet_ram_import_warnings() -> None:
    warnings.filterwarnings(
        "ignore",
        category=FutureWarning,
        message=r"Importing from timm\.models\..* is deprecated.*",
    )
    try:
        from transformers.utils import logging as transformers_logging

        transformers_logging.set_verbosity_error()
    except Exception:
        pass


def _hf_snapshot_dirs(repo_id: str) -> list[Path]:
    cache_home = os.environ.get("HF_HOME")
    hub = (
        Path(cache_home) / "hub"
        if cache_home
        else Path.home() / ".cache" / "huggingface" / "hub"
    )
    snapshots = hub / f"models--{repo_id.replace('/', '--')}" / "snapshots"
    if not snapshots.exists():
        return []
    return sorted(
        (path for path in snapshots.iterdir() if path.is_dir()),
        key=lambda path: path.stat().st_mtime,
        reverse=True,
    )


def _has_bert_tokenizer_files(path: Path) -> bool:
    return path.is_dir() and all((path / name).exists() for name in BERT_TOKENIZER_REQUIRED_FILES)


def _find_ram_plus_checkpoint() -> Path | None:
    base = model_path("ram-plus")
    if not base.exists():
        return None
    candidates = sorted(base.rglob("*.pth"))
    if not candidates:
        return None
    for candidate in candidates:
        if "ram_plus" in candidate.name.lower():
            return candidate
    return candidates[0]


def tagger_runtime() -> TaggerRuntime:
    global _runtime
    if _runtime is None:
        _runtime = TaggerRuntime()
    return _runtime
