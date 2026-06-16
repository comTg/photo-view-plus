# photo-view-plus

本地优先的 Windows 图片资产管理工具：扫描目录 → 缩略图浏览 → 去重 → AI 标签 → 自然语言搜图。  
为本地大图库（家庭照片、素材库、NAS）设计，所有计算在本机完成，不上传任何图片。

> 目标平台：**Windows 10/11，配 NVIDIA GPU（推荐 RTX 50 系列）**。开发可跨平台，运行时 AI 能力依赖 CUDA。

## 截图

> _UI 设计稿见 `/Volumes/HK 1/vetoer/code/mygit/my-doc/assets/Pasted image 20260616121202.png`，实物截图待 MVP1 上线后补。_

## 功能概览

| 阶段 | 能力 | 状态 |
|------|------|------|
| MVP1 | 多目录扫描 / 缩略图缓存 / 瀑布流浏览 / 基础筛选 | 规划中 |
| MVP2 | 完全重复（BLAKE3）+ 视觉近似（pHash）去重，批量移到回收站 | 规划中 |
| MVP3 | CLIP 向量库 / 自然语言搜图 / 自动标签 / 以图搜图 | 规划中 |
| MVP4 | OCR / 人脸聚类 / 时间轴 / 地图 EXIF / 智能相册 | 规划中 |

## 技术栈

- **桌面壳**：Tauri 2
- **前端**：Next.js 15（SSG 模式）+ React 19 + Radix UI + Tailwind + Zustand
- **后端**：Rust（src-tauri/）
- **存储**：SQLite（元数据/标签）+ LanceDB（向量）+ 本地 WebP 缩略图缓存
- **AI**：Python FastAPI Worker，PyTorch CUDA + ONNX Runtime CUDA，模型含 CLIP / RAM / PaddleOCR / InsightFace

## 快速上手（开发）

```bash
# 安装
pnpm install

# 启动
pnpm tauri dev
```

更多命令见 [`CLAUDE.md`](./CLAUDE.md#common-development-commands)。

## 文档

| 文档 | 内容 |
|------|------|
| [`CLAUDE.md`](./CLAUDE.md) | 项目规则、红线、开发命令 |
| [`docs/00-architecture.md`](./docs/00-architecture.md) | 总体架构 + 技术栈决策 |
| [`docs/01-data-model.md`](./docs/01-data-model.md) | 数据库表结构 + 向量库 + 缩略图缓存 |
| [`docs/02-ui-design.md`](./docs/02-ui-design.md) | UI 拆解 + 交互 + 状态管理 |
| [`docs/03-mvp1-browse.md`](./docs/03-mvp1-browse.md) | MVP1 任务清单：基础浏览 |
| [`docs/04-mvp2-dedup.md`](./docs/04-mvp2-dedup.md) | MVP2 任务清单：去重 |
| [`docs/05-mvp3-ai-search.md`](./docs/05-mvp3-ai-search.md) | MVP3 任务清单：AI 与语义搜索 |
| [`docs/06-mvp4-advanced.md`](./docs/06-mvp4-advanced.md) | MVP4 任务清单：高级管理 |
| [`docs/07-windows-platform.md`](./docs/07-windows-platform.md) | Windows 平台关键事项 |
| [`docs/08-roadmap.md`](./docs/08-roadmap.md) | 里程碑 + 风险 + ADR |

## 设计原则

1. **本地优先**：所有数据、模型、缓存留在本机；网络只用于按需下载模型。
2. **不破坏原文件**：去重默认走回收站，提供撤销与导出清单。
3. **响应优先**：UI 不为任何后台任务等待；扫描 / pHash / AI 全部异步队列。
4. **分阶段可用**：每个 MVP 自身可独立交付，不依赖未来阶段。

## License

未确定。
