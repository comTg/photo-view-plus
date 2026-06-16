# 06 · MVP4 · 高级管理

> 范围：OCR / 人脸聚类 / 时间轴视图 / 地图 EXIF / 智能相册 / 文件监听。  
> 前置：MVP1 + MVP2 + MVP3 完成。

## 0. 目标与验收

**做完 MVP4 后，应能：**

1. 自动 OCR 截图，搜"报错"找到包含"Error"的截图
2. 自动人脸聚类，左侧"人物"分组显示各人物的代表图
3. 时间轴视图（按年 / 月分桶），可快速跳转
4. 有 GPS 的图在地图视图上显示
5. 用户可保存"4 星以上 + 2026 拍摄 + 标签'风景'"为智能相册
6. 监听已添加 roots 的文件变化，自动入库 / 删除
7. 隐私敏感功能（OCR、人脸）默认关闭，用户主动启用

## 1. 模块边界

引入：

| 路径 | 引入 |
|------|------|
| `src/app/timeline/page.tsx` `map/page.tsx` `people/page.tsx` | 三个新视图 |
| `src/components/{timeline,map,people,ocr}/` | 视图组件 |
| `src-tauri/src/services/{ocr_service,face_service,watcher_service}.rs` | 业务 |
| `src-tauri/src/commands/{ocr,face,smart_album,watcher}.rs` | 命令 |
| `src-tauri/src/migrations/0004_ocr_face.sql` `0005_smart_albums.sql` | schema |
| `ai-worker/src/api/{ocr.py,face.py}` | Python 端 |

## 2. 任务清单

### T1 Migration 0004 / 0005

具体：
- 0004：images 加 `ocr_status` / `ocr_text` / `face_status`；新建 `faces` 表 + `face_clusters` 表（见 `docs/01` § 2.9）
- 0005：`smart_albums` 表（见 `docs/01` § 2.8）

**验收**：迁移幂等。

### T2 OCR 服务（PaddleOCR / RapidOCR）

**目标**：截图、文档图、海报的文字可索引可搜。

具体：
- 默认关闭（设置页有开关）
- AI Worker `/ocr/run`：用 `RapidOCR`（ONNX，无 PaddlePaddle 依赖更轻）
- 输入：缩略图 512px（不是 256，OCR 需要清晰文字）
- 输出：`{text: str, lines: [{bbox, content, confidence}]}`
- 主进程：任务 `OcrTask`，P4 优先
- 触发策略：仅对"看起来是截图"的图跑（启发式：宽高比接近屏幕、低色彩复杂度），或用户在某 root 上手动启用
- 写库：`images.ocr_text` + 加标签 `tag(name='截图', source='ocr')`

**验收**：截图样本集 OCR 准确率 ≥ 90%。

### T3 全文索引（SQLite FTS5）

**目标**：用 FTS5 索引 `ocr_text` 与 `filename`，支持中文。

具体：
- 创建虚拟表 `images_fts(rowid, filename, ocr_text)`，token = `unicode61`（中文按字符 token）
- 同步：触发器 ON INSERT / UPDATE images
- 命令 `images_search_text(q) -> ImageList`：先 FTS 再 JOIN 主表

**验收**：搜"报错"返回所有含该文字的截图。

### T4 人脸检测

**目标**：找出所有图里的人脸框。

具体：
- 默认关闭
- AI Worker `/face/detect`：用 `InsightFace` 的 `buffalo_l`（ONNX）
- 输入：缩略图；输出：`[{bbox, confidence}]`
- 主进程任务 `FaceDetectTask`，P7
- 写 `faces` 表（embedding 字段留空，等 T5）

**验收**：100 张含人脸的样本图，检出召回 ≥ 95%。

### T5 人脸 embedding + 聚类

**目标**：把脸聚成"人物"。

具体：
- `/face/embed`：对每个 bbox 裁剪 → 输入到 ArcFace → 512 维向量
- 向量写 LanceDB 的 `face_embeddings` 表
- 聚类：用 HDBSCAN 或简化的 Chinese Whispers
  - 离线跑：每晚或用户触发时执行
  - 输入：所有 face 向量
  - 输出：cluster_id → faces 列表
- 写回 `face_clusters` + `faces.cluster_id`
- 提供 UI：把多个 cluster 合并、把一张脸移到另一个 cluster

**验收**：50 张包含 5 个不同人的图聚成 5 ± 1 个 cluster。

### T6 人脸 UI

具体：
- 左侧"人物"分组列出 cluster（封面 = sample_image_id）
- 点击 cluster → 中间瀑布流显示该人所有图
- 单图详情面板 → 显示该图含的人脸框 + cluster 链接
- 用户操作：命名 cluster、合并、拆分、忽略（标 "非人脸"）

**验收**：基本可用，命名后持久化。

### T7 时间轴视图

**目标**：按拍摄时间快速回顾。

具体：
- 路由 `/timeline`
- 分桶策略：年 → 月 → 日，按层级折叠
- 视觉：左侧时间标尺、右侧每段时间的图墙（每段最多 50 张，更多有"查看全部"）
- 用 `taken_at`（缺失则 fallback 到 `mtime`）
- 与瀑布流共用 react-virtuoso

