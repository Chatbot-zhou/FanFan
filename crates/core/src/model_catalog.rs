use serde::{Deserialize, Serialize};

use crate::{AppError, ModelFormat, ModelRole, ModelSource};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelEdition {
    pub edition_id: String,
    pub name: String,
    pub description: String,
    pub recommended_memory_gb: u32,
    pub download_size_bytes: u64,
    pub capabilities: Vec<String>,
    pub artifacts: Vec<DownloadArtifact>,
}

/// A user-facing, role-scoped entry in the verified model pool.  Unlike
/// `ModelEdition`, this contract describes one choice and may deliberately be
/// non-installable until its immutable files have been verified.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCatalogEntry {
    pub catalog_id: String,
    pub role: ModelRole,
    /// 模型家族名（如 "Qwen3"、"BGE"），同一家族的多个尺寸/量化版本在前端
    /// 聚合为一张卡片，由用户切换版本。
    pub family: String,
    pub name: String,
    pub model_id: String,
    pub description: String,
    pub strengths: Vec<String>,
    pub limitations: Vec<String>,
    pub download_size_bytes: Option<u64>,
    pub estimated_memory_gb: f32,
    pub estimated_vram_gb: Option<f32>,
    pub cpu_speed: String,
    pub license_name: String,
    pub recommended: bool,
    pub device_guidance: String,
    pub verification_status: String,
    pub install_edition_id: Option<String>,
    pub supported_sources: Vec<String>,
}

/// 能力档案：业务代码只能判断 `capabilities`，不得判断具体模型名 / model_id。
/// 这是「四档核心功能一致」的关键契约——轻量模式只是 reranker/asr 为 false，
/// 其余功能语义保持一致。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilityProfile {
    pub generation: bool,
    pub embedding: bool,
    pub reranker: bool,
    pub ocr: bool,
    pub asr: bool,
}

/// 硬件档案：描述该档的目标硬件，并作为推荐规则的依据（描述性 + 阈值）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HardwareProfile {
    pub min_ram_gb: u32,
    pub min_vram_gb: Option<u32>,
    pub description: String,
}

