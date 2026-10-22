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
    pub ai_tagger_model: String,
    pub ocr_enabled: bool,
    pub face_enabled: bool,
    pub gps_enabled: bool,
    pub file_watcher_enabled: bool,
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
            ai_tagger_model: "ram-plus".to_string(),
            ocr_enabled: false,
            face_enabled: false,
            gps_enabled: true,
            file_watcher_enabled: true,
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
    pub ai_tagger_model: Option<String>,
    pub ocr_enabled: Option<bool>,
    pub face_enabled: Option<bool>,
    pub gps_enabled: Option<bool>,
    pub file_watcher_enabled: Option<bool>,
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
        if let Some(ai_tagger_model) = patch.ai_tagger_model {
            self.ai_tagger_model = sanitize_model_key(ai_tagger_model, "ram-plus");
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
