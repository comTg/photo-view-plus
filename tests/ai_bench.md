# MVP3 AI Bench

Date: 2026-06-17
Machine: Windows, NVIDIA GeForce RTX 5070

## Runtime

`pnpm ai:check`

- torch: `2.11.0+cu128`
- CUDA available: `true`
- CUDA version: `12.8`
- device: `NVIDIA GeForce RTX 5070`
- compute capability: `12.0`
- VRAM: `10.76 / 11.94 GB` free at check time
- onnxruntime-gpu: `1.27.0`
- open_clip_torch: `3.3.0`
- clip-vit-b-32: downloaded in HuggingFace cache, open_clip smoke `fallback=false`

## Worker Smoke

Endpoint smoke with one generated PNG thumbnail:

- `/health`: ok
- `/diagnostics`: cuda
- `/clip/embed`: 1 item, 512 dims, `fallback=false`
- `/tagger/run`: 1 item, 11 tags, `fallback=false`

## Pending Manual Bench

- 1000 image CLIP embedding throughput on the real thumbnail cache.
- 50 query top-20 hit-rate on the fixture query set.
- 5 万图库 end-to-end embedding/tagging duration.
- LanceDB 10 万 vector top-100 latency.