/// 生成运行时参数。四档先给保守默认值，后续真实 Benchmark 再调，不在这里过度优化。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationRuntimeConfig {
    pub context_length: u32,
    pub max_output_tokens: u32,
    pub threads: u32,
    pub batch_size: u32,
    /// `auto` / `gpu` / `cpu`。`auto` 表示交给 RuntimeManager 按实测能力决定。
    pub device_default: String,
    pub keep_alive_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmbeddingRuntimeConfig {
    pub device_default: String,
    pub batch_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RerankerRuntimeConfig {
    pub enabled: bool,
    pub device_default: String,
    pub batch_size: u32,
    pub top_k: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcrRuntimeProfile {
    pub worker_count: u32,
    pub device_default: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpeechRuntimeConfig {
    pub device_default: String,
}

/// 四档不仅是一份模型列表，还允许为每个 Runtime 配置不同参数。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeProfile {
    pub generation: GenerationRuntimeConfig,
    pub embedding: EmbeddingRuntimeConfig,
    pub reranker: RerankerRuntimeConfig,
    pub ocr: OcrRuntimeProfile,
    pub asr: SpeechRuntimeConfig,
}

/// 统一官方模型配置预设：普通用户直接选档，系统据档位组装整套模型组合。
/// 每个模型引用都是 `ModelCatalog` 的 catalog_id（通过「Preset → Model ID →
/// Model Catalog → Artifact / Runtime」链路解析），绝不内嵌下载 URL 或文件路径。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPreset {
    pub preset_id: String,
    /// UI 展示名（与内部 model_id / catalog_id 严格分离）。
    pub display_name: String,
    pub description: String,
    /// 各角色对应的 ModelCatalog catalog_id；`None` 表示该档不启用该能力。
    pub generation: String,
    pub embedding: String,
    pub reranker: Option<String>,
    pub asr: Option<String>,
    pub ocr: String,
    pub hardware_profile: HardwareProfile,
    pub capability_profile: CapabilityProfile,
    pub runtime_profile: RuntimeProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadFile {
    pub file_name: String,
    pub remote_path: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadArtifact {
    pub model_id: String,
    pub role: ModelRole,
    pub format: ModelFormat,
    pub source: ModelSource,
    pub repository_id: String,
    pub revision: String,
    pub file_name: String,
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub companion_files: Vec<DownloadFile>,
    pub license_name: String,
    pub query_prefix: Option<String>,
    pub max_length: Option<u32>,
}

impl DownloadArtifact {
    pub fn total_size_bytes(&self) -> u64 {
        self.size_bytes
            + self
                .companion_files
                .iter()
                .map(|file| file.size_bytes)
                .sum::<u64>()
    }

    pub fn files(&self) -> Vec<DownloadFile> {
        let mut files = vec![DownloadFile {
            file_name: self.file_name.clone(),
            remote_path: self.file_name.clone(),
            url: self.url.clone(),
            sha256: self.sha256.clone(),
            size_bytes: self.size_bytes,
        }];
        files.extend(self.companion_files.clone());
        files
    }
}

/// 组合一份 `RuntimeProfile`，避免四档 Preset 内重复书写整套嵌套参数。
/// 这里只给「保守默认值」，后续真实 Benchmark 再按档位微调，不在此过度优化。
fn make_runtime_profile(
    context_length: u32,
    max_output_tokens: u32,
    threads: u32,
    generation_batch_size: u32,
    reranker_enabled: bool,
    ocr_worker_count: u32,
) -> RuntimeProfile {
    RuntimeProfile {
        generation: GenerationRuntimeConfig {
            context_length,
            max_output_tokens,
            threads,
            batch_size: generation_batch_size,
            device_default: "auto".into(),
            keep_alive_seconds: 300,
        },
        embedding: EmbeddingRuntimeConfig {
            device_default: "auto".into(),
            batch_size: 256,
        },
        reranker: RerankerRuntimeConfig {
            enabled: reranker_enabled,
            device_default: "auto".into(),
            batch_size: 32,
            top_k: 20,
        },
        ocr: OcrRuntimeProfile {
            worker_count: ocr_worker_count,
            device_default: "cpu".into(),
        },
        asr: SpeechRuntimeConfig {
            device_default: "cpu".into(),
        },
    }
}

/// 官方模型预设的 schema 版本。未来升级 Qwen / Embedding / Reranker 型号时递增，
/// 便于按版本迁移 `selected_preset_id` 而不破坏旧数据。当前为第一版。
pub const MODEL_PRESET_VERSION: u32 = 1;

/// 唯一官方四档 ModelPreset 定义，业务层与 UI 不得在别处复制模型名称。
/// 每个模型引用都是 `built_in_model_catalog()` 的 catalog_id，通过「Preset →
/// catalog_id → Model Catalog → Artifact / Runtime」链路解析，不内嵌 URL / 路径。
pub fn built_in_model_presets() -> Vec<ModelPreset> {
    vec![
        // 轻量模式：8GB RAM，无独立 GPU 亦可。Reranker / ASR 缺失。
        ModelPreset {
            preset_id: "basic".into(),
            display_name: "轻量模式".into(),
            description: "最低资源占用，8GB 内存、无独立显卡也能完成基础搜索与 RAG。".into(),
            generation: "qwen3-5-0-8b-q4".into(),
            embedding: "bge-small-zh-int8".into(),
            reranker: None,
            asr: None,
            ocr: "ppocr-v6-small".into(),
            hardware_profile: HardwareProfile {
                min_ram_gb: 8,
                min_vram_gb: None,
                description: "8GB 内存，无独立 GPU 亦可运行".into(),
            },
            capability_profile: CapabilityProfile {
                generation: true,
                embedding: true,
                reranker: false,
                ocr: true,
                asr: false,
            },
            runtime_profile: make_runtime_profile(4096, 1024, 4, 512, false, 1),
        },
        // 标准模式：开发机与性能优化主锚点，R7 5800H + RTX 3050Ti 4GB + 16GB RAM。
        ModelPreset {
            preset_id: "smooth".into(),
            display_name: "标准模式".into(),
            description: "适合大多数配备独显的电脑，完整问答、智能检索、语音输入、增强 OCR。"
                .into(),
            generation: "qwen3-5-2b-q4".into(),
            embedding: "bge-small-zh-int8".into(),
            reranker: Some("bge-reranker-base-int8".into()),
            asr: Some("sensevoice-small".into()),
            ocr: "ppocr-v6-small".into(),
            hardware_profile: HardwareProfile {
                min_ram_gb: 16,
                min_vram_gb: Some(4),
                description: "16GB 内存 / 4GB 显存".into(),
            },
            capability_profile: CapabilityProfile {
                generation: true,
                embedding: true,
                reranker: true,
                ocr: true,
                asr: true,
            },
            runtime_profile: make_runtime_profile(8192, 2048, 8, 512, true, 2),
        },
        // 增强模式：更高问答质量与更稳定的跨文档检索。
        ModelPreset {
            preset_id: "balanced".into(),
            display_name: "增强模式".into(),
            description: "更高的问答质量与更稳定的 Query Understanding、跨文档检索与综合。".into(),
            generation: "qwen3-5-4b-q4".into(),
            embedding: "bge-m3".into(),
            reranker: Some("bge-reranker-base-int8".into()),
            asr: Some("sensevoice-small".into()),
            ocr: "ppocr-v6-medium".into(),
            hardware_profile: HardwareProfile {
                min_ram_gb: 32,
                min_vram_gb: Some(8),
                description: "32GB 内存 / 约 8GB 显存".into(),
            },
            capability_profile: CapabilityProfile {
                generation: true,
                embedding: true,
                reranker: true,
                ocr: true,
                asr: true,
            },
            runtime_profile: make_runtime_profile(16384, 3072, 8, 512, true, 2),
        },
        // 旗舰模式：翻翻最高质量本地配置。
        ModelPreset {
            preset_id: "high".into(),
            display_name: "旗舰模式".into(),
            description: "翻翻最高质量本地配置，适合 32GB+ 内存与 12GB+ 显存的工作站。".into(),
            generation: "qwen3-5-9b-q4".into(),
            embedding: "bge-m3".into(),
            reranker: Some("bge-reranker-base-int8".into()),
            asr: Some("sensevoice-small".into()),
            ocr: "ppocr-v6-medium".into(),
            hardware_profile: HardwareProfile {
                min_ram_gb: 32,
                min_vram_gb: Some(12),
                description: "32GB+ 内存 / 建议 12GB~16GB+ 显存".into(),
            },
            capability_profile: CapabilityProfile {
                generation: true,
                embedding: true,
                reranker: true,
                ocr: true,
                asr: true,
            },
            runtime_profile: make_runtime_profile(32768, 4096, 8, 256, true, 4),
        },
    ]
}

/// 依硬件档案返回推荐的四档 preset_id。规则保守：
/// 无独显或不足 16GB 内存 → 轻量模式；16GB+4GB VRAM → 标准模式；
/// 32GB+8GB VRAM → 增强模式；32GB+12GB VRAM → 旗舰模式。
/// 推荐不等于强制，用户始终可手动选择其他档。
pub fn recommended_preset_id(
    memory_total_gb: Option<u64>,
    gpu_memory_gb: Option<u64>,
) -> &'static str {
    let ram = memory_total_gb.unwrap_or(0);
    let vram = gpu_memory_gb.unwrap_or(0);
    if ram >= 32 && vram >= 12 {
        "high"
    } else if ram >= 32 && vram >= 8 {
        "balanced"
    } else if ram >= 16 && vram >= 4 {
        "smooth"
    } else {
        "basic"
    }
}

/// 按 preset_id 取官方四档之一，未知 id 返回 `None`。
pub fn preset_by_id(preset_id: &str) -> Option<ModelPreset> {
    built_in_model_presets()
        .into_iter()
        .find(|preset| preset.preset_id == preset_id)
}

/// 判断给定硬件是否足以承载某档（RAM 与 VRAM 均需满足其 min）。
/// 前端据此对「明显高于设备能力」的选择显示提示而非禁止。
pub fn preset_fits_hardware(
    preset: &ModelPreset,
    memory_total_gb: u64,
    gpu_memory_gb: Option<u64>,
) -> bool {
    memory_total_gb >= preset.hardware_profile.min_ram_gb as u64
        && match preset.hardware_profile.min_vram_gb {
            Some(min_vram) => gpu_memory_gb.is_some_and(|vram| vram >= min_vram as u64),
            None => true,
        }
}

impl ModelPreset {
    /// 返回该档需要下载/就绪的全部 catalog_id（按 generation/embedding/reranker/
    /// asr/ocr 顺序去重）。下载编排据此计算「已装/缺失」，避免按 Preset 复制一份。
    pub fn required_catalog_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = Vec::new();
        ids.push(self.generation.as_str());
        ids.push(self.embedding.as_str());
        if let Some(value) = &self.reranker {
            ids.push(value.as_str());
        }
        if let Some(value) = &self.asr {
            ids.push(value.as_str());
        }
        ids.push(self.ocr.as_str());
        ids
    }
}

/// Computes the hardware-based recommendation: at most one catalog entry per
/// role, chosen from what fits the reported RAM (with headroom) and VRAM.
/// When nothing fits, the lightest entry still gets recommended. Optional
/// roles (reranker, ocr) are never recommended.
pub fn recommended_catalog_ids(
    catalog: &[ModelCatalogEntry],
    memory_total_gb: Option<u64>,
    gpu_memory_gb: Option<u64>,
) -> Vec<String> {
    // Conservative fallbacks when hardware cannot be measured: assume a
    // modest 8 GB RAM / 4 GB VRAM machine instead of the lightest option.
    let memory_gb = memory_total_gb.unwrap_or(8) as f32;
    let vram_gb = gpu_memory_gb.unwrap_or(4) as f32;
    [
        ModelRole::Generation,
        ModelRole::Embedding,
        ModelRole::Vision,
    ]
    .into_iter()
    .filter_map(|role| recommend_for_role(catalog, role, memory_gb, vram_gb))
    .collect()
}

fn recommend_for_role(
    catalog: &[ModelCatalogEntry],
    role: ModelRole,
    memory_gb: f32,
    vram_gb: f32,
) -> Option<String> {
    let entries: Vec<&ModelCatalogEntry> = catalog
        .iter()
        .filter(|entry| entry.role == role && entry.install_edition_id.is_some())
        .collect();
    if entries.is_empty() {
        return None;
    }
    let fitting: Vec<&ModelCatalogEntry> = entries
        .iter()
        .copied()
        .filter(|entry| {
            memory_gb >= entry.estimated_memory_gb * 1.3
                && entry.estimated_vram_gb.is_none_or(|needs| vram_gb >= needs)
        })
        .collect();
    let pick = if fitting.is_empty() {
        // Nothing fits the reported hardware: recommend the lightest option.
        entries
            .iter()
            .copied()
            .min_by(|a, b| a.estimated_memory_gb.total_cmp(&b.estimated_memory_gb))
    } else {
        fitting.into_iter().max_by(|a, b| {
            a.estimated_memory_gb
                .total_cmp(&b.estimated_memory_gb)
                .then_with(|| {
                    b.estimated_vram_gb
                        .unwrap_or(f32::MAX)
                        .total_cmp(&a.estimated_vram_gb.unwrap_or(f32::MAX))
                })
        })
    };
    pick.map(|entry| entry.catalog_id.clone())
}

// The `recommended` flags in this catalog are intentionally all false: the
// desktop layer recomputes them from the user's actual hardware before the
// catalog reaches the UI.
pub fn built_in_model_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        catalog_entry(
            "bge-small-zh-int8",
            ModelRole::Embedding,
            "BGE",
            "BGE-small-zh-v1.5 · 默认",
            "bge-small-zh-v1.5-onnx-int8",
            "面向中文资料检索的轻量向量模型。",
            &["中文检索稳定", "索引速度快", "内存占用低"],
            &["细粒度语义区分弱于更大的向量模型"],
            Some(24_304_813),
            1.0,
            None,
            "快",
            "MIT",
            false,
            "所有受支持设备均推荐；更换后需要新建索引代际",
            Some("embedding_bge_small"),
            &["modelscope"],
        ),
        catalog_entry(
            "bge-reranker-base-int8",
            ModelRole::Reranker,
            "BGE Reranker",
            "BGE Reranker Base · 可选",
            "bge-reranker-base-onnx-int8",
            "对少量混合召回候选做相关性复核。",
            &["提升复杂查询排序", "不改变原始索引"],
            &["增加问答延迟", "低配置设备可不安装"],
            Some(296_335_457),
            2.0,
            None,
            "中等",
            "MIT",
            false,
            "可选；更重视速度时保持未配置",
            Some("reranker_bge_base_int8"),
            &["modelscope"],
        ),
        catalog_entry(
            "qwen3-5-0-8b-q4",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 0.8B",
            "Qwen3.5-0.8B-Q4_K_M",
            "新一代 Qwen3.5 入门档，原生多模态架构，低配置设备也能流畅问答。",
            &[
                "比 Qwen3 同档质量更高",
                "原生多模态可转作视觉模型",
                "CPU 响应快",
            ],
            &["回答质量受量化影响"],
            Some(532_517_120),
            1.5,
            Some(0.9),
            "快",
            "Apache-2.0",
            false,
            "8GB 内存即可流畅运行；低配设备作为 Qwen3 的升级起点",
            Some("generation_qwen3_5_0_8b_q4"),
            // 官方 Preset 统一只从 ModelScope 下载（source_provider = modelscope）。
            &["modelscope"],
        ),
        catalog_entry(
            "qwen3-5-2b-q4",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 2B",
            "Qwen3.5-2B-Q4_K_M",
            "新一代 Qwen3.5 轻量主流档，综合能力比 0.8B 明显更强，低配设备也能流畅问答。",
            &[
                "比 0.8B 质量更高",
                "原生多模态可转作视觉模型",
                "8GB 内存即可运行",
            ],
            &["回答质量受量化影响"],
            Some(1_280_835_840),
            3.0,
            Some(1.8),
            "快",
            "Apache-2.0",
            false,
            "8GB 内存即可流畅运行；低配设备追求更好质量的升级档",
            Some("generation_qwen3_5_2b_q4"),
            &["modelscope"],
        ),
        catalog_entry(
            "qwen3-5-4b-q4",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 4B",
            "Qwen3.5-4B-Q4_K_M",
            "新一代 Qwen3.5 主流档，跨文件综合与复杂追问更强。",
            &[
                "复杂问题质量更高",
                "原生多模态可转作视觉模型",
                "长上下文组织更好",
            ],
            &["CPU 推理较慢", "需要更多内存"],
            Some(2_740_937_888),
            6.5,
            Some(3.5),
            "较慢",
            "Apache-2.0",
            false,
            "建议 12GB 以上内存；4GB 显存采用部分 GPU 卸载",
            Some("generation_qwen3_5_4b_q4"),
            &["modelscope"],
        ),
        catalog_entry(
            "qwen3-5-9b-q4",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 9B",
            "Qwen3.5-9B-Q4_K_M",
            "新一代 Qwen3.5 大参数档，接近旗舰水平的中文问答。",
            &["本地最强问答质量", "原生多模态可转作视觉模型"],
            &["下载 5.7GB", "需要 16GB 以上内存"],
            Some(5_680_522_464),
            12.0,
            Some(7.0),
            "慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_qwen3_5_9b_q4"),
            &["modelscope"],
        ),
        catalog_entry(
            "bge-m3",
            ModelRole::Embedding,
            "BGE",
            "BGE M3 · 多语言",
            "bge-m3-int8",
            "BGE-M3 多语言向量模型，中文与英文资料库均可获得稳定召回。",
            &[
                "多语言检索稳定",
                "召回质量高于 BGE-small",
                "官方 ONNX 双源下载",
            ],
            &["索引更慢且占用更多空间", "内存占用高于 BGE-small"],
            Some(585_539_515),
            2.5,
            None,
            "中等",
            "MIT",
            false,
            "建议 8GB 以上内存；更换后需要新建索引代际",
            Some("embedding_bge_m3"),
            &["modelscope"],
        ),
        catalog_entry(
            "sensevoice-small",
            ModelRole::Asr,
            "SenseVoice",
            "SenseVoice Small · 中文语音识别",
            "sensevoice-small",
            "标准模式及以上预设的中文语音识别组件，仅在用户明确录音时运行。",
            &["中文识别稳定", "体积小", "无需额外 VAD"],
            &["仅提供 ModelScope 源"],
            Some(239_549_735),
            1.0,
            None,
            "快",
            "MIT",
            false,
            "标准模式及以上预设组件；ModelScope 单源",
            Some("asr_sensevoice"),
            &["modelscope"],
        ),
        catalog_entry(
            "ppocr-v6-small",
            ModelRole::Ocr,
            "PP-OCRv6",
            "PP-OCRv6 Small · 中文文字识别",
            "PP-OCRv6-small",
            "轻量模式预设的轻量 OCR 组件。",
            &["轻量中文 OCR"],
            &["仅提供 ModelScope 源"],
            Some(31_824_478),
            1.0,
            None,
            "快",
            "Apache-2.0",
            false,
            "轻量模式预设组件；ModelScope 单源",
            Some("ocr_ppocrv6_small"),
            &["modelscope"],
        ),
        catalog_entry(
            "ppocr-v6-medium",
            ModelRole::Ocr,
            "PP-OCRv6",
            "PP-OCRv6 Medium · 中文文字识别",
            "PP-OCRv6-medium",
            "标准模式与增强模式预设的中文 OCR 组件。",
            &["中文 OCR 稳定"],
            &["仅提供 ModelScope 源"],
            Some(138_909_938),
            1.5,
            None,
            "中等",
            "Apache-2.0",
            false,
            "标准模式/增强模式预设组件；ModelScope 单源",
            Some("ocr_ppocrv6_medium"),
            &["modelscope"],
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn catalog_entry(
    catalog_id: &str,
    role: ModelRole,
    family: &str,
    name: &str,
    model_id: &str,
    description: &str,
    strengths: &[&str],
    limitations: &[&str],
    download_size_bytes: Option<u64>,
    estimated_memory_gb: f32,
    estimated_vram_gb: Option<f32>,
    cpu_speed: &str,
    license_name: &str,
    recommended: bool,
    device_guidance: &str,
    install_edition_id: Option<&str>,
    supported_sources: &[&str],
) -> ModelCatalogEntry {
    ModelCatalogEntry {
        catalog_id: catalog_id.into(),
        role,
        family: family.into(),
        name: name.into(),
        model_id: model_id.into(),
        description: description.into(),
        strengths: strengths.iter().map(|value| (*value).into()).collect(),
        limitations: limitations.iter().map(|value| (*value).into()).collect(),
        download_size_bytes,
        estimated_memory_gb,
        estimated_vram_gb,
        cpu_speed: cpu_speed.into(),
        license_name: license_name.into(),
        recommended,
        device_guidance: device_guidance.into(),
        verification_status: if install_edition_id.is_some() {
            "verified".into()
        } else {
            "local_import_only".into()
        },
        install_edition_id: install_edition_id.map(str::to_owned),
        supported_sources: supported_sources
            .iter()
            .map(|value| (*value).into())
            .collect(),
    }
}

pub fn model_edition_by_id(edition_id: &str, source: &str) -> Result<ModelEdition, AppError> {
    resolved_model_edition(edition_id, source)
}

pub fn locked_download_artifact(model_id: &str, source: ModelSource) -> Option<DownloadArtifact> {
    let source_name = match source {
        ModelSource::Huggingface => "huggingface",
        ModelSource::Modelscope => "modelscope",
        ModelSource::LocalImport => return None,
    };
    [
        "embedding_bge_small",
        "reranker_bge_base_int8",
        // 2026-08-19 锁定：PP-OCRv6 与 SenseVoice（ModelScope 单源）
        "ocr_ppocrv6_small",
        "ocr_ppocrv6_medium",
        "asr_sensevoice",
        "generation_qwen3_5_0_8b_q4",
        "generation_qwen3_5_2b_q4",
        "generation_qwen3_5_4b_q4",
        "generation_qwen3_5_9b_q4",
        "embedding_bge_m3",
    ]
    .into_iter()
    .filter_map(|edition_id| resolved_model_edition(edition_id, source_name).ok())
    .flat_map(|edition| edition.artifacts)
    .find(|artifact| artifact.model_id == model_id)
}

fn resolved_model_edition(edition_id: &str, source: &str) -> Result<ModelEdition, AppError> {
    let source = match source {
        "huggingface" => ModelSource::Huggingface,
        "modelscope" => ModelSource::Modelscope,
        _ => {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_SOURCE_UNAVAILABLE",
                "官方模型当前只允许 ModelScope 源（本地导入除外）",
                false,
            ));
        }
    };
    match edition_id {
        "embedding_bge_small" => Ok(edition(
            edition_id,
            "BGE-small-zh-v1.5",
            "仅安装中文 Embedding；启用后建立新的向量索引代际。",
            8,
            vec![embedding_artifact(source)],
            &["embedding"],
        )),
        "reranker_bge_base_int8" => {
            // 2026-08-13: reranker 不再强制 Hugging Face 源——魔搭上有同模型的
            // ONNX 导出（Xenova/bge-reranker-base），国内网络可直连下载。
            Ok(edition(
                edition_id,
                "BGE Reranker Base",
                "安装可选的候选重排模型。",
                10,
                vec![reranker_artifact(source)],
                &["reranker"],
            ))
        }
        "ocr_ppocrv6_small" => {
            require_modelscope(source)?;
            Ok(edition(
                edition_id,
                "PP-OCRv6 Small",
                "安装轻量中文 OCR（检测、方向分类、识别与词典）。",
                4,
                vec![ocr_v6_artifact(source, "small")],
                &["ocr"],
            ))
        }
        "ocr_ppocrv6_medium" => {
            require_modelscope(source)?;
            Ok(edition(
                edition_id,
                "PP-OCRv6 Medium",
                "安装更稳的中文 OCR（检测、方向分类、识别与词典）。",
                5,
                vec![ocr_v6_artifact(source, "medium")],
                &["ocr"],
            ))
        }
        "asr_sensevoice" => {
            require_modelscope(source)?;
            Ok(edition(
                edition_id,
                "SenseVoice Small",
                "安装 SenseVoice-Small 中文语音识别（主模型+词表，无需 VAD）。",
                3,
                vec![asr_sensevoice_artifact(source)],
                &["asr"],
            ))
        }
        "generation_qwen3_5_0_8b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 0.8B",
            "安装 Qwen3.5 0.8B Q4_K_M 问答基础模型。",
            8,
            vec![qwen35_generation_artifact(source, "0.8B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_qwen3_5_2b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 2B",
            "安装 Qwen3.5 2B Q4_K_M 问答基础模型。",
            8,
            vec![qwen35_generation_artifact(source, "2B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_qwen3_5_4b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 4B",
            "安装 Qwen3.5 4B Q4_K_M 问答基础模型。",
            12,
            vec![qwen35_generation_artifact(source, "4B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_qwen3_5_9b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 9B",
            "安装 Qwen3.5 9B Q4_K_M 问答基础模型。",
            16,
            vec![qwen35_generation_artifact(source, "9B", "Q4_K_M")],
            &["generation"],
        )),
        "embedding_bge_m3" => Ok(edition(
            edition_id,
            "BGE-M3",
            "安装多语言向量模型 BGE-M3 ONNX；启用后建立新的向量索引代际。",
            8,
            vec![bge_m3_artifact(source)],
            &["embedding"],
        )),
        _ => Err(AppError::new(
            "MODEL_EDITION_NOT_FOUND",
            "模型版本不存在",
            false,
        )),
    }
}

fn edition(
    edition_id: &str,
    name: &str,
    description: &str,
    recommended_memory_gb: u32,
    artifacts: Vec<DownloadArtifact>,
    capabilities: &[&str],
) -> ModelEdition {
    let download_size_bytes = artifacts
        .iter()
        .map(DownloadArtifact::total_size_bytes)
        .sum();
    ModelEdition {
        edition_id: edition_id.into(),
        name: name.into(),
        description: description.into(),
        recommended_memory_gb,
        download_size_bytes,
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        artifacts,
    }
}

fn require_modelscope(source: ModelSource) -> Result<(), AppError> {
    if source == ModelSource::Modelscope {
        Ok(())
    } else {
        Err(AppError::new(
            "MODEL_DOWNLOAD_SOURCE_UNAVAILABLE",
            "这个组件当前只有通过校验的 ModelScope 固定版本",
            true,
        ))
    }
}

#[derive(Clone, Copy)]
struct DownloadSourceSpec {
    repository_id: &'static str,
    revision: &'static str,
    file_name: &'static str,
    sha256: &'static str,
    size_bytes: u64,
}

fn generation_artifact(
    source: ModelSource,
    model_id: &str,
    huggingface: DownloadSourceSpec,
    modelscope: DownloadSourceSpec,
) -> DownloadArtifact {
    let selected = match source {
        ModelSource::Huggingface => huggingface,
        ModelSource::Modelscope => modelscope,
        ModelSource::LocalImport => unreachable!("download catalog has no local source"),
    };
    DownloadArtifact {
        model_id: model_id.to_owned(),
        role: ModelRole::Generation,
        format: ModelFormat::Gguf,
        source,
        repository_id: selected.repository_id.to_owned(),
        revision: selected.revision.to_owned(),
        file_name: selected.file_name.to_owned(),
        url: artifact_url(
            source,
            selected.repository_id,
            selected.revision,
            selected.file_name,
        ),
        sha256: selected.sha256.to_owned(),
        size_bytes: selected.size_bytes,
        companion_files: Vec::new(),
        license_name: "Apache-2.0".into(),
        query_prefix: None,
        max_length: None,
    }
}

fn src_spec(
    repository_id: &'static str,
    revision: &'static str,
    file_name: &'static str,
    sha256: &'static str,
    size_bytes: u64,
) -> DownloadSourceSpec {
    DownloadSourceSpec {
        repository_id,
        revision,
        file_name,
        sha256,
        size_bytes,
    }
}

fn qwen35_generation_artifact(source: ModelSource, size: &str, quant: &str) -> DownloadArtifact {
    let (hf, ms) = qwen35_specs(size, quant);
    generation_artifact(source, &format!("Qwen3.5-{size}-{quant}"), hf, ms)
}

fn qwen35_specs(size: &str, quant: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match (size, quant) {
        ("0.8B", "Q4_K_M") => (
            src_spec(
                "unsloth/Qwen3.5-0.8B-GGUF",
                "6ab461498e2023f6e3c1baea90a8f0fe38ab64d0",
                "Qwen3.5-0.8B-Q4_K_M.gguf",
                "bd258782e35f7f458f8aced1adc053e6e92e89bc735ba3be89d38a06121dc517",
                532_517_120,
            ),
            src_spec(
                "unsloth/Qwen3.5-0.8B-GGUF",
                "88467eb7c8e3b6e7894c794f373050d4dbc6ae8a",
                "Qwen3.5-0.8B-Q4_K_M.gguf",
                "bd258782e35f7f458f8aced1adc053e6e92e89bc735ba3be89d38a06121dc517",
                532_517_120,
            ),
        ),
        ("0.8B", "Q8_0") => (
            src_spec(
                "unsloth/Qwen3.5-0.8B-GGUF",
                "6ab461498e2023f6e3c1baea90a8f0fe38ab64d0",
                "Qwen3.5-0.8B-Q8_0.gguf",
                "0ad885ffd4bb022fc4f0d33a3308fa108ef8613159d3b3a67e23abca056b7a6c",
                811_843_840,
            ),
            src_spec(
                "unsloth/Qwen3.5-0.8B-GGUF",
                "88467eb7c8e3b6e7894c794f373050d4dbc6ae8a",
                "Qwen3.5-0.8B-Q8_0.gguf",
                "0ad885ffd4bb022fc4f0d33a3308fa108ef8613159d3b3a67e23abca056b7a6c",
                811_843_840,
            ),
        ),
        ("2B", "Q4_K_M") => (
            src_spec(
                "unsloth/Qwen3.5-2B-GGUF",
                "f6d5376be1edb4d416d56da11e5397a961aca8ae",
                "Qwen3.5-2B-Q4_K_M.gguf",
                "aaf42c8b7c3cab2bf3d69c355048d4a0ee9973d48f16c731c0520ee914699223",
                1_280_835_840,
            ),
            src_spec(
                "unsloth/Qwen3.5-2B-GGUF",
                "90057e31161eb95cc0bc1413c4f53b44de9b49c8",
                "Qwen3.5-2B-Q4_K_M.gguf",
                "aaf42c8b7c3cab2bf3d69c355048d4a0ee9973d48f16c731c0520ee914699223",
                1_280_835_840,
            ),
        ),
        ("2B", "Q8_0") => (
            src_spec(
                "unsloth/Qwen3.5-2B-GGUF",
                "f6d5376be1edb4d416d56da11e5397a961aca8ae",
                "Qwen3.5-2B-Q8_0.gguf",
                "1b04acba824817554f4ce23639bc8495ff70453b8fcb047900c731521021f2c1",
                2_012_012_800,
            ),
            src_spec(
                "unsloth/Qwen3.5-2B-GGUF",
                "90057e31161eb95cc0bc1413c4f53b44de9b49c8",
                "Qwen3.5-2B-Q8_0.gguf",
                "1b04acba824817554f4ce23639bc8495ff70453b8fcb047900c731521021f2c1",
                2_012_012_800,
            ),
        ),
        ("4B", "Q4_K_M") => (
            src_spec(
                "unsloth/Qwen3.5-4B-GGUF",
                "e87f176479d0855a907a41277aca2f8ee7a09523",
                "Qwen3.5-4B-Q4_K_M.gguf",
                "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4",
                2_740_937_888,
            ),
            src_spec(
                "unsloth/Qwen3.5-4B-GGUF",
                "167b4afc359863325cb4164418c715421b4e9118",
                "Qwen3.5-4B-Q4_K_M.gguf",
                "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4",
                2_740_937_888,
            ),
        ),
        ("4B", "Q8_0") => (
            src_spec(
                "unsloth/Qwen3.5-4B-GGUF",
                "e87f176479d0855a907a41277aca2f8ee7a09523",
                "Qwen3.5-4B-Q8_0.gguf",
                "10cc391b403021dd11c614679d2fd92f611c3681d29e29651b717316965d61e1",
                4_482_403_488,
            ),
            src_spec(
                "unsloth/Qwen3.5-4B-GGUF",
                "167b4afc359863325cb4164418c715421b4e9118",
                "Qwen3.5-4B-Q8_0.gguf",
                "10cc391b403021dd11c614679d2fd92f611c3681d29e29651b717316965d61e1",
                4_482_403_488,
            ),
        ),
        ("9B", "Q4_K_M") => (
            src_spec(
                "unsloth/Qwen3.5-9B-GGUF",
                "3885219b6810b007914f3a7950a8d1b469d598a5",
                "Qwen3.5-9B-Q4_K_M.gguf",
                "03b74727a860a56338e042c4420bb3f04b2fec5734175f4cb9fa853daf52b7e8",
                5_680_522_464,
            ),
            src_spec(
                "unsloth/Qwen3.5-9B-GGUF",
                "ae90f0d1c1be2b9250b0ef68265615f6fe3c777b",
                "Qwen3.5-9B-Q4_K_M.gguf",
                "03b74727a860a56338e042c4420bb3f04b2fec5734175f4cb9fa853daf52b7e8",
                5_680_522_464,
            ),
        ),
        ("9B", "Q8_0") => (
            src_spec(
                "unsloth/Qwen3.5-9B-GGUF",
                "3885219b6810b007914f3a7950a8d1b469d598a5",
                "Qwen3.5-9B-Q8_0.gguf",
                "809626574d0cb43d4becfa56169980da2bb448f2299270f7be443cb89d0a6ae4",
                9_527_502_048,
            ),
            src_spec(
                "unsloth/Qwen3.5-9B-GGUF",
                "ae90f0d1c1be2b9250b0ef68265615f6fe3c777b",
                "Qwen3.5-9B-Q8_0.gguf",
                "809626574d0cb43d4becfa56169980da2bb448f2299270f7be443cb89d0a6ae4",
                9_527_502_048,
            ),
        ),
        _ => unreachable!("unknown Qwen3.5 size/quant"),
    }
}

/// BGE-M3：Xenova 的 transformers.js ONNX 导出（int8 自包含），双源，支持多语言。
fn bge_m3_artifact(source: ModelSource) -> DownloadArtifact {
    const REPOSITORY: &str = "Xenova/bge-m3";
    let revision = match source {
        ModelSource::Huggingface => "4de13258303883538bd53b696b452bf8099f0858",
        ModelSource::Modelscope => "fcfa0e19ea3493a798eedbdafbb31bb71088e01c",
        ModelSource::LocalImport => unreachable!("download catalog has no local source"),
    };
    DownloadArtifact {
        model_id: "bge-m3-int8".into(),
        role: ModelRole::Embedding,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: revision.into(),
        file_name: "model_int8.onnx".into(),
        url: artifact_url(source, REPOSITORY, revision, "onnx/model_int8.onnx"),
        sha256: "a206e10e995aa2a833924bcd725ba5dd6c3425cd34bac3cf2b5677cd2a1c51d6".into(),
        size_bytes: 568_456_694,
        companion_files: vec![DownloadFile {
            file_name: "tokenizer.json".into(),
            remote_path: "tokenizer.json".into(),
            url: artifact_url(source, REPOSITORY, revision, "tokenizer.json"),
            sha256: "6710678b12670bc442b99edc952c4d996ae309a7020c1fa0096dd245c2faf790".into(),
            size_bytes: 17_082_821,
        }],
        license_name: "MIT".into(),
        query_prefix: None,
        max_length: Some(8192),
    }
}

fn reranker_artifact(source: ModelSource) -> DownloadArtifact {
    // 2026-08-13: 从 onnx-community/bge-reranker-base-ONNX（仅 HF，镜像下载
    // 反复失败）切换为 Xenova/bge-reranker-base——同一模型（bge-reranker-base）
    // 的 transformers.js ONNX 导出，魔搭与 HF 双源直连可下。格式（model_quantized
    // .onnx + tokenizer.json）与推理输入输出与旧版一致。两个文件已在本机
    // 下载并核对 sha256（2026-08-13）。
    const REPOSITORY: &str = "Xenova/bge-reranker-base";
    const REVISION: &str = "master";
    DownloadArtifact {
        model_id: "bge-reranker-base-onnx-int8".into(),
        role: ModelRole::Reranker,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: REVISION.into(),
        file_name: "model_quantized.onnx".into(),
        url: artifact_url(source, REPOSITORY, REVISION, "onnx/model_quantized.onnx"),
        sha256: "dd98f3e67837d23210a6b7550c08cced4f61845b940ac45be3565840a10f3244".into(),
        size_bytes: 279_301_077,
        companion_files: vec![DownloadFile {
            file_name: "tokenizer.json".into(),
            remote_path: "tokenizer.json".into(),
            url: artifact_url(source, REPOSITORY, REVISION, "tokenizer.json"),
            sha256: "48564c5c7d3fa64d85d95e65414a542385f88b0f128fd8d4163fd7a57f2be05c".into(),
            size_bytes: 17_098_079,
        }],
        license_name: "MIT".into(),
        query_prefix: None,
        max_length: Some(512),
    }
}

/// PP-OCRv6 官方 ONNX（small/medium 两档），源为 RapidAI/RapidOCR（ModelScope，
/// HF 无同源镜像）。sha256/size 均经 ModelScope API 与本地下载双重核对
/// （2026-08-19），并已用 RapidOCR 3.9.2 端到端识别验证。分类器固定复用
/// PP-OCRv4 mobile CLS；词典 small/medium 同用 ppocrv6_dict.txt。
fn ocr_v6_artifact(source: ModelSource, size: &str) -> DownloadArtifact {
    const REPOSITORY: &str = "RapidAI/RapidOCR";
    const REVISION: &str = "master";
    let (det_sha256, det_bytes, rec_sha256, rec_bytes) = match size {
        "small" => (
            "090f04abcd9d9a7498bc4ebf677e4cb9bdce1fe4197ddb7e529f1ef44e1ff94f",
            9_929_594,
            "6f327246b50388f3c176ae304bd95767ea6dc0c9ae92153ef8cbe210b3c14884",
            21_234_383,
        ),
        "medium" => (
            "92078b7355007ccfffcd4c8cd441a3afd4538904d06881b29a155e1e679907c2",
            62_119_454,
            "eef444829dbbe18d7fea59a3f6eb75647518d2b3a9568d27c92e42940204894b",
            76_629_984,
        ),
        _ => unreachable!("ocr_v6_artifact supports only small/medium"),
    };
    let det_file = format!("PP-OCRv6_det_{size}.onnx");
    let rec_file = format!("PP-OCRv6_rec_{size}.onnx");
    let det_path = format!("onnx/PP-OCRv6/det/{det_file}");
    let rec_path = format!("onnx/PP-OCRv6/rec/{rec_file}");
    DownloadArtifact {
        model_id: format!("PP-OCRv6-{size}"),
        role: ModelRole::Ocr,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: REVISION.into(),
        file_name: rec_file.clone(),
        url: artifact_url(source, REPOSITORY, REVISION, &rec_path),
        sha256: rec_sha256.into(),
        size_bytes: rec_bytes,
        companion_files: vec![
            DownloadFile {
                file_name: det_file.clone(),
                remote_path: det_path.clone(),
                url: artifact_url(source, REPOSITORY, REVISION, &det_path),
                sha256: det_sha256.into(),
                size_bytes: det_bytes,
            },
            DownloadFile {
                file_name: "ch_ppocr_mobile_v2.0_cls_mobile.onnx".into(),
                remote_path: "onnx/PP-OCRv4/cls/ch_ppocr_mobile_v2.0_cls_mobile.onnx".into(),
                url: artifact_url(
                    source,
                    REPOSITORY,
                    REVISION,
                    "onnx/PP-OCRv4/cls/ch_ppocr_mobile_v2.0_cls_mobile.onnx",
                ),
                sha256: "e47acedf663230f8863ff1ab0e64dd2d82b838fceb5957146dab185a89d6215c".into(),
                size_bytes: 585_532,
            },
            DownloadFile {
                file_name: "ppocrv6_dict.txt".into(),
                remote_path: "paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt".into(),
                url: artifact_url(
                    source,
                    REPOSITORY,
                    REVISION,
                    "paddle/PP-OCRv6/rec/PP-OCRv6_rec_small/ppocrv6_dict.txt",
                ),
                sha256: "b5f2bfe2bdd9448429e3e82b51c789775d9b42f2403d082b00662eb77e401c5d".into(),
                size_bytes: 74_947,
            },
        ],
        license_name: "Apache-2.0".into(),
        query_prefix: None,
        max_length: None,
    }
}

/// SenseVoice-Small 中文语音识别（int8），源为 chriscrs/sherpa-onnx-sense-voice
/// -zh-en-ja-ko-yue-int8-2024-07-17（ModelScope，HF 下载超时故仅锁单源）。
/// sha256/size 经 ModelScope API 与本地下载双重核对（2026-08-19）。运行时按
/// `arch = sensevoice` 分支加载，无需 Silero VAD。
fn asr_sensevoice_artifact(source: ModelSource) -> DownloadArtifact {
    const REPOSITORY: &str = "chriscrs/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17";
    const REVISION: &str = "master";
    DownloadArtifact {
        model_id: "sensevoice-small".into(),
        role: ModelRole::Asr,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: REVISION.into(),
        file_name: "model.int8.onnx".into(),
        url: artifact_url(source, REPOSITORY, REVISION, "model.int8.onnx"),
        sha256: "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51".into(),
        size_bytes: 239_233_841,
        companion_files: vec![DownloadFile {
            file_name: "tokens.txt".into(),
            remote_path: "tokens.txt".into(),
            url: artifact_url(source, REPOSITORY, REVISION, "tokens.txt"),
            sha256: "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc".into(),
            size_bytes: 315_894,
        }],
        license_name: "MIT".into(),
        query_prefix: None,
        max_length: None,
    }
}

fn embedding_artifact(source: ModelSource) -> DownloadArtifact {
    const REPOSITORY: &str = "onnx-community/bge-small-zh-v1.5-ONNX";
    const HF_REVISION: &str = "9507db33464b5da99a532ac26b2a251767cbc62b";
    const MS_REVISION: &str = "5b414c2c2d177d066e7bdc32b6ad2a4518a59333";
    let revision = match source {
        ModelSource::Huggingface => HF_REVISION,
        ModelSource::Modelscope => MS_REVISION,
        ModelSource::LocalImport => unreachable!("download catalog has no local source"),
    };
    let primary_path = "onnx/model_quantized.onnx";
    let companion_specs = [
        (
            "model_quantized.onnx_data",
            "onnx/model_quantized.onnx_data",
            "952623481ca8beea884e3d3c9ecaf8a3c7bf1d0c21de29e970cd31af9d37a90b",
            23_774_208,
        ),
        (
            "tokenizer.json",
            "tokenizer.json",
            "3d09c84ebd10306706a79a8276b3ab736a40d8ec03251c7639f4e52c3a1a4f8e",
            362_603,
        ),
    ];
    DownloadArtifact {
        model_id: "bge-small-zh-v1.5-onnx-int8".into(),
        role: ModelRole::Embedding,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: revision.into(),
        file_name: "model_quantized.onnx".into(),
        url: artifact_url(source, REPOSITORY, revision, primary_path),
        sha256: "99a6e522710c00220c89f8c52e0cc5aa09d4cbb1c34c0e932eab3a9dfdc65df3".into(),
        size_bytes: 168_002,
        companion_files: companion_specs
            .into_iter()
            .map(
                |(file_name, remote_path, sha256, size_bytes)| DownloadFile {
                    file_name: file_name.into(),
                    remote_path: remote_path.into(),
                    url: artifact_url(source, REPOSITORY, revision, remote_path),
                    sha256: sha256.into(),
                    size_bytes,
                },
            )
            .collect(),
        license_name: "MIT".into(),
        query_prefix: Some("为这个句子生成表示以用于检索相关文章：".into()),
        max_length: Some(512),
    }
}

fn artifact_url(source: ModelSource, repository_id: &str, revision: &str, path: &str) -> String {
    match source {
        // HF host is overridable via FANFAN_HF_MIRROR (e.g. "hf-mirror.com" for
        // networks that cannot reach huggingface.co directly). Unset = unchanged.
        ModelSource::Huggingface => {
            let host =
                std::env::var("FANFAN_HF_MIRROR").unwrap_or_else(|_| "huggingface.co".to_string());
            format!("https://{host}/{repository_id}/resolve/{revision}/{path}?download=true")
        }
        ModelSource::Modelscope => format!(
            "https://modelscope.cn/api/v1/models/{repository_id}/repo?Revision={revision}&FilePath={path}"
        ),
        ModelSource::LocalImport => unreachable!("download catalog has no local source"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_based_recommendation_fits_ram_and_vram() {
        let catalog = built_in_model_catalog();
        // 8 GB RAM, unknown GPU: Qwen3.5 2B Q4 (3.0 GB) is the largest generation
        // that fits 8 GB (4B needs 8.5 GB with headroom and 9B needs 16 GB);
        // BGE-M3 (2.5 GB) is the largest fitting embedding. The trimmed catalog
        // ships no vision model, so only a generation and an embedding are picked.
        let ids = recommended_catalog_ids(&catalog, Some(8), None);
        assert_eq!(ids, vec!["qwen3-5-2b-q4".to_owned(), "bge-m3".to_owned()]);
        // 16 GB RAM + 8 GB VRAM: Qwen3.5 9B Q4 (12.0 GB memory, 7.0 GB VRAM) is
        // the largest generation that fits both memory and VRAM.
        let ids = recommended_catalog_ids(&catalog, Some(16), Some(8));
        assert_eq!(ids, vec!["qwen3-5-9b-q4".to_owned(), "bge-m3".to_owned()]);
        // 4 GB RAM: Qwen3.5 2B Q4 (3.0 GB, needs 3.9 GB with headroom) is the
        // largest generation that fits; BGE-M3 (2.5 GB, needs 3.25 GB) also fits.
        let ids = recommended_catalog_ids(&catalog, Some(4), None);
        assert_eq!(ids, vec!["qwen3-5-2b-q4".to_owned(), "bge-m3".to_owned()]);
    }

    #[test]
    fn four_official_presets_have_correct_model_mappings() {
        let presets = built_in_model_presets();
        assert_eq!(presets.len(), 4);

        let basic = preset_by_id("basic").unwrap();
        assert_eq!(basic.display_name, "轻量模式");
        assert_eq!(basic.generation, "qwen3-5-0-8b-q4");
        assert_eq!(basic.embedding, "bge-small-zh-int8");
        assert_eq!(basic.reranker, None);
        assert_eq!(basic.asr, None);
        assert_eq!(basic.ocr, "ppocr-v6-small");
        assert!(basic.capability_profile.generation);
        assert!(basic.capability_profile.embedding);
        assert!(!basic.capability_profile.reranker);
        assert!(basic.capability_profile.ocr);
        assert!(!basic.capability_profile.asr);

        let smooth = preset_by_id("smooth").unwrap();
        assert_eq!(smooth.generation, "qwen3-5-2b-q4");
        assert_eq!(smooth.embedding, "bge-small-zh-int8");
        assert_eq!(smooth.reranker.as_deref(), Some("bge-reranker-base-int8"));
        assert_eq!(smooth.asr.as_deref(), Some("sensevoice-small"));
        assert_eq!(smooth.ocr, "ppocr-v6-small");
        assert!(smooth.capability_profile.reranker);
        assert!(smooth.capability_profile.asr);

        let balanced = preset_by_id("balanced").unwrap();
        assert_eq!(balanced.generation, "qwen3-5-4b-q4");
        assert_eq!(balanced.embedding, "bge-m3");
        assert_eq!(balanced.reranker.as_deref(), Some("bge-reranker-base-int8"));
        assert_eq!(balanced.ocr, "ppocr-v6-medium");

        let high = preset_by_id("high").unwrap();
        assert_eq!(high.generation, "qwen3-5-9b-q4");
        assert_eq!(high.embedding, "bge-m3");
        assert_eq!(high.reranker.as_deref(), Some("bge-reranker-base-int8"));
        assert_eq!(high.ocr, "ppocr-v6-medium");
    }

    #[test]
    fn presets_reference_existing_catalog_ids() {
        let catalog = built_in_model_catalog();
        let ids: std::collections::HashSet<&str> = catalog
            .iter()
            .map(|entry| entry.catalog_id.as_str())
            .collect();
        for preset in built_in_model_presets() {
            assert!(ids.contains(preset.generation.as_str()));
            assert!(ids.contains(preset.embedding.as_str()));
            assert!(ids.contains(preset.ocr.as_str()));
            if let Some(value) = &preset.reranker {
                assert!(ids.contains(value.as_str()));
            }
            if let Some(value) = &preset.asr {
                assert!(ids.contains(value.as_str()));
            }
        }
        assert_eq!(preset_by_id("nonexistent"), None);
    }

    #[test]
    fn no_paddleocr_vl_references_in_presets_or_catalog() {
        fn normalized_contains_vl(text: &str) -> bool {
            let compact: String = text
                .to_lowercase()
                .chars()
                .filter(|ch| ch.is_ascii_alphanumeric())
                .collect();
            compact.contains("paddleocrvl") || compact.contains("ocrvlm")
        }
        for preset in built_in_model_presets() {
            for id in [
                preset.generation,
                preset.embedding,
                preset.ocr,
                preset.reranker.unwrap_or_default(),
                preset.asr.unwrap_or_default(),
            ] {
                assert!(!normalized_contains_vl(&id), "unexpected VL OCR id: {id}");
            }
        }
        for entry in built_in_model_catalog() {
            assert!(!normalized_contains_vl(&entry.catalog_id));
            assert!(!normalized_contains_vl(&entry.model_id));
        }
    }

    #[test]
    fn non_generation_components_are_reused_across_presets() {
        let smooth = preset_by_id("smooth").unwrap();
        let balanced = preset_by_id("balanced").unwrap();
        let high = preset_by_id("high").unwrap();
        // smooth -> balanced 复用 SenseVoice；OCR 分档 small / medium。
        assert_eq!(smooth.asr, balanced.asr);
        // balanced -> high 复用 BGE-M3 / reranker-base / SenseVoice / PP-OCRv6-medium。
        assert_eq!(balanced.embedding, high.embedding);
        assert_eq!(balanced.reranker, high.reranker);
        assert_eq!(balanced.asr, high.asr);
        assert_eq!(balanced.ocr, high.ocr);
    }

    #[test]
    fn required_catalog_ids_reflect_preset_components() {
        // basic 只有生成 + embedding + OCR 三个必需组件。
        let basic = preset_by_id("basic").unwrap();
        assert_eq!(
            basic.required_catalog_ids(),
            vec!["qwen3-5-0-8b-q4", "bge-small-zh-int8", "ppocr-v6-small"]
        );
        // smooth 六组件完整（含 reranker / ASR），OCR 用 v6-small。
        let smooth = preset_by_id("smooth").unwrap();
        assert_eq!(
            smooth.required_catalog_ids(),
            vec![
                "qwen3-5-2b-q4",
                "bge-small-zh-int8",
                "bge-reranker-base-int8",
                "sensevoice-small",
                "ppocr-v6-small",
            ]
        );
    }

    #[test]
    fn hardware_recommends_preset_conservatively() {
        // RTX 3050 Ti 4GB + 16GB → smooth（文档指定锚点）。
        assert_eq!(recommended_preset_id(Some(16), Some(4)), "smooth");
        // 无独显 + 8GB → basic。
        assert_eq!(recommended_preset_id(Some(8), None), "basic");
        // 32GB + 8GB → balanced。
        assert_eq!(recommended_preset_id(Some(32), Some(8)), "balanced");
        // 32GB + 12GB → high。
        assert_eq!(recommended_preset_id(Some(32), Some(12)), "high");
    }

    #[test]
    fn preset_fits_hardware_checks_ram_and_vram() {
        let smooth = preset_by_id("smooth").unwrap();
        assert!(preset_fits_hardware(&smooth, 16, Some(4)));
        assert!(!preset_fits_hardware(&smooth, 8, Some(4)));
        assert!(!preset_fits_hardware(&smooth, 16, None));
        let basic = preset_by_id("basic").unwrap();
        assert!(preset_fits_hardware(&basic, 8, None));
        let high = preset_by_id("high").unwrap();
        assert!(!preset_fits_hardware(&high, 32, Some(8)));
        assert!(preset_fits_hardware(&high, 32, Some(12)));
    }
}
