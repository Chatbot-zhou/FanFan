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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelPreset {
    pub preset_id: String,
    pub name: String,
    pub description: String,
    pub recommended_memory_gb: u32,
    pub role_catalog_ids: Vec<String>,
    pub edition_id: String,
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

pub fn built_in_model_editions() -> Vec<ModelEdition> {
    ["light", "standard"]
        .into_iter()
        .map(|edition_id| {
            resolved_model_edition(edition_id, "huggingface")
                .expect("built-in Hugging Face catalog is valid")
        })
        .collect()
}

pub fn built_in_model_presets() -> Vec<ModelPreset> {
    vec![
        ModelPreset {
            preset_id: "light".into(),
            name: "推荐省内存组合".into(),
            description: "Qwen3 0.6B + BGE-small，优先保证 8GB 设备的搜索和预览响应。".into(),
            recommended_memory_gb: 8,
            role_catalog_ids: vec!["qwen3-0.6b-q8".into(), "bge-small-zh-int8".into()],
            edition_id: "light".into(),
        },
        ModelPreset {
            preset_id: "standard".into(),
            name: "推荐质量组合".into(),
            description: "Qwen3 4B + BGE-small，适合内存更充足且更重视回答质量的设备。".into(),
            recommended_memory_gb: 12,
            role_catalog_ids: vec!["qwen3-4b-q4".into(), "bge-small-zh-int8".into()],
            edition_id: "standard".into(),
        },
    ]
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
            "qwen3-0.6b-q4",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 0.6B · 更省内存",
            "Qwen3-0.6B-Q4_K_M",
            "最省内存的入门档，低配置设备也能流畅问答。",
            &["下载最小", "CPU 响应最快", "低配设备首选"],
            &["回答质量略低于 Q8_0 版本"],
            Some(396_704_416),
            1.5,
            Some(0.9),
            "快",
            "Apache-2.0",
            false,
            "8GB 内存即可流畅运行；低配设备作为默认起点",
            Some("generation_qwen3_0_6b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-0.6b-q8",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 0.6B · 省内存",
            "Qwen3-0.6B-Q8_0",
            "低资源设备上的基础中文证据问答。",
            &["下载小", "CPU 响应较快", "适合作为默认起点"],
            &["长答案和复杂综合能力有限"],
            Some(639_446_688),
            2.0,
            Some(1.2),
            "较快",
            "Apache-2.0",
            false,
            "8GB 内存可用；GPU 可进一步降低 CPU 占用",
            Some("generation_qwen3_0_6b"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-1.7b-q4",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 1.7B · 均衡省内存",
            "Qwen3-1.7B-Q4_K_M",
            "均衡档的更省内存选择，占用更低、响应更流畅。",
            &["比 Q8_0 省约 40% 内存", "中文组织能力好"],
            &["综合能力略低于 Q8_0", "仅提供 Hugging Face 下载源"],
            Some(1_107_409_472),
            3.0,
            Some(1.8),
            "中等",
            "Apache-2.0",
            false,
            "10GB 内存优先；CUDA 可用时自动卸载到 GPU",
            Some("generation_qwen3_1_7b_q4"),
            &["huggingface"],
        ),
        catalog_entry(
            "qwen3-1.7b-q8",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 1.7B · 均衡",
            "Qwen3-1.7B-Q8_0",
            "回答质量与本地资源占用之间更均衡。",
            &["中文组织能力更好", "多文件综合更稳定"],
            &["CPU 生成慢于 0.6B", "4GB 显存需要自动分层卸载"],
            Some(1_834_426_016),
            4.0,
            Some(2.4),
            "中等",
            "Apache-2.0",
            false,
            "当前设备优先推荐；CUDA 运行时可用时自动卸载到 GPU",
            Some("generation_qwen3_1_7b"),
            &["huggingface"],
        ),
        catalog_entry(
            "qwen3-4b-q4",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 4B · 质量优先",
            "Qwen3-4B-Q4_K_M",
            "更擅长跨文件综合、结构化回答和复杂追问。",
            &["复杂问题质量更高", "长上下文组织更好"],
            &["CPU 推理较慢", "需要更多内存并可能挤占 4GB 显存"],
            Some(2_497_280_256),
            7.0,
            Some(4.5),
            "较慢",
            "Apache-2.0",
            false,
            "建议 12GB 以上内存；4GB 显存采用部分 GPU 卸载",
            Some("generation_qwen3_4b"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-4b-q8",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 4B · 极致质量",
            "Qwen3-4B-Q8_0",
            "本地可用的最高质量档，适合内存充足的设备。",
            &["质量最佳", "复杂综合与长答案更强"],
            &["下载与内存占用大", "CPU 推理明显变慢"],
            Some(4_280_404_704),
            12.0,
            Some(7.5),
            "慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；显存低于 8GB 时部分卸载到 CPU",
            Some("generation_qwen3_4b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-8b-q4",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 8B · 高质量省内存",
            "Qwen3-8B-Q4_K_M",
            "大参数模型的质量档，适合内存充足的设备。",
            &["复杂推理质量高", "长文档综合更强"],
            &["下载较大", "内存占用高"],
            Some(5_027_783_488),
            14.0,
            Some(9.0),
            "较慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；显存低于 12GB 时部分卸载到 CPU",
            Some("generation_qwen3_8b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-8b-q8",
            ModelRole::Generation,
            "Qwen3",
            "Qwen3 8B · 极致质量",
            "Qwen3-8B-Q8_0",
            "本地可用的最高质量档，适合大内存工作站。",
            &["本地最强问答质量", "长上下文组织最好"],
            &["下载 8.7GB", "需要 24GB 以上内存"],
            Some(8_709_518_112),
            24.0,
            Some(14.0),
            "慢",
            "Apache-2.0",
            false,
            "建议 32GB 内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_qwen3_8b_q8"),
            &["huggingface", "modelscope"],
        ),
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
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "bge-base-zh",
            ModelRole::Embedding,
            "BGE",
            "BGE-base-zh-v1.5 · 精度优先",
            "bge-base-zh-v1.5-onnx",
            "更大的中文向量模型，适合更强调召回质量的资料库。",
            &["语义区分能力更强"],
            &["索引更慢且占用更多空间", "验证构建尚未锁定"],
            None,
            2.5,
            None,
            "中等",
            "MIT",
            false,
            "可通过本地导入配置；远程文件完成项目基准前不开放下载",
            None,
            &[],
        ),
        catalog_entry(
            "qwen3-vl-2b-q4",
            ModelRole::Vision,
            "Qwen3-VL",
            "Qwen3-VL 2B · 省显存",
            "Qwen3VL-2B-Instruct-Q4_K_M",
            "理解独立图片、图表和文档内嵌图片。",
            &["支持中文图片理解", "量化后更适合 4GB 显存"],
            &["图片分析必须串行", "首次加载约需数秒"],
            Some(1_552_463_168),
            5.0,
            Some(3.2),
            "较慢",
            "Apache-2.0",
            false,
            "4GB 显存优先选择；前台问答时暂停后台图片分析",
            Some("vision_qwen3_vl_2b_q4"),
            &["huggingface"],
        ),
        catalog_entry(
            "qwen3-vl-2b-q8",
            ModelRole::Vision,
            "Qwen3-VL",
            "Qwen3-VL 2B · 质量优先",
            "Qwen3VL-2B-Instruct-Q8_0",
            "保留更多视觉细节，适合图表和密集页面。",
            &["图表和细节理解更稳"],
            &["需要更多内存和显存", "4GB 显存通常只能部分卸载"],
            Some(2_279_480_640),
            7.0,
            Some(5.0),
            "慢",
            "Apache-2.0",
            false,
            "建议显存高于 6GB；当前 4GB 设备优先省显存版",
            Some("vision_qwen3_vl_2b_q8"),
            &["huggingface"],
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
            &["huggingface"],
        ),
        catalog_entry(
            "ocr_paddleocr",
            ModelRole::Ocr,
            "PP-OCRv5",
            "PP-OCRv5 Mobile · 中文文字识别",
            "ch_PP-OCRv5_mobile",
            "RapidOCR + ONNX Runtime 的 PP-OCRv5 检测、方向分类和中文识别组件。",
            &["轻量中文OCR", "检测、方向与识别完整链路", "会话按需加载"],
            &["复杂表格与公式需要后续增强", "后台默认使用1至2个CPU线程"],
            Some(22_110_426),
            1.0,
            None,
            "快",
            "Apache-2.0",
            false,
            "推荐；图片和扫描文档默认使用的本地OCR组件",
            Some("ocr_paddleocr"),
            &["modelscope"],
        ),
        catalog_entry(
            "tts_sherpa_vits",
            ModelRole::Tts,
            "sherpa-onnx",
            "sherpa-onnx VITS · 中文语音合成",
            "vits_zh_ll",
            "sherpa-onnx 官方 VITS 中文语音合成模型，用于显式朗读已经通过引用校验的回答。",
            &["官方开源 ONNX 格式", "多说话人中文音色"],
            &["需要用户主动点击朗读", "不会自动播放或上传文本"],
            Some(121_478_002),
            1.5,
            None,
            "快",
            "Apache-2.0",
            false,
            "可选；为后续本地语音合成引擎预置的模型组件",
            Some("tts_sherpa_vits"),
            &["huggingface"],
        ),
        catalog_entry(
            "asr_sherpa_paraformer",
            ModelRole::Asr,
            "sherpa-onnx",
            "sherpa-onnx Paraformer · 中文语音识别",
            "paraformer-zh-small",
            "sherpa-onnx 官方 Paraformer 中文语音识别模型，随预设安装 Silero VAD，仅在用户明确录音时运行。",
            &["官方开源 ONNX 格式", "体积小识别稳定", "自动裁剪录音静音段"],
            &["当前优先中文普通话", "不会后台静默录音"],
            Some(82_547_881),
            1.0,
            None,
            "快",
            "Apache-2.0",
            false,
            "可选；为后续本地语音识别引擎预置的模型组件",
            Some("asr_sherpa_paraformer"),
            &["huggingface"],
        ),
        // ============ 2026-08-15 新增家族（Qwen3.5 / Gemma 4 / R1 / Qwen3-VL / Embedding） ============
        catalog_entry(
            "qwen3-5-0-8b-q4",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 0.8B · 省内存",
            "Qwen3.5-0.8B-Q4_K_M",
            "新一代 Qwen3.5 入门档，原生多模态架构，低配置设备也能流畅问答。",
            &["比 Qwen3 同档质量更高", "原生多模态可转作视觉模型", "CPU 响应快"],
            &["回答质量受量化影响"],
            Some(532_517_120),
            1.5,
            Some(0.9),
            "快",
            "Apache-2.0",
            false,
            "8GB 内存即可流畅运行；低配设备作为 Qwen3 的升级起点",
            Some("generation_qwen3_5_0_8b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-0-8b-q8",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 0.8B · 质量优先",
            "Qwen3.5-0.8B-Q8_0",
            "新一代 Qwen3.5 低资源档，Q8_0 量化保留更多细节。",
            &["质量优于同档 Q4 版本", "原生多模态可转作视觉模型"],
            &["下载与内存占用略高"],
            Some(811_843_840),
            2.0,
            Some(1.3),
            "快",
            "Apache-2.0",
            false,
            "8GB 内存可用；GPU 可进一步降低 CPU 占用",
            Some("generation_qwen3_5_0_8b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-4b-q4",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 4B · 省内存",
            "Qwen3.5-4B-Q4_K_M",
            "新一代 Qwen3.5 主流档，跨文件综合与复杂追问更强。",
            &["复杂问题质量更高", "原生多模态可转作视觉模型", "长上下文组织更好"],
            &["CPU 推理较慢", "需要更多内存"],
            Some(2_740_937_888),
            6.5,
            Some(3.5),
            "较慢",
            "Apache-2.0",
            false,
            "建议 12GB 以上内存；4GB 显存采用部分 GPU 卸载",
            Some("generation_qwen3_5_4b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-4b-q8",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 4B · 质量优先",
            "Qwen3.5-4B-Q8_0",
            "新一代 Qwen3.5 高质量档，适合内存更充足的设备。",
            &["质量最佳", "原生多模态可转作视觉模型"],
            &["下载与内存占用大", "CPU 推理明显变慢"],
            Some(4_482_403_488),
            10.0,
            Some(6.0),
            "较慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；显存低于 8GB 时部分卸载到 CPU",
            Some("generation_qwen3_5_4b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-9b-q4",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 9B · 省内存",
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
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-9b-q8",
            ModelRole::Generation,
            "Qwen3.5",
            "Qwen3.5 9B · 质量优先",
            "Qwen3.5-9B-Q8_0",
            "新一代 Qwen3.5 旗舰档，本地可用的最高质量选择。",
            &["本地最强问答质量", "原生多模态可转作视觉模型"],
            &["下载 9.5GB", "需要 24GB 以上内存"],
            Some(9_527_502_048),
            24.0,
            Some(12.0),
            "慢",
            "Apache-2.0",
            false,
            "建议 32GB 内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_qwen3_5_9b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-vision-0-8b-q4",
            ModelRole::Vision,
            "Qwen3.5",
            "Qwen3.5 0.8B · 视觉省内存",
            "Qwen3.5-0.8B-Visual-Q4_K_M",
            "Qwen3.5 原生多模态：理解独立图片、图表与文档内嵌图片。",
            &["支持中文图片理解", "同时可作为问答基础模型"],
            &["图片分析必须串行", "入门档视觉细节有限"],
            Some(737_504_352),
            2.0,
            Some(1.2),
            "快",
            "Apache-2.0",
            false,
            "低配置设备首选；也适用于纯文本问答场景",
            Some("vision_qwen3_5_0_8b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-vision-0-8b-q8",
            ModelRole::Vision,
            "Qwen3.5",
            "Qwen3.5 0.8B · 视觉质量优先",
            "Qwen3.5-0.8B-Visual-Q8_0",
            "Qwen3.5 原生多模态 Q8_0 档，保留更多视觉细节。",
            &["视觉细节更稳", "同时可作为问答基础模型"],
            &["需要更多内存和显存"],
            Some(1_016_831_072),
            2.5,
            Some(1.6),
            "快",
            "Apache-2.0",
            false,
            "8GB 内存可用；也适用于纯文本问答场景",
            Some("vision_qwen3_5_0_8b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-vision-4b-q4",
            ModelRole::Vision,
            "Qwen3.5",
            "Qwen3.5 4B · 视觉省内存",
            "Qwen3.5-4B-Visual-Q4_K_M",
            "Qwen3.5 原生多模态主流档，图表与密集页面理解更稳。",
            &["图表和细节理解更稳", "同时可作为问答基础模型"],
            &["需要更多内存和显存", "4GB 显存通常只能部分卸载"],
            Some(3_413_361_504),
            7.0,
            Some(4.0),
            "较慢",
            "Apache-2.0",
            false,
            "建议 12GB 以上内存；4GB 显存采用部分 GPU 卸载",
            Some("vision_qwen3_5_4b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-vision-4b-q8",
            ModelRole::Vision,
            "Qwen3.5",
            "Qwen3.5 4B · 视觉质量优先",
            "Qwen3.5-4B-Visual-Q8_0",
            "Qwen3.5 原生多模态高质量档，适合更重视视觉质量的设备。",
            &["视觉质量最佳", "同时可作为问答基础模型"],
            &["下载与内存占用大", "CPU 推理明显变慢"],
            Some(5_154_827_104),
            10.5,
            Some(6.5),
            "较慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；显存低于 8GB 时部分卸载到 CPU",
            Some("vision_qwen3_5_4b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-vision-9b-q4",
            ModelRole::Vision,
            "Qwen3.5",
            "Qwen3.5 9B · 视觉省内存",
            "Qwen3.5-9B-Visual-Q4_K_M",
            "Qwen3.5 原生多模态大参数档，复杂图表与长文档图片理解。",
            &["本地最强视觉理解", "同时可作为问答基础模型"],
            &["下载 6.6GB", "需要 16GB 以上内存"],
            Some(6_598_688_544),
            12.5,
            Some(7.5),
            "慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("vision_qwen3_5_9b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-5-vision-9b-q8",
            ModelRole::Vision,
            "Qwen3.5",
            "Qwen3.5 9B · 视觉质量优先",
            "Qwen3.5-9B-Visual-Q8_0",
            "Qwen3.5 原生多模态旗舰档，本地最强图片理解。",
            &["本地最强视觉理解", "同时可作为问答基础模型"],
            &["下载 10.4GB", "需要 24GB 以上内存"],
            Some(10_445_668_128),
            24.5,
            Some(12.5),
            "慢",
            "Apache-2.0",
            false,
            "建议 32GB 内存；需要较大显存或依赖 CPU 卸载",
            Some("vision_qwen3_5_9b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-e2b-q4",
            ModelRole::Generation,
            "Gemma 4",
            "Gemma 4 E2B · 省内存",
            "Gemma-4-E2B-Q4_K_M",
            "Google Gemma 4 边缘档，原生多模态架构，英文与代码能力强。",
            &["原生多模态可转作视觉模型", "代码与结构化输出好"],
            &["中文能力弱于同档 Qwen", "需要更多内存"],
            Some(3_106_738_272),
            7.0,
            Some(4.0),
            "较慢",
            "Gemma",
            false,
            "建议 12GB 以上内存；4GB 显存采用部分 GPU 卸载",
            Some("generation_gemma4_e2b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-e2b-q8",
            ModelRole::Generation,
            "Gemma 4",
            "Gemma 4 E2B · 质量优先",
            "Gemma-4-E2B-Q8_0",
            "Google Gemma 4 边缘档 Q8_0，质量与占用更均衡。",
            &["质量优于 Q4 版本", "原生多模态可转作视觉模型"],
            &["中文能力弱于同档 Qwen", "下载与内存占用更大"],
            Some(5_048_352_864),
            12.0,
            Some(6.5),
            "慢",
            "Gemma",
            false,
            "建议 16GB 以上内存；显存低于 8GB 时部分卸载到 CPU",
            Some("generation_gemma4_e2b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-e4b-q4",
            ModelRole::Generation,
            "Gemma 4",
            "Gemma 4 E4B · 省内存",
            "Gemma-4-E4B-Q4_K_M",
            "Google Gemma 4 均衡档，原生多模态，通用能力更强。",
            &["通用能力更强", "原生多模态可转作视觉模型"],
            &["中文能力弱于同档 Qwen"],
            Some(4_977_171_584),
            11.0,
            Some(6.5),
            "慢",
            "Gemma",
            false,
            "建议 16GB 以上内存；显存低于 8GB 时部分卸载到 CPU",
            Some("generation_gemma4_e4b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-e4b-q8",
            ModelRole::Generation,
            "Gemma 4",
            "Gemma 4 E4B · 质量优先",
            "Gemma-4-E4B-Q8_0",
            "Google Gemma 4 均衡档 Q8_0，本地高质量通用问答。",
            &["本地高质量通用问答", "原生多模态可转作视觉模型"],
            &["中文能力弱于同档 Qwen", "内存占用大"],
            Some(8_192_953_472),
            18.0,
            Some(10.5),
            "慢",
            "Gemma",
            false,
            "建议 24GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_gemma4_e4b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-12b-q4",
            ModelRole::Generation,
            "Gemma 4",
            "Gemma 4 12B · 省内存",
            "Gemma-4-12b-Q4_K_M",
            "Google Gemma 4 旗舰档，原生多模态，接近顶级通用能力。",
            &["本地最强通用问答", "原生多模态可转作视觉模型"],
            &["中文能力弱于同档 Qwen", "需要 20GB 以上内存"],
            Some(7_121_861_440),
            16.0,
            Some(9.0),
            "慢",
            "Gemma",
            false,
            "建议 20GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_gemma4_12b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-12b-q8",
            ModelRole::Generation,
            "Gemma 4",
            "Gemma 4 12B · 质量优先",
            "Gemma-4-12b-Q8_0",
            "Google Gemma 4 旗舰档 Q8_0，本地最强通用问答之一。",
            &["本地最强通用问答", "原生多模态可转作视觉模型"],
            &["下载 12.7GB", "需要 32GB 以上内存"],
            Some(12_669_647_680),
            28.0,
            Some(16.0),
            "慢",
            "Gemma",
            false,
            "建议 32GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_gemma4_12b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-vision-e2b-q4",
            ModelRole::Vision,
            "Gemma 4",
            "Gemma 4 E2B · 视觉省内存",
            "Gemma-4-E2B-Visual-Q4_K_M",
            "Gemma 4 原生多模态边缘档：图片理解、图表与文档内嵌图片。",
            &["原生多模态", "同时可作为问答基础模型"],
            &["中文能力弱于同档 Qwen", "视觉细节有限"],
            Some(4_092_392_352),
            7.5,
            Some(4.5),
            "较慢",
            "Gemma",
            false,
            "建议 12GB 以上内存；也适用于纯文本问答场景",
            Some("vision_gemma4_e2b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-vision-e2b-q8",
            ModelRole::Vision,
            "Gemma 4",
            "Gemma 4 E2B · 视觉质量优先",
            "Gemma-4-E2B-Visual-Q8_0",
            "Gemma 4 原生多模态边缘档 Q8_0，保留更多视觉细节。",
            &["视觉细节更稳", "同时可作为问答基础模型"],
            &["中文能力弱于同档 Qwen", "内存占用更大"],
            Some(6_034_006_944),
            12.5,
            Some(7.0),
            "慢",
            "Gemma",
            false,
            "建议 16GB 以上内存；也适用于纯文本问答场景",
            Some("vision_gemma4_e2b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-vision-e4b-q4",
            ModelRole::Vision,
            "Gemma 4",
            "Gemma 4 E4B · 视觉省内存",
            "Gemma-4-E4B-Visual-Q4_K_M",
            "Gemma 4 原生多模态均衡档，通用图片理解更稳。",
            &["通用视觉理解更稳", "同时可作为问答基础模型"],
            &["中文能力弱于同档 Qwen"],
            Some(5_967_544_256),
            11.5,
            Some(7.0),
            "慢",
            "Gemma",
            false,
            "建议 16GB 以上内存；也适用于纯文本问答场景",
            Some("vision_gemma4_e4b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-vision-e4b-q8",
            ModelRole::Vision,
            "Gemma 4",
            "Gemma 4 E4B · 视觉质量优先",
            "Gemma-4-E4B-Visual-Q8_0",
            "Gemma 4 原生多模态均衡档 Q8_0，本地高质量图片理解。",
            &["本地高质量视觉理解", "同时可作为问答基础模型"],
            &["中文能力弱于同档 Qwen", "内存占用大"],
            Some(9_183_326_144),
            18.5,
            Some(11.0),
            "慢",
            "Gemma",
            false,
            "建议 24GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("vision_gemma4_e4b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-vision-12b-q4",
            ModelRole::Vision,
            "Gemma 4",
            "Gemma 4 12B · 视觉省内存",
            "Gemma-4-12b-Visual-Q4_K_M",
            "Gemma 4 原生多模态旗舰档，复杂图表与长文档图片理解。",
            &["本地最强视觉理解", "同时可作为问答基础模型"],
            &["中文能力弱于同档 Qwen", "需要 20GB 以上内存"],
            Some(7_296_977_280),
            16.5,
            Some(9.5),
            "慢",
            "Gemma",
            false,
            "建议 20GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("vision_gemma4_12b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "gemma4-vision-12b-q8",
            ModelRole::Vision,
            "Gemma 4",
            "Gemma 4 12B · 视觉质量优先",
            "Gemma-4-12b-Visual-Q8_0",
            "Gemma 4 原生多模态旗舰档 Q8_0，本地最强图片理解。",
            &["本地最强视觉理解", "同时可作为问答基础模型"],
            &["下载 12.8GB", "需要 32GB 以上内存"],
            Some(12_844_763_520),
            28.5,
            Some(16.5),
            "慢",
            "Gemma",
            false,
            "建议 32GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("vision_gemma4_12b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "r1-1-5b-q4",
            ModelRole::Generation,
            "DeepSeek-R1",
            "DeepSeek-R1 1.5B · 省内存",
            "DeepSeek-R1-Distill-Qwen-1.5B-Q4_K_M",
            "DeepSeek-R1 蒸馏推理模型：先展示推理过程再给出结论。",
            &["带推理过程", "数学与逻辑推理更强"],
            &["回答前会输出思考步骤", "CPU 生成速度中等"],
            Some(1_117_321_312),
            3.0,
            Some(1.8),
            "中等",
            "MIT",
            false,
            "10GB 内存优先；适合偏好推理展示的用户",
            Some("generation_r1_1_5b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "r1-1-5b-q8",
            ModelRole::Generation,
            "DeepSeek-R1",
            "DeepSeek-R1 1.5B · 质量优先",
            "DeepSeek-R1-Distill-Qwen-1.5B-Q8_0",
            "DeepSeek-R1 蒸馏推理模型 Q8_0 档，推理质量更稳。",
            &["推理质量优于 Q4", "带推理过程"],
            &["回答前会输出思考步骤", "内存占用更大"],
            Some(1_894_532_416),
            4.5,
            Some(2.7),
            "中等",
            "MIT",
            false,
            "8GB 内存可用；CUDA 可用时自动卸载到 GPU",
            Some("generation_r1_1_5b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "r1-7b-q4",
            ModelRole::Generation,
            "DeepSeek-R1",
            "DeepSeek-R1 7B · 省内存",
            "DeepSeek-R1-Distill-Qwen-7B-Q4_K_M",
            "DeepSeek-R1 蒸馏 7B 档，推理与中文综合能力明显增强。",
            &["推理与中文综合更强", "带推理过程"],
            &["回答前会输出思考步骤", "CPU 推理慢"],
            Some(4_683_073_248),
            10.0,
            Some(6.0),
            "慢",
            "MIT",
            false,
            "建议 16GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_r1_7b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "r1-7b-q8",
            ModelRole::Generation,
            "DeepSeek-R1",
            "DeepSeek-R1 7B · 质量优先",
            "DeepSeek-R1-Distill-Qwen-7B-Q8_0",
            "DeepSeek-R1 蒸馏 7B Q8_0 档，本地高质量推理问答。",
            &["本地高质量推理问答", "带推理过程"],
            &["下载 8.1GB", "需要 24GB 以上内存"],
            Some(8_098_524_896),
            18.0,
            Some(10.5),
            "慢",
            "MIT",
            false,
            "建议 24GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_r1_7b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "r1-8b-q4",
            ModelRole::Generation,
            "DeepSeek-R1",
            "DeepSeek-R1 8B · 省内存",
            "DeepSeek-R1-Distill-Llama-8B-Q4_K_M",
            "DeepSeek-R1 蒸馏 Llama-8B 档，通用能力与推理并重。",
            &["通用能力与推理并重", "带推理过程"],
            &["回答前会输出思考步骤", "CPU 推理慢"],
            Some(4_920_737_216),
            11.0,
            Some(6.5),
            "慢",
            "MIT",
            false,
            "建议 16GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_r1_8b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "r1-8b-q8",
            ModelRole::Generation,
            "DeepSeek-R1",
            "DeepSeek-R1 8B · 质量优先",
            "DeepSeek-R1-Distill-Llama-8B-Q8_0",
            "DeepSeek-R1 蒸馏 Llama-8B Q8_0 档，本地最强推理问答。",
            &["本地最强推理问答", "带推理过程"],
            &["下载 8.5GB", "需要 24GB 以上内存"],
            Some(8_540_773_088),
            19.0,
            Some(11.0),
            "慢",
            "MIT",
            false,
            "建议 24GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("generation_r1_8b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-vl-4b-q4",
            ModelRole::Vision,
            "Qwen3-VL",
            "Qwen3-VL 4B · 省显存",
            "Qwen3VL-4B-Instruct-Q4_K_M",
            "Qwen 官方视觉语言模型 4B 档，理解独立图片、图表与文档内嵌图片。",
            &["官方 GGUF 支持中文图片理解", "量化后更适合 8GB 显存"],
            &["图片分析必须串行", "首次加载约需数秒"],
            Some(3_333_461_920),
            6.5,
            Some(4.0),
            "较慢",
            "Apache-2.0",
            false,
            "建议 12GB 以上内存；4GB 显存优先选择",
            Some("vision_qwen3_vl_4b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-vl-4b-q8",
            ModelRole::Vision,
            "Qwen3-VL",
            "Qwen3-VL 4B · 质量优先",
            "Qwen3VL-4B-Instruct-Q8_0",
            "Qwen 官方视觉语言模型 4B Q8_0 档，保留更多视觉细节。",
            &["图表和细节理解更稳", "官方 GGUF 双源下载"],
            &["需要更多内存和显存"],
            Some(5_116_586_400),
            10.0,
            Some(6.5),
            "慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；显存低于 8GB 时部分卸载到 CPU",
            Some("vision_qwen3_vl_4b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-vl-8b-q4",
            ModelRole::Vision,
            "Qwen3-VL",
            "Qwen3-VL 8B · 省显存",
            "Qwen3VL-8B-Instruct-Q4_K_M",
            "Qwen 官方视觉语言模型 8B 档，复杂图表与长文档图片理解。",
            &["复杂视觉理解更强", "官方 GGUF 双源下载"],
            &["需要 16GB 以上内存", "4GB 显存只能部分卸载"],
            Some(6_186_814_624),
            12.0,
            Some(7.0),
            "慢",
            "Apache-2.0",
            false,
            "建议 16GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("vision_qwen3_vl_8b_q4"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-vl-8b-q8",
            ModelRole::Vision,
            "Qwen3-VL",
            "Qwen3-VL 8B · 质量优先",
            "Qwen3VL-8B-Instruct-Q8_0",
            "Qwen 官方视觉语言模型 8B Q8_0 档，本地最强图片理解。",
            &["本地最强视觉理解", "官方 GGUF 双源下载"],
            &["下载 9.9GB", "需要 24GB 以上内存"],
            Some(9_868_549_280),
            20.0,
            Some(12.0),
            "慢",
            "Apache-2.0",
            false,
            "建议 24GB 以上内存；需要较大显存或依赖 CPU 卸载",
            Some("vision_qwen3_vl_8b_q8"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "qwen3-embedding-0-6b",
            ModelRole::Embedding,
            "Qwen3-Embedding",
            "Qwen3-Embedding 0.6B · 新代际",
            "qwen3-embedding-0.6b-int8",
            "Qwen 官方新一代稠密向量模型，中文检索质量显著优于 BGE-small。",
            &["检索质量显著提升", "支持长文本 8K 上下文", "官方 ONNX 双源下载"],
            &["索引速度慢于 BGE-small", "需要更多内存"],
            Some(624_951_244),
            1.5,
            None,
            "快",
            "Apache-2.0",
            false,
            "所有受支持设备均推荐；更换后需要新建索引代际",
            Some("embedding_qwen3_embedding_0_6b"),
            &["huggingface", "modelscope"],
        ),
        catalog_entry(
            "bge-m3",
            ModelRole::Embedding,
            "BGE",
            "BGE M3 · 多语言",
            "bge-m3-int8",
            "BGE-M3 多语言向量模型，中文与英文资料库均可获得稳定召回。",
            &["多语言检索稳定", "召回质量高于 BGE-small", "官方 ONNX 双源下载"],
            &["索引更慢且占用更多空间", "内存占用高于 BGE-small"],
            Some(585_539_515),
            2.5,
            None,
            "中等",
            "MIT",
            false,
            "建议 8GB 以上内存；更换后需要新建索引代际",
            Some("embedding_bge_m3"),
            &["huggingface", "modelscope"],
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
        "generation_qwen3_0_6b",
        "generation_qwen3_0_6b_q4",
        "generation_qwen3_1_7b",
        "generation_qwen3_1_7b_q4",
        "generation_qwen3_4b",
        "generation_qwen3_4b_q8",
        "generation_qwen3_8b_q4",
        "generation_qwen3_8b_q8",
        "embedding_bge_small",
        "vision_qwen3_vl_2b_q4",
        "vision_qwen3_vl_2b_q8",
        "reranker_bge_base_int8",
        "ocr_paddleocr",
        "tts_sherpa_vits",
        "asr_sherpa_paraformer",
        // 2026-08-15 新增家族
        "generation_qwen3_5_0_8b_q4",
        "generation_qwen3_5_0_8b_q8",
        "generation_qwen3_5_4b_q4",
        "generation_qwen3_5_4b_q8",
        "generation_qwen3_5_9b_q4",
        "generation_qwen3_5_9b_q8",
        "vision_qwen3_5_0_8b_q4",
        "vision_qwen3_5_0_8b_q8",
        "vision_qwen3_5_4b_q4",
        "vision_qwen3_5_4b_q8",
        "vision_qwen3_5_9b_q4",
        "vision_qwen3_5_9b_q8",
        "generation_gemma4_e2b_q4",
        "generation_gemma4_e2b_q8",
        "generation_gemma4_e4b_q4",
        "generation_gemma4_e4b_q8",
        "generation_gemma4_12b_q4",
        "generation_gemma4_12b_q8",
        "vision_gemma4_e2b_q4",
        "vision_gemma4_e2b_q8",
        "vision_gemma4_e4b_q4",
        "vision_gemma4_e4b_q8",
        "vision_gemma4_12b_q4",
        "vision_gemma4_12b_q8",
        "generation_r1_1_5b_q4",
        "generation_r1_1_5b_q8",
        "generation_r1_7b_q4",
        "generation_r1_7b_q8",
        "generation_r1_8b_q4",
        "generation_r1_8b_q8",
        "vision_qwen3_vl_4b_q4",
        "vision_qwen3_vl_4b_q8",
        "vision_qwen3_vl_8b_q4",
        "vision_qwen3_vl_8b_q8",
        "embedding_qwen3_embedding_0_6b",
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
                "模型下载来源必须是Hugging Face或ModelScope",
                false,
            ));
        }
    };
    let qwen_06 = || {
        generation_artifact(
            source,
            "Qwen3-0.6B-Q8_0",
            DownloadSourceSpec {
                repository_id: "Qwen/Qwen3-0.6B-GGUF",
                revision: "1eaf4d9657fe65ad10a51eab76a8db5b363bddaa",
                file_name: "Qwen3-0.6B-Q8_0.gguf",
                sha256: "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031",
                size_bytes: 639_446_688,
            },
            DownloadSourceSpec {
                repository_id: "unsloth/Qwen3-0.6B-GGUF",
                revision: "6091bc857fe0dffa19c581a7ccc7def1b126ff54",
                file_name: "Qwen3-0.6B-Q8_0.gguf",
                sha256: "e150ed544dfe6016930c026a93913a5e3184181ebfe6ab2223ae01dd0491784c",
                size_bytes: 639_447_744,
            },
        )
    };
    let qwen_06_q4 = || {
        generation_artifact(
            source,
            "Qwen3-0.6B-Q4_K_M",
            DownloadSourceSpec {
                repository_id: "Qwen/Qwen3-0.6B-GGUF",
                revision: "1eaf4d9657fe65ad10a51eab76a8db5b363bddaa",
                file_name: "Qwen3-0.6B-Q4_K_M.gguf",
                sha256: "b0638f08417a2d3c8652760462eb5407c6e30173cf9608ad0820757a281eea0e",
                size_bytes: 396_704_416,
            },
            DownloadSourceSpec {
                repository_id: "unsloth/Qwen3-0.6B-GGUF",
                revision: "6091bc857fe0dffa19c581a7ccc7def1b126ff54",
                file_name: "Qwen3-0.6B-Q4_K_M.gguf",
                sha256: "ac2d97712095a558e31573f62f466a3f9d93990898b0ec79d7c974c1780d524a",
                size_bytes: 396_705_472,
            },
        )
    };
    let qwen_17_q4 = || {
        generation_artifact_from_spec(
            source,
            "Qwen3-1.7B-Q4_K_M",
            DownloadSourceSpec {
                repository_id: "unsloth/Qwen3-1.7B-GGUF",
                revision: "d7f544eead698dbd1f15126ef60b45a1e1933222",
                file_name: "Qwen3-1.7B-Q4_K_M.gguf",
                sha256: "b139949c5bd74937ad8ed8c8cf3d9ffb1e99c866c823204dc42c0d91fa181897",
                size_bytes: 1_107_409_472,
            },
        )
    };
    let qwen_8_q4 = || {
        generation_artifact(
            source,
            "Qwen3-8B-Q4_K_M",
            DownloadSourceSpec {
                repository_id: "Qwen/Qwen3-8B-GGUF",
                revision: "7c41481f57cb95916b40956ab2f0b139b296d974",
                file_name: "Qwen3-8B-Q4_K_M.gguf",
                sha256: "d98cdcbd03e17ce47681435b5150e34c1417f50b5c0019dd560e4882c5745785",
                size_bytes: 5_027_783_488,
            },
            DownloadSourceSpec {
                repository_id: "unsloth/Qwen3-8B-GGUF",
                revision: "baaddd6fb19e702c1d54c5bb2a5746012c122619",
                file_name: "Qwen3-8B-Q4_K_M.gguf",
                sha256: "120307ba529eb2439d6c430d94104dabd578497bc7bfe7e322b5d9933b449bd4",
                size_bytes: 5_027_784_512,
            },
        )
    };
    let qwen_8_q8 = || {
        generation_artifact(
            source,
            "Qwen3-8B-Q8_0",
            DownloadSourceSpec {
                repository_id: "Qwen/Qwen3-8B-GGUF",
                revision: "7c41481f57cb95916b40956ab2f0b139b296d974",
                file_name: "Qwen3-8B-Q8_0.gguf",
                sha256: "408b955510e196121c1c375201744783b5c9a43c7956d73fc78df54c66e883d6",
                size_bytes: 8_709_518_112,
            },
            DownloadSourceSpec {
                repository_id: "unsloth/Qwen3-8B-GGUF",
                revision: "baaddd6fb19e702c1d54c5bb2a5746012c122619",
                file_name: "Qwen3-8B-Q8_0.gguf",
                sha256: "0cfbf745760f07a76ddeb358dd025a27f2e11d1ca9c9a4169a373d52990fe86e",
                size_bytes: 8_709_519_168,
            },
        )
    };
    let qwen_4_q8 = || {
        generation_artifact(
            source,
            "Qwen3-4B-Q8_0",
            DownloadSourceSpec {
                repository_id: "Qwen/Qwen3-4B-GGUF",
                revision: "a9a60d009fa7ff9606305047c2bf77ac25dbec49",
                file_name: "Qwen3-4B-Q8_0.gguf",
                sha256: "8c2f07f26af9747e41988551106f149b03eb9b5cb6df636027b6bf6278473300",
                size_bytes: 4_280_404_704,
            },
            DownloadSourceSpec {
                repository_id: "unsloth/Qwen3-4B-128K-GGUF",
                revision: "b88bdf30994e2cfec7e8a46ecd7f55d7fd20738b",
                file_name: "Qwen3-4B-128K-Q8_0.gguf",
                sha256: "70406550575ba36264119d8b54fe0593e46e82a2bf19ad40f3eecc497e7728cf",
                size_bytes: 4_280_405_984,
            },
        )
    };
    let qwen_4 = || {
        generation_artifact(
            source,
            "Qwen3-4B-Q4_K_M",
            DownloadSourceSpec {
                repository_id: "Qwen/Qwen3-4B-GGUF",
                revision: "a9a60d009fa7ff9606305047c2bf77ac25dbec49",
                file_name: "Qwen3-4B-Q4_K_M.gguf",
                sha256: "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5",
                size_bytes: 2_497_280_256,
            },
            DownloadSourceSpec {
                repository_id: "unsloth/Qwen3-4B-128K-GGUF",
                revision: "b88bdf30994e2cfec7e8a46ecd7f55d7fd20738b",
                file_name: "Qwen3-4B-128K-Q4_K_M.gguf",
                sha256: "f145a1bd60fec420ca4d9b7645ebcdf657e301463bc4dd3af4a8c0b548b5eb1a",
                size_bytes: 2_497_281_504,
            },
        )
    };
    match edition_id {
        "light" => Ok(edition(
            edition_id,
            "轻量版",
            "适合低配置电脑的完整本地RAG套件，包含0.6B生成模型和共享中文语义模型。",
            8,
            vec![qwen_06(), embedding_artifact(source)],
            &["generation", "embedding", "rag"],
        )),
        "standard" => Ok(edition(
            edition_id,
            "标准版",
            "效果更好的完整本地RAG套件，包含4B生成模型和共享中文语义模型。",
            12,
            vec![qwen_4(), embedding_artifact(source)],
            &["generation", "embedding", "rag"],
        )),
        "generation_qwen3_0_6b" => Ok(edition(
            edition_id,
            "Qwen3 0.6B",
            "仅安装并切换问答基础模型。",
            8,
            vec![qwen_06()],
            &["generation"],
        )),
        "generation_qwen3_0_6b_q4" => Ok(edition(
            edition_id,
            "Qwen3 0.6B 更省内存",
            "仅安装并切换 Q4_K_M 量化的入门问答基础模型。",
            8,
            vec![qwen_06_q4()],
            &["generation"],
        )),
        "generation_qwen3_1_7b" => {
            require_huggingface(source)?;
            Ok(edition(
                edition_id,
                "Qwen3 1.7B",
                "仅安装并切换均衡型问答基础模型。",
                10,
                vec![generation_artifact_from_spec(
                    source,
                    "Qwen3-1.7B-Q8_0",
                    DownloadSourceSpec {
                        repository_id: "Qwen/Qwen3-1.7B-GGUF",
                        revision: "90862c4b9d2787eaed51d12237eafdfe7c5f6077",
                        file_name: "Qwen3-1.7B-Q8_0.gguf",
                        sha256: "061b54daade076b5d3362dac252678d17da8c68f07560be70818cace6590cb1a",
                        size_bytes: 1_834_426_016,
                    },
                )],
                &["generation"],
            ))
        }
        // Qwen3 1.7B 官方仓库只有 Q8_0 一个量化，省内存档走 unsloth 仓库，
        // 两者都仅提供 Hugging Face 源（与 Q8_0 版本保持一致）。
        "generation_qwen3_1_7b_q4" => {
            require_huggingface(source)?;
            Ok(edition(
                edition_id,
                "Qwen3 1.7B 均衡省内存",
                "仅安装并切换 Q4_K_M 量化的均衡问答基础模型。",
                10,
                vec![qwen_17_q4()],
                &["generation"],
            ))
        }
        "generation_qwen3_4b" => Ok(edition(
            edition_id,
            "Qwen3 4B",
            "仅安装并切换质量优先的问答基础模型。",
            12,
            vec![qwen_4()],
            &["generation"],
        )),
        "generation_qwen3_4b_q8" => Ok(edition(
            edition_id,
            "Qwen3 4B 极致质量",
            "仅安装并切换 Q8_0 量化的最高质量问答基础模型。",
            16,
            vec![qwen_4_q8()],
            &["generation"],
        )),
        "generation_qwen3_8b_q4" => Ok(edition(
            edition_id,
            "Qwen3 8B 高质量省内存",
            "仅安装并切换 Q4_K_M 量化的大参数问答基础模型。",
            16,
            vec![qwen_8_q4()],
            &["generation"],
        )),
        "generation_qwen3_8b_q8" => Ok(edition(
            edition_id,
            "Qwen3 8B 极致质量",
            "仅安装并切换 Q8_0 量化的本地最高质量问答基础模型。",
            24,
            vec![qwen_8_q8()],
            &["generation"],
        )),
        "embedding_bge_small" => Ok(edition(
            edition_id,
            "BGE-small-zh-v1.5",
            "仅安装中文 Embedding；启用后建立新的向量索引代际。",
            8,
            vec![embedding_artifact(source)],
            &["embedding"],
        )),
        "vision_qwen3_vl_2b_q4" => {
            require_huggingface(source)?;
            Ok(edition(
                edition_id,
                "Qwen3-VL 2B 省显存",
                "安装视觉语言模型与匹配的视觉投影文件。",
                10,
                vec![vision_artifact(source, false)],
                &["vision"],
            ))
        }
        "vision_qwen3_vl_2b_q8" => {
            require_huggingface(source)?;
            Ok(edition(
                edition_id,
                "Qwen3-VL 2B 质量优先",
                "安装高质量视觉语言模型与匹配的视觉投影文件。",
                14,
                vec![vision_artifact(source, true)],
                &["vision"],
            ))
        }
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
        "ocr_paddleocr" => Ok(edition(
            edition_id,
            "PP-OCRv5 Mobile",
            "安装中文文字识别模型（检测、方向分类、识别与词典）。",
            4,
            vec![ocr_artifact(source)],
            &["ocr"],
        )),
        "tts_sherpa_vits" => Ok(edition(
            edition_id,
            "sherpa-onnx VITS",
            "安装中文语音合成模型（VITS 主模型+词表与词典）。",
            2,
            vec![tts_vits_artifact(source)],
            &["tts"],
        )),
        "asr_sherpa_paraformer" => Ok(edition(
            edition_id,
            "sherpa-onnx Paraformer",
            "安装中文语音识别模型（Paraformer 主模型+词表）。",
            2,
            vec![asr_paraformer_artifact(source)],
            &["asr"],
        )),
        // ============ 2026-08-15 新增家族（Qwen3.5 / Gemma 4 / R1 / Qwen3-VL / Embedding） ============
        "generation_qwen3_5_0_8b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 0.8B 省内存",
            "安装 Qwen3.5 0.8B Q4_K_M 问答基础模型。",
            8,
            vec![qwen35_generation_artifact(source, "0.8B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_qwen3_5_0_8b_q8" => Ok(edition(
            edition_id,
            "Qwen3.5 0.8B 质量优先",
            "安装 Qwen3.5 0.8B Q8_0 问答基础模型。",
            8,
            vec![qwen35_generation_artifact(source, "0.8B", "Q8_0")],
            &["generation"],
        )),
        "generation_qwen3_5_4b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 4B 省内存",
            "安装 Qwen3.5 4B Q4_K_M 问答基础模型。",
            12,
            vec![qwen35_generation_artifact(source, "4B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_qwen3_5_4b_q8" => Ok(edition(
            edition_id,
            "Qwen3.5 4B 质量优先",
            "安装 Qwen3.5 4B Q8_0 问答基础模型。",
            16,
            vec![qwen35_generation_artifact(source, "4B", "Q8_0")],
            &["generation"],
        )),
        "generation_qwen3_5_9b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 9B 省内存",
            "安装 Qwen3.5 9B Q4_K_M 问答基础模型。",
            16,
            vec![qwen35_generation_artifact(source, "9B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_qwen3_5_9b_q8" => Ok(edition(
            edition_id,
            "Qwen3.5 9B 质量优先",
            "安装 Qwen3.5 9B Q8_0 问答基础模型。",
            24,
            vec![qwen35_generation_artifact(source, "9B", "Q8_0")],
            &["generation"],
        )),
        "vision_qwen3_5_0_8b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 0.8B 视觉省内存",
            "安装 Qwen3.5 0.8B Q4_K_M 视觉模型与视觉投影文件。",
            8,
            vec![qwen35_vision_artifact(source, "0.8B", "Q4_K_M")],
            &["vision"],
        )),
        "vision_qwen3_5_0_8b_q8" => Ok(edition(
            edition_id,
            "Qwen3.5 0.8B 视觉质量优先",
            "安装 Qwen3.5 0.8B Q8_0 视觉模型与视觉投影文件。",
            8,
            vec![qwen35_vision_artifact(source, "0.8B", "Q8_0")],
            &["vision"],
        )),
        "vision_qwen3_5_4b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 4B 视觉省内存",
            "安装 Qwen3.5 4B Q4_K_M 视觉模型与视觉投影文件。",
            12,
            vec![qwen35_vision_artifact(source, "4B", "Q4_K_M")],
            &["vision"],
        )),
        "vision_qwen3_5_4b_q8" => Ok(edition(
            edition_id,
            "Qwen3.5 4B 视觉质量优先",
            "安装 Qwen3.5 4B Q8_0 视觉模型与视觉投影文件。",
            16,
            vec![qwen35_vision_artifact(source, "4B", "Q8_0")],
            &["vision"],
        )),
        "vision_qwen3_5_9b_q4" => Ok(edition(
            edition_id,
            "Qwen3.5 9B 视觉省内存",
            "安装 Qwen3.5 9B Q4_K_M 视觉模型与视觉投影文件。",
            16,
            vec![qwen35_vision_artifact(source, "9B", "Q4_K_M")],
            &["vision"],
        )),
        "vision_qwen3_5_9b_q8" => Ok(edition(
            edition_id,
            "Qwen3.5 9B 视觉质量优先",
            "安装 Qwen3.5 9B Q8_0 视觉模型与视觉投影文件。",
            24,
            vec![qwen35_vision_artifact(source, "9B", "Q8_0")],
            &["vision"],
        )),
        "generation_gemma4_e2b_q4" => Ok(edition(
            edition_id,
            "Gemma 4 E2B 省内存",
            "安装 Gemma 4 E2B Q4_K_M 问答基础模型。",
            12,
            vec![gemma4_generation_artifact(source, "E2B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_gemma4_e2b_q8" => Ok(edition(
            edition_id,
            "Gemma 4 E2B 质量优先",
            "安装 Gemma 4 E2B Q8_0 问答基础模型。",
            16,
            vec![gemma4_generation_artifact(source, "E2B", "Q8_0")],
            &["generation"],
        )),
        "generation_gemma4_e4b_q4" => Ok(edition(
            edition_id,
            "Gemma 4 E4B 省内存",
            "安装 Gemma 4 E4B Q4_K_M 问答基础模型。",
            16,
            vec![gemma4_generation_artifact(source, "E4B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_gemma4_e4b_q8" => Ok(edition(
            edition_id,
            "Gemma 4 E4B 质量优先",
            "安装 Gemma 4 E4B Q8_0 问答基础模型。",
            24,
            vec![gemma4_generation_artifact(source, "E4B", "Q8_0")],
            &["generation"],
        )),
        "generation_gemma4_12b_q4" => Ok(edition(
            edition_id,
            "Gemma 4 12B 省内存",
            "安装 Gemma 4 12B Q4_K_M 问答基础模型。",
            20,
            vec![gemma4_generation_artifact(source, "12b", "Q4_K_M")],
            &["generation"],
        )),
        "generation_gemma4_12b_q8" => Ok(edition(
            edition_id,
            "Gemma 4 12B 质量优先",
            "安装 Gemma 4 12B Q8_0 问答基础模型。",
            32,
            vec![gemma4_generation_artifact(source, "12b", "Q8_0")],
            &["generation"],
        )),
        "vision_gemma4_e2b_q4" => Ok(edition(
            edition_id,
            "Gemma 4 E2B 视觉省内存",
            "安装 Gemma 4 E2B Q4_K_M 视觉模型与视觉投影文件。",
            12,
            vec![gemma4_vision_artifact(source, "E2B", "Q4_K_M")],
            &["vision"],
        )),
        "vision_gemma4_e2b_q8" => Ok(edition(
            edition_id,
            "Gemma 4 E2B 视觉质量优先",
            "安装 Gemma 4 E2B Q8_0 视觉模型与视觉投影文件。",
            16,
            vec![gemma4_vision_artifact(source, "E2B", "Q8_0")],
            &["vision"],
        )),
        "vision_gemma4_e4b_q4" => Ok(edition(
            edition_id,
            "Gemma 4 E4B 视觉省内存",
            "安装 Gemma 4 E4B Q4_K_M 视觉模型与视觉投影文件。",
            16,
            vec![gemma4_vision_artifact(source, "E4B", "Q4_K_M")],
            &["vision"],
        )),
        "vision_gemma4_e4b_q8" => Ok(edition(
            edition_id,
            "Gemma 4 E4B 视觉质量优先",
            "安装 Gemma 4 E4B Q8_0 视觉模型与视觉投影文件。",
            24,
            vec![gemma4_vision_artifact(source, "E4B", "Q8_0")],
            &["vision"],
        )),
        "vision_gemma4_12b_q4" => Ok(edition(
            edition_id,
            "Gemma 4 12B 视觉省内存",
            "安装 Gemma 4 12B Q4_K_M 视觉模型与视觉投影文件。",
            20,
            vec![gemma4_vision_artifact(source, "12b", "Q4_K_M")],
            &["vision"],
        )),
        "vision_gemma4_12b_q8" => Ok(edition(
            edition_id,
            "Gemma 4 12B 视觉质量优先",
            "安装 Gemma 4 12B Q8_0 视觉模型与视觉投影文件。",
            32,
            vec![gemma4_vision_artifact(source, "12b", "Q8_0")],
            &["vision"],
        )),
        "generation_r1_1_5b_q4" => Ok(edition(
            edition_id,
            "DeepSeek-R1 1.5B 省内存",
            "安装 DeepSeek-R1-Distill 1.5B Q4_K_M 问答基础模型（带推理过程）。",
            8,
            vec![r1_generation_artifact(source, "Qwen-1.5B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_r1_1_5b_q8" => Ok(edition(
            edition_id,
            "DeepSeek-R1 1.5B 质量优先",
            "安装 DeepSeek-R1-Distill 1.5B Q8_0 问答基础模型（带推理过程）。",
            8,
            vec![r1_generation_artifact(source, "Qwen-1.5B", "Q8_0")],
            &["generation"],
        )),
        "generation_r1_7b_q4" => Ok(edition(
            edition_id,
            "DeepSeek-R1 7B 省内存",
            "安装 DeepSeek-R1-Distill 7B Q4_K_M 问答基础模型（带推理过程）。",
            16,
            vec![r1_generation_artifact(source, "Qwen-7B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_r1_7b_q8" => Ok(edition(
            edition_id,
            "DeepSeek-R1 7B 质量优先",
            "安装 DeepSeek-R1-Distill 7B Q8_0 问答基础模型（带推理过程）。",
            24,
            vec![r1_generation_artifact(source, "Qwen-7B", "Q8_0")],
            &["generation"],
        )),
        "generation_r1_8b_q4" => Ok(edition(
            edition_id,
            "DeepSeek-R1 8B 省内存",
            "安装 DeepSeek-R1-Distill 8B Q4_K_M 问答基础模型（带推理过程）。",
            16,
            vec![r1_generation_artifact(source, "Llama-8B", "Q4_K_M")],
            &["generation"],
        )),
        "generation_r1_8b_q8" => Ok(edition(
            edition_id,
            "DeepSeek-R1 8B 质量优先",
            "安装 DeepSeek-R1-Distill 8B Q8_0 问答基础模型（带推理过程）。",
            24,
            vec![r1_generation_artifact(source, "Llama-8B", "Q8_0")],
            &["generation"],
        )),
        "vision_qwen3_vl_4b_q4" => Ok(edition(
            edition_id,
            "Qwen3-VL 4B 省显存",
            "安装 Qwen3-VL 4B Q4_K_M 视觉语言模型与匹配的视觉投影文件。",
            12,
            vec![qwen3vl_artifact(source, "4B", "Q4_K_M")],
            &["vision"],
        )),
        "vision_qwen3_vl_4b_q8" => Ok(edition(
            edition_id,
            "Qwen3-VL 4B 质量优先",
            "安装 Qwen3-VL 4B Q8_0 视觉语言模型与匹配的视觉投影文件。",
            16,
            vec![qwen3vl_artifact(source, "4B", "Q8_0")],
            &["vision"],
        )),
        "vision_qwen3_vl_8b_q4" => Ok(edition(
            edition_id,
            "Qwen3-VL 8B 省显存",
            "安装 Qwen3-VL 8B Q4_K_M 视觉语言模型与匹配的视觉投影文件。",
            16,
            vec![qwen3vl_artifact(source, "8B", "Q4_K_M")],
            &["vision"],
        )),
        "vision_qwen3_vl_8b_q8" => Ok(edition(
            edition_id,
            "Qwen3-VL 8B 质量优先",
            "安装 Qwen3-VL 8B Q8_0 视觉语言模型与匹配的视觉投影文件。",
            24,
            vec![qwen3vl_artifact(source, "8B", "Q8_0")],
            &["vision"],
        )),
        "embedding_qwen3_embedding_0_6b" => Ok(edition(
            edition_id,
            "Qwen3-Embedding-0.6B",
            "安装 Qwen3-Embedding-0.6B ONNX 向量模型；启用后建立新的向量索引代际。",
            8,
            vec![qwen3_embedding_artifact(source)],
            &["embedding"],
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

fn require_huggingface(source: ModelSource) -> Result<(), AppError> {
    if source == ModelSource::Huggingface {
        Ok(())
    } else {
        Err(AppError::new(
            "MODEL_DOWNLOAD_SOURCE_UNAVAILABLE",
            "这个组件当前只有通过校验的 Hugging Face 固定版本",
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

fn generation_artifact_from_spec(
    source: ModelSource,
    model_id: &str,
    spec: DownloadSourceSpec,
) -> DownloadArtifact {
    DownloadArtifact {
        model_id: model_id.to_owned(),
        role: ModelRole::Generation,
        format: ModelFormat::Gguf,
        source,
        repository_id: spec.repository_id.to_owned(),
        revision: spec.revision.to_owned(),
        file_name: spec.file_name.to_owned(),
        url: artifact_url(source, spec.repository_id, spec.revision, spec.file_name),
        sha256: spec.sha256.to_owned(),
        size_bytes: spec.size_bytes,
        companion_files: Vec::new(),
        license_name: "Apache-2.0".into(),
        query_prefix: None,
        max_length: None,
    }
}

fn vision_artifact(source: ModelSource, quality: bool) -> DownloadArtifact {
    const REPOSITORY: &str = "Qwen/Qwen3-VL-2B-Instruct-GGUF";
    const REVISION: &str = "52d6c8ffea26cc873ac5ad116f8631268d7eb503";
    let (model_id, file_name, sha256, size_bytes) = if quality {
        (
            "Qwen3VL-2B-Instruct-Q8_0",
            "Qwen3VL-2B-Instruct-Q8_0.gguf",
            "1e8db19207c8ce0733ddd78c2eff8a9e22c27c82f4443df94c25792ed8fe04f2",
            1_834_427_424,
        )
    } else {
        (
            "Qwen3VL-2B-Instruct-Q4_K_M",
            "Qwen3VL-2B-Instruct-Q4_K_M.gguf",
            "089d75c52f4b7ffc56ba998ffc50aae89fcafc755f9e7208aacca281dca6c2ae",
            1_107_409_952,
        )
    };
    let projector_name = "mmproj-Qwen3VL-2B-Instruct-Q8_0.gguf";
    DownloadArtifact {
        model_id: model_id.into(),
        role: ModelRole::Vision,
        format: ModelFormat::Gguf,
        source,
        repository_id: REPOSITORY.into(),
        revision: REVISION.into(),
        file_name: file_name.into(),
        url: artifact_url(source, REPOSITORY, REVISION, file_name),
        sha256: sha256.into(),
        size_bytes,
        companion_files: vec![DownloadFile {
            file_name: projector_name.into(),
            remote_path: projector_name.into(),
            url: artifact_url(source, REPOSITORY, REVISION, projector_name),
            sha256: "f9a68fabba69c3b81e153367b2c7521030b0fa8bb0de400c9599c8e6725f9c82".into(),
            size_bytes: 445_053_216,
        }],
        license_name: "Apache-2.0".into(),
        query_prefix: None,
        max_length: None,
    }
}

// ==================== 2026-08-15 新增模型家族（双源验证） ====================
// Qwen3.5 与 Gemma 4 原生多模态：generation 与 vision 共用同一主模型 GGUF，
// vision 版额外携带 mmproj 投影文件；DeepSeek-R1 为纯文本推理模型。
// 全部 sha256 均取自 hf-mirror lfs oid，并经魔搭 CDN lfs-objects 交叉核对
// 一致（魔搭 tokenizer.json 与 HF 存在同源差异的条目按各自 sha 独立锁定）。

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

/// 双源视觉模型：主模型 + 同仓库配套视觉投影（如 mmproj），各自独立锁定。
fn vision_with_projector(
    source: ModelSource,
    model_id: &str,
    license_name: &str,
    main_hf: DownloadSourceSpec,
    main_ms: DownloadSourceSpec,
    proj_hf: DownloadSourceSpec,
    proj_ms: DownloadSourceSpec,
) -> DownloadArtifact {
    let main = if source == ModelSource::Huggingface {
        main_hf
    } else {
        main_ms
    };
    let proj = if source == ModelSource::Huggingface {
        proj_hf
    } else {
        proj_ms
    };
    DownloadArtifact {
        model_id: model_id.to_owned(),
        role: ModelRole::Vision,
        format: ModelFormat::Gguf,
        source,
        repository_id: main.repository_id.to_owned(),
        revision: main.revision.to_owned(),
        file_name: main.file_name.to_owned(),
        url: artifact_url(
            source,
            main.repository_id,
            main.revision,
            main.file_name,
        ),
        sha256: main.sha256.to_owned(),
        size_bytes: main.size_bytes,
        companion_files: vec![DownloadFile {
            file_name: proj.file_name.to_owned(),
            remote_path: proj.file_name.to_owned(),
            url: artifact_url(
                source,
                proj.repository_id,
                proj.revision,
                proj.file_name,
            ),
            sha256: proj.sha256.to_owned(),
            size_bytes: proj.size_bytes,
        }],
        license_name: license_name.into(),
        query_prefix: None,
        max_length: None,
    }
}

fn qwen35_generation_artifact(source: ModelSource, size: &str, quant: &str) -> DownloadArtifact {
    let (hf, ms) = qwen35_specs(size, quant);
    generation_artifact(source, &format!("Qwen3.5-{size}-{quant}"), hf, ms)
}

fn qwen35_vision_artifact(source: ModelSource, size: &str, quant: &str) -> DownloadArtifact {
    let (hf, ms) = qwen35_specs(size, quant);
    let (hf_mm, ms_mm) = qwen35_mmproj_specs(size);
    vision_with_projector(
        source,
        &format!("Qwen3.5-{size}-Visual-{quant}"),
        "Apache-2.0",
        hf,
        ms,
        hf_mm,
        ms_mm,
    )
}

fn qwen35_specs(size: &str, quant: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match (size, quant) {
        ("0.8B", "Q4_K_M") => (
            src_spec("unsloth/Qwen3.5-0.8B-GGUF", "6ab461498e2023f6e3c1baea90a8f0fe38ab64d0", "Qwen3.5-0.8B-Q4_K_M.gguf", "bd258782e35f7f458f8aced1adc053e6e92e89bc735ba3be89d38a06121dc517", 532_517_120),
            src_spec("unsloth/Qwen3.5-0.8B-GGUF", "88467eb7c8e3b6e7894c794f373050d4dbc6ae8a", "Qwen3.5-0.8B-Q4_K_M.gguf", "bd258782e35f7f458f8aced1adc053e6e92e89bc735ba3be89d38a06121dc517", 532_517_120),
        ),
        ("0.8B", "Q8_0") => (
            src_spec("unsloth/Qwen3.5-0.8B-GGUF", "6ab461498e2023f6e3c1baea90a8f0fe38ab64d0", "Qwen3.5-0.8B-Q8_0.gguf", "0ad885ffd4bb022fc4f0d33a3308fa108ef8613159d3b3a67e23abca056b7a6c", 811_843_840),
            src_spec("unsloth/Qwen3.5-0.8B-GGUF", "88467eb7c8e3b6e7894c794f373050d4dbc6ae8a", "Qwen3.5-0.8B-Q8_0.gguf", "0ad885ffd4bb022fc4f0d33a3308fa108ef8613159d3b3a67e23abca056b7a6c", 811_843_840),
        ),
        ("4B", "Q4_K_M") => (
            src_spec("unsloth/Qwen3.5-4B-GGUF", "e87f176479d0855a907a41277aca2f8ee7a09523", "Qwen3.5-4B-Q4_K_M.gguf", "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4", 2_740_937_888),
            src_spec("unsloth/Qwen3.5-4B-GGUF", "167b4afc359863325cb4164418c715421b4e9118", "Qwen3.5-4B-Q4_K_M.gguf", "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4", 2_740_937_888),
        ),
        ("4B", "Q8_0") => (
            src_spec("unsloth/Qwen3.5-4B-GGUF", "e87f176479d0855a907a41277aca2f8ee7a09523", "Qwen3.5-4B-Q8_0.gguf", "10cc391b403021dd11c614679d2fd92f611c3681d29e29651b717316965d61e1", 4_482_403_488),
            src_spec("unsloth/Qwen3.5-4B-GGUF", "167b4afc359863325cb4164418c715421b4e9118", "Qwen3.5-4B-Q8_0.gguf", "10cc391b403021dd11c614679d2fd92f611c3681d29e29651b717316965d61e1", 4_482_403_488),
        ),
        ("9B", "Q4_K_M") => (
            src_spec("unsloth/Qwen3.5-9B-GGUF", "3885219b6810b007914f3a7950a8d1b469d598a5", "Qwen3.5-9B-Q4_K_M.gguf", "03b74727a860a56338e042c4420bb3f04b2fec5734175f4cb9fa853daf52b7e8", 5_680_522_464),
            src_spec("unsloth/Qwen3.5-9B-GGUF", "ae90f0d1c1be2b9250b0ef68265615f6fe3c777b", "Qwen3.5-9B-Q4_K_M.gguf", "03b74727a860a56338e042c4420bb3f04b2fec5734175f4cb9fa853daf52b7e8", 5_680_522_464),
        ),
        ("9B", "Q8_0") => (
            src_spec("unsloth/Qwen3.5-9B-GGUF", "3885219b6810b007914f3a7950a8d1b469d598a5", "Qwen3.5-9B-Q8_0.gguf", "809626574d0cb43d4becfa56169980da2bb448f2299270f7be443cb89d0a6ae4", 9_527_502_048),
            src_spec("unsloth/Qwen3.5-9B-GGUF", "ae90f0d1c1be2b9250b0ef68265615f6fe3c777b", "Qwen3.5-9B-Q8_0.gguf", "809626574d0cb43d4becfa56169980da2bb448f2299270f7be443cb89d0a6ae4", 9_527_502_048),
        ),
        _ => unreachable!("unknown Qwen3.5 size/quant"),
    }
}

fn qwen35_mmproj_specs(size: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match size {
        "0.8B" => (
            src_spec("unsloth/Qwen3.5-0.8B-GGUF", "6ab461498e2023f6e3c1baea90a8f0fe38ab64d0", "mmproj-F16.gguf", "56e4c6cfe73b0c82e3e82bc518d7591997e61d81f723fc41a586f4fa69ea2453", 204_987_232),
            src_spec("unsloth/Qwen3.5-0.8B-GGUF", "88467eb7c8e3b6e7894c794f373050d4dbc6ae8a", "mmproj-F16.gguf", "56e4c6cfe73b0c82e3e82bc518d7591997e61d81f723fc41a586f4fa69ea2453", 204_987_232),
        ),
        "4B" => (
            src_spec("unsloth/Qwen3.5-4B-GGUF", "e87f176479d0855a907a41277aca2f8ee7a09523", "mmproj-F16.gguf", "cd88edcf8d031894960bb0c9c5b9b7e1fea6ebee02b9f7ce925a00d12891f864", 672_423_616),
            src_spec("unsloth/Qwen3.5-4B-GGUF", "167b4afc359863325cb4164418c715421b4e9118", "mmproj-F16.gguf", "cd88edcf8d031894960bb0c9c5b9b7e1fea6ebee02b9f7ce925a00d12891f864", 672_423_616),
        ),
        "9B" => (
            src_spec("unsloth/Qwen3.5-9B-GGUF", "3885219b6810b007914f3a7950a8d1b469d598a5", "mmproj-F16.gguf", "f70dc3509053962b0d0d3ee8a7eacebf5d60aa560cad78254ae8698516ae029f", 918_166_080),
            src_spec("unsloth/Qwen3.5-9B-GGUF", "ae90f0d1c1be2b9250b0ef68265615f6fe3c777b", "mmproj-F16.gguf", "f70dc3509053962b0d0d3ee8a7eacebf5d60aa560cad78254ae8698516ae029f", 918_166_080),
        ),
        _ => unreachable!("unknown Qwen3.5 size"),
    }
}

fn gemma4_generation_artifact(source: ModelSource, tag: &str, quant: &str) -> DownloadArtifact {
    let (hf, ms) = gemma4_specs(tag, quant);
    generation_artifact(source, &format!("Gemma-4-{tag}-{quant}"), hf, ms)
}

fn gemma4_vision_artifact(source: ModelSource, tag: &str, quant: &str) -> DownloadArtifact {
    let (hf, ms) = gemma4_specs(tag, quant);
    let (hf_mm, ms_mm) = gemma4_mmproj_specs(tag);
    vision_with_projector(
        source,
        &format!("Gemma-4-{tag}-Visual-{quant}"),
        "Gemma",
        hf,
        ms,
        hf_mm,
        ms_mm,
    )
}

fn gemma4_specs(tag: &str, quant: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match (tag, quant) {
        ("E2B", "Q4_K_M") => (
            src_spec("unsloth/gemma-4-E2B-it-GGUF", "0314792d7f1f7e229411f620751375812bb9faf2", "gemma-4-E2B-it-Q4_K_M.gguf", "740185b21d22ceb83a11c3aa62ad5842ef32c70f6096d756bbee85a1e4ec34b8", 3_106_738_272),
            src_spec("unsloth/gemma-4-E2B-it-GGUF", "ca58cd60378599dbbc95cde2872dd92b1c155344", "gemma-4-E2B-it-Q4_K_M.gguf", "740185b21d22ceb83a11c3aa62ad5842ef32c70f6096d756bbee85a1e4ec34b8", 3_106_738_272),
        ),
        ("E2B", "Q8_0") => (
            src_spec("unsloth/gemma-4-E2B-it-GGUF", "0314792d7f1f7e229411f620751375812bb9faf2", "gemma-4-E2B-it-Q8_0.gguf", "605d3c2647d7c58c1e4b5375ccb5702acf94c2611b4c8d4877812f8fdd32d053", 5_048_352_864),
            src_spec("unsloth/gemma-4-E2B-it-GGUF", "ca58cd60378599dbbc95cde2872dd92b1c155344", "gemma-4-E2B-it-Q8_0.gguf", "605d3c2647d7c58c1e4b5375ccb5702acf94c2611b4c8d4877812f8fdd32d053", 5_048_352_864),
        ),
        ("E4B", "Q4_K_M") => (
            src_spec("unsloth/gemma-4-E4B-it-GGUF", "bfc15c382204943c3a8fff0c750b94ae2364d7a3", "gemma-4-E4B-it-Q4_K_M.gguf", "85a896a047553e842f25297ee5b031d64ff30147d9c4af17b1e4b394cd1fab87", 4_977_171_584),
            src_spec("unsloth/gemma-4-E4B-it-GGUF", "3bb557d24864440e2cd06363f9747b227597283e", "gemma-4-E4B-it-Q4_K_M.gguf", "85a896a047553e842f25297ee5b031d64ff30147d9c4af17b1e4b394cd1fab87", 4_977_171_584),
        ),
        ("E4B", "Q8_0") => (
            src_spec("unsloth/gemma-4-E4B-it-GGUF", "bfc15c382204943c3a8fff0c750b94ae2364d7a3", "gemma-4-E4B-it-Q8_0.gguf", "f8854aa4480df62585a279e7ca0a881554fc18a41c59c4f62642d16a2ae47012", 8_192_953_472),
            src_spec("unsloth/gemma-4-E4B-it-GGUF", "3bb557d24864440e2cd06363f9747b227597283e", "gemma-4-E4B-it-Q8_0.gguf", "f8854aa4480df62585a279e7ca0a881554fc18a41c59c4f62642d16a2ae47012", 8_192_953_472),
        ),
        ("12b", "Q4_K_M") => (
            src_spec("unsloth/gemma-4-12b-it-GGUF", "fc034cfff751157913579611efad8462ac1be606", "gemma-4-12b-it-Q4_K_M.gguf", "0a270ec9fe6b34f4a0d33992b6135117b484ebc4766ab76b51d4ae8c457e4c42", 7_121_861_440),
            src_spec("unsloth/gemma-4-12b-it-GGUF", "f95e741be36791c2da72b68f6cb43715ebaec32d", "gemma-4-12b-it-Q4_K_M.gguf", "0a270ec9fe6b34f4a0d33992b6135117b484ebc4766ab76b51d4ae8c457e4c42", 7_121_861_440),
        ),
        ("12b", "Q8_0") => (
            src_spec("unsloth/gemma-4-12b-it-GGUF", "fc034cfff751157913579611efad8462ac1be606", "gemma-4-12b-it-Q8_0.gguf", "f20e7ff1be28c283eeeb18fc895733791c56a5851d5cd3fe9691b7f7d12afa72", 12_669_647_680),
            src_spec("unsloth/gemma-4-12b-it-GGUF", "f95e741be36791c2da72b68f6cb43715ebaec32d", "gemma-4-12b-it-Q8_0.gguf", "f20e7ff1be28c283eeeb18fc895733791c56a5851d5cd3fe9691b7f7d12afa72", 12_669_647_680),
        ),
        _ => unreachable!("unknown Gemma 4 tag/quant"),
    }
}

fn gemma4_mmproj_specs(tag: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match tag {
        "E2B" => (
            src_spec("unsloth/gemma-4-E2B-it-GGUF", "0314792d7f1f7e229411f620751375812bb9faf2", "mmproj-F16.gguf", "140be8d7849741f88c50757d529b84373ee8e27052cc2236855b537f4a8215fa", 985_654_080),
            src_spec("unsloth/gemma-4-E2B-it-GGUF", "ca58cd60378599dbbc95cde2872dd92b1c155344", "mmproj-F16.gguf", "140be8d7849741f88c50757d529b84373ee8e27052cc2236855b537f4a8215fa", 985_654_080),
        ),
        "E4B" => (
            src_spec("unsloth/gemma-4-E4B-it-GGUF", "bfc15c382204943c3a8fff0c750b94ae2364d7a3", "mmproj-F16.gguf", "ddf46c21d7078e95338cfc22306b19b276a29a5ad089023449dd54d4b6170a51", 990_372_672),
            src_spec("unsloth/gemma-4-E4B-it-GGUF", "3bb557d24864440e2cd06363f9747b227597283e", "mmproj-F16.gguf", "ddf46c21d7078e95338cfc22306b19b276a29a5ad089023449dd54d4b6170a51", 990_372_672),
        ),
        "12b" => (
            src_spec("unsloth/gemma-4-12b-it-GGUF", "fc034cfff751157913579611efad8462ac1be606", "mmproj-F16.gguf", "91f086971e56d7a7d8d39e271873fccdb49541bd259d6e02c401a4f1cb7a219e", 175_115_840),
            src_spec("unsloth/gemma-4-12b-it-GGUF", "f95e741be36791c2da72b68f6cb43715ebaec32d", "mmproj-F16.gguf", "91f086971e56d7a7d8d39e271873fccdb49541bd259d6e02c401a4f1cb7a219e", 175_115_840),
        ),
        _ => unreachable!("unknown Gemma 4 tag"),
    }
}

fn r1_generation_artifact(source: ModelSource, tag: &str, quant: &str) -> DownloadArtifact {
    let (hf, ms) = r1_specs(tag, quant);
    let selected = if source == ModelSource::Huggingface { hf } else { ms };
    let model_id = format!("DeepSeek-R1-Distill-{tag}-{quant}");
    DownloadArtifact {
        model_id,
        role: ModelRole::Generation,
        format: ModelFormat::Gguf,
        source,
        repository_id: selected.repository_id.to_owned(),
        revision: selected.revision.to_owned(),
        file_name: selected.file_name.to_owned(),
        url: artifact_url(source, selected.repository_id, selected.revision, selected.file_name),
        sha256: selected.sha256.to_owned(),
        size_bytes: selected.size_bytes,
        companion_files: Vec::new(),
        license_name: "MIT".into(),
        query_prefix: None,
        max_length: None,
    }
}

fn r1_specs(tag: &str, quant: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match (tag, quant) {
        ("Qwen-1.5B", "Q4_K_M") => (
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-1.5B-GGUF", "3cb4d15544a2a5e07439592b9a0965b6445fbd34", "DeepSeek-R1-Distill-Qwen-1.5B-Q4_K_M.gguf", "f3bdf9cf31dee4b57ae4e455a1cb0d01b5c2c1b50d72d3112141c195506c2840", 1_117_321_312),
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-1.5B-GGUF", "fa0539ea11c2f7ce5b540004ce6e8ed20d462f77", "DeepSeek-R1-Distill-Qwen-1.5B-Q4_K_M.gguf", "f3bdf9cf31dee4b57ae4e455a1cb0d01b5c2c1b50d72d3112141c195506c2840", 1_117_321_312),
        ),
        ("Qwen-1.5B", "Q8_0") => (
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-1.5B-GGUF", "3cb4d15544a2a5e07439592b9a0965b6445fbd34", "DeepSeek-R1-Distill-Qwen-1.5B-Q8_0.gguf", "068a721e47419ccfc94b6420118f772478544e1a0d4fad7118212774b3f9ba9e", 1_894_532_416),
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-1.5B-GGUF", "fa0539ea11c2f7ce5b540004ce6e8ed20d462f77", "DeepSeek-R1-Distill-Qwen-1.5B-Q8_0.gguf", "068a721e47419ccfc94b6420118f772478544e1a0d4fad7118212774b3f9ba9e", 1_894_532_416),
        ),
        ("Qwen-7B", "Q4_K_M") => (
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-7B-GGUF", "097680e4eed7a83b3df6b0bb5e5134099cadf1b0", "DeepSeek-R1-Distill-Qwen-7B-Q4_K_M.gguf", "78272d8d32084548bd450394a560eb2d70de8232ab96a725769b1f9171235c1c", 4_683_073_248),
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-7B-GGUF", "e11beec1cba0ea15a9ad4ab9615e0561ce09fbfe", "DeepSeek-R1-Distill-Qwen-7B-Q4_K_M.gguf", "78272d8d32084548bd450394a560eb2d70de8232ab96a725769b1f9171235c1c", 4_683_073_248),
        ),
        ("Qwen-7B", "Q8_0") => (
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-7B-GGUF", "097680e4eed7a83b3df6b0bb5e5134099cadf1b0", "DeepSeek-R1-Distill-Qwen-7B-Q8_0.gguf", "7307bfbb539fd2e8ed636315440781fed23140dc7f911f64b33418f5b978281b", 8_098_524_896),
            src_spec("unsloth/DeepSeek-R1-Distill-Qwen-7B-GGUF", "e11beec1cba0ea15a9ad4ab9615e0561ce09fbfe", "DeepSeek-R1-Distill-Qwen-7B-Q8_0.gguf", "7307bfbb539fd2e8ed636315440781fed23140dc7f911f64b33418f5b978281b", 8_098_524_896),
        ),
        ("Llama-8B", "Q4_K_M") => (
            src_spec("unsloth/DeepSeek-R1-Distill-Llama-8B-GGUF", "615f8936e16dfde29dcc00be71145d4d5ce8ed53", "DeepSeek-R1-Distill-Llama-8B-Q4_K_M.gguf", "0addb1339a82385bcd973186cd80d18dcc71885d45eabd899781a118d03827d9", 4_920_737_216),
            src_spec("unsloth/DeepSeek-R1-Distill-Llama-8B-GGUF", "56071ca28dbd0b796b51b45d623c73d43d9ba6ec", "DeepSeek-R1-Distill-Llama-8B-Q4_K_M.gguf", "0addb1339a82385bcd973186cd80d18dcc71885d45eabd899781a118d03827d9", 4_920_737_216),
        ),
        ("Llama-8B", "Q8_0") => (
            src_spec("unsloth/DeepSeek-R1-Distill-Llama-8B-GGUF", "615f8936e16dfde29dcc00be71145d4d5ce8ed53", "DeepSeek-R1-Distill-Llama-8B-Q8_0.gguf", "8c6e3924d662d3f24a96b228a5c317510c27e91c587e71e78877ed18a875ec82", 8_540_773_088),
            src_spec("unsloth/DeepSeek-R1-Distill-Llama-8B-GGUF", "56071ca28dbd0b796b51b45d623c73d43d9ba6ec", "DeepSeek-R1-Distill-Llama-8B-Q8_0.gguf", "8c6e3924d662d3f24a96b228a5c317510c27e91c587e71e78877ed18a875ec82", 8_540_773_088),
        ),
        _ => unreachable!("unknown R1 tag/quant"),
    }
}

fn qwen3vl_artifact(source: ModelSource, size: &str, quant: &str) -> DownloadArtifact {
    let (hf, ms) = qwen3vl_specs(size, quant);
    let (hf_mm, ms_mm) = qwen3vl_mmproj_specs(size);
    vision_with_projector(
        source,
        &format!("Qwen3VL-{size}-Instruct-{quant}"),
        "Apache-2.0",
        hf,
        ms,
        hf_mm,
        ms_mm,
    )
}

fn qwen3vl_specs(size: &str, quant: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match (size, quant) {
        ("4B", "Q4_K_M") => (
            src_spec("Qwen/Qwen3-VL-4B-Instruct-GGUF", "1cd86afb9a95c410a6038ab3b40d8b578c892266", "Qwen3VL-4B-Instruct-Q4_K_M.gguf", "66358cb18bb6b3b1b6675aa412c7a88ef01d228f481184d13668e5201c730a0a", 2_497_281_664),
            src_spec("Qwen/Qwen3-VL-4B-Instruct-GGUF", "1813bcf9426c995f21c6e674692d3406df7c1f4b", "Qwen3VL-4B-Instruct-Q4_K_M.gguf", "66358cb18bb6b3b1b6675aa412c7a88ef01d228f481184d13668e5201c730a0a", 2_497_281_664),
        ),
        ("4B", "Q8_0") => (
            src_spec("Qwen/Qwen3-VL-4B-Instruct-GGUF", "1cd86afb9a95c410a6038ab3b40d8b578c892266", "Qwen3VL-4B-Instruct-Q8_0.gguf", "054721f478bc5fa6beffb7f38eae575d45298f88cbb8d2f83ef675a727863eb1", 4_280_406_144),
            src_spec("Qwen/Qwen3-VL-4B-Instruct-GGUF", "1813bcf9426c995f21c6e674692d3406df7c1f4b", "Qwen3VL-4B-Instruct-Q8_0.gguf", "054721f478bc5fa6beffb7f38eae575d45298f88cbb8d2f83ef675a727863eb1", 4_280_406_144),
        ),
        ("8B", "Q4_K_M") => (
            src_spec("Qwen/Qwen3-VL-8B-Instruct-GGUF", "f982a07559d4a2f6c8744d840bf6fccab30eea96", "Qwen3VL-8B-Instruct-Q4_K_M.gguf", "67d1659bfe71b89d50b45a4ad1a9e5b997e5bb16ce5da66a6a6167abd569e9e2", 5_027_784_800),
            src_spec("Qwen/Qwen3-VL-8B-Instruct-GGUF", "d01926b298a22e965c213b82cacc9cdf50dbbad7", "Qwen3VL-8B-Instruct-Q4_K_M.gguf", "67d1659bfe71b89d50b45a4ad1a9e5b997e5bb16ce5da66a6a6167abd569e9e2", 5_027_784_800),
        ),
        ("8B", "Q8_0") => (
            src_spec("Qwen/Qwen3-VL-8B-Instruct-GGUF", "f982a07559d4a2f6c8744d840bf6fccab30eea96", "Qwen3VL-8B-Instruct-Q8_0.gguf", "0d264b3941185d00a74f75c4245521dae088ff1efc90ab8d1754e83f5844adb0", 8_709_519_456),
            src_spec("Qwen/Qwen3-VL-8B-Instruct-GGUF", "d01926b298a22e965c213b82cacc9cdf50dbbad7", "Qwen3VL-8B-Instruct-Q8_0.gguf", "0d264b3941185d00a74f75c4245521dae088ff1efc90ab8d1754e83f5844adb0", 8_709_519_456),
        ),
        _ => unreachable!("unknown Qwen3-VL size/quant"),
    }
}

fn qwen3vl_mmproj_specs(size: &str) -> (DownloadSourceSpec, DownloadSourceSpec) {
    match size {
        "4B" => (
            src_spec("Qwen/Qwen3-VL-4B-Instruct-GGUF", "1cd86afb9a95c410a6038ab3b40d8b578c892266", "mmproj-Qwen3VL-4B-Instruct-F16.gguf", "256f3a43bd4205ffef48d6b92715e1e70b5b0e9aef06522584967513a9985331", 836_180_256),
            src_spec("Qwen/Qwen3-VL-4B-Instruct-GGUF", "1813bcf9426c995f21c6e674692d3406df7c1f4b", "mmproj-Qwen3VL-4B-Instruct-F16.gguf", "256f3a43bd4205ffef48d6b92715e1e70b5b0e9aef06522584967513a9985331", 836_180_256),
        ),
        "8B" => (
            src_spec("Qwen/Qwen3-VL-8B-Instruct-GGUF", "f982a07559d4a2f6c8744d840bf6fccab30eea96", "mmproj-Qwen3VL-8B-Instruct-F16.gguf", "ca524100ebf825c9a870db1c580d03879e0da0ab2541697e2458e64891cf9d38", 1_159_029_824),
            src_spec("Qwen/Qwen3-VL-8B-Instruct-GGUF", "d01926b298a22e965c213b82cacc9cdf50dbbad7", "mmproj-Qwen3VL-8B-Instruct-F16.gguf", "ca524100ebf825c9a870db1c580d03879e0da0ab2541697e2458e64891cf9d38", 1_159_029_824),
        ),
        _ => unreachable!("unknown Qwen3-VL size"),
    }
}

/// Qwen3-Embedding-0.6B：onnx-community 官方 ONNX 导出（int8 自包含），双源。
/// 魔搭镜像的 tokenizer.json 与 HF 为同源不同提交（sha 不同），按源独立锁定。
fn qwen3_embedding_artifact(source: ModelSource) -> DownloadArtifact {
    const REPOSITORY: &str = "onnx-community/Qwen3-Embedding-0.6B-ONNX";
    let (revision, tokenizer_sha) = match source {
        ModelSource::Huggingface => (
            "c25a394dd583836952667c12f008335071b3f43d",
            "def76fb086971c7831053bcac4a9148ea76a4c83daf5b6dcbd987106aa8a85d2",
        ),
        ModelSource::Modelscope => (
            "69da7161993e010fc5c62d2311fe77f2a4f888ec",
            "def76fb086971c7867b829c23a26261e38d9d74e02139253b38aeb9df8b4b50a",
        ),
        ModelSource::LocalImport => unreachable!("download catalog has no local source"),
    };
    DownloadArtifact {
        model_id: "qwen3-embedding-0.6b-int8".into(),
        role: ModelRole::Embedding,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: revision.into(),
        file_name: "model_int8.onnx".into(),
        url: artifact_url(source, REPOSITORY, revision, "onnx/model_int8.onnx"),
        sha256: "6d0ea863f78b4a84afa3c7fcba1ec341572b5e28121aef77b7092b1dfdf679c7".into(),
        size_bytes: 613_527_539,
        companion_files: vec![DownloadFile {
            file_name: "tokenizer.json".into(),
            remote_path: "tokenizer.json".into(),
            url: artifact_url(source, REPOSITORY, revision, "tokenizer.json"),
            sha256: tokenizer_sha.into(),
            size_bytes: 11_423_705,
        }],
        license_name: "Apache-2.0".into(),
        query_prefix: Some("为这个句子生成表示以用于检索相关文章：".into()),
        max_length: Some(8192),
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

fn ocr_artifact(source: ModelSource) -> DownloadArtifact {
    // Locked to RapidAI/RapidOCR v3.5.0 on ModelScope. All four files were
    // downloaded and verified against the sha256 values below on 2026-08-13.
    const REPOSITORY: &str = "RapidAI/RapidOCR";
    const REVISION: &str = "v3.5.0";
    DownloadArtifact {
        model_id: "ch_PP-OCRv5_mobile".into(),
        role: ModelRole::Ocr,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: REVISION.into(),
        file_name: "ch_PP-OCRv5_rec_mobile_infer.onnx".into(),
        url: artifact_url(
            source,
            REPOSITORY,
            REVISION,
            "onnx/PP-OCRv5/rec/ch_PP-OCRv5_rec_mobile_infer.onnx",
        ),
        sha256: "5825fc7ebf84ae7a412be049820b4d86d77620f204a041697b0494669b1742c5".into(),
        size_bytes: 16_631_306,
        companion_files: vec![
            DownloadFile {
                file_name: "ch_PP-OCRv5_mobile_det.onnx".into(),
                remote_path: "onnx/PP-OCRv5/det/ch_PP-OCRv5_mobile_det.onnx".into(),
                url: artifact_url(
                    source,
                    REPOSITORY,
                    REVISION,
                    "onnx/PP-OCRv5/det/ch_PP-OCRv5_mobile_det.onnx",
                ),
                sha256: "4d97c44a20d30a81aad087d6a396b08f786c4635742afc391f6621f5c6ae78ae".into(),
                size_bytes: 4_819_576,
            },
            DownloadFile {
                file_name: "ch_ppocr_mobile_v2.0_cls_infer.onnx".into(),
                remote_path: "onnx/PP-OCRv4/cls/ch_ppocr_mobile_v2.0_cls_infer.onnx".into(),
                url: artifact_url(
                    source,
                    REPOSITORY,
                    REVISION,
                    "onnx/PP-OCRv4/cls/ch_ppocr_mobile_v2.0_cls_infer.onnx",
                ),
                sha256: "e47acedf663230f8863ff1ab0e64dd2d82b838fceb5957146dab185a89d6215c".into(),
                size_bytes: 585_532,
            },
            DownloadFile {
                file_name: "ppocrv5_dict.txt".into(),
                remote_path: "paddle/PP-OCRv5/rec/ch_PP-OCRv5_rec_mobile_infer/ppocrv5_dict.txt"
                    .into(),
                url: artifact_url(
                    source,
                    REPOSITORY,
                    REVISION,
                    "paddle/PP-OCRv5/rec/ch_PP-OCRv5_rec_mobile_infer/ppocrv5_dict.txt",
                ),
                sha256: "d1979e9f794c464c0d2e0b70a7fe14dd978e9dc644c0e71f14158cdf8342af1b".into(),
                size_bytes: 74_012,
            },
        ],
        license_name: "Apache-2.0".into(),
        query_prefix: None,
        max_length: None,
    }
}

fn tts_vits_artifact(source: ModelSource) -> DownloadArtifact {
    // Locked to csukuangfj/sherpa-onnx-vits-zh-ll on Hugging Face (revision pinned).
    // model.onnx is LFS-locked via its ETag; tokens/lexicon were downloaded and
    // verified locally on 2026-08-12.
    const REPOSITORY: &str = "csukuangfj/sherpa-onnx-vits-zh-ll";
    const REVISION: &str = "7ddf37bcacf05ed56afee360d96835be633a5265";
    DownloadArtifact {
        model_id: "vits_zh_ll".into(),
        role: ModelRole::Tts,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: REVISION.into(),
        file_name: "model.onnx".into(),
        url: artifact_url(source, REPOSITORY, REVISION, "model.onnx"),
        sha256: "4704ba4197dbdb1e91eadb912dc90cc6a8c58935d0da36fe40e62e13d3675c0b".into(),
        size_bytes: 121_100_803,
        companion_files: vec![
            DownloadFile {
                file_name: "tokens.txt".into(),
                remote_path: "tokens.txt".into(),
                url: artifact_url(source, REPOSITORY, REVISION, "tokens.txt"),
                sha256: "34b035b9aeb070df6188b022f29c00e0e142c7ade9f25611ced65db5e9cc8402".into(),
                size_bytes: 331,
            },
            DownloadFile {
                file_name: "lexicon.txt".into(),
                remote_path: "lexicon.txt".into(),
                url: artifact_url(source, REPOSITORY, REVISION, "lexicon.txt"),
                sha256: "b3a82f16b286c424953dea3686039e7ab465fa8e15d87ef8abd0ec69175beb21".into(),
                size_bytes: 376_868,
            },
        ],
        license_name: "Apache-2.0".into(),
        query_prefix: None,
        max_length: None,
    }
}

fn asr_paraformer_artifact(source: ModelSource) -> DownloadArtifact {
    // Locked to csukuangfj/sherpa-onnx-paraformer-zh-small-2024-03-09 on Hugging
    // Face (revision pinned). model.int8.onnx is LFS-locked via its ETag;
    // tokens.txt was downloaded and verified locally on 2026-08-12.
    const REPOSITORY: &str = "csukuangfj/sherpa-onnx-paraformer-zh-small-2024-03-09";
    const REVISION: &str = "63ddc3cd0f2810b68289a7b3876e62ef5d53d6df";
    DownloadArtifact {
        model_id: "paraformer-zh-small".into(),
        role: ModelRole::Asr,
        format: ModelFormat::Onnx,
        source,
        repository_id: REPOSITORY.into(),
        revision: REVISION.into(),
        file_name: "model.int8.onnx".into(),
        url: artifact_url(source, REPOSITORY, REVISION, "model.int8.onnx"),
        sha256: "3ef6c19369b912f7caf3cef8e545c5ccd1a33d9d7ec792a46668dc41c4b229ec".into(),
        size_bytes: 81_828_675,
        companion_files: vec![
            DownloadFile {
                file_name: "tokens.txt".into(),
                remote_path: "tokens.txt".into(),
                url: artifact_url(source, REPOSITORY, REVISION, "tokens.txt"),
                sha256: "4b2d964e18b9cf139b473003b6698fb2ed9a2a5ec55b93daa677b28f578897aa".into(),
                size_bytes: 75_352,
            },
            DownloadFile {
                file_name: "silero_vad.onnx".into(),
                remote_path: "sherpa-onnx/asr-models/silero_vad.onnx".into(),
                url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx".into(),
                sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6".into(),
                size_bytes: 643_854,
            },
        ],
        license_name: "Apache-2.0".into(),
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
    fn online_catalog_is_a_complete_rag_bundle() {
        for edition in built_in_model_editions() {
            assert_eq!(edition.artifacts.len(), 2);
            assert!(edition.capabilities.contains(&"rag".to_owned()));
            assert!(
                edition
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.role == ModelRole::Generation)
            );
            let embedding = edition
                .artifacts
                .iter()
                .find(|artifact| artifact.role == ModelRole::Embedding)
                .expect("embedding component");
            assert!(embedding.query_prefix.is_some());
            assert!(
                embedding
                    .companion_files
                    .iter()
                    .any(|file| file.file_name == "tokenizer.json")
            );
            assert_eq!(
                edition.download_size_bytes,
                edition
                    .artifacts
                    .iter()
                    .map(DownloadArtifact::total_size_bytes)
                    .sum::<u64>()
            );
        }
    }

    #[test]
    fn hardware_based_recommendation_fits_ram_and_vram() {
        let catalog = built_in_model_catalog();
        // 8 GB RAM, unknown GPU: R1 1.5B Q8 (4.5 GB) is the largest generation that
        // fits 8 GB; BGE-M3 (2.5 GB) is the largest fitting embedding.
        let ids = recommended_catalog_ids(&catalog, Some(8), None);
        assert_eq!(
            ids,
            vec![
                "r1-1-5b-q8".to_owned(),
                "bge-m3".to_owned(),
                "qwen3-vl-2b-q4".to_owned(),
            ]
        );
        // 16 GB RAM + 8 GB VRAM: quality-first generation and vision.
        // Gemma 4 E2B Q8 (12 GB) is the largest fitting generation; Qwen3-VL 8B Q4
        // (12 GB, 7 GB VRAM) is the largest vision that still fits 8 GB VRAM.
        let ids = recommended_catalog_ids(&catalog, Some(16), Some(8));
        assert_eq!(
            ids,
            vec![
                "gemma4-e2b-q8".to_owned(),
                "bge-m3".to_owned(),
                "qwen3-vl-8b-q4".to_owned(),
            ]
        );
        // 4 GB RAM: R1 1.5B Q4 (3.0 GB) and 1.7B Q4 tie on memory and VRAM;
        // max_by keeps the last equal maximum (standard-library behavior), so R1
        // wins. BGE-M3 (2.5 GB) and Qwen3.5 0.8B vision Q8 also fit.
        let ids = recommended_catalog_ids(&catalog, Some(4), None);
        assert_eq!(
            ids,
            vec![
                "r1-1-5b-q4".to_owned(),
                "bge-m3".to_owned(),
                "qwen3-5-vision-0-8b-q8".to_owned(),
            ]
        );
    }

    #[test]
    fn providers_have_independent_immutable_revisions() {
        for edition_id in ["light", "standard"] {
            let huggingface = model_edition_by_id(edition_id, "huggingface").expect("hf source");
            let modelscope = model_edition_by_id(edition_id, "modelscope").expect("ms source");
            for (hf, ms) in huggingface.artifacts.iter().zip(&modelscope.artifacts) {
                assert_ne!(hf.revision, ms.revision);
                assert!(hf.url.starts_with("https://huggingface.co/"));
                assert!(ms.url.starts_with("https://modelscope.cn/api/v1/models/"));
                assert!(!hf.revision.eq("main") && !ms.revision.eq("master"));
                assert_eq!(hf.sha256.len(), 64);
                assert_eq!(ms.sha256.len(), 64);
                assert!(hf.size_bytes > 0 && ms.size_bytes > 0);
            }
            let hf_generation = huggingface
                .artifacts
                .iter()
                .find(|artifact| artifact.role == ModelRole::Generation)
                .expect("hf generation");
            let ms_generation = modelscope
                .artifacts
                .iter()
                .find(|artifact| artifact.role == ModelRole::Generation)
                .expect("ms generation");
            assert_ne!(hf_generation.repository_id, ms_generation.repository_id);
            assert_ne!(hf_generation.sha256, ms_generation.sha256);
            assert_ne!(hf_generation.size_bytes, ms_generation.size_bytes);
        }
    }
}
