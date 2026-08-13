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
            "qwen3-0.6b-q8",
            ModelRole::Generation,
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
            "qwen3-1.7b-q8",
            ModelRole::Generation,
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
            "bge-small-zh-int8",
            ModelRole::Embedding,
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
    ]
}

#[allow(clippy::too_many_arguments)]
fn catalog_entry(
    catalog_id: &str,
    role: ModelRole,
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
        "generation_qwen3_1_7b",
        "generation_qwen3_4b",
        "embedding_bge_small",
        "vision_qwen3_vl_2b_q4",
        "vision_qwen3_vl_2b_q8",
        "reranker_bge_base_int8",
        "ocr_paddleocr",
        "tts_sherpa_vits",
        "asr_sherpa_paraformer",
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
        "generation_qwen3_4b" => Ok(edition(
            edition_id,
            "Qwen3 4B",
            "仅安装并切换质量优先的问答基础模型。",
            12,
            vec![qwen_4()],
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
        // 8 GB RAM, unknown GPU: mid-tier generation, 省显存 vision, bge-small.
        let ids = recommended_catalog_ids(&catalog, Some(8), None);
        assert_eq!(
            ids,
            vec![
                "qwen3-1.7b-q8".to_owned(),
                "bge-small-zh-int8".to_owned(),
                "qwen3-vl-2b-q4".to_owned(),
            ]
        );
        // 16 GB RAM + 8 GB VRAM: quality-first generation and vision.
        let ids = recommended_catalog_ids(&catalog, Some(16), Some(8));
        assert_eq!(
            ids,
            vec![
                "qwen3-4b-q4".to_owned(),
                "bge-small-zh-int8".to_owned(),
                "qwen3-vl-2b-q8".to_owned(),
            ]
        );
        // 4 GB RAM: nothing fits, fall back to the lightest generation entry.
        let ids = recommended_catalog_ids(&catalog, Some(4), None);
        assert_eq!(
            ids,
            vec![
                "qwen3-0.6b-q8".to_owned(),
                "bge-small-zh-int8".to_owned(),
                "qwen3-vl-2b-q4".to_owned(),
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
