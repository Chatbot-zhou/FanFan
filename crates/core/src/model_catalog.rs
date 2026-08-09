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

pub fn model_edition_by_id(edition_id: &str, source: &str) -> Result<ModelEdition, AppError> {
    resolved_model_edition(edition_id, source)
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
    let (name, description, recommended_memory_gb, generation) = match edition_id {
        "light" => (
            "轻量版",
            "适合低配置电脑的完整本地RAG套件，包含0.6B生成模型和共享中文语义模型。",
            8,
            generation_artifact(
                source,
                "Qwen3-0.6B-Q8_0",
                "Qwen/Qwen3-0.6B-GGUF",
                "Qwen3-0.6B-Q8_0.gguf",
                "1eaf4d9657fe65ad10a51eab76a8db5b363bddaa",
                "6abe20cd0aed577f4d0b267935868ecae190aee9",
                "9465e63a22add5354d9bb4b99e90117043c7124007664907259bd16d043bb031",
                639_446_688,
            ),
        ),
        "standard" => (
            "标准版",
            "效果更好的完整本地RAG套件，包含4B生成模型和共享中文语义模型。",
            12,
            generation_artifact(
                source,
                "Qwen3-4B-Q4_K_M",
                "Qwen/Qwen3-4B-GGUF",
                "Qwen3-4B-Q4_K_M.gguf",
                "a9a60d009fa7ff9606305047c2bf77ac25dbec49",
                "8cca206c792af04f8d452368bd30b43ec18007f1",
                "7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5",
                2_497_280_256,
            ),
        ),
        _ => {
            return Err(AppError::new(
                "MODEL_EDITION_NOT_FOUND",
                "模型版本不存在",
                false,
            ));
        }
    };
    let embedding = embedding_artifact(source);
    let artifacts = vec![generation, embedding];
    let download_size_bytes = artifacts
        .iter()
        .map(DownloadArtifact::total_size_bytes)
        .sum();
    Ok(ModelEdition {
        edition_id: edition_id.to_owned(),
        name: name.to_owned(),
        description: description.to_owned(),
        recommended_memory_gb,
        download_size_bytes,
        capabilities: vec!["generation".into(), "embedding".into(), "rag".into()],
        artifacts,
    })
}

#[allow(clippy::too_many_arguments)]
fn generation_artifact(
    source: ModelSource,
    model_id: &str,
    repository_id: &str,
    file_name: &str,
    huggingface_revision: &str,
    modelscope_revision: &str,
    sha256: &str,
    size_bytes: u64,
) -> DownloadArtifact {
    let revision = match source {
        ModelSource::Huggingface => huggingface_revision,
        ModelSource::Modelscope => modelscope_revision,
        ModelSource::LocalImport => unreachable!("download catalog has no local source"),
    };
    DownloadArtifact {
        model_id: model_id.to_owned(),
        role: ModelRole::Generation,
        format: ModelFormat::Gguf,
        source,
        repository_id: repository_id.to_owned(),
        revision: revision.to_owned(),
        file_name: file_name.to_owned(),
        url: artifact_url(source, repository_id, revision, file_name),
        sha256: sha256.to_owned(),
        size_bytes,
        companion_files: Vec::new(),
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
        ModelSource::Huggingface => format!(
            "https://huggingface.co/{repository_id}/resolve/{revision}/{path}?download=true"
        ),
        ModelSource::Modelscope => {
            format!("https://modelscope.cn/models/{repository_id}/resolve/{revision}/{path}")
        }
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
    fn providers_have_independent_immutable_revisions() {
        for edition_id in ["light", "standard"] {
            let huggingface = model_edition_by_id(edition_id, "huggingface").expect("hf source");
            let modelscope = model_edition_by_id(edition_id, "modelscope").expect("ms source");
            for (hf, ms) in huggingface.artifacts.iter().zip(&modelscope.artifacts) {
                assert_eq!(hf.sha256, ms.sha256);
                assert_eq!(hf.size_bytes, ms.size_bytes);
                assert_ne!(hf.revision, ms.revision);
                assert!(hf.url.starts_with("https://huggingface.co/"));
                assert!(ms.url.starts_with("https://modelscope.cn/"));
                assert!(!hf.revision.eq("main") && !ms.revision.eq("master"));
            }
        }
    }
}
