use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Dev,
    Test,
    Prod,
}

impl Profile {
    pub fn from_env() -> Self {
        std::env::var("PVP_PROFILE")
            .ok()
            .and_then(|value| Self::from_str(&value))
            .or_else(|| Self::from_str(option_env!("PVP_BUILD_PROFILE").unwrap_or("dev")))
            .unwrap_or(Self::Dev)
    }

    fn from_str(value: &str) -> Option<Self> {
        match value.trim() {
            "dev" => Some(Self::Dev),
            "test" => Some(Self::Test),
            "prod" => Some(Self::Prod),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Test => "test",
            Self::Prod => "prod",
        }
    }

    /// 这个 profile 的数据目录名，与 `tauri*.json` 里的 identifier 一致。
    pub fn identifier(self) -> &'static str {
        match self {
            Self::Dev => "com.vetoer.photoviewplus.dev",
            Self::Test => "com.vetoer.photoviewplus.test",
            Self::Prod => "com.vetoer.photoviewplus",
        }
    }

    /// 环境变量 `PVP_PROFILE` 明确指定的 profile；没设或值不认识时为 `None`。
    pub fn explicit_from_env() -> Option<Self> {
        std::env::var("PVP_PROFILE")
            .ok()
            .and_then(|value| Self::from_str(&value))
    }
}

/// `PVP_PROFILE` 明确指定的 profile 和构建里定死的 identifier 对不上时，返回拒绝启动的说明。
///
/// 数据目录由编译时的 identifier 决定，`PVP_PROFILE` 只改日志目录和窗口标题。两者不一致时照常启动，
/// 会打开另一个 profile 的库并执行 migration（docs/11 R6：测量脚本用 `PVP_PROFILE=test` 启动了
/// 正式版 identifier 的构建，正式库被迁移）。没设 `PVP_PROFILE` 时不检查（安装版就是这样启动的）。
pub fn profile_mismatch(explicit: Option<Profile>, identifier: &str) -> Option<String> {
    let profile = explicit?;
    (profile.identifier() != identifier).then(|| {
        format!(
            "PVP_PROFILE={profile} 应使用数据目录 {}，但这个构建的数据目录是 {identifier}。\
             为避免读写其他 profile 的数据，拒绝启动；请用对应的配置重新构建（例如 --config src-tauri/tauri.{profile}.json）",
            profile.identifier()
        )
    })
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub thumbs_dir: PathBuf,
    pub vectors_dir: PathBuf,
    pub models_dir: PathBuf,
    pub config_path: PathBuf,
}

impl AppPaths {
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        Self {
            db_path: data_dir.join("db").join("app.sqlite"),
            thumbs_dir: data_dir.join("thumbs"),
            vectors_dir: data_dir.join("vectors"),
            // 模型权重体积大且与 profile 无关：统一放 %LOCALAPPDATA%\PhotoViewPlus\models，
            // dev/test/prod 共享，并与 pnpm ai:download 的下载目录一致（CLAUDE.md 红线 #7）。
            // 否则 supervisor 注入给 worker 的查找目录会和下载目录对不上，模型下了也加载不到。
            models_dir: shared_models_dir().unwrap_or_else(|| data_dir.join("models")),
            config_path: data_dir.join("config.json"),
            data_dir,
        }
    }
}

/// 统一的模型目录：优先 `PVP_MODEL_DIR`，否则 `%LOCALAPPDATA%\PhotoViewPlus\models`。
/// 与 ai-worker/src/model_registry.py 的 default_model_dir 保持同一规则。
fn shared_models_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PVP_MODEL_DIR") {
        let trimmed = dir.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    std::env::var_os("LOCALAPPDATA")
        .map(|local| PathBuf::from(local).join("PhotoViewPlus").join("models"))
}

