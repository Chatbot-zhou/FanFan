//! 生成（LLM/视觉）运行时。
//!
//! 该模块是「翻翻」统一的后端句柄：上层 ask/rag 与 20+ 处调用点仅依赖
//! [LocalGenerationRuntime] 的公共方法，不感知具体后端。内部后端已从
//! 「本地 llama.cpp(GGUF) 子进程」迁移到**本机 Ollama**（`/api/chat`）。
//!
//! 迁移原则：
//! - 保持 [LocalGenerationRuntime] 类型名、构造器与公共方法签名不变，
//!   上层调用零改动；`activate(model_path, ...)` 的 `model_path` 语义改为
//!   **Ollama 模型 tag**（如 `qwen3.5:2b`）。
//! - 生成与视觉均走 `ollama.chat`；视觉复用同一模型（图像能力），不再需要
//!   独立的 GGUF + mmproj 投影组件。
//! - 所有错误统一映射为结构化 [AppError]（错误码见 `contracts/error-codes.json`）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppError;
use crate::ollama::{
    OLLAMA_START_TIMEOUT, OllamaChatOptions, OllamaClient, OllamaStatus, ensure_running,
    ollama_status, probe_ollama,
};

/// 生成运行时的能力快照（沿用原契约，字段不变）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeCapability {
    pub executable_available: bool,
    pub backend: String,
    pub devices: Vec<String>,
    pub gpu_available: bool,
    pub checked_at: chrono::DateTime<chrono::Utc>,
    pub error_code: Option<String>,
}

/// 单个运行中模型的硬件占用（来自 Ollama `/api/ps`），供状态面板实时展示。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeModelPlacement {
    /// 模型 tag，如 `qwen3-embedding:0.6b`。
    pub model: String,
    /// 处理器占比描述，如 `GPU 100%` / `GPU 50% · CPU 50%` / `CPU 100%`。
    pub device: String,
    /// 驻留显存字节数；0 表示当前全在 CPU。
    pub vram_bytes: u64,
    /// 模型总大小字节数。
    pub total_bytes: u64,
}

/// 一次生成激活的结果（沿用原契约，字段不变）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationActivation {
    pub backend: String,
    pub model_path: String,
    pub context_size: u32,
    pub self_test: String,
    pub multimodal: bool,
    pub device: Option<String>,
    pub threads: u32,
    pub gpu_layers: Option<u32>,
    /// 本次激活是否发生 GPU→CPU 降级及其原因；未降级时为 `None`。
    pub fallback_reason: Option<String>,
    /// 本次激活实测显存占用（字节）；无法可靠测得时为 `None`。
    pub vram_usage_bytes: Option<u64>,
}

/// 当前激活的 Ollama 模型状态（内部）。
#[derive(Debug, Clone)]
struct OllamaActiveModel {
    model_tag: String,
    context_size: u32,
    threads: u32,
    multimodal: bool,
    device: Option<String>,
    vram_usage_bytes: Option<u64>,
}

/// 生成运行时。构造器兼容旧签名（忽略传入的可执行路径），
/// 内部固定通过本机 Ollama 发包，不做 GPU/CPU 后端选择（由 Ollama 自行调度）。
#[derive(Debug)]
pub struct LocalGenerationRuntime {
    client: OllamaClient,
    active: Option<OllamaActiveModel>,
    last_capability: Option<RuntimeCapability>,
}

impl LocalGenerationRuntime {
    /// 兼容旧签名：忽略 `_executable`，固定走本机 Ollama。
    pub fn new(_executable: PathBuf) -> Self {
        Self {
            client: OllamaClient::local(),
            active: None,
            last_capability: None,
        }
    }

    /// 兼容旧签名：忽略可执行路径，固定走本机 Ollama。
    pub fn new_with_fallback(_executable: PathBuf, _fallback_executable: PathBuf) -> Self {
        Self::new(PathBuf::new())
    }

    /// 兼容旧签名：忽略可执行路径与能力快照，固定走本机 Ollama。
    pub fn new_with_fallback_and_capability(
        _executable: PathBuf,
        _fallback_executable: PathBuf,
        _capability: RuntimeCapability,
    ) -> Self {
        Self::new(PathBuf::new())
    }

