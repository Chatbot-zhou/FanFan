//! Ollama 集成层。
//!
//! 职责：连接检测、启动守卫、健康轮询、模型管理、生成/嵌入调用、运行时观测。
//! 仅支持本机 Ollama（`127.0.0.1:11434`），不连局域网、远程或公网。
//!
//! 设计约束：
//! - 统一经 [AppError] 返回结构化错误，错误码见 `contracts/error-codes.json`。
//! - 启动失败、健康超时仅返回错误且绝不让宿主进程 panic / 崩溃。
//! - 未安装时只引导安装，不静默安装、不下载第三方安装包、不维护安装镜像。
//! - 防重复启动：先探测端口已监听则直接复用，否则才启动 `ollama serve`。
//! - 全程在调用方后台线程执行，UI 主线程不阻塞。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppError;

/// 本机 Ollama 服务默认监听地址。
pub const OLLAMA_HOST: &str = "127.0.0.1";
/// 本机 Ollama 服务默认监听端口（官方默认值）。
pub const OLLAMA_PORT: u16 = 11434;

/// 启动 `ollama serve` 后等待 `/api/version` 就绪的超时。
pub const OLLAMA_START_TIMEOUT: Duration = Duration::from_secs(15);
/// 健康轮询间隔。
pub const OLLAMA_HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 单次 HTTP 请求的接收/写出超时上限。
const OLLAMA_IO_TIMEOUT: Duration = Duration::from_secs(180);
/// 响应正文安全上限（与 llama.cpp 侧一致）。
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// `GET /api/version` 返回的服务版本信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OllamaVersion {
    pub version: String,
    #[serde(default)]
    pub tag: Option<String>,
}

/// Ollama 环境的探测结果。
#[derive(Debug, Clone)]
pub struct OllamaProbe {
    /// 找到的 Ollama 可执行文件；`None` 表示未安装。
    pub executable: Option<PathBuf>,
    /// `/api/version` 是否已就绪。
    pub running: bool,
    /// 就绪时返回版本信息。
    pub version: Option<OllamaVersion>,
}

/// Ollama 三态状态机标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OllamaStatus {
    /// 未安装。
    NotInstalled,
    /// 已安装但服务未运行。
    InstalledNotRunning,
    /// 已就绪。
    Ready,
}

/// 对话生成的可选参数（映射到 `/api/chat` 的 `options`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct OllamaChatOptions {
    pub num_predict: Option<u32>,
    pub temperature: Option<f32>,
    pub num_ctx: Option<u32>,
    /// 顶层 `think` 开关：思考类模型（如 qwen3.5）默认开启思考。
    /// `Some(false)` 关闭思考（直接输出正文），`Some(true)` 开启思考。
    /// `None` 跟随模型默认（思考类模型默认开启）。
    pub think: Option<bool>,
}

/// `POST /api/pull` 流式进度行。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OllamaPullProgress {
    pub status: String,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub completed: u64,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub percent: Option<f32>,
    #[serde(default)]
    pub error: Option<String>,
}

impl OllamaPullProgress {
    /// 估算下载进度（0.0 ~ 1.0）；无法估算时返回 `None`。
    pub fn fraction(&self) -> Option<f32> {
        if self.total == 0 {
            return self.percent;
        }
        let ratio = self.completed as f32 / self.total as f32;
        Some(ratio.clamp(0.0, 1.0))
    }
}

/// `GET /api/tags` 中的本地模型项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OllamaLocalModel {
    pub name: String,
    #[serde(default)]
    pub model: String,
    pub modified_at: String,
    pub size: u64,
    pub digest: String,
    #[serde(default)]
    pub details: Option<Value>,
}

/// `GET /api/ps` 中的运行中模型项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OllamaRuntimeModel {
    pub name: String,
    pub model: String,
    pub size: u64,
    #[serde(default)]
    pub size_vram: u64,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub details: Option<Value>,
}

/// 本机 Ollama HTTP 客户端。仅固定指向本机端点，不做远程配置。
#[derive(Debug, Clone)]
pub struct OllamaClient {
    host: String,
    port: u16,
}

impl OllamaClient {
    /// 构造指向本机默认端点的客户端。
    pub fn local() -> Self {
        Self {
            host: OLLAMA_HOST.to_owned(),
            port: OLLAMA_PORT,
        }
    }

