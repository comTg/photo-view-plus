# 08 · 路线图

> 项目从 0 → 1.0 的整体规划。本文档随开发推进**持续更新**：里程碑勾选、风险关闭、ADR 追加。

## 1. 里程碑

| 里程碑 | 范围 | 发布判据 | 状态 | 计划周 |
|--------|------|---------|------|--------|
| **M0** | 文档与规则（本批 10 份） | 全部 docs 写完并审过 | 进行中 | 2026-W24 |
| **M1** | MVP1 基础浏览 | `docs/03` § 0 全部验收过 | 待开始 | W24-W26（约 2-3 周） |
| **M2** | MVP2 去重 | `docs/04` § 0 全部验收过 | 待开始 | W27-W28（约 2 周） |
| **M3** | MVP3 AI 与搜索 | `docs/05` § 0 全部验收过 | 待开始 | W29-W33（约 4-5 周） |
| **M4** | MVP4 高级管理 | `docs/06` § 0 全部验收过 | 待开始 | W34-W37（约 3-4 周） |
| **M5** | 1.0 RC | 性能基准达标、安装包签名、用户文档 | 待开始 | W38 |

> 周数为 ISO 周；时间预估为单人全职粗估，按"全职 1 人 + 没大坑"算。AI 阶段坑多，留缓冲。

## 2. 任务进度（速查）

每个 MVP 的具体任务清单见对应文档，本节只勾里程碑级状态。

### M0 文档
- [ ] CLAUDE.md
- [ ] README.md
- [ ] docs/00-architecture.md
- [ ] docs/01-data-model.md
- [ ] docs/02-ui-design.md
- [ ] docs/03-mvp1-browse.md
- [ ] docs/04-mvp2-dedup.md
- [ ] docs/05-mvp3-ai-search.md
- [ ] docs/06-mvp4-advanced.md
- [ ] docs/07-windows-platform.md
- [ ] docs/08-roadmap.md（本文档）

> 完成后由用户复查，全部签字才进入 M1。

### M1 MVP1（详见 `docs/03` § 2）
- [ ] T1-T16 共 16 个任务

### M2 MVP2（详见 `docs/04` § 2）
- [ ] T1-T14 共 14 个任务

### M3 MVP3（详见 `docs/05` § 2）
- [ ] T1-T16 共 16 个任务

### M4 MVP4（详见 `docs/06` § 2）
- [ ] T1-T13 共 13 个任务

### M5 1.0 RC
- [ ] 性能基准（见 § 4）全部达标
- [ ] 代码签名证书购买并签名
- [ ] 用户手册（替换 README placeholder）
- [ ] 已知问题清单
- [ ] 安装包 MSI 在干净 Windows 11 上完整跑通

## 3. 风险表

按概率 × 影响排序。每条 owner = "项目负责人"（即用户）。

| 风险 | 概率 | 影响 | 缓解 | 状态 |
|------|------|------|------|------|
| RTX 5070 sm_120 在 PyTorch stable 不支持 | 中 | 高 | 用 nightly / 临时回退 sm_89；监控 PyTorch 2.6 发布 | 待观察 |
| HEIC 解码集成（libheif）卡壳 | 中 | 中 | MVP1 内可标 `unsupported` 暂留；libheif PoC 提前在 W23 跑 | 待 PoC |
| 100 万图扫描性能不达标 | 中 | 高 | MVP1 完成后用 fixture 集做基准；不达标时拆分扫描器 | 未测 |
| 网络盘哈希耗带宽 | 高 | 中 | 默认跳过 NAS 哈希；设置中允许打开 | 已规划 |
| 人脸聚类精度不稳 | 高 | 低 | 可手动合并/拆分；不影响主流程 | 已规划 |
| AI Worker 内存泄漏（长跑后 OOM） | 中 | 中 | 闲置自动重启；运行 24h+ 看 RSS | 未测 |
| Windows 杀软误报 | 中 | 中 | 申请代码签名证书；放白名单文档 | 待 1.0 前 |
| LanceDB 在 Windows 上行为差异 | 低 | 中 | 早期 PoC 验证；备选 sqlite-vec | 未测 |
| SQLite WAL 文件膨胀 | 中 | 低 | 定期 wal_checkpoint | 已规划 |
| 用户误删图未察觉 | 中 | 高 | 全走回收站；30 天 undo；导出报告 | 已规划 |

