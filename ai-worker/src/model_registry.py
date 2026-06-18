from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any

WORKER_ROOT = Path(__file__).resolve().parents[1]
MANIFEST_PATH = WORKER_ROOT / "models.json"
BERT_TOKENIZER_REPO = "bert-base-uncased"
BERT_TOKENIZER_FILES = [
    "config.json",
    "tokenizer_config.json",
    "tokenizer.json",
    "vocab.txt",
    "special_tokens_map.json",
    "added_tokens.json",
]


def default_model_dir() -> Path:
    env_dir = os.environ.get("PVP_MODEL_DIR")
    if env_dir:
        return Path(env_dir)
    local_app_data = os.environ.get("LOCALAPPDATA")
    if local_app_data:
        return Path(local_app_data) / "PhotoViewPlus" / "models"
    return Path.home() / ".photo-view-plus" / "models"


def model_manifest() -> dict[str, dict[str, Any]]:
    return json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))


def model_path(model_key: str) -> Path:
    return default_model_dir() / model_key


def model_inventory() -> dict[str, dict[str, Any]]:
    manifest = model_manifest()
    inventory: dict[str, dict[str, Any]] = {}
    for key, item in manifest.items():
        path = model_path(key)
        repo = str(item.get("hf_repo", ""))
        size_mb = item.get("size_mb")
        inventory[key] = {
            **item,
            "sizeMb": size_mb,
            "downloaded": (path.exists() and any(path.iterdir())) or hf_cache_exists(repo),
            "loaded": False,
            "path": str(path),
        }
        inventory[key].pop("size_mb", None)
    return inventory


def hf_cache_exists(repo_id: str) -> bool:
    if not repo_id or "/" not in repo_id:
        return False
    cache_home = os.environ.get("HF_HOME")
    base = Path(cache_home) / "hub" if cache_home else Path.home() / ".cache" / "huggingface" / "hub"
    return (base / f"models--{repo_id.replace('/', '--')}").exists()


def download_model(model_key: str) -> Path:
    manifest = model_manifest()
    if model_key not in manifest:
        raise ValueError(f"unknown model: {model_key}")
    from huggingface_hub import snapshot_download

    target = model_path(model_key)
    target.mkdir(parents=True, exist_ok=True)
    snapshot_download(
        repo_id=manifest[model_key]["hf_repo"],
        local_dir=target,
        local_dir_use_symlinks=False,
        resume_download=True,
    )
    if model_key == "ram-plus":
        download_bert_tokenizer(target / BERT_TOKENIZER_REPO)
    return target


def download_bert_tokenizer(target: Path) -> Path:
    from huggingface_hub import snapshot_download

    target.mkdir(parents=True, exist_ok=True)
    snapshot_download(
        repo_id=BERT_TOKENIZER_REPO,
        local_dir=target,
        local_dir_use_symlinks=False,
        resume_download=True,
        allow_patterns=BERT_TOKENIZER_FILES,
    )
    return target