**验收**：5 万图按月分桶秒级渲染；跳到 2026-06 立即定位。

### T8 地图视图

**目标**：在地图上看带 GPS 的图。

具体：
- 路由 `/map`
- 库：Leaflet + OpenStreetMap 瓦片
- 离线选项：MapTiler / 高德的离线包（可选项，默认在线）
- 聚合：大量近点用 Marker Cluster
- 点击 marker → 弹出该地点的图（小瀑布流）
- 选区：用户拖选地图区域，反查图

**验收**：上海某天拍的 100 张图全部正确显示。

### T9 智能相册

**目标**：保存的筛选条件，一键回到该视图。

具体：
- DSL = `browseStore.filters` 的 JSON 序列化 + 排序 + 视图
- 创建：在筛选面板"保存为智能相册"按钮
- 命名 / 图标 / 排序
- 左侧栏"智能相册"分组显示
- 命令 `smart_album_save / list / delete / apply`
- 当 DSL 引用的 root / tag 被删时 → 智能相册标 "失效"

**验收**：创建 5 个不同相册，重启后顺序、内容正确。

### T10 文件监听

**目标**：用户改了图，应用自动同步。

具体：
- 依赖：`notify` crate（跨平台），底层 ReadDirectoryChangesW
- 监听已添加 roots 的子树
- 事件：Create / Modify / Remove / Rename → 转成扫描器的任务
- 限速：500ms 内的多事件 debounce 合并
- 网络盘默认不监听（ReadDirectoryChangesW 在 SMB 上不可靠，改成定时 rescan，默认 4 小时）

**验收**：在监听目录下新建图 → 5s 内入库；删图 → 标 deleted_at；改图 → 重新生成缩略图。

### T11 隐私设置面板

**目标**：所有隐私敏感功能默认关，用户可见可控。

具体：
- 设置 → 隐私 Tab：
  - 启用 OCR（默认 OFF）
  - 启用人脸识别（默认 OFF）
  - 启用 GPS 显示（默认 ON，但提示用户分享前清除 EXIF）
  - 数据全部本地存储（说明文字）
  - 一键清除 AI 数据（删 LanceDB + tags + faces）
- 切换关闭时：停止相关队列，已存数据保留但 UI 不展示

**验收**：默认安装下 OCR / 人脸完全静默，打开开关后才开始处理。

### T12 性能与稳定性

具体：
- 全部队列任务在低电量 / 高 CPU 时降速（设置可调）
- 数据库 VACUUM 任务：每月一次（应用空闲时）
- 备份：导出 SQLite + LanceDB 到 zip（不含缩略图，可选含模型）
- 导入：从 zip 恢复

**验收**：导出导入回往，数据完整一致。

### T13 MVP4 端到端验收

清单：
1. 启用 OCR，截图被识别出文字
2. 启用人脸，5 张同一人物的图聚成 1 个 cluster
3. 时间轴跳到任意年月
4. 地图视图显示有 GPS 的图
5. 智能相册创建 + 持久化
6. 文件监听验证 Create/Modify/Delete
7. 关闭所有隐私开关后处理立即停
8. 数据备份导出 / 导入

## 3. 依赖关系

```
T1 ──┬─→ T2 ─→ T3
     │
     ├─→ T4 ─→ T5 ─→ T6
     │
     ├─→ T7
     │
     ├─→ T8
     │
     ├─→ T9
     │
     ├─→ T10
     │
     ├─→ T11
     │
     └─→ T12 ─→ T13
```

T2-T10 互不依赖，可并行；T11 / T12 在前面都完成后再做。

## 4. 关键技术点与坑

- **OCR 截图启发式可能误判**：宽高比 + 主色调 + 文字密度三特征做粗筛，宁可漏跑也不要对所有图开 OCR
- **InsightFace ArcFace 在 RTX 50 系列上同样可能遇 sm_120 问题**：方案同 MVP3
- **人脸聚类是 NP 难近似**：Chinese Whispers 简单但精度一般；HDBSCAN 稳但参数敏感。建议先用 ChineseWhispers，后续可换 HDBSCAN 做对比
- **GPS 反查到城市名**：需要 reverse geocoding，不要在线请求；用离线 `Nominatim` 数据集或简化为"按经纬度 0.1° 网格分组"
- **`notify` 在 SMB 上不可靠**：明确文档说明，网络盘走定时 rescan
- **FTS5 中文分词差**：unicode61 按字符切，搜"报错"能命中但效率低。10 万级别可以接受；100 万需要换 `jieba-rs` 做分词后写入 FTS5（参考 smart-search 的做法）
- **VACUUM 是阻塞操作**：要在用户不操作时跑（idle 检测）
- **备份大小**：100 万图的 SQLite ≈ 1.5GB + LanceDB ≈ 1GB，备份 zip 控制在用户可接受范围

## 5. 完成判据

- 13 个任务"验收"段全部打勾
- 隐私清单（README + 设置页）经用户确认
- ADR 写入 `docs/08-roadmap.md`：聚类算法、地图离线方案
- `docs/08-roadmap.md` 标 MVP4 完成，进入 1.0 候选发布
