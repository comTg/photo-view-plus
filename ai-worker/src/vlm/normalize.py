"""VLM 输出规范化（docs/10 §6）：清洗标签、按同义词表归一、夹取越界值、给标签按顺序赋分。

VLM 不给置信度，标签分数按顺序赋值：0.95 − 0.02 × 序号，最低 0.6。
"""

from __future__ import annotations

import json
import re
from dataclasses import asdict, dataclass, field
from functools import lru_cache
from pathlib import Path
from typing import Any

from .prompts import CATEGORIES

MAX_TAGS = 15
MAX_KEYWORDS = 15
MAX_TAG_CHARS = 8
MAX_KEYWORD_CHARS = 10
MAX_VISIBLE_TEXT = 40
MAX_DESCRIPTION = 300
MAX_PEOPLE = 10

# 不当标签用的泛词。
STOP_TAGS = {
    "照片", "图片", "画面", "背景", "图像", "影像", "相片", "特写", "镜头", "拍摄", "摄影", "一张",
    "无", "其他", "未知", "内容", "主体", "场景", "画面中", "照片中", "图中",
}
# 纯颜色词不当标签（标签云里是噪声），挪进 keywords，仍然搜得到。
COLOR_TAGS = {
    "红色", "橙色", "黄色", "绿色", "青色", "蓝色", "紫色", "粉色", "粉红色", "白色", "黑色", "灰色",
    "棕色", "褐色", "金色", "银色", "米色", "米白色", "浅色", "深色", "彩色", "黑白", "暖色", "冷色",
}
SPLIT_RE = re.compile(r"[/／、，,；;|｜\n]+")
# 连续 8 位以上数字（可夹 - 和空格）多半是手机号、证件号、卡号：打码。日期（2024-05-01）保留。
LONG_NUMBER_RE = re.compile(r"[A-Za-z]{0,2}\d[\d\- ]{6,}\d")
DATE_RE = re.compile(r"\d{4}[-/.]\d{1,2}[-/.]\d{1,2}")
REDACTED = "＊＊＊"
# 去掉标点和空白（保留汉字、字母、数字）。
PUNCT_RE = re.compile(r"[\s\"'`~!@#$%^&*()\-_=+\[\]{}\\:<>?.·…—～“”‘’（）【】《》「」『』〈〉！？。：·#＃]+")
SPACE_RE = re.compile(r"\s+")


@dataclass
class TagScore:
    name: str
    score: float


@dataclass
class Analysis:
    description: str
    categories: list[str]
    tags: list[TagScore]
    keywords: list[str]
    people_count: int
    visible_text: str
    sensitive: bool
    raw: dict[str, Any] = field(default_factory=dict)

    def to_dict(self) -> dict[str, Any]:
        data = asdict(self)
        data["raw_json"] = json.dumps(self.raw, ensure_ascii=False)
        del data["raw"]
        return data


@lru_cache(maxsize=1)
def synonym_map() -> dict[str, str]:
    """同义说法 → 规范说法。"""
    path = Path(__file__).with_name("synonyms.json")
    table = json.loads(path.read_text(encoding="utf-8"))
    mapping: dict[str, str] = {}
    for canonical, variants in table.items():
        if canonical.startswith("_"):
            continue
        for variant in variants:
            mapping[variant] = canonical
    return mapping


def clean_term(value: str) -> str:
    return PUNCT_RE.sub("", str(value)).strip()


def split_terms(values: Any) -> list[str]:
    """把列表（或误给的字符串）按斜杠、顿号等拆开、去标点，保持顺序、去重。"""
    if isinstance(values, str):
        values = [values]
    if not isinstance(values, list):
        return []
    out: list[str] = []
    for value in values:
        for part in SPLIT_RE.split(str(value)):
            term = clean_term(part)
            if term and term not in out:
                out.append(term)
    return out


def tag_score(index: int) -> float:
    return round(max(0.6, 0.95 - 0.02 * index), 2)


def redact_numbers(text: str) -> str:
    """把手机号、证件号、卡号这类长数字打码（docs/10 §6：个人信息不抄录，模型偶尔还是会抄）。"""

    def replace(match: re.Match[str]) -> str:
        value = match.group(0)
        if DATE_RE.fullmatch(value.strip()):
            return value
        digits = sum(ch.isdigit() for ch in value)
        return REDACTED if digits >= 8 else value

    return LONG_NUMBER_RE.sub(replace, text)


def normalize_tags(raw_tags: Any) -> tuple[list[str], list[str]]:
    """返回（规范化后的标签, 要挪进 keywords 的词：被同义词归一掉的原词、颜色词）。"""
    synonyms = synonym_map()
    tags: list[str] = []
    originals: list[str] = []
    for term in split_terms(raw_tags):
        if term in STOP_TAGS or not 1 <= len(term) <= MAX_TAG_CHARS:
            continue
        if term in COLOR_TAGS:
            if term not in originals:
                originals.append(term)
            continue
        canonical = synonyms.get(term, term)
        if canonical != term and term not in originals:
            originals.append(term)
        if canonical not in tags:
            tags.append(canonical)
        if len(tags) >= MAX_TAGS:
            break
    return tags, originals


def normalize_keywords(raw_keywords: Any, tags: list[str], extra: list[str]) -> list[str]:
    keywords: list[str] = []
    for term in [*split_terms(raw_keywords), *extra]:
        if term in tags or term in keywords or term in STOP_TAGS:
            continue
        if not 1 <= len(term) <= MAX_KEYWORD_CHARS:
            continue
        keywords.append(term)
        if len(keywords) >= MAX_KEYWORDS:
            break
    return keywords


def normalize_categories(raw: Any) -> list[str]:
    values = raw if isinstance(raw, list) else [raw]
    picked = [str(value).strip() for value in values if str(value).strip() in CATEGORIES]
    return list(dict.fromkeys(picked))[:2] or ["其他"]


def clamp_people(raw: Any) -> int:
    try:
        count = int(raw)
    except (TypeError, ValueError):
        return 0
    return min(MAX_PEOPLE, max(0, count))


def collapse(text: Any, limit: int) -> str:
    value = SPACE_RE.sub(" ", str(text or "")).strip()
    return value[:limit]


def to_bool(raw: Any) -> bool:
    if isinstance(raw, str):
        return raw.strip().lower() in {"true", "1", "yes", "是"}
    return bool(raw)


def normalize(raw: dict[str, Any]) -> Analysis:
    tags, originals = normalize_tags(raw.get("tags"))
    keywords = normalize_keywords(raw.get("keywords"), tags, originals)
    sensitive = to_bool(raw.get("sensitive"))
    # 证件、票据这类敏感图：模型即使被要求只写类别，也常把姓名、住址抄进画面文字。
    # 描述和标签已经说明了是什么（"身份证"），画面文字直接不存。
    visible_text = "" if sensitive else collapse(redact_numbers(str(raw.get("visible_text") or "")), MAX_VISIBLE_TEXT)
    return Analysis(
        description=collapse(redact_numbers(str(raw.get("description") or "")), MAX_DESCRIPTION),
        categories=normalize_categories(raw.get("categories")),
        tags=[TagScore(name=name, score=tag_score(index)) for index, name in enumerate(tags)],
        keywords=keywords,
        people_count=clamp_people(raw.get("people_count")),
        visible_text=visible_text,
        sensitive=sensitive,
        raw=raw,
    )