/// 默认图像向量模型（ADR-008，docs/10 A5）：Chinese-CLIP，中文查询直接检索图片。
pub const DEFAULT_CLIP_MODEL: &str = "chinese-clip-vit-b-16";
/// 旧默认（OpenAI CLIP，只懂英文），仅作回退。
pub const LEGACY_CLIP_MODEL: &str = "clip-vit-b-32";
/// 设置项可选的图像向量模型。
pub const CLIP_MODELS: &[&str] = &[DEFAULT_CLIP_MODEL, LEGACY_CLIP_MODEL];
/// 默认的描述文本向量模型（docs/10 A4）。
pub const DEFAULT_TEXT_MODEL: &str = "bge-small-zh-v1.5";
/// 设置项可选的描述文本向量模型。
pub const TEXT_MODELS: &[&str] = &[DEFAULT_TEXT_MODEL, "bge-base-zh-v1.5"];
/// 标签来源：RAM-plus（torch worker，约 2GB 显存）或视觉大模型（docs/10 A7，VLM 打标时顺带出标签）。
pub const TAGGER_RAM_PLUS: &str = "ram-plus";
pub const TAGGER_VLM: &str = "vlm";
pub const TAGGERS: &[&str] = &[TAGGER_RAM_PLUS, TAGGER_VLM];

/// 图像向量模型的维度（向量表按"模型 + 维度"命名，查已存向量时要先知道维度）。
pub fn clip_model_dims(model: &str) -> usize {
    match model {
        DEFAULT_CLIP_MODEL | LEGACY_CLIP_MODEL => 512,
        _ => 512,
    }
}

/// 当前设置文件版本：每次需要迁移旧配置时 +1，见 [`AppSettings::migrate`]。
pub const SETTINGS_VERSION: u32 = 1;

/// 应用设置（config.json）。`#[serde(default)]`：旧配置文件缺的字段用默认值补齐，
/// 而不是整份解析失败、把用户的全部设置重置成默认。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// 配置格式版本。旧文件没有这个字段，读出来是 0（字段级 default 用 `u32::default()`，
    /// 不取 `AppSettings::default()` 里的当前版本，否则旧文件永远不会被迁移）。
    #[serde(default)]
    pub settings_version: u32,
    pub locale: String,
    pub theme: String,
    pub thumb_cache_gb: u32,
    pub local_scan_concurrency: u8,
    pub network_scan_concurrency: u8,
    pub ai_enabled: bool,
    pub ai_idle_stop_minutes: u16,
    pub ai_clip_model: String,
    /// 描述文本向量模型（"描述语义"检索通道）。
    pub ai_text_model: String,
    pub ai_tagger_model: String,
    pub ocr_enabled: bool,
    pub face_enabled: bool,
    pub gps_enabled: bool,
    pub file_watcher_enabled: bool,
    /// 多少分钟没有键盘鼠标输入算"空闲"，后台全速（docs/11 §5.1）。
    pub idle_threshold_minutes: u16,
    /// "使用电脑时也运行 AI"：使用中档的 GPU 任务按 30% 占空比跑，而不是暂停（默认关）。
    pub ai_while_active: bool,
    /// "大模型只在空闲时运行"（默认开，docs/10 A6）。
    pub vlm_idle_only: bool,
    /// 大模型打标（docs/10 A6）：默认关，在设置页打开。全库分析要几个小时 GPU，只在空闲时跑。
    pub vlm_enabled: bool,
    /// llama-server 程序与模型文件；三项都空时自动查找（见 `analysis_pipeline::resolve_vlm_paths`）。
    pub vlm_server_exe: String,
    pub vlm_model_path: String,
    pub vlm_mmproj_path: String,
    /// llama-server 的并发槽位数（`-np`），同 memora 默认 4。
    pub vlm_parallel: u8,
    /// 送模型的图片长边（1024 同 memora；768 更快）。
    pub vlm_long_edge: u16,
    /// 夜间自动分析（docs/10 A6b）：打开时注册 Windows 计划任务，每天在窗口开始时运行后台分析。
    pub night_analyze_enabled: bool,
    /// 夜间窗口 `[开始, 结束)`，本地时间的整点；默认 3–7 点，避开 memora 02:00 的任务。
    pub night_start_hour: u8,
    pub night_end_hour: u8,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            settings_version: SETTINGS_VERSION,
            locale: "zh-CN".to_string(),
            theme: "system".to_string(),
            thumb_cache_gb: 5,
            local_scan_concurrency: 16,
            network_scan_concurrency: 4,
            ai_enabled: true,
            ai_idle_stop_minutes: 10,
            ai_clip_model: DEFAULT_CLIP_MODEL.to_string(),
            ai_text_model: DEFAULT_TEXT_MODEL.to_string(),
            ai_tagger_model: "ram-plus".to_string(),
            ocr_enabled: false,
            face_enabled: false,
            gps_enabled: true,
            file_watcher_enabled: true,
            idle_threshold_minutes: 3,
            ai_while_active: false,
            vlm_idle_only: true,
            vlm_enabled: false,
            vlm_server_exe: String::new(),
            vlm_model_path: String::new(),
            vlm_mmproj_path: String::new(),
            vlm_parallel: 4,
            vlm_long_edge: 1024,
            night_analyze_enabled: false,
            night_start_hour: 3,
            night_end_hour: 7,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettingsPatch {
    pub locale: Option<String>,
    pub theme: Option<String>,
    pub thumb_cache_gb: Option<u32>,
    pub local_scan_concurrency: Option<u8>,
    pub network_scan_concurrency: Option<u8>,
    pub ai_enabled: Option<bool>,
    pub ai_idle_stop_minutes: Option<u16>,
    pub ai_clip_model: Option<String>,
    pub ai_text_model: Option<String>,
    pub ai_tagger_model: Option<String>,
    pub ocr_enabled: Option<bool>,
    pub face_enabled: Option<bool>,
    pub gps_enabled: Option<bool>,
    pub file_watcher_enabled: Option<bool>,
    pub idle_threshold_minutes: Option<u16>,
    pub ai_while_active: Option<bool>,
    pub vlm_idle_only: Option<bool>,
    pub vlm_enabled: Option<bool>,
    pub vlm_server_exe: Option<String>,
    pub vlm_model_path: Option<String>,
    pub vlm_mmproj_path: Option<String>,
    pub vlm_parallel: Option<u8>,
    pub vlm_long_edge: Option<u16>,
    pub night_analyze_enabled: Option<bool>,
    pub night_start_hour: Option<u8>,
    pub night_end_hour: Option<u8>,
}

