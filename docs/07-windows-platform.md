# 07 · Windows 平台关键事项

> 本项目目标平台仅 Windows。所有跨平台兼容代码可以省略；但 Windows 自身的"坑"必须正面处理。

## 1. CUDA 与 RTX 5070（sm_120）兼容

### 1.1 现状

RTX 5070 / 5070 Ti 属于 NVIDIA Blackwell 架构，compute capability `sm_120`。PyTorch 与 ONNX Runtime 对它的支持时间窗：

| 组件 | 何时完整支持 sm_120 |
|------|---------------------|
| CUDA Toolkit | 12.8+ |
| PyTorch | 2.6+（stable），2.5 仅 nightly |
| ONNX Runtime | 1.20+ |
| open_clip | 不依赖 sm 直接支持 |

### 1.2 安装步骤

```powershell
# 1. CUDA Toolkit 12.8（NVIDIA 官网下载）
# 2. cuDNN 9.x（NVIDIA 开发者中心）
# 3. Python 3.11
# 4. PyTorch（按 PyTorch 官网指引）
uv pip install torch --index-url https://download.pytorch.org/whl/cu128
# 5. ONNX Runtime GPU
uv pip install onnxruntime-gpu
```

### 1.3 报错与对策

| 报错 | 原因 | 对策 |
|------|------|------|
| `no kernel image is available for execution on the device` | PyTorch 不识别 sm_120 | 升级 PyTorch 到 2.6+ 或临时 `TORCH_CUDA_ARCH_LIST="8.9"` 跑 Ada 内核（性能 -20%） |
| `CUDA driver version is insufficient for CUDA runtime version` | 驱动太旧 | 升级 NVIDIA 驱动到 560+ |
| `CUBLAS_STATUS_NOT_INITIALIZED` | VRAM 不够 / 驱动崩 | 重启进程、关其他 GPU 程序 |
| ONNX 运行时找不到 CUDA EP | 没装 cuDNN 或路径不对 | 装 cuDNN + `PATH` 加 CUDA bin |

### 1.4 回退策略

按优先级：CUDA → DirectML（不依赖 NVIDIA 驱动版本，但慢 30-50%） → CPU。检测在 `ai-worker/src/runtime.py`，启动时记日志。

## 2. HEIC 解码

### 2.1 选项对比

| 方案 | 优点 | 缺点 |
|------|------|------|
| `libheif-rs` + `libheif.dll` | 跨平台、稳定 | 要打包 dll（约 3MB） |
| Windows HEIF Image Extension（系统组件） | 无需打包 | 用户得在 Microsoft Store 装；OEM 安装可能没有 |
| `image-rs` 原生 | 无 | 截至 2026-06 仍不支持 HEIC |

**选定**：`libheif-rs`。在 `src-tauri/build.rs` 里下载预编译 `libheif.dll` 放到 Tauri resources，运行时按 `tauri::path::resource_dir()` 加载。

### 2.2 注意

- HEIC 解码慢 5-10 倍于 JPG，缩略图任务里单独排在 P0 的次优先（不阻塞 JPG）
- HEIC 内嵌的小缩略图可优先用（`libheif::ImageHandle::list_thumbnails`），跳过完整解码
- iOS Live Photo 的 .heic 含视频流（未来 MVP5），MVP4 仅取静态帧

## 3. SMB / 网络盘

### 3.1 限制

- 默认每 root 并发 4（在 `roots.settings_json.max_concurrency` 可调）
- 文件 IO 超时 30s，超时入失败队列等下轮重试
- BLAKE3 哈希在网络盘上 = 全文件读 = 占用带宽。提供"跳过此 root 的哈希"选项
- ReadDirectoryChangesW 在 SMB 上不可靠 → 改用定时 rescan（默认 4 小时）

### 3.2 路径形态

| 输入 | 内部存储 |
|------|----------|
| 映射盘 `Z:\share` | 用户选时询问"这是否实际指向 `\\server\share`？" |
| UNC `\\server\share` | 原样存 |
| 离线 NAS | root.enabled = 0，扫描跳过但保留数据 |

### 3.3 凭据

不在本应用管理；用户应预先用 Windows `net use` 建立连接，或在 Windows 凭据管理器存好。应用启动时若读不到 root，提示用户连接。

## 4. 回收站

### 4.1 API

- 用 `trash` crate v5+，底层 `IFileOperation::DeleteItem(FOFX_RECYCLEONDELETE)`
- 处理 `S_OK` / `0x80270000`（用户取消）/ `0x80070005`（权限不足）
- **必须开 `coinit_multithreaded` 特性，且必须在后台线程调用**：`trash` 走 COM。Tauri
  同步 `#[tauri::command]` 跑在主 UI 线程（被 wry/tao `OleInitialize` 设为 COM STA），
  在主线程上调用带 `coinit_multithreaded` 的 `trash` 会请求 MTA → `CoInitializeEx`
  返回 `RPC_E_CHANGED_MODE (0x80010106)` → panic 跨 WebView2 C++ 回调无法 unwind →
  整进程 abort。`trash_service::run_on_worker` 把所有 `trash`/`os_limited` 调用放到全新
  后台线程（COM 未初始化，可干净按 MTA 初始化）。
- **网络盘回退永久删除**：SMB / UNC / 映射网络盘没有回收站，`trash::delete` 必然失败。
  `delete_one` 检测到 `is_network_path` 后回退 `fs::remove_file`（永久删除，不可恢复），
  并在 `undo_log` 标 `permanent=true`。本地盘删除失败绝不回退（见 CLAUDE.md 红线 2）。

### 4.2 撤销