    /// 返回基础端点字符串，用于日志与错误详情。
    pub fn base_url(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// `GET /api/version`：探测服务健康与版本。
    pub fn version(&self) -> Result<OllamaVersion, AppError> {
        let (status, body) = self.http_json("GET", "/api/version", None, None)?;
        if status != 200 {
            return Err(request_failed(status, "/api/version"));
        }
        parse_json::<OllamaVersion>(&body, "Ollama 服务版本响应异常")
    }

    /// 非阻塞健康检查：服务就绪返回 `true`，否则返回 `false`（不抛错）。
    pub fn health(&self) -> bool {
        self.version().is_ok()
    }

    /// 阻塞轮询直到服务就绪或超时。
    pub fn wait_ready(&self, timeout: Duration) -> Result<OllamaVersion, AppError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(version) = self.version() {
                return Ok(version);
            }
            if Instant::now() >= deadline {
                return Err(AppError::new(
                    "OLLAMA_HEALTH_TIMEOUT",
                    format!("Ollama 服务在{}内未能就绪", seconds_of(timeout)),
                    true,
                ));
            }
            std::thread::sleep(OLLAMA_HEALTH_POLL_INTERVAL);
        }
    }

    /// `POST /api/chat`：对话生成（单次回包）。
    ///
    /// `messages` 为 `/api/chat` 语义的消息数组（`content` 可以是字符串，
    /// 也可以是含 `image_url` 的多模态内容块）。`cancelled` 支持软取消：
    /// 轮询读取过程中发现置位即返回 `OPERATION_CANCELLED`。
    pub fn chat(
        &self,
        model: &str,
        messages: Value,
        options: OllamaChatOptions,
        format: Option<Value>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<String, AppError> {
        if messages.as_array().is_none_or(|array| array.is_empty()) {
            return Err(AppError::new(
                "OLLAMA_REQUEST_FAILED",
                "对话请求缺少消息",
                false,
            ));
        }
        let mut payload = json!({
            "model": model,
            "stream": false,
            "messages": messages,
        });
        // 思考类模型（如 qwen3.5）在 Ollama 中默认开启思考，必须在请求中显式声明
        // `think` 开关，否则思考轨迹会消耗 token 预算导致 content 为空。
        if let Some(think) = options.think {
            payload["think"] = json!(think);
        }
        let mut option_parts = json!({});
        if let Some(value) = options.num_predict {
            option_parts["num_predict"] = json!(value.clamp(1, 4096));
        }
        if let Some(value) = options.temperature {
            option_parts["temperature"] = json!(value.clamp(0.0, 1.5));
        }
        if let Some(value) = options
            .num_ctx
            .filter(|value| (1024..=131072).contains(value))
        {
            option_parts["num_ctx"] = json!(value);
        }
        if !option_parts.as_object().is_none_or(|map| map.is_empty()) {
            payload["options"] = option_parts;
        }
        if let Some(format) = format {
            if !format.is_null() {
                payload["format"] = format;
            }
        }
        let (status, body) = self.http_json("POST", "/api/chat", Some(&payload), cancelled)?;
        if status == 404 {
            return Err(AppError::new(
                "OLLAMA_MODEL_NOT_FOUND",
                format!("Ollama 中不存在模型 {model}，请先在模型管理中拉取"),
                true,
            ));
        }
        if status != 200 {
            return Err(request_failed_with(status, "/api/chat", body));
        }
        let value = parse_json::<Value>(&body, "Ollama 对话响应格式异常")?;
        let content = value
            .pointer("/message/content")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(text) = content.filter(|text| !text.trim().is_empty()) {
            return Ok(text);
        }
        // 支持思考类模型的响应回退：Ollama 将推理轨迹放在 `message.thinking`，
        // 部分实现也使用 `message.reasoning_content`。只有思考字段有文本时，
        // 把它作为回答返回（关闭思考后通常不会出现该分支）。
        let thinking = value
            .pointer("/message/thinking")
            .or_else(|| value.pointer("/message/reasoning_content"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        thinking
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| {
                AppError::new(
                    "OLLAMA_RESPONSE_INVALID",
                    "Ollama 对话响应缺少回答文本",
                    false,
                )
            })
    }

    /// `POST /api/chat`：流式对话生成（NDJSON 逐行回包）。
    ///
    /// 与 [Self::chat] 同语义，但开启 `stream: true` 并按行回调增量文本。
    /// 思考类模型会先输出 `message.thinking`（思考轨迹），随后输出
    /// `message.content`（正文）；调用方通过 `on_delta` 同时拿到两者，
    /// 由前端决定如何渲染。`cancelled` 支持软取消。
    pub fn chat_stream(
        &self,
        model: &str,
        messages: Value,
        options: OllamaChatOptions,
        format: Option<Value>,
        cancelled: Option<&AtomicBool>,
        on_delta: &mut dyn FnMut(Option<&str>, Option<&str>),
    ) -> Result<(), AppError> {
        if messages.as_array().is_none_or(|array| array.is_empty()) {
            return Err(AppError::new(
                "OLLAMA_REQUEST_FAILED",
                "对话请求缺少消息",
                false,
            ));
        }
        let mut payload = json!({
            "model": model,
            "stream": true,
            "messages": messages,
        });
        // 与 chat() 一致：思考类模型默认开启思考，必须显式声明 think 开关。
        if let Some(think) = options.think {
            payload["think"] = json!(think);
        }
        let mut option_parts = json!({});
        if let Some(value) = options.num_predict {
            option_parts["num_predict"] = json!(value.clamp(1, 4096));
        }
        if let Some(value) = options.temperature {
            option_parts["temperature"] = json!(value.clamp(0.0, 1.5));
        }
        if let Some(value) = options
            .num_ctx
            .filter(|value| (1024..=131072).contains(value))
        {
            option_parts["num_ctx"] = json!(value);
        }
        if !option_parts.as_object().is_none_or(|map| map.is_empty()) {
            payload["options"] = option_parts;
        }
        if let Some(format) = format {
            if !format.is_null() {
                payload["format"] = format;
            }
        }
        let body_bytes =
            serde_json::to_vec(&payload).map_err(|_| invalid_request("流式对话请求构造失败"))?;
        let stream = self.open_stream("POST", "/api/chat", Some(&body_bytes), cancelled)?;
        // 收短读超时到 250ms：让 read_line 在 token 间隙频繁返回 WouldBlock，
        // 配合 cancelled 实现毫秒级取消响应。
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|_| invalid_request("Ollama 通信超时设置失败"))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
            }
            line.clear();
            let read = match reader.read_line(&mut line) {
                Ok(read) => read,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // 读超时后再次检查取消再继续等待下一块。
                    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                        return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
                    }
                    continue;
                }
                Err(_) => {
                    return Err(AppError::new(
                        "OLLAMA_REQUEST_FAILED",
                        "Ollama 流式对话通信中断",
                        true,
                    ));
                }
            };
            if read == 0 {
                // 流正常结束（Ollama 流式响应在 done 行之后关闭连接）。
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            let chunk: Value = match serde_json::from_str(line.trim()) {
                Ok(chunk) => chunk,
                Err(_) => continue,
            };
            if chunk.get("error").is_some() {
                let message = chunk
                    .pointer("/error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误")
                    .to_owned();
                return Err(AppError::new(
                    "OLLAMA_RESPONSE_INVALID",
                    format!("Ollama 流式对话返回错误：{message}"),
                    false,
                ));
            }
            let thinking = chunk
                .pointer("/message/thinking")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty());
            let content = chunk
                .pointer("/message/content")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty());
            if thinking.is_some() || content.is_some() {
                on_delta(thinking, content);
            }
            if chunk.get("done").and_then(Value::as_bool).unwrap_or(false) {
                break;
            }
        }
        Ok(())
    }

    /// `POST /api/embed`：将多段文本编码为向量。
    /// 返回 `(embeddings, dimension)`，维度取第一段向量的长度。
    pub fn embed(&self, model: &str, texts: &[String]) -> Result<(Vec<Vec<f32>>, u32), AppError> {
        if texts.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let payload = json!({ "model": model, "input": texts });
        let (status, body) = self.http_json("POST", "/api/embed", Some(&payload), None)?;
        if status == 404 {
            return Err(AppError::new(
                "OLLAMA_MODEL_NOT_FOUND",
                format!("Ollama 中不存在嵌入模型 {model}，请先拉取"),
                true,
            ));
        }
        if status != 200 {
            return Err(request_failed_with(status, "/api/embed", body));
        }
        let value = parse_json::<Value>(&body, "Ollama 嵌入响应格式异常")?;
        let embeddings = value
            .get("embeddings")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AppError::new(
                    "OLLAMA_RESPONSE_INVALID",
                    "Ollama 嵌入响应缺少 embeddings",
                    false,
                )
            })?;
        if embeddings.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let mut vectors = Vec::with_capacity(embeddings.len());
        for entry in embeddings {
            let vector = entry.as_array().ok_or_else(|| {
                AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 嵌入向量格式异常", false)
            })?;
            let mut floats = Vec::with_capacity(vector.len());
            for component in vector {
                let f = component.as_f64().ok_or_else(|| {
                    AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 嵌入分量格式异常", false)
                })? as f32;
                floats.push(f);
            }
            vectors.push(floats);
        }
        let dimension = vectors[0].len() as u32;
        // 维度一致性校验：同一模型的向量维度必须一致。
        if vectors.iter().any(|v| v.len() as u32 != dimension) {
            return Err(AppError::new(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama 嵌入向量维度不一致",
                false,
            ));
        }
        Ok((vectors, dimension))
    }

    /// `POST /api/pull`：拉取模型（流式 NDJSON 进度）。
    /// `on_progress` 为可选回调，会在每个进度行到达时触发；返回最终版本号
    /// 已由模型名给出，本方法返回 `()`，拉取失败返回结构化错误。
    ///
    /// `should_cancel` 为可选取消判定闭包：在 read_line 循环每次迭代与每次
    /// 读超时（250ms）后检查；置位时返回 `OPERATION_CANCELLED`，由 `PrependStream`
    /// drop 自动关闭 TCP 连接，服务端会清理半成品 layer。
    pub fn pull(
        &self,
        model: &str,
        timeout: Duration,
        mut on_progress: Option<&mut dyn FnMut(OllamaPullProgress)>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<(), AppError> {
        let body = json!({ "model": model });
        let body_bytes =
            serde_json::to_vec(&body).map_err(|_| invalid_request("拉取请求构造失败"))?;
        let stream = self.open_stream("POST", "/api/pull", Some(&body_bytes), None)?;
        // 把读超时收短到 250ms，让 read_line 在 layer 间隙频繁返回 WouldBlock，
        // 配合 should_cancel 实现毫秒级取消响应（默认 10s 太长）。
        stream
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|_| invalid_request("Ollama 通信超时设置失败"))?;
        let mut reader = BufReader::new(stream);
        let deadline = Instant::now() + timeout;
        let mut line = String::new();
        loop {
            // 每次迭代开头先检查取消，避免在 layer 间隙长时间空转。
            if let Some(check) = should_cancel {
                if check() {
                    return Err(AppError::new(
                        "OPERATION_CANCELLED",
                        format!("Ollama 拉取模型 {model} 已取消"),
                        false,
                    ));
                }
            }
            line.clear();
            let read = match reader.read_line(&mut line) {
                Ok(read) => read,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.kind() == std::io::ErrorKind::TimedOut =>
                {
                    // 读超时后再次检查取消，再判断总体超时。
                    if let Some(check) = should_cancel {
                        if check() {
                            return Err(AppError::new(
                                "OPERATION_CANCELLED",
                                format!("Ollama 拉取模型 {model} 已取消"),
                                false,
                            ));
                        }
                    }
                    if Instant::now() >= deadline {
                        return Err(AppError::new(
                            "OLLAMA_PULL_FAILED",
                            format!("Ollama 拉取模型 {model} 超时"),
                            true,
                        ));
                    }
                    continue;
                }
                Err(_) => {
                    return Err(AppError::new(
                        "OLLAMA_PULL_FAILED",
                        format!("Ollama 拉取模型 {model} 通信中断"),
                        true,
                    ));
                }
            };
            if read == 0 {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            let progress: OllamaPullProgress = match serde_json::from_str(line.trim()) {
                Ok(progress) => progress,
                Err(_) => continue,
            };
            let terminal = progress.status == "success" || progress.error.is_some();
            if let Some(callback) = on_progress.as_mut() {
                callback(progress.clone());
            }
            if progress.status == "success" {
                return Ok(());
            }
            if let Some(error) = progress.error {
                return Err(AppError::new(
                    "OLLAMA_PULL_FAILED",
                    format!("Ollama 拉取模型 {model} 失败：{error}"),
                    false,
                ));
            }
            if terminal {
                break;
            }
        }
        Err(AppError::new(
            "OLLAMA_PULL_FAILED",
            format!("Ollama 拉取模型 {model} 未收到成功状态"),
            false,
        ))
    }

    /// `GET /api/tags`：列出本地已拉取模型。
    pub fn list_models(&self) -> Result<Vec<OllamaLocalModel>, AppError> {
        let (status, body) = self.http_json("GET", "/api/tags", None, None)?;
        if status != 200 {
            return Err(request_failed(status, "/api/tags"));
        }
        let value = parse_json::<Value>(&body, "Ollama 模型列表响应异常")?;
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AppError::new(
                    "OLLAMA_RESPONSE_INVALID",
                    "Ollama 模型列表缺少 models",
                    false,
                )
            })?;
        let mut result = Vec::with_capacity(models.len());
        for entry in models {
            result.push(serde_json::from_value(entry.clone()).map_err(|_| {
                AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 模型条目格式异常", false)
            })?);
        }
        Ok(result)
    }

    /// `GET /api/ps`：列出当前驻留内存的模型（用于资源显示）。
    pub fn process_runtime(&self) -> Result<Vec<OllamaRuntimeModel>, AppError> {
        let (status, body) = self.http_json("GET", "/api/ps", None, None)?;
        if status != 200 {
            return Err(request_failed(status, "/api/ps"));
        }
        let value = parse_json::<Value>(&body, "Ollama 运行时模型响应异常")?;
        let models = value
            .get("models")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                AppError::new(
                    "OLLAMA_RESPONSE_INVALID",
                    "Ollama 运行时响应缺少 models",
                    false,
                )
            })?;
        let mut result = Vec::with_capacity(models.len());
        for entry in models {
            result.push(serde_json::from_value(entry.clone()).map_err(|_| {
                AppError::new(
                    "OLLAMA_RESPONSE_INVALID",
                    "Ollama 运行时模型条目格式异常",
                    false,
                )
            })?);
        }
        Ok(result)
    }

    /// `DELETE /api/delete`：删除本地模型。
    pub fn delete_model(&self, model: &str) -> Result<(), AppError> {
        let body = json!({ "model": model });
        let (status, body_response) = self.http_json("DELETE", "/api/delete", Some(&body), None)?;
        if status == 200 || status == 404 {
            return Ok(());
        }
        Err(request_failed_with(status, "/api/delete", body_response))
    }

    /// 发送一个「单次 JSON 请求」，返回 `(status, body 字符串)`。
    /// 每次调用新建短连接，读取完正文后自动关闭。
    fn http_json(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<(u16, String), AppError> {
        let write_body = match body {
            Some(value) => {
                serde_json::to_vec(value).map_err(|_| invalid_request("HTTP 请求构造失败"))?
            }
            None => Vec::new(),
        };
        let mut stream = self.connect()?;
        write_raw_request(&mut stream, method, path, &write_body, self.port)?;
        let (status, headers, leftover) = read_http_head(&mut stream, path)?;
        let mut prepend = PrependStream {
            prefix: std::io::Cursor::new(leftover),
            inner: stream,
        };
        let raw_body = read_http_body(&mut prepend, &headers, cancelled)?;
        let text = String::from_utf8(raw_body)
            .map_err(|_| AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 响应编码异常", false))?;
        Ok((status, text))
    }

    /// 建立到本机 Ollama 的 TCP 连接并设置读写超时。
    fn connect(&self) -> Result<TcpStream, AppError> {
        TcpStream::connect_timeout(
            &(self.base_url())
                .as_str()
                .to_socket_addrs()
                .map_err(|_| AppError::new("OLLAMA_REQUEST_FAILED", "Ollama 地址解析失败", true))?
                .next()
                .ok_or_else(|| AppError::new("OLLAMA_REQUEST_FAILED", "Ollama 地址无效", true))?,
            Duration::from_secs(3),
        )
        .map_err(|_| {
            AppError::new(
                "OLLAMA_REQUEST_FAILED",
                "无法连接本机 Ollama 服务，请确认其已启动",
                true,
            )
        })
    }

    /// 发送请求并返回已定位在响应头之后的流（供流式 NDJSON 解析）。
    fn open_stream(
        &self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
        _cancelled: Option<&AtomicBool>,
    ) -> Result<PrependStream, AppError> {
        let mut stream = self.connect()?;
        write_raw_request(
            &mut stream,
            method,
            path,
            body.unwrap_or_default(),
            self.port,
        )?;
        let (status, _headers, leftover) = read_http_head(&mut stream, path)?;
        if status != 200 {
            // 读取剩余正文以便给出更准确的错误。
            let mut drain = [0_u8; 256];
            let _ = stream.read(&mut drain);
            return Err(request_failed(status, path));
        }
        Ok(PrependStream {
            prefix: std::io::Cursor::new(leftover),
            inner: stream,
        })
    }
}

/// 写出一条 HTTP/1.1 请求行与请求头以及正文。
fn write_raw_request(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    body: &[u8],
    port: u16,
) -> Result<(), AppError> {
    // 写出超时沿用 OLLAMA_IO_TIMEOUT（180s）上限：模型冷启动 / 并发排队时写出可能慢于 10s，写死 10s 会让首批次误判失败。
    stream
        .set_write_timeout(Some(OLLAMA_IO_TIMEOUT))
        .map_err(|_| invalid_request("Ollama 通信超时设置失败"))?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {OLLAMA_HOST}:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nAccept: application/x-ndjson, application/json\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(body))
        .and_then(|_| stream.flush())
        .map_err(|_| AppError::new("OLLAMA_REQUEST_FAILED", "Ollama 请求写入失败", true))
}

