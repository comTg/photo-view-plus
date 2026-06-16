# 02 · UI 设计

> 基于 `/Volumes/HK 1/vetoer/code/mygit/my-doc/assets/Pasted image 20260616121202.png` 设计稿拆解。截图含 1 个主界面 + 4 个子界面缩略图。

## 1. 主界面（三栏布局）

```
┌─Logo──┬───────────────────搜索框───────────────────┬─工具栏──┐
│       │                                                       │
│ 左侧栏 │            中间瀑布流                    │  右侧详情 │
│ 240px │              flex 1                       │   320px   │
│       │                                           │           │
│       │                                           │           │
└───────┴───────────────────────────────────────────┴───────────┘
                          底部状态栏（任务进度）
```

### 1.1 左侧栏（导航）

设计稿里能看出的层级（自上而下）：

```
PhotoView                 ← App 名 / 设置入口

[搜索区]                   ← 顶部留出搜索/快捷入口（视稿）

我的相册                   ← 二级分组
  最近添加
  收藏
  截图（OCR 触发）

文件夹                     ← 用户添加的 roots
  D:\Photos
  E:\素材库
  \\nas\share

标签                       ← AI 标签 + 用户标签的树
  风景
  人物
  食物
  ...

智能相册                   ← MVP4 保存的筛选
  4 星以上
  本月拍摄
```

实现要点：
- 用 Radix `Collapsible` 做组的折叠
- 每项右侧有计数（"D:\Photos · 12,345"）
- 右键菜单：刷新 / 移除 root / 重新扫描
- 当前选中项高亮，URL 反映状态（`/?root=2`、`/?tag=12`）

### 1.2 顶部工具栏

```
[<] [>] [↻刷新]   [🔍 搜索框（支持自然语言 / #标签 / size>5MB） ]   [视图][排序][筛选][选择]  [⚙]
```

- 视图：网格大 / 网格中 / 网格小 / 列表
- 排序：拍摄时间 / 修改时间 / 文件名 / 文件大小（升降序）
- 筛选：弹出面板，多条件叠加（格式、尺寸范围、时间范围、有/无 GPS、有/无人脸）
- 选择：进入多选模式，工具栏变成批量操作（删除 / 移动 / 加标签 / 导出）

### 1.3 中间瀑布流

- 用 **react-virtuoso** 的 `VirtuosoGrid`
- 卡片元素：缩略图 + 文件名 + 选中态边框
- 鼠标悬停：显示快速操作（收藏、信息、删除）
- 双击 → 详情大图模式（lightbox 全屏）
- 增量加载：每次取 200 条，滚到底自动续

性能要求（验收线）：
- 10 万张图初次加载到能滚动 < 1.5 秒
- 滚动 60fps 稳定（开发者工具 Performance 面板验证）
- 缩略图未就绪时显示占位框 + 模糊主色（从 EXIF 缩略图取首像素）

### 1.4 右侧详情面板

设计稿里包含的内容（自上而下）：

```
[ 大缩略图，可点开全屏 ]

文件名（可编辑）
路径 · D:\Photos\2026\06\IMG_001.jpg
大小 · 4.2 MB · 4032×3024 · JPEG

[ 基本信息 ]
拍摄时间   2026-06-15 18:32
相机       Sony α7 IV  50mm  f/1.8  1/200s  ISO 400
位置       上海 徐汇区（点击展开地图）

[ 标签 ]
日落 · 海边 · 风景 · 黄色   [+]   用户标签 ▼

[ 颜色 ]              ← MVP3 后可加（CLIP 提取）
■■■■■

[ 相似 ]              ← MVP3 后可加（点击发起以图搜图）
[ 4 张相似图缩略图 ]
```

实现要点：
- 空状态：没选图时显示库统计（总数、总大小、按格式占比环形图）
- 多选时显示批量统计（"已选 23 张 · 共 84MB"）
- 用户编辑（文件名、标签）走 invoke，写入数据库 + undo_log
- EXIF 字段缺失时整条隐藏，不显示空字段

## 2. 四个子界面（设计稿底部小图）

设计稿底部有 4 个缩略图。逐个翻译：

### 2.1 搜索面板（左下小图）