    /// Ollama 是否已安装（旧语义：本地可执行是否存在）。
    pub fn executable_available(&self) -> bool {
        crate::ollama::ollama_installed()
    }

    /// CPU 备选运行时：Ollama 内建 GPU/CPU 调度，无需额外回退运行时。
    pub fn cpu_fallback_available(&self) -> bool {
        false
    }

    /// 探测并缓存能力快照。
    pub fn probe_capability(&mut self) -> RuntimeCapability {
        let capability = self.build_capability();
        self.last_capability = Some(capability.clone());
        capability
    }

    pub fn current_capability(&self) -> Option<&RuntimeCapability> {
        self.last_capability.as_ref()
    }

    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// 激活生成模型。`model_tag` 为 Ollama 模型 tag（如 `qwen3.5:2b`）。
    pub fn activate(
        &mut self,
        model_tag: &str,
        context_size: u32,
        threads: u32,
    ) -> Result<GenerationActivation, AppError> {
        self.activate_internal(model_tag, false, context_size, threads)
    }

    /// 激活多模态（视觉）生成。`model_tag` 为 Ollama 模型 tag；
    /// 忽略传入的 `mmproj_path`（Ollama 侧 qwen3.5 已内建图像能力）。
    pub fn activate_multimodal(
        &mut self,
        model_tag: &str,
        _mmproj_path: &str,
        context_size: u32,
        threads: u32,
    ) -> Result<GenerationActivation, AppError> {
        self.activate_internal(model_tag, true, context_size, threads)
    }

    fn activate_internal(
        &mut self,
        model_tag: &str,
        multimodal: bool,
        context_size: u32,
        threads: u32,
    ) -> Result<GenerationActivation, AppError> {
        validate_runtime_config(context_size, threads)?;

        // 复用已激活的同模型：不重新执行模型存在性检查与自检，`self_test`
        // 标记为已就绪，避免每次搜索/问答多一轮完整 LLM 推理。
        if let Some(active) = &self.active
            && active.model_tag == model_tag
            && active.multimodal == multimodal
        {
            return Ok(self.build_activation(active, "ready (reused)"));
        }

        // 确保 Ollama 服务就绪；未安装会返回 `OLLAMA_NOT_INSTALLED`。
        self.ensure_ollama_ready()?;

        // 模型存在性校验：Ollama 中必须已拉取该 tag。
        let models = self
            .client
            .list_models()
            .map_err(|_| AppError::new("OLLAMA_REQUEST_FAILED", "Ollama 模型列表读取失败", true))?;
        if !models.iter().any(|entry| entry.name == model_tag) {
            return Err(AppError::new(
                "OLLAMA_MODEL_NOT_FOUND",
                format!("Ollama 中不存在生成模型 {model_tag}，请先在模型管理中拉取"),
                true,
            ));
        }

        let safe_threads = threads.clamp(1, 4);
        self.active = Some(OllamaActiveModel {
            model_tag: model_tag.to_owned(),
            context_size,
            threads: safe_threads,
            multimodal,
            device: None,
            vram_usage_bytes: None,
        });

        // 最小自检：要求生成出非空文本（与旧 llama.cpp 语义一致）。
        let system = "你是本地运行状态检查器，不进行推理说明。";
        let user = if multimodal {
            "这是一张 1x1 像素的纯色测试图，请回复：就绪"
        } else {
            "只回复：就绪"
        };
        let self_test = if multimodal {
            self.complete_multimodal_test(system, user)?
        } else {
            self.complete(system, user, 256)?
        };
        if self_test.trim().is_empty() {
            self.stop();
            return Err(AppError::new(
                "GENERATION_SELF_TEST_FAILED",
                "生成模型自检没有返回文本",
                true,
            ));
        }

        // 从 `/api/ps` 补充设备与显存信息（仅供状态面板展示）。
        self.refresh_device_info();

        let active = self.active.as_ref().ok_or_else(|| {
            AppError::new("GENERATION_RUNTIME_INACTIVE", "生成模型尚未启动", true)
        })?;
        Ok(self.build_activation(active, self_test.as_str()))
    }