impl AppSettings {
    /// 把旧版本的配置迁移到当前版本，返回是否有改动（有改动时调用方应写回文件）。
    ///
    /// - v0 → v1（docs/10 A5 / ADR-008）：旧默认 `clip-vit-b-32` 只懂英文，中文查询接近随机，
    ///   换成 Chinese-CLIP。旧版界面只有这一个能用的选项，保存的值就是默认值，不代表用户特意选了它。
    pub fn migrate(&mut self) -> bool {
        if self.settings_version >= SETTINGS_VERSION {
            return false;
        }
        if self.settings_version < 1 && self.ai_clip_model == LEGACY_CLIP_MODEL {
            self.ai_clip_model = DEFAULT_CLIP_MODEL.to_string();
        }
        if !CLIP_MODELS.contains(&self.ai_clip_model.as_str()) {
            self.ai_clip_model = DEFAULT_CLIP_MODEL.to_string();
        }
        self.settings_version = SETTINGS_VERSION;
        true
    }

    pub fn apply_patch(&mut self, patch: AppSettingsPatch) {
        if let Some(locale) = patch.locale {
            self.locale = locale;
        }
        if let Some(theme) = patch.theme {
            self.theme = theme;
        }
        if let Some(thumb_cache_gb) = patch.thumb_cache_gb {
            self.thumb_cache_gb = thumb_cache_gb.clamp(1, 100);
        }
        if let Some(local_scan_concurrency) = patch.local_scan_concurrency {
            self.local_scan_concurrency = local_scan_concurrency.clamp(1, 16);
        }
        if let Some(network_scan_concurrency) = patch.network_scan_concurrency {
            self.network_scan_concurrency = network_scan_concurrency.clamp(1, 4);
        }
        if let Some(ai_enabled) = patch.ai_enabled {
            self.ai_enabled = ai_enabled;
        }
        if let Some(ai_idle_stop_minutes) = patch.ai_idle_stop_minutes {
            self.ai_idle_stop_minutes = ai_idle_stop_minutes.clamp(1, 120);
        }
        if let Some(ai_clip_model) = patch.ai_clip_model {
            let key = sanitize_model_key(ai_clip_model, DEFAULT_CLIP_MODEL);
            if CLIP_MODELS.contains(&key.as_str()) {
                self.ai_clip_model = key;
            }
        }
        if let Some(ai_text_model) = patch.ai_text_model {
            let key = sanitize_model_key(ai_text_model, DEFAULT_TEXT_MODEL);
            if TEXT_MODELS.contains(&key.as_str()) {
                self.ai_text_model = key;
            }
        }
        if let Some(ai_tagger_model) = patch.ai_tagger_model {
            let key = sanitize_model_key(ai_tagger_model, TAGGER_RAM_PLUS);
            if TAGGERS.contains(&key.as_str()) {
                self.ai_tagger_model = key;
            }
        }
        if let Some(ocr_enabled) = patch.ocr_enabled {
            self.ocr_enabled = ocr_enabled;
        }
        if let Some(face_enabled) = patch.face_enabled {
            self.face_enabled = face_enabled;
        }
        if let Some(gps_enabled) = patch.gps_enabled {
            self.gps_enabled = gps_enabled;
        }
        if let Some(file_watcher_enabled) = patch.file_watcher_enabled {
            self.file_watcher_enabled = file_watcher_enabled;
        }
        if let Some(minutes) = patch.idle_threshold_minutes {
            self.idle_threshold_minutes = minutes.clamp(1, 60);
        }
        if let Some(ai_while_active) = patch.ai_while_active {
            self.ai_while_active = ai_while_active;
        }
        if let Some(vlm_idle_only) = patch.vlm_idle_only {
            self.vlm_idle_only = vlm_idle_only;
        }
        if let Some(vlm_enabled) = patch.vlm_enabled {
            self.vlm_enabled = vlm_enabled;
        }
        if let Some(path) = patch.vlm_server_exe {
            self.vlm_server_exe = path.trim().to_string();
        }
        if let Some(path) = patch.vlm_model_path {
            self.vlm_model_path = path.trim().to_string();
        }
        if let Some(path) = patch.vlm_mmproj_path {
            self.vlm_mmproj_path = path.trim().to_string();
        }
        if let Some(parallel) = patch.vlm_parallel {
            self.vlm_parallel = parallel.clamp(1, 8);
        }
        if let Some(long_edge) = patch.vlm_long_edge {
            self.vlm_long_edge = long_edge.clamp(512, 1536);
        }
        if let Some(enabled) = patch.night_analyze_enabled {
            self.night_analyze_enabled = enabled;
        }
        if let Some(hour) = patch.night_start_hour {
            self.night_start_hour = hour.min(23);
        }
        if let Some(hour) = patch.night_end_hour {
            self.night_end_hour = hour.min(23);
        }
    }
}

fn sanitize_model_key(value: String, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_profile_must_match_the_build_identifier() {
        // 用 PVP_PROFILE=test 启动正式版 identifier 的构建：拒绝。
        let refused = profile_mismatch(Some(Profile::Test), "com.vetoer.photoviewplus");
        assert!(refused.is_some_and(|message| message.contains("com.vetoer.photoviewplus.test")));
        assert!(profile_mismatch(Some(Profile::Prod), "com.vetoer.photoviewplus.dev").is_some());
        // 对得上的放行。
        for profile in [Profile::Dev, Profile::Test, Profile::Prod] {
            assert_eq!(profile_mismatch(Some(profile), profile.identifier()), None);
        }
        // 没设 PVP_PROFILE（安装版）不检查。
        assert_eq!(
            profile_mismatch(None, "com.vetoer.photoviewplus.test"),
            None
        );
    }
}
