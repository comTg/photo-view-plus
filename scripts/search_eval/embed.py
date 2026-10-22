"""离线计算评测用的向量（docs/10 §9 的 S0 / S1 / S4 通道）。

用法（用 ai-worker 的虚拟环境）：
  python scripts/search_eval/embed.py clip_openai   # S0：open_clip ViT-B-32 + OpenAI 权重（与线上一致）
  python scripts/search_eval/embed.py cnclip        # S1：Chinese-CLIP ViT-B/16
  python scripts/search_eval/embed.py bge           # S4：bge-small-zh-v1.5，对 memora 描述编码

图像向量只对"本应用已有缩略图"的图计算，和线上覆盖范围一致。
模型目录：%LOCALAPPDATA%\\PhotoViewPlus\\models\\<name>（PVP_MODEL_DIR 可覆盖）。
"""

from __future__ import annotations

import json
import os
import sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import numpy as np
import torch
from PIL import Image

from common import eval_dir, load_corpus, load_queries

BATCH = 64
BGE_QUERY_PREFIX = "为这个句子生成表示以用于检索相关文章："


def models_dir() -> Path:
    env = os.environ.get("PVP_MODEL_DIR")
    return Path(env) if env else Path(os.environ["LOCALAPPDATA"]) / "PhotoViewPlus" / "models"


def device() -> str:
    return "cuda" if torch.cuda.is_available() else "cpu"


def normalize(x: torch.Tensor) -> np.ndarray:
    x = x.float()
    x = x / x.norm(dim=-1, keepdim=True).clamp_min(1e-12)
    return x.detach().cpu().numpy().astype(np.float32)


def load_rgb(path: str) -> Image.Image | None:
    try:
        with Image.open(path) as img:
            return img.convert("RGB")
    except Exception:
        return None


def image_items() -> list[dict]:
    return [item for item in load_corpus() if item["thumb"] and Path(item["thumb"]).exists()]


def save(name: str, vectors: np.ndarray, keys: list[str]) -> None:
    out = eval_dir() / "emb"
    out.mkdir(parents=True, exist_ok=True)
    np.save(out / f"{name}.npy", vectors)
    (out / f"{name}.keys.json").write_text(json.dumps(keys, ensure_ascii=False), encoding="utf-8")
    print(f"saved {name}: {vectors.shape}", flush=True)


def embed_images(encode_batch, name: str) -> None:
    items = image_items()
    keys, chunks = [], []
    with ThreadPoolExecutor(max_workers=4) as pool:
        for start in range(0, len(items), BATCH):
            batch = items[start : start + BATCH]
            images = list(pool.map(lambda it: load_rgb(it["thumb"]), batch))
            pairs = [(it["key"], img) for it, img in zip(batch, images) if img is not None]
            if not pairs:
                continue
            with torch.no_grad():
                chunks.append(encode_batch([img for _, img in pairs]))
            keys.extend(key for key, _ in pairs)
            if (start // BATCH) % 20 == 0:
                print(f"  {name}: {start + len(batch)}/{len(items)}", flush=True)
    save(name, np.concatenate(chunks), keys)


def run_clip_openai() -> None:
    import open_clip

    dev = device()
    model, _, preprocess = open_clip.create_model_and_transforms("ViT-B-32", pretrained="openai", device=dev)
    model.eval()
    tokenizer = open_clip.get_tokenizer("ViT-B-32")

    def encode(images):
        tensor = torch.stack([preprocess(img) for img in images]).to(dev)
        return normalize(model.encode_image(tensor))

    embed_images(encode, "clip_openai_img")
    queries = load_queries()
    with torch.no_grad():
        q = normalize(model.encode_text(tokenizer([x["text"] for x in queries]).to(dev)))
    save("clip_openai_query", q, [x["id"] for x in queries])


def run_cnclip() -> None:
    from transformers import ChineseCLIPModel, ChineseCLIPProcessor

    path = models_dir() / "chinese-clip-vit-base-patch16"
    dev = device()
    model = ChineseCLIPModel.from_pretrained(path).to(dev).eval()
    processor = ChineseCLIPProcessor.from_pretrained(path)

    # 不用 get_image_features / get_text_features：当前 transformers 版本里它们取到的 pooled 输出为空，
    # 这里按 Chinese-CLIP 的定义手动取 [CLS] 再过投影层。
    def encode(images):
        pixels = processor(images=images, return_tensors="pt")["pixel_values"].to(dev)
        out = model.vision_model(pixel_values=pixels)
        pooled = out.pooler_output if out.pooler_output is not None else out.last_hidden_state[:, 0, :]
        return normalize(model.visual_projection(pooled))

    embed_images(encode, "cnclip_img")
    queries = load_queries()
    with torch.no_grad():
        inputs = processor(text=[x["text"] for x in queries], padding=True, return_tensors="pt").to(dev)
        out = model.text_model(**{k: v for k, v in inputs.items() if k != "pixel_values"})
        q = normalize(model.text_projection(out.last_hidden_state[:, 0, :]))
    save("cnclip_query", q, [x["id"] for x in queries])


def run_bge() -> None:
    from transformers import AutoModel, AutoTokenizer

    path = models_dir() / "bge-small-zh-v1.5"
    dev = device()
    tokenizer = AutoTokenizer.from_pretrained(path)
    model = AutoModel.from_pretrained(path).to(dev).eval()

    def encode(texts: list[str]) -> np.ndarray:
        enc = tokenizer(texts, padding=True, truncation=True, max_length=256, return_tensors="pt").to(dev)
        with torch.no_grad():
            return normalize(model(**enc).last_hidden_state[:, 0])

    items = [item for item in load_corpus() if item["desc"]]
    chunks = [encode([it["desc"] for it in items[i : i + 128]]) for i in range(0, len(items), 128)]
    save("bge_desc", np.concatenate(chunks), [it["key"] for it in items])
    queries = load_queries()
    save("bge_query", encode([BGE_QUERY_PREFIX + x["text"] for x in queries]), [x["id"] for x in queries])


def main() -> None:
    torch.set_num_threads(4)  # 别把 CPU 占满：用户可能正在用电脑
    which = sys.argv[1] if len(sys.argv) > 1 else ""
    runners = {"clip_openai": run_clip_openai, "cnclip": run_cnclip, "bge": run_bge}
    if which not in runners:
        raise SystemExit(f"usage: embed.py {{{'|'.join(runners)}}}")
    runners[which]()


if __name__ == "__main__":
    main()