弹出层（或独立路由 `/search`），居中显示：

```
┌──────────────────────────────────────────────┐
│  🔍 输入文字、拖入图片，或选择标签           │
│  ──────────────────────────────────────────  │
│                                              │
│  [拖入图片做"以图搜图"]                      │
│                                              │
│  快速：#日落  #人物  #截图  #大于10MB        │
│                                              │
│  最近搜索：海边日落 · 红色跑车 · 报错截图    │
│                                              │
│  [关闭]                          [开始搜索]  │
└──────────────────────────────────────────────┘
```

- 文本：触发 CLIP text→image 检索（MVP3）
- 拖入图片：图片转 embedding，搜 LanceDB（MVP3）
- DSL：`#tag` 走标签倒排；`size>10MB`、`taken:2026`、`format:heic` 走 SQL 过滤
- 组合：文本 + DSL 并存时，先 SQL 过滤候选集，再向量排序

### 2.2 去重对比（中下小图）

```
┌─────分组列表────┬──────对比视图──────────────────┐
│  组 1 (3 张)    │   [图 A]      [图 B]           │
│  组 2 (2 张)    │   保留 ●      保留 ○           │
│  组 3 (5 张)    │   4032×3024   1920×1080        │
│ ...             │   4.2MB       780KB             │
│                 │   2026-06-15  2026-06-15        │
│                 │                                 │
│                 │   建议保留：图 A（最大分辨率）  │
│                 │   [跳过]  [应用并下一组]        │
└─────────────────┴────────────────────────────────┘

底部：[批量规则▼ 保留最大分辨率]  [确认全部 · 23 组]
```

- 组之间用键盘 ↑↓ 切换，A/B 用 ←→ 切保留
- 批量规则：最大分辨率 / 最新文件 / 最大体积 / 文件名最短 / 我手动选
- 删除走回收站，结果右下角弹 toast 含"撤销"按钮（30 天有效）

### 2.3 AI 处理状态（右下小图之一）

```
┌──────── 后台任务 ────────┐
│ 缩略图   45,231 / 84,000  ─────────── 53%  │
│ 哈希     12,400 / 84,000  ───            16% │
│ AI 标签  8,200 / 84,000   ──              10% │
│ CLIP     7,800 / 84,000   ─                 9% │
│                                              │
│ [⏸ 暂停]  [⚙ 并发设置]  [▶ 优先处理某目录]   │
└──────────────────────────────────────────────┘
```

- 长驻在底部状态栏，点击展开全屏面板
- 显示当前并发数、估计剩余时间、CPU/GPU 占用
- 设置每类任务的优先级、是否仅充电时跑、是否仅闲置时跑
- AI Worker 状态：运行中 / 加载模型中 / CUDA 不可用 / 已停止

### 2.4 设置（右下小图之二）

分类：
- **常规**：语言、主题、启动行为、自动检查更新
- **扫描**：本地/网络并发、忽略规则（glob）、文件监听开关
- **缩略图**：缓存大小上限、清缓存按钮
- **AI**：模型选择、CUDA / DirectML / CPU、模型下载管理
- **隐私**：人脸识别（默认关）、OCR（默认关）、日志上报（默认关）
- **关于**：版本、日志目录、问题反馈

## 3. 关键交互

| 场景 | 交互 |
|------|------|
| 大量图片选择 | 点击 + Shift 范围选 + Ctrl/Cmd 多选；Ctrl+A 全选当前视图 |
| 拖拽 | 卡片可拖到左侧标签上加标签；可拖到 OS 外部当文件 |
| 键盘快捷键 | `J/K` 上下张、`I` 切换详情面板、`Space` 全屏预览、`Del` 移到回收站、`F` 收藏 |
| 大图预览 | 全屏 lightbox：滚轮缩放、按住空格临时显示原图、Esc 退出 |
| 任务取消 | 状态栏点暂停 / 取消，需求是"用户随时可中止"，对扫描必须支持 |
| 多窗口 | MVP1 不做；MVP4 可考虑"对比窗口"独立开 |

## 4. 设计系统

