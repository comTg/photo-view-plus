from __future__ import annotations

from pathlib import Path

from PIL import Image, ImageStat

from .clip_loader import clip_runtime

TAG_CANDIDATES: list[tuple[str, str]] = [
    ("sunset", "scene"),
    ("beach", "scene"),
    ("mountain", "scene"),
    ("city", "scene"),
    ("forest", "scene"),
    ("food", "object"),
    ("portrait", "person"),
    ("animal", "object"),
    ("screenshot", "type"),
    ("document", "type"),
    ("night", "scene"),
    ("sky", "scene"),
    ("water", "scene"),
    ("flower", "object"),
    ("vehicle", "object"),
]

_runtime: "TaggerRuntime | None" = None


class TaggerRuntime:
    model_key = "clip-zero-shot"

    @property
    def fallback(self) -> bool:
        return clip_runtime().fallback

    def tags_for_image(self, path: Path) -> list[dict[str, float | str]]:
        with Image.open(path) as image:
            image = image.convert("RGB").resize((64, 64))
            stat = ImageStat.Stat(image)
            width, height = image.size
        tags = self._visual_tags(stat.mean, width, height)
        if not clip_runtime().fallback:
            tags.extend(self._clip_tags(path))
        dedup: dict[str, dict[str, float | str]] = {}
        for tag in tags:
            name = str(tag["name"])
            prior = dedup.get(name)
            if prior is None or float(tag["score"]) > float(prior["score"]):
                dedup[name] = tag
        return sorted(dedup.values(), key=lambda item: float(item["score"]), reverse=True)[:12]

    def _visual_tags(self, mean: list[float], width: int, height: int) -> list[dict[str, float | str]]:
        red, green, blue = mean
        tags: list[dict[str, float | str]] = []
        if blue > red * 1.08 and blue > green * 1.02:
            tags.append({"name": "blue", "score": 0.82, "category": "color"})
            tags.append({"name": "sky", "score": 0.66, "category": "scene"})
        if red > blue * 1.08 and red > green:
            tags.append({"name": "warm", "score": 0.76, "category": "color"})
            tags.append({"name": "sunset", "score": 0.58, "category": "scene"})
        if green > red * 1.04 and green > blue * 1.04:
            tags.append({"name": "green", "score": 0.78, "category": "color"})
            tags.append({"name": "forest", "score": 0.58, "category": "scene"})
        if abs(width - height) < 8:
            tags.append({"name": "square", "score": 0.5, "category": "shape"})
        if not tags:
            tags.append({"name": "photo", "score": 0.6, "category": "type"})
        return tags

    def _clip_tags(self, path: Path) -> list[dict[str, float | str]]:
        clip = clip_runtime()
        image_vec = clip.embed_image(path)
        scored = []
        for name, category in TAG_CANDIDATES:
            text_vec = clip.encode_text(f"a photo of {name}")
            score = sum(a * b for a, b in zip(image_vec, text_vec))
            scored.append({"name": name, "score": max(0.0, min(1.0, (score + 1.0) / 2.0)), "category": category})
        return sorted(scored, key=lambda item: float(item["score"]), reverse=True)[:8]


def tagger_runtime() -> TaggerRuntime:
    global _runtime
    if _runtime is None:
        _runtime = TaggerRuntime()
    return _runtime
