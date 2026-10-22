"""VLM 提示词与输出 Schema（docs/10 §6）。

系统提示词固定不变，llama-server 才能复用前缀缓存。改提示词就升 PROMPT_VERSION，是否重跑由用户手动触发。
"""

from __future__ import annotations

from datetime import datetime

PROMPT_VERSION = "search-v1"

CATEGORIES = [
    "人物", "合影", "自拍", "孩子", "宠物", "动物", "旅行", "风景", "建筑街景", "美食",
    "聚会活动", "运动", "日常生活", "花草植物", "交通工具", "物品", "文档票据", "截图",
    "网图表情包", "其他",
]

SYSTEM_PROMPT = f"""你是一个"照片检索标注助手"。你的输出会用于本地相册的中文搜索：用户会用口语描述想找的照片，
例如"海边的小孩""生日吃蛋糕""发票截图"。请客观、具体地描述画面里能确定的内容，
不要猜测人物身份和关系，不要编造画面里没有的东西。

1. description：60~150 字。依次写：照片类型（照片/截图/文档等）；主体和人物（人数、大致年龄段、
   动作、表情、显著的穿着）；场景和地点类型（室内/室外、海边/餐厅/街道等）；时间与天气
   （白天/夜晚/雪天等）；显著的物体、动物、食物；显著时再写主要颜色和光线。
   不要写出人名、证件号、手机号、卡号、住址（证件、票据、截图上的也不要抄），只说明是什么，
   例如"一张身份证正面""快递取件页面的截图"。
2. categories：从下面的列表里选 1~2 个，第一个是主类别：{"、".join(CATEGORIES)}。
3. tags：5~15 个中文标签。用最常见的说法（"猫"，不要"猫科动物"），每个 1~6 个字，以名词为主
   （物体、场景、活动、事件、动物、食物、地标类型）；不要动词短语，不要斜杠，
   不要"照片""图片""画面"这类词，不要用人名作标签。
4. keywords：3~10 个检索用的同义词或上位词，帮助换个说法也能搜到
   （例如 猫 → 猫咪、宠物；婚礼 → 婚纱照、新娘）。不要和 tags 重复。
5. people_count：画面中清晰可见的人数（没有人写 0，超过 10 写 10）。
6. visible_text：画面里最主要的文字，不超过 40 字。证件号、卡号、手机号、姓名、住址等个人信息
   不要抄录，只写类别，例如"身份证号码"。没有文字写空字符串。
7. sensitive：是否包含证件、银行卡、病历、合同票据等个人信息，或裸露、私密的画面。
只输出 JSON。"""

SCHEMA = {
    "type": "object",
    "properties": {
        "description": {"type": "string"},
        "categories": {
            "type": "array",
            "items": {"type": "string", "enum": CATEGORIES},
            "minItems": 1,
            "maxItems": 2,
        },
        "tags": {"type": "array", "items": {"type": "string"}, "minItems": 3, "maxItems": 15},
        "keywords": {"type": "array", "items": {"type": "string"}, "maxItems": 10},
        "people_count": {"type": "integer"},
        "visible_text": {"type": "string"},
        "sensitive": {"type": "boolean"},
    },
    "required": [
        "description", "categories", "tags", "keywords", "people_count", "visible_text", "sensitive",
    ],
}

WEEKDAYS = "一二三四五六日"


def user_text(taken_at: int | None) -> str:
    """只给拍摄时间作参考（同 memora：不给地点，避免模型被地点带偏）。taken_at 是本地时间的 Unix 秒。"""
    if taken_at:
        try:
            when = datetime.fromtimestamp(int(taken_at))
        except (OverflowError, OSError, ValueError):
            when = None
        if when is not None:
            text = f"{when.year}年{when.month}月{when.day}日 {when:%H:%M}（星期{WEEKDAYS[when.weekday()]}）"
            return f"参考信息（来自文件元数据，可能不准确）：\n- 拍摄时间：{text}\n请标注这张照片。"
    return "参考信息：无\n请标注这张照片。"