- **色板**：以 macOS-like 中性灰 + 单一蓝色品牌色，深色模式必备
- **字体**：系统字（`-apple-system, Segoe UI, ...`）+ 中文 `Microsoft YaHei UI`
- **间距**：4 / 8 / 12 / 16 / 24 / 32 网格
- **组件**：Radix UI（accessible 原语）+ 自封装表层（参考 smart-search 的 `components/ui/`）
- **图标**：Lucide React
- **动效**：节制；用 `motion`/`framer-motion` 仅在路由切换、选中态、toast 弹出处加 100-200ms 过渡

## 5. 前端状态管理（Zustand）

按职责拆 store，避免巨型单 store：

```ts
// src/stores/appStore.ts  应用级
{
  theme: 'system' | 'light' | 'dark',
  locale: 'zh-CN' | 'en-US',
  view: 'grid-lg' | 'grid-md' | 'grid-sm' | 'list',
  selection: number[],                      // 选中的 image_id
  sidebarCollapsed: boolean,
  detailPaneOpen: boolean,
}

// src/stores/scanStore.ts  扫描相关
{
  roots: Root[],
  scanTasks: ScanTask[],
  progress: Record<rootId, ProgressInfo>,   // 来自 Tauri event
  taskCounts: { thumb, hash, ai, clip },    // 队列长度
  addRoot, removeRoot, rescan, pauseAll,
}

// src/stores/browseStore.ts  浏览状态
{
  currentRootId: number | null,
  currentTagIds: number[],
  filters: FilterDSL,
  sort: { field, direction },
  pagination: { offset, limit, total },
  images: Image[],
  loadMore, applyFilter, clearFilter,
}

// src/stores/dedupStore.ts  MVP2
{
  groups: DupGroup[],
  currentGroupId: number | null,
  pickRule: 'largest' | 'newest' | 'biggest' | 'manual',
  preview: { left: Image, right: Image, keep: 'left' | 'right' },
  loadGroups, resolveGroup, batchApply, undo,
}

// src/stores/aiStore.ts  MVP3
{
  workerStatus: 'unknown' | 'starting' | 'ready' | 'failed',
  cudaInfo: { device, vram, compute },
  selectedModels: { clip, tagger, ocr, face },
  searchQuery: string,
  searchResults: SearchResult[],
  searchMode: 'text' | 'image' | 'mixed',
  search, cancelSearch,
}
```

跨 store 通信：通过 Tauri event（如 `scan:progress`、`dedup:resolved`）写回对应 store，不直接在 store 之间相互引用。

## 6. Tauri 命令清单（前端能 invoke 的）

按 MVP 阶段标注：

```ts
// MVP1
roots_list, roots_add, roots_remove, roots_update
scan_start, scan_pause, scan_cancel, scan_status
images_query(params), images_get_detail, images_update_meta
thumbs_path(image_id, size)
events: scan:progress, scan:done, scan:error

// MVP2
hash_start, phash_start, dedup_groups, dedup_resolve, dedup_undo, trash_move
events: hash:progress, dedup:new_group, undo:committed

// MVP3
ai_worker_start, ai_worker_status, ai_worker_stop
ai_search(text | image_id), ai_tag_image(image_id)
events: ai:progress, ai:worker_status

// MVP4
ocr_run, faces_cluster, smart_album_save, smart_album_query
```

类型定义集中在 `src/lib/tauri-types.ts`，由 `cargo run --bin gen-types` 从 Rust 生成（用 `ts-rs` crate）。

## 7. 国际化

i18n 走 `next-intl`：

- 默认中文，准备英文（MVP3 后做）
- 用户全局规则要求"用中文"，所以默认 zh-CN，开发期可临时切英文测渲染

## 8. 验收（UI 部分）

- 主三栏在 1280 / 1440 / 1920 / 4K 四档分辨率下不溢出
- 深色 / 浅色切换无闪烁
- 在 Windows 高 DPI（150% / 175%）下文字与图标无模糊
- 键盘可完全操作（Tab 焦点环可见）
- 右键菜单全部用 Radix `ContextMenu`，与 Windows 风格融合

## 9. 接下来读

- 第一个 MVP 怎么实现这套 UI 的"骨架部分" → [`03-mvp1-browse.md`](./03-mvp1-browse.md)