每条风险关闭时改为 ✅ + 关闭日期。

## 4. 性能基准

测试机假设：Intel i7-13700K + RTX 5070 + 32GB DDR5 + NVMe SSD。  
基准数据集：5 万张混合（JPG/PNG/HEIC），平均 4MB / 张。

| 指标 | MVP1 目标 | MVP2 目标 | MVP3 目标 | 实际 |
|------|-----------|-----------|-----------|------|
| 首次扫描 5 万图 | < 60s | - | - | - |
| 二次扫描（无变化） | < 5s | - | - | - |
| 缩略图 1 万张 | < 90s | - | - | - |
| 主页瀑布流首屏 | < 1.5s | - | - | - |
| 瀑布流滚动 FPS | ≥ 55 | ≥ 55 | ≥ 55 | - |
| BLAKE3 1000 张 | - | < 60s | - | - |
| pHash 1 万张 | - | < 120s | - | - |
| pHash top-100 检索 | - | < 200ms | - | - |
| 完全重复检出率 | - | 100% | - | - |
| 视觉近似 F1（fixture） | - | ≥ 0.92 | - | - |
| CLIP 1000 张 embed | - | - | < 30s | - |
| 文本搜 top-20 命中率 | - | - | ≥ 0.85 | - |
| LanceDB 10 万向量查询 | - | - | < 100ms | - |
| 内存峰值（5 万图 + AI 关） | < 800MB | < 1GB | - | - |
| 内存峰值（10 万图 + AI 开） | - | - | < 4GB | - |

完成对应 MVP 时在"实际"列填数据。

## 5. ADR（架构决策记录）

> Architecture Decision Records，做一条改动就追加。格式：编号 / 标题 / 状态 / 时间 / 上下文 / 决定 / 后果。

### ADR-001 · 选 Tauri 2 而非 Electron
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：用户对前两者均不陌生，需在内存/体积与生态成熟度间权衡
- 决定：Tauri 2 + Rust + Vite + React 19（最初定 Next.js 15 SSG，MVP1 T1 实施时改为 Vite——桌面 SPA 不需要 SSR/SSG，Vite 启动快、依赖轻）
- 后果：开发节奏略慢于 Electron；运行时优势明显；与同栈的 smart-search 共享经验

### ADR-002 · 选 LanceDB 而非 sqlite-vec
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：50 万向量后查询性能差异显著
- 决定：LanceDB
- 后果：多一个本地数据存储；备份/恢复需要包含两个目录

### ADR-003 · Python AI Worker 独立子进程
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：Rust 直接调 ONNX Runtime 也能跑 CLIP，但模型生态在 Python 侧成熟
- 决定：Python FastAPI worker；通过 127.0.0.1 HTTP 通信
- 后果：多一个进程；多了进程托管复杂度；获得 Python 生态的全部 AI 模型

### ADR-004 · 删除一律走回收站
- 状态：已决（M0）
- 时间：2026-06-15
- 上下文：本地图库删错代价高
- 决定：禁用永久删除；保留 30 天 undo 窗口
- 后果：回收站可能满；增加 undo_log 表与撤销 UI

