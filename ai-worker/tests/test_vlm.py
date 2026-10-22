"""docs/10 A6：VLM 输出规范化、Schema、输入图准备（含 HEIC）、/vlm/analyze 的错误分类。"""

from __future__ import annotations

import io
import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import pytest
from PIL import Image

from src.api.vlm import analyze_bytes
from src.vlm import images
from src.vlm.normalize import normalize, synonym_map, tag_score
from src.vlm.prompts import CATEGORIES, PROMPT_VERSION, SCHEMA, user_text


def raw_result(**overrides):
    base = {
        "description": "一张照片，一只橘猫趴在窗台上晒太阳。",
        "categories": ["宠物"],
        "tags": ["猫", "窗台", "阳光"],
        "keywords": ["宠物"],
        "people_count": 0,
        "visible_text": "",
        "sensitive": False,
    }
    base.update(overrides)
    return base


def test_tags_are_split_cleaned_and_mapped_to_synonyms():
    result = normalize(
        raw_result(tags=["猫/猫咪", "照片", "小猫", "草地、阳光", "「窗台」", "一只非常非常可爱的小橘猫咪"])
    )
    names = [tag.name for tag in result.tags]
    assert names == ["猫", "草地", "阳光", "窗台"]
    # 归一掉的原词进 keywords，模型给的 keywords 排前面。
    assert result.keywords == ["宠物", "猫咪", "小猫"]


def test_tag_scores_follow_order_and_cap():
    many = [f"标签{i}" for i in range(30)]
    result = normalize(raw_result(tags=many))
    assert len(result.tags) == 15
    assert result.tags[0].score == 0.95
    assert result.tags[1].score == 0.93
    assert tag_score(40) == 0.6


def test_keywords_skip_tags_and_duplicates():
    result = normalize(raw_result(tags=["婚礼", "新娘"], keywords=["婚礼", "婚纱照", "婚纱照", "新娘", "喜宴"]))
    assert result.keywords == ["婚纱照", "喜宴"]


def test_categories_people_text_are_clamped():
    result = normalize(
        raw_result(
            categories=["宠物", "不存在的类别", "宠物", "动物", "风景"],
            people_count=25,
            visible_text="  欢迎光临  " + "字" * 60,
            sensitive="false",
        )
    )
    assert result.categories == ["宠物", "动物"]
    assert result.people_count == 10
    assert len(result.visible_text) == 40
    assert result.visible_text.startswith("欢迎光临")
    assert result.sensitive is False
    assert normalize(raw_result(sensitive="true")).sensitive is True
    assert normalize(raw_result(categories=[], people_count="很多")).categories == ["其他"]
    assert normalize(raw_result(people_count=-2)).people_count == 0


def test_color_tags_move_to_keywords():
    result = normalize(raw_result(tags=["婴儿", "黄色", "玩具", "白色"], keywords=["宝宝"]))
    assert [tag.name for tag in result.tags] == ["婴儿", "玩具"]
    assert result.keywords == ["宝宝", "黄色", "白色"]


def test_long_numbers_are_redacted_but_dates_kept():
    result = normalize(
        raw_result(
            visible_text="结婚证字号 J510122-2021-004728 登记日期 2021-12-03",
            description="手机号 138 0013 8000，取件码 1-3-6005，卡号 6222021234567890123。",
        )
    )
    assert "004728" not in result.visible_text
    assert "＊＊＊" in result.visible_text
    assert "2021-12-03" in result.visible_text
    assert "13800138000" not in result.description.replace(" ", "")
    assert "6222021234567890123" not in result.description
    assert "1-3-6005" in result.description


def test_sensitive_images_keep_no_visible_text():
    result = normalize(
        raw_result(
            categories=["文档票据"],
            tags=["身份证", "证件照"],
            visible_text="姓名 张三 性别 女 出生日期 1996年12月13日 住址 成都市",
            sensitive=True,
        )
    )
    assert result.sensitive is True
    assert result.visible_text == ""
    assert [tag.name for tag in result.tags] == ["身份证", "证件照"]


def test_raw_json_is_kept():
    data = normalize(raw_result()).to_dict()
    assert json.loads(data["raw_json"])["tags"] == ["猫", "窗台", "阳光"]
    assert data["tags"][0] == {"name": "猫", "score": 0.95}


def test_schema_matches_prompt_categories():
    assert SCHEMA["properties"]["categories"]["items"]["enum"] == CATEGORIES
    assert set(SCHEMA["required"]) == set(SCHEMA["properties"])
    assert PROMPT_VERSION == "search-v1"
    mapping = synonym_map()
    assert mapping["猫咪"] == "猫"
    # 规范说法自己不能又是别人的同义词（否则归一会链式跳转）。
    assert not set(mapping.values()) & set(mapping)