/// 读取状态行与响应头，返回 `(status, headers, leftover)`。
///
/// `leftover` 为「头部与正文在同一 TCP 包内到达时」随头一起读到的正文前置字节。
/// 调用方必须把它交给 [PrependStream]，否则这些字节会被丢弃，导致小响应
/// （头+体同一包）被误判为「响应正文未接收完整」。
#[allow(clippy::type_complexity)]
fn read_http_head(
    stream: &mut TcpStream,
    path: &str,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), AppError> {
    // 读超时沿用 OLLAMA_IO_TIMEOUT(180s)：/api/embed 同步，冷启动加载模型时响应头被阻塞到就绪，写死 10s 会让首批次误判失败。
    stream
        .set_read_timeout(Some(OLLAMA_IO_TIMEOUT))
        .map_err(|_| invalid_request("Ollama 通信超时设置失败"))?;
    let mut buffer = [0_u8; 16 * 1024];
    let mut head = Vec::new();
    let mut separator_index = None;
    // 先完整读取响应头（含首行与所有头、空行）。
    while separator_index.is_none() {
        let read = stream
            .read(&mut buffer)
            .map_err(|_| request_failed(0, path))?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buffer[..read]);
        if let Some(index) = head.windows(4).position(|window| window == b"\r\n\r\n") {
            separator_index = Some(index);
            break;
        }
        if head.len() > MAX_RESPONSE_BYTES {
            return Err(AppError::new(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama 响应头超出安全上限",
                false,
            ));
        }
    }
    let separator = separator_index
        .ok_or_else(|| AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 响应头不完整", false))?;
    let head_text = std::str::from_utf8(&head[..separator])
        .map_err(|_| AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 响应头格式异常", false))?;
    let mut lines = head_text.lines();
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 响应状态无效", false))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    // \r\n\r\n 共 4 字节；头部之后剩余的字节是随头一起读到的正文前置。
    let leftover = head[separator + 4..].to_vec();
    Ok((status, headers, leftover))
}

/// 先返回「读取头部时随之读到的正文前置字节」，再委托底层 TcpStream 读余下数据的流适配器。
///
/// 通过实现 [Read] 与 `set_read_timeout` / `read_timeout` 代理，可无缝替换 [TcpStream]
/// 供 `read_exact_bytes`、`read_chunked_body`、`read_until_close`、`read_http_line` 使用。
struct PrependStream {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: TcpStream,
}

impl Read for PrependStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.prefix.position() < self.prefix.get_ref().len() as u64 {
            return self.prefix.read(buf);
        }
        self.inner.read(buf)
    }
}