### ADR-005 · 用 `image_hasher` crate，借鉴 czkawka 但不依赖 czkawka_core
- 状态：已决（M0）
- 时间：2026-06-16
- 上下文：MVP2 需要 pHash / dHash 实现。候选：(a) 用 `rustdct` 自实现；(b) 直接 depend on `czkawka_core`（多媒体去重工具，11.0.1, MIT, 2026-02 仍维护）；(c) 单独用 czkawka 依赖的 `image_hasher` crate
- 决定：选 (c)。`image_hasher` 是 czkawka 的算法核心、独立发布在 crates.io，自带 pHash/dHash/aHash/Wavelet/Blockhash，仅依赖 `image` + `transpose`，无额外负担。在 `docs/04-mvp2-dedup.md` T3 直接调用
- 不选 (a) 的理由：DCT 是数学题不是工程题，自实现没有任何业务价值；`image_hasher` 在 czkawka 上有百万级图库实战验证
- 不选 (b) 的理由：
  - czkawka_core 还带音频（symphonia/lofty）、PDF（lopdf）、视频（similario_core）、压缩档（zip/7z/tar）等我们一律用不到的模块，二进制 +10-30MB，编译时间显著拉长
  - 它的 API 是"自带扫描器 + 输出报告"流程，假设你给它一组目录，并发模型自己说了算，无法插入我们的优先级队列（P0 缩略图 / P2 BLAKE3 / P3 pHash）
  - `FileEntry` 与我们的 `images.id` 表错位，要写一层 adapter，得不偿失
- 借鉴方式：把 czkawka 当**参考实现**，从 `czkawka_core/src/tools/similar_images.rs` 学习阈值默认值、多 hash size 取舍、特殊格式（RAW/JXL/动图）跳过策略。MIT 允许直接抄代码（保留 license 头），但具体逻辑短，我们重写更清爽——抄思路不抄实现
- 后果：MVP2 T3 工作量从"写 DCT + 测对齐 czkawka"减到"配 `HasherConfig` + 喂缩略图"；默认参数与 czkawka 一致（**dHash/Gradient, hash_size=8, Lanczos3, 不启用 DCT**），可以直接对照它的用户反馈调优。具体阈值表见 `docs/04` T3 速查（czkawka 的 `SIMILAR_VALUES` 完整照搬）

后续每个 MVP 完成时追加新的 ADR：
- ADR-006 · MVP1 完成总结与遗留事项（待填）
- ADR-007 · MVP2 pHash 阈值选定依据（待填）
- ADR-008 · MVP3 CLIP vs SigLIP 选型（待填）
- ADR-009 · MVP4 人脸聚类算法对比（待填）

## 6. 未决议题（讨论后再写 ADR）

- 是否支持 RAW（.cr3 / .arw / .nef）？解码慢 + 库依赖（libraw）→ 1.0 之后看用户反馈
- 视频缩略图（.mp4 / .mov）？需引入 ffmpeg-rs → 单列一个 MVP5
- 云备份（家庭场景）？默认本地优先，但有人会要 → 1.0 之后看
- iCloud / OneDrive 在线占位文件如何处理？读取会触发下载，扫描时如何识别"占位"→ MVP1 调研

## 7. 发布渠道

| 渠道 | 准备 | 状态 |
|------|------|------|
| GitHub Releases | MVP1 完成后开始放 nightly | 未开 |
| 官网下载（如建站） | 1.0 时再说 | 未开 |
| Microsoft Store | 1.0 之后可选 | 未开 |
| WinGet | 1.0 之后 | 未开 |

## 8. 不做的事

- 多语言文字界面（仅中英两套）：在 MVP3 之后；MVP1/2 默认中文
- 移动端：永不
- Web 在线版：永不（本地优先）
- 多用户/多账户：永不
- 内嵌 LLM 对话："和图库聊天"暂不在范围（如做单列 MVP6）

## 9. 关键日期

| 日期 | 事件 |
|------|------|
| 2026-06-15 | 项目启动（M0 开始） |
| TBD | M0 文档审完 |
| TBD | MVP1 验收 |
| TBD | MVP2 验收 |
| TBD | MVP3 验收 |
| TBD | MVP4 验收 |
| TBD | 1.0 发布 |

完成时填实际日期，便于复盘节奏。
