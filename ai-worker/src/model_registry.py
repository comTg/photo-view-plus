from __future__ import annotations

import json
import os
import shutil
import urllib.request
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
# 本机访问 huggingface.co 只有约 10KB/s（docs/10 §5.1）。没设 HF_ENDPOINT 时依次试官方地址和
# hf-mirror 镜像；ModelScope 写在 manifest 的 modelscope_repo 里，排在最前（它偶尔整站 504）。
HF_MIRROR = "https://hf-mirror.com"
DOWNLOAD_TIMEOUT = 60


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


def model_spec(model_key: str) -> dict[str, Any]:
    """manifest 里的一项；未知的模型返回空字典（调用方自己决定兜底）。"""
    return model_manifest().get(model_key, {})


def model_path(model_key: str) -> Path:
    """模型目录：manifest 里写了 dir 就用它（沿用评测时下好的目录名），否则用模型 key。"""
    spec = model_manifest().get(model_key, {})
    return default_model_dir() / str(spec.get("dir") or model_key)


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
            "downloaded": _is_downloaded(path, item) or hf_cache_exists(repo),
            "loaded": False,
            "path": str(path),
        }
        inventory[key].pop("size_mb", None)
    return inventory


def _is_downloaded(path: Path, spec: dict[str, Any]) -> bool:
    if not path.exists():
        return False
    files = spec.get("files")
    if files:
        return all((path / name).is_file() for name in files)
    return any(path.iterdir())


def hf_cache_exists(repo_id: str) -> bool:
    if not repo_id or "/" not in repo_id:
        return False
    cache_home = os.environ.get("HF_HOME")
    base = Path(cache_home) / "hub" if cache_home else Path.home() / ".cache" / "huggingface" / "hub"
    return (base / f"models--{repo_id.replace('/', '--')}").exists()


def download_sources(spec: dict[str, Any]) -> list[str]:
    """按顺序尝试的下载根地址（文件 URL = 根地址 + 文件名）。"""
    sources: list[str] = []
    ms_repo = spec.get("modelscope_repo")
    if ms_repo:
        sources.append(f"https://www.modelscope.cn/models/{ms_repo}/resolve/master/")
    hf_repo = spec.get("hf_repo")
    if hf_repo:
        endpoints = [os.environ["HF_ENDPOINT"]] if os.environ.get("HF_ENDPOINT") else ["https://huggingface.co", HF_MIRROR]
        sources.extend(f"{endpoint.rstrip('/')}/{hf_repo}/resolve/main/" for endpoint in endpoints)
    return sources


def download_model(model_key: str) -> Path:
    manifest = model_manifest()
    if model_key not in manifest:
        raise ValueError(f"unknown model: {model_key}")
    spec = manifest[model_key]
    target = model_path(model_key)
    target.mkdir(parents=True, exist_ok=True)
    if spec.get("files"):
        # 有明确文件清单的模型逐个文件下载，可以在 ModelScope / 官方 / 镜像之间切换
        for name in spec["files"]:
            if not (target / name).is_file():
                download_file([base + name for base in download_sources(spec)], target / name)
        return target

    from huggingface_hub import snapshot_download

    snapshot_download(
        repo_id=spec["hf_repo"],
        local_dir=target,
        local_dir_use_symlinks=False,
        resume_download=True,
    )
    if model_key == "ram-plus":
        download_bert_tokenizer(target / BERT_TOKENIZER_REPO)
    return target


def download_file(urls: list[str], dest: Path) -> Path:
    """依次尝试各个地址，写临时文件、完整下载后再改名，中途失败不会留下半个文件。"""
    errors: list[str] = []
    tmp = dest.with_name(dest.name + ".part")
    for url in urls:
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "photo-view-plus"})
            with urllib.request.urlopen(request, timeout=DOWNLOAD_TIMEOUT) as response, tmp.open("wb") as out:
                shutil.copyfileobj(response, out, length=1024 * 1024)
            os.replace(tmp, dest)
            return dest
        except Exception as exc:  # 换下一个源
            errors.append(f"{url}: {exc}")
            tmp.unlink(missing_ok=True)
    raise RuntimeError(f"下载 {dest.name} 失败：" + "；".join(errors))


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