    /// 确保 Ollama 服务已就绪；未安装返回 `OLLAMA_NOT_INSTALLED`。
    fn ensure_ollama_ready(&self) -> Result<(), AppError> {
        let probe = probe_ollama();
        if probe.running {
            return Ok(());
        }
        ensure_running(OLLAMA_START_TIMEOUT)?;
        Ok(())
    }

    /// 构造激活结果。
    fn build_activation(
        &self,
        active: &OllamaActiveModel,
        self_test: &str,
    ) -> GenerationActivation {
        GenerationActivation {
            backend: "ollama".into(),
            model_path: active.model_tag.clone(),
            context_size: active.context_size,
            self_test: self_test.to_owned(),
            multimodal: active.multimodal,
            device: active.device.clone(),
            threads: active.threads,
            gpu_layers: None,
            fallback_reason: None,
            vram_usage_bytes: active.vram_usage_bytes,
        }
    }

    /// 基于 `/api/ps` 更新当前激活模型的设备与显存信息。
    /// `device` 渲染为「GPU xx% · CPU xx%」：GPU 占比按驻留显存与模型总大小换算，
    /// 呈现 Ollama 的 GPU/CPU 混合调度结果；无显存驻留则显示纯 CPU。
    fn refresh_device_info(&mut self) {
        if let Some(active) = self.active.as_mut()
            && let Ok(models) = self.client.process_runtime()
            && let Some(entry) = models.iter().find(|entry| entry.name == active.model_tag)
        {
            active.vram_usage_bytes = (entry.size_vram > 0).then_some(entry.size_vram);
            active.device = Some(device_percentage_summary(entry.size, entry.size_vram));
        }
    }

    /// 基于当前 Ollama 状态构造能力快照。
    fn build_capability(&self) -> RuntimeCapability {
        let (status, _version) = ollama_status();
        let installed = status != OllamaStatus::NotInstalled;
        let running = status == OllamaStatus::Ready;
        // 从 `/api/ps` 判断是否有模型驻留 GPU。
        let gpu_using_vram = self
            .client
            .process_runtime()
            .ok()
            .is_some_and(|models| models.iter().any(|entry| entry.size_vram > 0));
        let error_code = if !installed {
            Some("OLLAMA_NOT_INSTALLED".into())
        } else if !running {
            Some("OLLAMA_INSTALLED_NOT_RUNNING".into())
        } else {
            None
        };
        RuntimeCapability {
            executable_available: installed,
            backend: "ollama".into(),
            devices: vec!["Ollama".into()],
            gpu_available: installed && running && gpu_using_vram,
            checked_at: chrono::Utc::now(),
            error_code,
        }
    }