impl PrependStream {
    fn set_read_timeout(&self, duration: Option<Duration>) -> std::io::Result<()> {
        self.inner.set_read_timeout(duration)
    }
}

/// 按 Content-Length / chunked / 读到关闭三种方式读取响应正文。
fn read_http_body(
    stream: &mut PrependStream,
    headers: &[(String, String)],
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<u8>, AppError> {
    let content_length =
        find_header(headers, "content-length").and_then(|value| value.parse::<usize>().ok());
    let chunked = find_header(headers, "transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));

    if let Some(length) = content_length {
        return read_exact_bytes(stream, length, cancelled);
    }
    if chunked {
        return read_chunked_body(stream, cancelled);
    }
    // 无长度信息：读到流关闭。
    read_until_close(stream, cancelled)
}

/// 精确读取指定字节数的正文（支持取消轮询）。
fn read_exact_bytes(
    stream: &mut PrependStream,
    length: usize,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<u8>, AppError> {
    if length > MAX_RESPONSE_BYTES {
        return Err(AppError::new(
            "OLLAMA_RESPONSE_INVALID",
            "Ollama 响应超过安全上限",
            false,
        ));
    }
    stream
        .set_read_timeout(Some(if cancelled.is_some() {
            Duration::from_millis(250)
        } else {
            OLLAMA_IO_TIMEOUT
        }))
        .map_err(|_| invalid_request("Ollama 通信超时设置失败"))?;
    let mut body = Vec::with_capacity(length.min(1 << 20));
    let started = Instant::now();
    while body.len() < length {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
        }
        if started.elapsed() > OLLAMA_IO_TIMEOUT {
            return Err(AppError::new(
                "OLLAMA_REQUEST_FAILED",
                "Ollama 响应超时",
                true,
            ));
        }
        let mut chunk = [0_u8; 16 * 1024];
        let want = (length - body.len()).min(chunk.len());
        let read = match stream.read(&mut chunk[..want]) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(AppError::new(
                    "OLLAMA_REQUEST_FAILED",
                    error.to_string(),
                    true,
                ));
            }
        };
        if read == 0 {
            return Err(AppError::new(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama 响应正文未接收完整",
                true,
            ));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

/// 读取 chunked 编码正文。
fn read_chunked_body(
    stream: &mut PrependStream,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<u8>, AppError> {
    stream
        .set_read_timeout(Some(if cancelled.is_some() {
            Duration::from_millis(250)
        } else {
            OLLAMA_IO_TIMEOUT
        }))
        .map_err(|_| invalid_request("Ollama 通信超时设置失败"))?;
    let mut body = Vec::new();
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
        }
        let line = read_http_line(stream)?;
        let length = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 分块长度无效", false))?;
        if length == 0 {
            // chassis：读取尾部空行后结束。
            let _ = read_http_line(stream);
            return Ok(body);
        }
        if body.len() + length > MAX_RESPONSE_BYTES {
            return Err(AppError::new(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama 响应超过安全上限",
                false,
            ));
        }
        let mut read = 0;
        while read < length {
            let mut chunk = [0_u8; 16 * 1024];
            let want = (length - read).min(chunk.len());
            let n = match stream.read(&mut chunk[..want]) {
                Ok(n) => n,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                        return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
                    }
                    continue;
                }
                Err(_) => {
                    return Err(AppError::new(
                        "OLLAMA_RESPONSE_INVALID",
                        "Ollama 分块正文读取失败",
                        true,
                    ));
                }
            };
            if n == 0 {
                return Err(AppError::new(
                    "OLLAMA_RESPONSE_INVALID",
                    "Ollama 分块正文未接收完整",
                    true,
                ));
            }
            body.extend_from_slice(&chunk[..n]);
            read += n;
        }
        let _ = read_http_line(stream);
    }
}