def test_user_text_only_mentions_time():
    assert "参考信息：无" in user_text(None)
    text = user_text(1714545000)  # 2024-05-01 前后（本地时区）
    assert "拍摄时间：2024年" in text
    assert "星期" in text


def jpeg_bytes(size, orientation=None):
    img = Image.new("RGB", size, (200, 30, 30))
    buffer = io.BytesIO()
    if orientation:
        exif = Image.Exif()
        exif[0x0112] = orientation
        img.save(buffer, "JPEG", exif=exif.tobytes())
    else:
        img.save(buffer, "JPEG")
    return buffer.getvalue()


def test_prepare_applies_exif_orientation_and_long_edge():
    data = jpeg_bytes((3000, 2000), orientation=6)  # 旋转 90°：横图变竖图
    out = Image.open(io.BytesIO(images.prepare(data, 1024)))
    assert out.format == "JPEG"
    assert max(out.size) == 1024
    assert out.height > out.width


def test_prepare_flattens_alpha_on_white():
    img = Image.new("RGBA", (64, 32), (0, 0, 0, 0))
    buffer = io.BytesIO()
    img.save(buffer, "PNG")
    out = Image.open(io.BytesIO(images.prepare(buffer.getvalue(), 1024)))
    assert out.mode == "RGB"
    assert out.getpixel((10, 10))[0] > 240


def test_prepare_reads_heic():
    pillow_heif = pytest.importorskip("pillow_heif")
    img = Image.new("RGB", (1600, 1200), (20, 120, 220))
    buffer = io.BytesIO()
    try:
        pillow_heif.from_pillow(img).save(buffer, format="HEIF", quality=80)
    except Exception as exc:  # noqa: BLE001 - 没有 HEVC 编码器的环境
        pytest.skip(f"HEIF encoder unavailable: {exc}")
    out = Image.open(io.BytesIO(images.prepare(buffer.getvalue(), 1024)))
    assert out.size == (1024, 768)


class FakeLlama(BaseHTTPRequestHandler):
    responses: list[tuple[int, str]] = []
    requests: list[dict] = []

    def do_POST(self):  # noqa: N802 - http.server 约定
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        FakeLlama.requests.append(body)
        status, content = FakeLlama.responses.pop(0)
        if status != 200:
            payload = json.dumps({"error": {"message": content}}).encode()
        else:
            payload = json.dumps(
                {"choices": [{"message": {"content": content}}], "timings": {"prompt_n": 812, "predicted_n": 190}}
            ).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args):
        pass


@pytest.fixture
def fake_llama():
    server = HTTPServer(("127.0.0.1", 0), FakeLlama)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    FakeLlama.responses = []
    FakeLlama.requests = []
    yield f"http://127.0.0.1:{server.server_address[1]}"
    server.shutdown()


def test_analyze_retries_truncated_json(fake_llama):
    FakeLlama.responses = [(200, '{"description": "一只'), (200, json.dumps(raw_result(), ensure_ascii=False))]
    result = analyze_bytes(jpeg_bytes((800, 600)), server_url=fake_llama, long_edge=1024, taken_at=None)
    assert result["ok"] is True
    assert result["requests"] == 2
    assert result["result"]["tags"][0]["name"] == "猫"
    assert result["prompt_tokens"] == 812
    request = FakeLlama.requests[0]
    assert request["temperature"] == 0.2
    assert request["response_format"]["type"] == "json_schema"
    assert request["messages"][1]["content"][0]["image_url"]["url"].startswith("data:image/jpeg;base64,")


def test_analyze_error_kinds(fake_llama):
    garbage = analyze_bytes(b"not an image", server_url=fake_llama, long_edge=1024, taken_at=None)
    assert garbage["ok"] is False and garbage["error_kind"] == "decode"

    FakeLlama.responses = [(503, "Loading model")]
    loading = analyze_bytes(jpeg_bytes((80, 60)), server_url=fake_llama, long_edge=1024, taken_at=None)
    assert loading["error_kind"] == "server"

    FakeLlama.responses = [(400, "the request exceeds the available context size")]
    too_long = analyze_bytes(jpeg_bytes((80, 60)), server_url=fake_llama, long_edge=1024, taken_at=None)
    assert too_long["error_kind"] == "model"

    FakeLlama.responses = [(200, "{"), (200, "{"), (200, "{")]
    broken = analyze_bytes(jpeg_bytes((80, 60)), server_url=fake_llama, long_edge=1024, taken_at=None)
    assert broken["error_kind"] == "model"


def test_analyze_server_down():
    result = analyze_bytes(
        jpeg_bytes((80, 60)), server_url="http://127.0.0.1:9", long_edge=1024, taken_at=None, timeout=2
    )
    assert result["ok"] is False and result["error_kind"] == "server"