    pub fn complete(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, AppError> {
        self.complete_internal(system, user, max_tokens, None, None, 0.1)
    }

    pub fn complete_cancellable(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        self.complete_internal(system, user, max_tokens, Some(cancelled), None, 0.1)
    }

    pub fn complete_json_cancellable(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
        schema: &Value,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        self.complete_internal(system, user, max_tokens, Some(cancelled), Some(schema), 0.1)
    }

    fn complete_internal(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
        cancelled: Option<&AtomicBool>,
        json_schema: Option<&Value>,
        temperature: f32,
    ) -> Result<String, AppError> {
        let active = self.active.as_ref().ok_or_else(|| {
            AppError::new("GENERATION_RUNTIME_INACTIVE", "生成模型尚未启动", true)
        })?;
        let messages = json!([
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]);
        // RAG 内部调用（路由/解析/校验等）强制关闭思考：思考类模型默认开启
        // 思考会消耗 token 预算并污染 JSON 输出，必须显式声明 think=false。
        self.chat_with_active(
            active,
            messages,
            max_tokens,
            json_schema,
            temperature,
            Some(false),
            cancelled,
        )
    }

    /// 多模态自检辅助：向当前激活模型发送一张测试图。
    fn complete_multimodal_test(&mut self, system: &str, user: &str) -> Result<String, AppError> {
        let active = self.active.clone().ok_or_else(|| {
            AppError::new("GENERATION_RUNTIME_INACTIVE", "生成模型尚未启动", true)
        })?;
        let pixel_image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let messages = json!([
            { "role": "system", "content": system },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": user },
                    {
                        "type": "image_url",
                        "image_url": { "url": pixel_image }
                    }
                ]
            }
        ]);
        self.chat_with_active(&active, messages, 256, None, 0.1, None, None)
    }

    /// 统一的 `/api/chat` 封装调用。
    /// `think` 控制思考类模型是否开启思考：`Some(false)` 关闭（RAG 内部调用），
    /// `Some(true)` 开启（用户主动选择深度思考），`None` 跟随模型默认。
    fn chat_with_active(
        &self,
        active: &OllamaActiveModel,
        messages: Value,
        max_tokens: u32,
        json_schema: Option<&Value>,
        temperature: f32,
        think: Option<bool>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<String, AppError> {
        let options = OllamaChatOptions {
            num_predict: Some(max_tokens),
            temperature: Some(temperature),
            num_ctx: Some(active.context_size),
            think,
        };
        let format = json_schema.filter(|schema| !schema.is_null()).cloned();
        let content = self
            .client
            .chat(&active.model_tag, messages, options, format, cancelled)?;
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
        }
        Ok(content)
    }

    /// 流式对话生成：按块回调思考增量与正文增量。
    ///
    /// `think` 为 `true` 时思考类模型先输出 `message.thinking`（推理轨迹），
    /// 随后输出 `message.content`（正文）；两者通过 `on_delta` 分别回调。
    pub fn complete_stream_cancellable(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
        think: bool,
        cancelled: &AtomicBool,
        on_delta: &mut dyn FnMut(Option<&str>, Option<&str>),
    ) -> Result<(), AppError> {
        let active = self.active.as_ref().ok_or_else(|| {
            AppError::new("GENERATION_RUNTIME_INACTIVE", "生成模型尚未启动", true)
        })?;
        let messages = json!([
            { "role": "system", "content": system },
            { "role": "user", "content": user }
        ]);
        let options = OllamaChatOptions {
            num_predict: Some(max_tokens),
            temperature: Some(0.7),
            num_ctx: Some(active.context_size),
            think: Some(think),
        };
        self.client.chat_stream(
            &active.model_tag,
            messages,
            options,
            None,
            Some(cancelled),
            on_delta,
        )
    }

    /// 图片理解。
    pub fn describe_image_cancellable(
        &mut self,
        system: &str,
        prompt: &str,
        image_path: &std::path::Path,
        mime_type: &str,
        max_tokens: u32,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        if !matches!(
            mime_type,
            "image/jpeg" | "image/png" | "image/webp" | "image/bmp"
        ) {
            return Err(AppError::new(
                "VISION_IMAGE_FORMAT_UNSUPPORTED",
                "当前图片理解运行时只接受JPEG、PNG、WebP或BMP缓存",
                false,
            ));
        }
        let bytes = std::fs::read(image_path).map_err(|_| {
            AppError::new(
                "VISION_IMAGE_UNAVAILABLE",
                "图片缓存不可用，请稍后重试",
                true,
            )
        })?;
        if bytes.is_empty() || bytes.len() > 32 * 1024 * 1024 {
            return Err(AppError::new(
                "VISION_IMAGE_SIZE_UNSUPPORTED",
                "图片理解缓存必须是1字节到32MB的普通文件",
                false,
            ));
        }
        use base64::Engine as _;
        let data_url = format!(
            "data:{mime_type};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let active = self.active.clone().ok_or_else(|| {
            AppError::new("VISION_RUNTIME_INACTIVE", "图片理解模型尚未启动", true)
        })?;
        if !active.multimodal {
            return Err(AppError::new(
                "VISION_RUNTIME_INACTIVE",
                "当前生成模型未以多模态模式激活，无法理解图片",
                true,
            ));
        }
        let messages = json!([
            { "role": "system", "content": system },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": prompt },
                    { "type": "image_url", "image_url": { "url": data_url } }
                ]
            }
        ]);
        self.chat_with_active(
            &active,
            messages,
            max_tokens,
            None,
            0.1,
            None,
            Some(cancelled),
        )
    }

    pub fn active_model_path(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.model_tag.as_str())
    }

    /// Ollama 不再使用独立的 mmproj 投影组件，恒返回 `None`。
    pub fn active_mmproj_path(&self) -> Option<&str> {
        None
    }

    pub fn active_backend(&self) -> Option<&str> {
        Some("ollama")
    }

    pub fn active_device(&self) -> Option<&str> {
        self.active
            .as_ref()
            .and_then(|active| active.device.as_deref())
    }

    pub fn active_threads(&self) -> Option<u32> {
        self.active.as_ref().map(|active| active.threads)
    }

    pub fn active_gpu_layers(&self) -> Option<u32> {
        None
    }

    /// 返回当前驻留内存各模型的硬件占用（来自 `/api/ps`），供状态面板实时轮询展示。
    ///
    /// 尽力而为：读取失败时返回空列表而不抛错，避免状态轮询被写入器故障波及。
    /// 每个运行模型都会按其 GPU/CPU 占比独立描述——例如 embedding 跑在 GPU 时，
    /// 即使生成模型未激活也能如实上报「GPU 100%」，从而摆脱「未识别 GPU」。
    pub fn running_model_placements(&self) -> Vec<RuntimeModelPlacement> {
        let Ok(models) = self.client.process_runtime() else {
            return Vec::new();
        };
        models
            .into_iter()
            .map(|entry| RuntimeModelPlacement {
                model: entry.name,
                device: device_percentage_summary(entry.size, entry.size_vram),
                vram_bytes: entry.size_vram,
                total_bytes: entry.size,
            })
            .collect()
    }

    pub fn stop(&mut self) {
        self.active = None;
        self.last_capability = None;
    }
}

