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
    pub artifact: DownloadArtifact,
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
    pub license_name: String,
}

pub fn built_in_model_editions() -> Vec<ModelEdition> {
    vec![
        ModelEdition {
            edition_id: "light".into(),
            name: "轻量版".into(),
            description: "适合低配置电脑的0.6B本地对话模型；全文搜索和Windows OCR无需模型。".into(),
            recommended_memory_gb: 8,
            download_size_bytes: 639_446_688,
            capabilities: vec!["generation".into()],
            artifact: DownloadArtifact {
                model_id: "Qwen3-0.6B-Q8_0".into(),
                role: ModelRole::Generation,
                format: ModelFormat::Gguf,
                source: ModelSource::Huggingface,
                repository_id: "Qwen/Qwen3-0.6B-GGUF".into(),
                revision: "1eaf4d9657fe65ad10a51eab76a8db5b363bddaa".into(),
                file_name: "Qwen3-0.6B-Q8_0.gguf".into(),
                url: "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/resolve/1eaf4d9657fe65ad10a51eab76a8db5b363bddaa/Qwen3-0.6B-Q8_0.gguf?download=true".into(),
                sha256: "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031".into(),
                size_bytes: 639_446_688,
                license_name: "Apache-2.0".into(),
            },
        },
        ModelEdition {
            edition_id: "standard".into(),
            name: "标准版".into(),
            description: "效果更好的4B Q4本地对话模型；建议至少12 GB内存。".into(),
            recommended_memory_gb: 12,
            download_size_bytes: 2_497_280_256,
            capabilities: vec!["generation".into()],
            artifact: DownloadArtifact {
                model_id: "Qwen3-4B-Q4_K_M".into(),
                role: ModelRole::Generation,
                format: ModelFormat::Gguf,
                source: ModelSource::Huggingface,
                repository_id: "Qwen/Qwen3-4B-GGUF".into(),
                revision: "a9a60d009fa7ff9606305047c2bf77ac25dbec49".into(),
                file_name: "Qwen3-4B-Q4_K_M.gguf".into(),
                url: "https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/a9a60d009fa7ff9606305047c2bf77ac25dbec49/Qwen3-4B-Q4_K_M.gguf?download=true".into(),
                sha256: "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5".into(),
                size_bytes: 2_497_280_256,
                license_name: "Apache-2.0".into(),
            },
        },
    ]
}

pub fn model_edition_by_id(edition_id: &str, source: &str) -> Result<ModelEdition, AppError> {
    let mut edition = built_in_model_editions()
        .into_iter()
        .find(|edition| edition.edition_id == edition_id)
        .ok_or_else(|| AppError::new("MODEL_EDITION_NOT_FOUND", "模型版本不存在", false))?;
    match source {
        "huggingface" => Ok(edition),
        "modelscope" => {
            edition.artifact.source = ModelSource::Modelscope;
            edition.artifact.revision = "master".into();
            edition.artifact.url = format!(
                "https://modelscope.cn/models/{}/resolve/master/{}",
                edition.artifact.repository_id, edition.artifact.file_name
            );
            Ok(edition)
        }
        _ => Err(AppError::new(
            "MODEL_DOWNLOAD_SOURCE_UNAVAILABLE",
            "模型下载来源必须是Hugging Face或ModelScope",
            false,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_catalog_is_revision_and_hash_pinned() {
        for edition in built_in_model_editions() {
            assert!(edition.artifact.url.starts_with("https://huggingface.co/"));
            assert!(edition.artifact.url.contains(&edition.artifact.revision));
            assert_eq!(edition.artifact.sha256.len(), 64);
            assert_eq!(edition.download_size_bytes, edition.artifact.size_bytes);
            assert_eq!(edition.artifact.source, ModelSource::Huggingface);
        }
    }

    #[test]
    fn modelscope_mirror_keeps_the_same_exact_artifact_hash() {
        for edition_id in ["light", "standard"] {
            let huggingface = model_edition_by_id(edition_id, "huggingface").expect("hf source");
            let modelscope = model_edition_by_id(edition_id, "modelscope").expect("ms source");
            assert_eq!(modelscope.artifact.source, ModelSource::Modelscope);
            assert!(
                modelscope
                    .artifact
                    .url
                    .starts_with("https://modelscope.cn/models/")
            );
            assert_eq!(modelscope.artifact.sha256, huggingface.artifact.sha256);
            assert_eq!(
                modelscope.artifact.size_bytes,
                huggingface.artifact.size_bytes
            );
        }
    }
}
