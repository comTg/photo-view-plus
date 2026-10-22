# 搜索评测原型（docs/10 §9）

离线计算各个检索通道，池化后生成拼图做人工标注，再计算 nDCG@20、P@10 等指标。用 ai-worker 的虚拟环境运行（需要 torch、open_clip、transformers、Pillow）：

```powershell
$py = "ai-worker\.venv\Scripts\python.exe"
cd scripts\search_eval
& $py build_corpus.py          # 正式库只读快照 → 语料（按忽略大小写的路径去重，合并 RAM 标签和 memora 描述）
& $py embed.py clip_openai     # S0：线上现用的 OpenAI CLIP
& $py embed.py cnclip          # S1：Chinese-CLIP（模型放在 %LOCALAPPDATA%\PhotoViewPlus\models\chinese-clip-vit-base-patch16）
& $py embed.py bge             # S4：bge-small-zh-v1.5，对 memora 描述编码
& $py run_systems.py           # 各方案排名，含融合方案
& $py make_sheets.py           # 池化，只为没标过的图生成拼图
& $py label.py q05_1 3,7,12    # 记一张拼图的标注：列出的格子编号判为相关，其余判为不相关
& $py score.py                 # 计算指标，汇总写进 tests/search_eval/report.md
```

- **数据位置**：语料、向量、拼图、判定都在 `%LOCALAPPDATA%\PhotoViewPlus\eval\`，可用 `PVP_EVAL_DIR` 覆盖。这些文件含私人照片的路径，不进仓库。仓库里只放查询集 `tests/search_eval/queries.json` 和不含路径的汇总 `report.md`。
- **只读**：被评测的应用数据目录默认是正式版 profile（可用 `PVP_EVAL_SOURCE_DIR` 覆盖），只做只读快照，不会改动。
- **下载模型**：ModelScope 不可用时，用 hf-mirror.com 镜像，例如 `https://hf-mirror.com/OFA-Sys/chinese-clip-vit-base-patch16/resolve/main/pytorch_model.bin`。模型权重不进 git（CLAUDE.md 红线 7）。
- **加方案**：在 `run_systems.py` 里加一路排名或融合，然后重新执行 `make_sheets.py`，只需补标新出现的图。