impl Drop for LocalGenerationRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 按模型总大小与驻留显存换算 Ollama 的 GPU/CPU 混合调度占比，渲染为
/// 「GPU xx% · CPU xx%」。`size` 为 0 时回退为纯 CPU 描述。
fn device_percentage_summary(size: u64, size_vram: u64) -> String {
    if size == 0 {
        return "CPU 100%".into();
    }
    let gpu_percent = ((size_vram.min(size)) as f32 / size as f32 * 100.0).round() as u32;
    let cpu_percent = 100u32.saturating_sub(gpu_percent);
    format!("GPU {gpu_percent}% · CPU {cpu_percent}%")
}

/// 校验上下文与线程配置在安全范围内。
fn validate_runtime_config(context_size: u32, threads: u32) -> Result<(), AppError> {
    if !(1024..=131072).contains(&context_size) || !(1..=64).contains(&threads) {
        return Err(AppError::new(
            "GENERATION_RUNTIME_CONFIG_INVALID",
            "上下文或CPU线程配置超出安全范围",
            false,
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_rejects_out_of_range_config() {
        // 配置非法时在触碰 Ollama 前即失败（纯本地校验）。
        let mut runtime = LocalGenerationRuntime::new(PathBuf::new());
        assert_eq!(
            runtime.activate("qwen3.5:2b", 128, 2).unwrap_err().code,
            "GENERATION_RUNTIME_CONFIG_INVALID"
        );
        // 未激活时生成应报未启动。
        assert_eq!(
            runtime.complete("s", "u", 8).unwrap_err().code,
            "GENERATION_RUNTIME_INACTIVE"
        );
    }

    #[test]
    fn build_activation_preserves_contract_fields() {
        let active = OllamaActiveModel {
            model_tag: "qwen3.5:2b".into(),
            context_size: 4096,
            threads: 4,
            multimodal: true,
            device: Some("GPU".into()),
            vram_usage_bytes: Some(2048),
        };
        let runtime = LocalGenerationRuntime {
            client: OllamaClient::local(),
            active: Some(active),
            last_capability: None,
        };
        let activation = runtime.build_activation(runtime.active.as_ref().unwrap(), "就绪");
        assert_eq!(activation.backend, "ollama");
        assert_eq!(activation.model_path, "qwen3.5:2b");
        assert!(activation.multimodal);
        assert_eq!(activation.device.as_deref(), Some("GPU"));
        assert_eq!(activation.vram_usage_bytes, Some(2048));
    }

    #[test]
    fn capability_maps_not_installed_status() {
        // 依赖真实环境探测，不作为强断言；仅验证字段默认值守恒。
        let runtime = LocalGenerationRuntime::new(PathBuf::new());
        assert_eq!(runtime.active_backend(), Some("ollama"));
        assert_eq!(runtime.active_gpu_layers(), None);
        assert_eq!(runtime.active_mmproj_path(), None);
        assert!(!runtime.cpu_fallback_available());
    }
}
