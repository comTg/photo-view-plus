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
    /// 图像向量模型（设置项 `aiClipModel`）；`None` 时 worker 沿用已加载的模型或默认模型。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
    /// 向量维度（旧 worker 不返回，按 512 处理）。
    #[serde(default = "default_clip_dims")]
    pub dims: usize,
    pub fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEncodeRequest {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEncodeResponse {
    pub embedding: Vec<f32>,
    pub model: String,
    #[serde(default = "default_clip_dims")]
    pub dims: usize,
    pub fallback: bool,
}

fn default_clip_dims() -> usize {
    512
}

/// `/text/embed`：描述（或查询）文本 → 文本向量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextEmbedRequest {
    pub texts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 查询文本：worker 按模型卡加检索指令前缀。
    pub query: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEmbedResponse {
    pub embeddings: Vec<Vec<f32>>,
    pub model: String,
    pub dims: usize,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrItem {
    pub id: i64,
    pub thumb_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrRequest {
    pub items: Vec<OcrItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrLine {
    pub bbox: serde_json::Value,
    pub content: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrResult {
    pub id: i64,
    pub text: String,
    pub lines: Vec<OcrLine>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrResponse {
    pub items: Vec<OcrResult>,
    pub model: String,
    pub fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceDetectItem {
    pub id: i64,
    pub thumb_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceDetectRequest {
    pub items: Vec<FaceDetectItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceDetection {
    pub bbox: FaceBox,
    pub confidence: f64,
    pub embedding: Option<Vec<f32>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceDetectResult {
    pub id: i64,
    pub faces: Vec<FaceDetection>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FaceDetectResponse {
    pub items: Vec<FaceDetectResult>,
    pub model: String,
    pub fallback: bool,
}

/// `/vlm/analyze` 的参数（走查询串；请求体是原图字节）。
#[derive(Debug, Clone, Serialize)]
pub struct VlmAnalyzeParams {
    pub server_url: String,
    pub long_edge: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub taken_at: Option<i64>,
}

/// VLM 规范化后的一条标签（分数按顺序赋值）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VlmTag {
    pub name: String,
    pub score: f64,
}

/// worker 规范化后的分析结果（docs/10 §6）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VlmResult {
    pub description: String,
    pub categories: Vec<String>,
    pub tags: Vec<VlmTag>,
    pub keywords: Vec<String>,
    pub people_count: i64,
    pub visible_text: String,
    pub sensitive: bool,
    #[serde(default)]
    pub raw_json: Option<String>,
}

/// `/vlm/analyze` 的响应。`error_kind`：decode / model 算这张图失败；server 是临时错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VlmAnalyzeResponse {
    pub ok: bool,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub result: Option<VlmResult>,
    #[serde(default)]
    pub prompt_version: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
    #[serde(default)]
    pub infer_ms: Option<i64>,
    #[serde(default)]
    pub prompt_tokens: Option<i64>,
    #[serde(default)]
    pub predicted_tokens: Option<i64>,
}

/// VLM 单张图的请求超时：llama-server 槽位全忙时请求在那边排队，给足时间。
const VLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

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

    /// 一张图的 VLM 分析：原图字节作请求体（docs/10 §4）。
    pub async fn vlm_analyze(
        &self,
        port: u16,
        image: Vec<u8>,
        params: &VlmAnalyzeParams,
    ) -> AppResult<VlmAnalyzeResponse> {
        let url = endpoint(port, "/vlm/analyze");
        Ok(self
            .client
            .post(url)
            .query(params)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(image)
            .timeout(VLM_REQUEST_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// 调整 worker 的数学库线程数（docs/11 §5.3 `/runtime/throttle`）。
    pub async fn throttle(&self, port: u16, threads: usize) -> AppResult<()> {
        let url = endpoint(port, "/runtime/throttle");
        self.client
            .post(url)
            .json(&serde_json::json!({ "threads": threads }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
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
        model: Option<String>,
    ) -> AppResult<ClipEmbedResponse> {
        let url = endpoint(port, "/clip/embed");
        Ok(self
            .client
            .post(url)
            .json(&ClipEmbedRequest { items, model })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn encode_text(
        &self,
        port: u16,
        text: String,
        model: Option<String>,
    ) -> AppResult<TextEncodeResponse> {
        let url = endpoint(port, "/clip/encode_text");
        Ok(self
            .client
            .post(url)
            .json(&TextEncodeRequest { text, model })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn embed_texts(
        &self,
        port: u16,
        texts: Vec<String>,
        model: Option<String>,
        query: bool,
    ) -> AppResult<TextEmbedResponse> {
        let url = endpoint(port, "/text/embed");
        Ok(self
            .client
            .post(url)
            .json(&TextEmbedRequest {
                texts,
                model,
                query,
            })
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

    pub async fn run_ocr(&self, port: u16, items: Vec<OcrItem>) -> AppResult<OcrResponse> {
        let url = endpoint(port, "/ocr/run");
        Ok(self
            .client
            .post(url)
            .json(&OcrRequest { items })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn detect_faces(
        &self,
        port: u16,
        items: Vec<FaceDetectItem>,
    ) -> AppResult<FaceDetectResponse> {
        let url = endpoint(port, "/face/detect");
        Ok(self
            .client
            .post(url)
            .json(&FaceDetectRequest { items })
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
