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
    pub config_path: PathBuf,
}

impl AppPaths {
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        Self {
            db_path: data_dir.join("db").join("app.sqlite"),
            thumbs_dir: data_dir.join("thumbs"),
            config_path: data_dir.join("config.json"),
            data_dir,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub locale: String,
    pub theme: String,
    pub thumb_cache_gb: u32,
    pub local_scan_concurrency: u8,
    pub network_scan_concurrency: u8,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            locale: "zh-CN".to_string(),
            theme: "system".to_string(),
            thumb_cache_gb: 5,
            local_scan_concurrency: 16,
            network_scan_concurrency: 4,
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
    }
}