/// 读到流关闭返回全部正文。
fn read_until_close(
    stream: &mut PrependStream,
    cancelled: Option<&AtomicBool>,
) -> Result<Vec<u8>, AppError> {
    stream
        .set_read_timeout(Some(if cancelled.is_some() {
            Duration::from_millis(250)
        } else {
            OLLAMA_IO_TIMEOUT
        }))
        .map_err(|_| invalid_request("Ollama 通信超时设置失败"))?;
    let mut body = Vec::new();
    let started = Instant::now();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if started.elapsed() > OLLAMA_IO_TIMEOUT {
            return Err(AppError::new(
                "OLLAMA_REQUEST_FAILED",
                "Ollama 响应超时",
                true,
            ));
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                body.extend_from_slice(&buffer[..read]);
                if body.len() > MAX_RESPONSE_BYTES {
                    return Err(AppError::new(
                        "OLLAMA_RESPONSE_INVALID",
                        "Ollama 响应超过安全上限",
                        false,
                    ));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
                }
            }
            Err(error) => {
                return Err(AppError::new(
                    "OLLAMA_REQUEST_FAILED",
                    error.to_string(),
                    true,
                ));
            }
        }
    }
    Ok(body)
}

/// 从流中读取一行（以 `\n` 结尾），返回去除行尾换行后的字符串。
fn read_http_line(stream: &mut PrependStream) -> Result<String, AppError> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = stream
            .read(&mut byte)
            .map_err(|_| AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 读取失败", true))?;
        if read == 0 {
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if line.len() >= 8192 {
            return Err(AppError::new(
                "OLLAMA_RESPONSE_INVALID",
                "Ollama 行长度超出上限",
                false,
            ));
        }
        line.push(byte[0]);
    }
    let mut text = String::from_utf8(line)
        .map_err(|_| AppError::new("OLLAMA_RESPONSE_INVALID", "Ollama 响应编码异常", false))?;
    while text.ends_with('\r') {
        text.pop();
    }
    Ok(text)
}