`trash` crate 当前版本对"从回收站恢复"支持不全。自己封装：
- 使用 `windows-rs` 调 `IFileOperation::MoveItem(回收站项 → 原路径)`
- 找回收站项需要 `IShellFolder` 的 `Bind` 到 Recycle Bin (`CSIDL_BITBUCKET`)
- 把 path → recycle bin item 的映射在删除时保存到 `undo_log.payload`

详细 PoC 代码放到 `tests/manual/trash_restore_demo.rs`，由 MVP2 T8 任务先在 Windows VM 验证可行。

### 4.3 跨盘符

回收站是每盘符独立的 `$Recycle.Bin`。若用户从 NAS 删，可能没回收站直接永久删 → 提前告警，让用户确认。

## 5. 长路径（> 260）

### 5.1 问题

NTFS 支持 32767 字符路径，但 Win32 API 默认上限 260。深目录或长文件名会读不出来。

### 5.2 启用

1. 注册表：`HKLM\SYSTEM\CurrentControlSet\Control\FileSystem\LongPathsEnabled = 1`
2. 应用 manifest 加 `<longPathAware>true</longPathAware>`（Tauri 2 默认包含）
3. Rust 端访问长路径自动加 `\\?\` 前缀，建议用 `dunce::canonicalize` 处理

### 5.3 检查

启动时检测 LongPathsEnabled，未启用时在设置页提示 + 给一键开启按钮（需管理员权限）。

## 6. 电源 / 待机

### 6.1 长任务保活

扫描 / 哈希 / AI 期间不希望系统进入睡眠：
- Tauri 2: `tauri-plugin-os` + 自定义 binding `SetThreadExecutionState(ES_SYSTEM_REQUIRED | ES_CONTINUOUS)`
- 任务完成或暂停时恢复

### 6.2 电池策略

笔记本场景（Surface 等）：
- 默认"仅充电时跑 AI"
- 仅充电时跑 OCR / pHash
- 监听 `WM_POWERBROADCAST` 切换状态

## 7. 文件监听

### 7.1 ReadDirectoryChangesW（本地盘）

- `notify` crate 已封装好
- 缓冲区默认 64KB；大量改动会丢事件 → 调到 1MB 并定期 reconciliation 扫描
- 重命名是 `Old + New` 两个事件，要按 cookie 合并

### 7.2 网络盘

- 不可靠，改定时 rescan
- 用户可在 root 设置开启"激进监听"（仍用 notify），自担风险

## 8. 安装包与权限

### 8.1 打包

- `pnpm tauri build` 输出 NSIS / MSI（Tauri 2 默认）
- 选 MSI，方便公司域部署（用户可能在企业环境）
- 安装位置：默认 `%LOCALAPPDATA%\Programs\photo-view-plus`，无需管理员

### 8.2 防病毒误报

- 用代码签名证书签 `app.exe`（成本：DigiCert / Sectigo 一年几千元；MVP 阶段可暂时不签）
- 未签名 EXE 在某些杀软（特别是国产）下被误杀；签名后误报率大幅下降

### 8.3 自动更新

Tauri 2 内置 updater：
- 服务器：暂用 GitHub Releases
- 签名公钥放进 `tauri.conf.json`
- 用户可在设置页关自动更新

## 9. 系统资源

### 9.1 内存

- Tauri 主进程 + WebView2：基准 150-300MB
- Rust 后端：基准 100MB，扫描时 +200-500MB
- AI Worker：模型加载 1-3GB（取决于模型）+ batch 内存
- 整体上限：低端机 4GB 内存场景下，AI Worker 默认关，主程序保留 800MB 以内

### 9.2 磁盘

- 缩略图缓存上限默认 5GB，可设置
- LanceDB 索引大小 ≈ 向量数 × 1KB（含元数据 + HNSW 图）
- 日志按天轮转，14 天后删

### 9.3 GPU

- 用户开始用 AI 时 emit `gpu_busy` 通知（防止用户同时跑游戏不知道为何 FPS 掉）
- 设置可设"游戏模式"：检测到全屏应用自动暂停 AI Worker

## 10. WebView2 注意

### 10.1 版本

Tauri 2 用 Windows 11 自带 / Win 10 单独装的 WebView2 Runtime。设置项：
- 启用 GPU 加速（默认开）
- DevTools 在 dev profile 开启，prod 关闭
- 禁用第三方 cookie / 远程调试

### 10.2 自定义协议

- `thumb://` 用于缩略图（见 `docs/03` T10）
- `image://` 用于原图（按需）
- 协议处理器在 Rust 端注册，不要让前端能自由读任意文件

## 11. 时区与文件时间

- 文件 `mtime` / `ctime` 用本地时间（Windows NTFS 存 UTC，但显示常按本地）
- EXIF DateTimeOriginal 是相机本地时间，没时区信息
- 应用统一存 Unix 秒（UTC），展示时按用户系统时区
- 用 `chrono` crate 处理

## 12. 测试矩阵（最小）

| 场景 | 必测 |
|------|------|
| Windows 10 22H2 vs 11 23H2 | ✓ |
| RTX 5070 vs 集显（CPU 回退） | ✓ |
| 本地 NVMe vs SMB NAS | ✓ |
| 长路径开 vs 关 | ✓ |
| 系统中文 vs 英文 | ✓ |
| HiDPI 100% / 150% / 200% | ✓ |
| 笔记本 / 桌面 / 平板触摸 | 仅笔记本必测，平板可选 |

## 13. 不在本文档范围

- Linux / macOS 兼容（项目不支持）
- 移动端（项目不支持）
- 服务器多用户（项目是单机应用）
- 网络同步（不做云）
