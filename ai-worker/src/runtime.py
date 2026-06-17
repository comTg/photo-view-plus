from __future__ import annotations

from dataclasses import dataclass
from importlib import metadata
from typing import Any

from .model_registry import model_inventory


@dataclass(frozen=True)
class RuntimeSummary:
    device: str
    compute_capability: str | None


def runtime_summary() -> RuntimeSummary:
    torch = _import_torch()
    if torch is None:
        return RuntimeSummary(device="cpu", compute_capability=None)

    try:
        if torch.cuda.is_available():
            capability = torch.cuda.get_device_capability()
            return RuntimeSummary(
                device="cuda",
                compute_capability=f"{capability[0]}.{capability[1]}",
            )
    except Exception:
        return RuntimeSummary(device="cpu", compute_capability=None)

    return RuntimeSummary(device="cpu", compute_capability=None)


def diagnostics() -> dict[str, Any]:
    torch = _import_torch()
    summary = runtime_summary()
    models = _model_inventory()
    if torch is None:
        return {
            "torchVersion": None,
            "cudaAvailable": False,
            "cudaVersion": None,
            "deviceName": None,
            "computeCapability": None,
            "vramTotalGb": None,
            "vramFreeGb": None,
            "device": summary.device,
            "models": models,
            "warnings": ["torch is not installed; install the cuda extra before running models"],
        }

    cuda_available = False
    device_name = None
    compute_capability = None
    vram_total_gb = None
    vram_free_gb = None
    warnings: list[str] = []

    try:
        cuda_available = bool(torch.cuda.is_available())
        if cuda_available:
            device_index = torch.cuda.current_device()
            device_name = torch.cuda.get_device_name(device_index)
            capability = torch.cuda.get_device_capability(device_index)
            compute_capability = f"{capability[0]}.{capability[1]}"
            free_bytes, total_bytes = torch.cuda.mem_get_info(device_index)
            vram_total_gb = round(total_bytes / 1024**3, 2)
            vram_free_gb = round(free_bytes / 1024**3, 2)
            if capability[0] >= 12 and _torch_version_major_minor(torch.__version__) < (2, 6):
                warnings.append("RTX 50 series requires PyTorch 2.6+ for full sm_120 support")
    except Exception as exc:
        warnings.append(f"cuda diagnostics failed: {exc}")

    return {
        "torchVersion": getattr(torch, "__version__", None),
        "cudaAvailable": cuda_available,
        "cudaVersion": getattr(getattr(torch, "version", None), "cuda", None),
        "deviceName": device_name,
        "computeCapability": compute_capability,
        "vramTotalGb": vram_total_gb,
        "vramFreeGb": vram_free_gb,
        "device": summary.device,
        "models": models,
        "warnings": warnings,
    }


def _import_torch() -> Any | None:
    try:
        import torch  # type: ignore

        return torch
    except Exception:
        return None


def _torch_version_major_minor(version: str) -> tuple[int, int]:
    prefix = version.split("+", 1)[0]
    parts = prefix.split(".")
    try:
        return (int(parts[0]), int(parts[1]))
    except (IndexError, ValueError):
        return (0, 0)


def _model_inventory() -> dict[str, dict[str, Any]]:
    return model_inventory()


def package_versions() -> dict[str, str | None]:
    names = ["fastapi", "uvicorn", "torch", "onnxruntime-gpu", "open_clip_torch"]
    versions: dict[str, str | None] = {}
    for name in names:
        try:
            versions[name] = metadata.version(name)
        except metadata.PackageNotFoundError:
            versions[name] = None
    return versions
