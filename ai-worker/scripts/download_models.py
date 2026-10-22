"""按需下载模型到 %LOCALAPPDATA%\\PhotoViewPlus\\models（CLAUDE.md 红线 7：权重不进 git）。

用法：pnpm ai:download chinese-clip-vit-b-16
来源顺序：ModelScope → HF_ENDPOINT（未设置时依次试 huggingface.co、hf-mirror.com）。
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from src.model_registry import download_model, model_manifest  # noqa: E402


def main() -> None:
    manifest = model_manifest()
    parser = argparse.ArgumentParser(description="Download PhotoView+ models")
    parser.add_argument("model_key", choices=sorted(manifest))
    parser.add_argument("--dest", type=Path, help="模型根目录（默认 PVP_MODEL_DIR 或 %%LOCALAPPDATA%%\\PhotoViewPlus\\models）")
    args = parser.parse_args()
    if args.dest:
        os.environ["PVP_MODEL_DIR"] = str(args.dest)
    print(download_model(args.model_key))


if __name__ == "__main__":
    main()