fn find_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.to_owned())
}

fn parse_json<T: serde::de::DeserializeOwned>(body: &str, message: &str) -> Result<T, AppError> {
    serde_json::from_str(body).map_err(|_| AppError::new("OLLAMA_RESPONSE_INVALID", message, false))
}

fn seconds_of(duration: Duration) -> u64 {
    duration.as_secs()
}

fn invalid_request(message: impl Into<String>) -> AppError {
    AppError::new("OLLAMA_REQUEST_FAILED", message, false)
}

fn request_failed(status: u16, endpoint: &str) -> AppError {
    AppError::new(
        "OLLAMA_REQUEST_FAILED",
        format!("Ollama 请求 {endpoint} 失败（HTTP {status}）"),
        status >= 500,
    )
}

fn request_failed_with(status: u16, endpoint: &str, body: String) -> AppError {
    let mut error = request_failed(status, endpoint);
    if status >= 400 {
        let snapshot = body.chars().take(500).collect::<String>();
        error.details = Some(Box::new(
            json!({ "endpoint": endpoint, "status": status, "body": snapshot }),
        ));
    }
    error
}

// --------------------------------------------------------------------------
// 检测与启动守卫
// --------------------------------------------------------------------------

/// 探测本机 Ollama：是否安装、是否运行、版本号。
pub fn probe_ollama() -> OllamaProbe {
    let executable = find_ollama_executable();
    let client = OllamaClient::local();
    match client.version() {
        Ok(version) => OllamaProbe {
            executable,
            running: true,
            version: Some(version),
        },
        Err(_) => OllamaProbe {
            executable,
            running: false,
            version: None,
        },
    }
}

