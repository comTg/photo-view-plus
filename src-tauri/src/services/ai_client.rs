use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::AppResult;

#[derive(Clone)]
pub struct AiHttpClient {
    client: reqwest::Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerHealth {
    pub status: String,
    pub pid: Option<u32>,
    pub device: Option<String>,
    pub compute: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipEmbedItem {
    pub id: i64,
    pub thumb_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipEmbedRequest {
    pub items: Vec<ClipEmbedItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipEmbedResult {
    pub id: i64,
    pub embedding: Option<Vec<f32>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipEmbedResponse {
    pub items: Vec<ClipEmbedResult>,
    pub model: String,
    pub fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEncodeRequest {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEncodeResponse {
    pub embedding: Vec<f32>,
    pub model: String,
    pub fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaggerItem {
    pub id: i64,
    pub thumb_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaggerRequest {
    pub items: Vec<TaggerItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TagScore {
    pub name: String,
    pub score: f64,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaggerResult {
    pub id: i64,
    pub tags: Vec<TagScore>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaggerResponse {
    pub items: Vec<TaggerResult>,
    pub model: String,
    pub fallback: bool,
}

impl AiHttpClient {
    pub fn new() -> AppResult<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { client })
    }

    pub async fn health(&self, port: u16) -> AppResult<WorkerHealth> {
        let url = endpoint(port, "/health");
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn diagnostics(&self, port: u16) -> AppResult<Value> {
        let url = endpoint(port, "/diagnostics");
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn shutdown(&self, port: u16) -> AppResult<()> {
        let url = endpoint(port, "/shutdown");
        self.client.post(url).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn embed_images(
        &self,
        port: u16,
        items: Vec<ClipEmbedItem>,
    ) -> AppResult<ClipEmbedResponse> {
        let url = endpoint(port, "/clip/embed");
        Ok(self
            .client
            .post(url)
            .json(&ClipEmbedRequest { items })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn encode_text(&self, port: u16, text: String) -> AppResult<TextEncodeResponse> {
        let url = endpoint(port, "/clip/encode_text");
        Ok(self
            .client
            .post(url)
            .json(&TextEncodeRequest { text })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn tag_images(&self, port: u16, items: Vec<TaggerItem>) -> AppResult<TaggerResponse> {
        let url = endpoint(port, "/tagger/run");
        Ok(self
            .client
            .post(url)
            .json(&TaggerRequest { items })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn models_status(&self, port: u16) -> AppResult<Value> {
        let url = endpoint(port, "/models/status");
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn model_download(&self, port: u16, model_key: &str) -> AppResult<Value> {
        let url = endpoint(port, "/models/download");
        Ok(self
            .client
            .post(url)
            .json(&serde_json::json!({ "model_key": model_key }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
}

fn endpoint(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}
