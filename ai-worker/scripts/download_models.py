from __future__ import annotations

import argparse
import os
from pathlib import Path


def default_model_dir() -> Path:
    local_app_data = os.environ.get("LOCALAPPDATA")
    if local_app_data:
        return Path(local_app_data) / "PhotoViewPlus" / "models"
    return Path.home() / ".photo-view-plus" / "models"


def main() -> None:
    parser = argparse.ArgumentParser(description="Download PhotoView+ model snapshots")
    parser.add_argument("model_key", choices=["clip-vit-b-32", "ram-plus", "siglip-so400m"])
    parser.add_argument("--dest", type=Path, default=default_model_dir())
    args = parser.parse_args()

    try:
        from huggingface_hub import snapshot_download
    except ImportError as exc:
        raise SystemExit("Install the models extra first: uv sync --extra models") from exc

    import sys

    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
    from src.model_registry import model_manifest

    model = model_manifest()[args.model_key]
    target = args.dest / args.model_key
    target.mkdir(parents=True, exist_ok=True)
    snapshot_download(
        repo_id=model["hf_repo"],
        local_dir=target,
        local_dir_use_symlinks=False,
        resume_download=True,
    )
    print(target)


if __name__ == "__main__":
    main()