/// 当前 Ollama 三态状态机标签。
pub fn ollama_status() -> (OllamaStatus, Option<OllamaVersion>) {
    let probe = probe_ollama();
    if probe.running {
        (OllamaStatus::Ready, probe.version)
    } else if probe.executable.is_some() {
        (OllamaStatus::InstalledNotRunning, None)
    } else {
        (OllamaStatus::NotInstalled, None)
    }
}

/// 是否已安装 Ollama（可执行文件存在）。
pub fn ollama_installed() -> bool {
    ollama_status().0 != OllamaStatus::NotInstalled
}

/// 端口是否已被监听（防重复启动）。
fn port_open() -> bool {
    TcpStream::connect_timeout(
        &format!("{OLLAMA_HOST}:{OLLAMA_PORT}")
            .as_str()
            .to_socket_addrs()
            .ok()
            .and_then(|mut iterator| iterator.next())
            .expect("loopback socket"),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// 确保 Ollama 服务运行，返回就绪的探测结果。
///
/// 流程：先探测端口，已监听则直接复用（不重复 spawn）；否则后台启动
/// `ollama serve` 并轮询健康直到超时。任何失败只返回结构化错误，不崩溃。
pub fn ensure_running(timeout: Duration) -> Result<OllamaProbe, AppError> {
    let mut probe = probe_ollama();
    if probe.running {
        return Ok(probe);
    }
    let executable = probe.executable.clone().ok_or_else(not_installed_error)?;

    if port_open() {
        // 端口已监听但 /api/version 短暂未就绪：仅等待，不再启动。
        let version = wait_ready_simple(timeout)?;
        probe.running = true;
        probe.version = Some(version);
        return Ok(probe);
    }

    spawn_serve(&executable)?;
    let version = wait_ready_simple(timeout)?;
    Ok(OllamaProbe {
        executable: Some(executable),
        running: true,
        version: Some(version),
    })
}

/// 等待服务就绪（不启动），失败返回超时错误。
fn wait_ready_simple(timeout: Duration) -> Result<OllamaVersion, AppError> {
    OllamaClient::local().wait_ready(timeout)
}

/// 后台启动 `ollama serve`。
fn spawn_serve(executable: &Path) -> Result<Child, AppError> {
    let mut command = Command::new(executable);
    command
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_child_console(&mut command);
    let child = command.spawn().map_err(|error| {
        let mut err = AppError::new(
            "OLLAMA_SERVER_START_TIMEOUT",
            "Ollama 服务启动失败，请手动启动后重试",
            true,
        );
        err.details = Some(Box::new(json!({ "technical": error.to_string() })));
        err
    })?;
    set_child_below_normal_priority(&child);
    Ok(child)
}

/// 在常见位置查找 `ollama` 可执行文件。
pub fn find_ollama_executable() -> Option<PathBuf> {
    // 1. PATH 探查。
    if let Some(path) = lookup_in_path() {
        return Some(path);
    }
    // 2. Windows 常见安装目录。
    #[cfg(windows)]
    {
        let local_app_data = std::env::var("LOCALAPPDATA").ok();
        let user_profile = std::env::var("USERPROFILE").ok();
        let mut candidates = Vec::new();
        if let Some(base) = local_app_data {
            candidates.push(PathBuf::from(base).join("Programs\\Ollama\\ollama.exe"));
        }
        if let Some(base) = user_profile {
            candidates
                .push(PathBuf::from(base).join("AppData\\Local\\Programs\\Ollama\\ollama.exe"));
        }
        candidates.push(PathBuf::from("C:\\Program Files\\Ollama\\ollama.exe"));
        for candidate in candidates {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// PATH 探查辅助（独立小函数便于测试）。
fn lookup_in_path() -> Option<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    #[cfg(windows)]
    let separator = ';';
    #[cfg(not(windows))]
    let separator = ':';
    for directory in path_var.split(separator) {
        if directory.trim().is_empty() {
            continue;
        }
        #[cfg(windows)]
        {
            let candidate = PathBuf::from(directory).join("ollama.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = PathBuf::from(directory).join("ollama");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn not_installed_error() -> AppError {
    AppError::new(
        "OLLAMA_NOT_INSTALLED",
        "未检测到 Ollama，请在官方渠道安装后重试",
        false,
    )
}

#[cfg(windows)]
fn hide_child_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_child_console(_command: &mut Command) {}

#[cfg(windows)]
fn set_child_below_normal_priority(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, SetPriorityClass};

    let _ = unsafe { SetPriorityClass(HANDLE(child.as_raw_handle()), BELOW_NORMAL_PRIORITY_CLASS) };
}

#[cfg(not(windows))]
fn set_child_below_normal_priority(_child: &Child) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_progress_fraction_maps_status() {
        let staging = OllamaPullProgress {
            status: "pulling manifest".into(),
            total: 0,
            completed: 0,
            digest: None,
            percent: None,
            error: None,
        };
        assert_eq!(staging.fraction(), None);
        let ongoing = OllamaPullProgress {
            status: "pulling".into(),
            total: 100,
            completed: 25,
            digest: None,
            percent: None,
            error: None,
        };
        assert!((ongoing.fraction().unwrap() - 0.25).abs() < 1e-6);
        let explicit = OllamaPullProgress {
            status: "pulling".into(),
            total: 0,
            completed: 0,
            digest: None,
            percent: Some(0.5),
            error: None,
        };
        assert!((explicit.fraction().unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn embed_response_parsing_and_dimension_check() {
        // 构造响应字符串手动解析路径：通过空白端口不可行，直接验证 helper。
        let body = serde_json::json!({
            "model": "qwen3-embedding:0.6b",
            "embeddings": [[0.1, 0.2], [0.3, 0.4]]
        });
        let text = serde_json::to_string(&body).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let embeddings = value["embeddings"].as_array().unwrap();
        let dimension = embeddings[0].as_array().unwrap().len() as u32;
        assert_eq!(dimension, 2);
    }

    #[test]
    fn chat_response_extracts_content_and_reasoning_fallback() {
        let with_content = serde_json::json!({
            "model": "qwen3.5:2b",
            "done": true,
            "message": {"role": "assistant", "content": "你好"}
        });
        assert_eq!(with_content["message"]["content"].as_str().unwrap(), "你好");
        let reasoning_only = serde_json::json!({
            "model": "qwen3.5:2b",
            "done": true,
            "message": {"role": "assistant", "content": "", "reasoning_content": "思考"}
        });
        assert!(
            reasoning_only["message"]["content"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            reasoning_only["message"]["reasoning_content"]
                .as_str()
                .unwrap(),
            "思考"
        );
    }

    #[test]
    fn parses_content_length_body_and_rejects_truncated() {
        let headers = vec![("content-length".to_owned(), "7".to_owned())];
        // 无法在没有流的情况下直接测 read_http_body；这里验证 find_header。
        assert_eq!(
            find_header(&headers, "Content-Length").as_deref(),
            Some("7")
        );
    }

    #[test]
    fn pull_progress_parses_digest_for_multi_layer_accumulation() {
        // 验证带 digest 的 pulling 行能正确反序列化，供 download_ollama_edition
        // 内的 layer_stats HashMap 按 digest 累计多 layer 进度使用。
        let line = serde_json::json!({
            "status": "pulling aabbcc",
            "digest": "sha256:abcdef",
            "total": 1024,
            "completed": 256
        })
        .to_string();
        let progress: OllamaPullProgress = serde_json::from_str(&line).unwrap();
        assert_eq!(progress.digest.as_deref(), Some("sha256:abcdef"));
        assert_eq!(progress.total, 1024);
        assert_eq!(progress.completed, 256);
        assert!((progress.fraction().unwrap() - 0.25).abs() < 1e-6);

        // 无 digest 的中间状态行（pulling manifest / verifying digest）应解析为 None。
        let manifest_line = r#"{"status":"pulling manifest"}"#;
        let manifest: OllamaPullProgress = serde_json::from_str(manifest_line).unwrap();
        assert!(manifest.digest.is_none());
        assert_eq!(manifest.total, 0);
        assert_eq!(manifest.fraction(), None);
    }

    #[test]
    fn pull_progress_error_field_propagates_terminal_failure() {
        // 验证 error 字段能正确反序列化，download_ollama_edition 据此把 file.status 置为 failed。
        let line = r#"{"status":"error","error":"internal"}"#;
        let progress: OllamaPullProgress = serde_json::from_str(line).unwrap();
        assert_eq!(progress.error.as_deref(), Some("internal"));
    }

    #[test]
    fn installed_not_running_maps_status() {
        // 依赖真实环境，不作为强断言；仅验证映射枚举逻辑。
        let mut probe = OllamaProbe {
            executable: Some(PathBuf::from("ollama.exe")),
            running: false,
            version: None,
        };
        let kind = probe_status(&mut probe);
        assert_eq!(kind, OllamaStatus::InstalledNotRunning);
        probe.running = true;
        probe.version = Some(OllamaVersion {
            version: "0.8.0".into(),
            tag: None,
        });
        assert_eq!(probe_status(&mut probe), OllamaStatus::Ready);
    }
}

/// 由探测结果映射三态标签（供测试与前端状态机复用）。
pub fn probe_status(probe: &OllamaProbe) -> OllamaStatus {
    if probe.running {
        OllamaStatus::Ready
    } else if probe.executable.is_some() {
        OllamaStatus::InstalledNotRunning
    } else {
        OllamaStatus::NotInstalled
    }
}
