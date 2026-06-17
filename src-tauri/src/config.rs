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
        match std::env::var("PVP_PROFILE").as_deref() {
            Ok("dev") => Self::Dev,
            Ok("test") => Self::Test,
            Ok("prod") => Self::Prod,
            _ => Self::Dev,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub locale: String,
    pub theme: String,
    pub thumb_cache_gb: u32,
    pub local_scan_concurrency: u8,
    pub network_scan_concurrency: u8,
    pub ai_enabled: bool,
    pub ai_idle_stop_minutes: u16,
    pub ai_clip_model: String,
    pub ai_tagger_model: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            locale: "zh-CN".to_string(),
            theme: "system".to_string(),
            thumb_cache_gb: 5,
            local_scan_concurrency: 16,
            network_scan_concurrency: 4,
            ai_enabled: true,
            ai_idle_stop_minutes: 10,
            ai_clip_model: "clip-vit-b-32".to_string(),
            ai_tagger_model: "ram-plus".to_string(),
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
}

impl AppSettings {
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
            self.ai_clip_model = sanitize_model_key(ai_clip_model, "clip-vit-b-32");
        }
        if let Some(ai_tagger_model) = patch.ai_tagger_model {
            self.ai_tagger_model = sanitize_model_key(ai_tagger_model, "ram-plus");
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
