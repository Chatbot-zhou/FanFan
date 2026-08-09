use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppError;

const REGISTRY_VERSION: u32 = 1;
const MAX_IMPORT_FILES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Generation,
    Embedding,
    Reranker,
    Ocr,
}

impl ModelRole {
    fn directory_name(self) -> &'static str {
        match self {
            Self::Generation => "generation",
            Self::Embedding => "embedding",
            Self::Reranker => "reranker",
            Self::Ocr => "ocr",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFormat {
    Gguf,
    Onnx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    LocalImport,
    Modelscope,
    Huggingface,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportCandidate {
    pub candidate_id: Uuid,
    pub source_path: String,
    pub display_name: String,
    pub format: ModelFormat,
    pub suggested_role: Option<ModelRole>,
    pub size_bytes: u64,
    pub sha256: String,
    pub companion_files: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelImportSelection {
    pub source_path: String,
    pub role: ModelRole,
}

#[derive(Debug, Clone)]
pub struct DownloadedModelMetadata {
    pub source: ModelSource,
    pub repository_id: String,
    pub revision: String,
    pub license_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelArtifact {
    pub artifact_id: Uuid,
    pub role: ModelRole,
    pub format: ModelFormat,
    pub model_id: String,
    pub model_version: Option<String>,
    pub source: ModelSource,
    pub repository_id: Option<String>,
    pub revision: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub local_path: String,
    pub quantization: Option<String>,
    pub context_length: Option<u32>,
    pub embedding_dimension: Option<u32>,
    pub license_name: Option<String>,
    pub status: String,
    pub imported_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistryState {
    pub registry_version: u32,
    pub artifacts: Vec<ModelArtifact>,
    pub active_artifacts: BTreeMap<String, Uuid>,
    pub updated_at: DateTime<Utc>,
}

impl Default for ModelRegistryState {
    fn default() -> Self {
        Self {
            registry_version: REGISTRY_VERSION,
            artifacts: Vec::new(),
            active_artifacts: BTreeMap::new(),
            updated_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelManager {
    model_root: PathBuf,
    registry_path: PathBuf,
}

impl ModelManager {
    pub fn open(data_directory: impl Into<PathBuf>) -> Result<Self, AppError> {
        let model_root = data_directory.into().join("models");
        fs::create_dir_all(&model_root).map_err(|error| {
            AppError::new("MODEL_DIRECTORY_CREATE_FAILED", error.to_string(), true)
        })?;
        let manager = Self {
            registry_path: model_root.join("registry.json"),
            model_root,
        };
        manager.load_registry()?;
        Ok(manager)
    }

    pub fn scan_import_paths(&self, paths: &[String]) -> Result<Vec<ImportCandidate>, AppError> {
        if paths.is_empty() || paths.len() > 32 {
            return Err(AppError::new(
                "MODEL_IMPORT_REQUEST_INVALID",
                "请选择1到32个模型文件或目录",
                false,
            ));
        }
        let mut model_files = Vec::new();
        for path in paths {
            collect_model_files(Path::new(path), &mut model_files)?;
            if model_files.len() > MAX_IMPORT_FILES {
                return Err(AppError::new(
                    "MODEL_IMPORT_TOO_LARGE",
                    "一次导入发现的模型文件过多，请缩小选择范围",
                    false,
                ));
            }
        }
        model_files.sort();
        model_files.dedup();
        if model_files.is_empty() {
            return Err(AppError::new(
                "MODEL_FORMAT_UNSUPPORTED",
                "选择范围内没有找到GGUF或ONNX模型",
                false,
            ));
        }
        model_files
            .into_iter()
            .map(|path| import_candidate(&path))
            .collect()
    }

    pub fn import_artifacts(
        &self,
        selections: &[ModelImportSelection],
    ) -> Result<Vec<ModelArtifact>, AppError> {
        self.import_artifacts_internal(selections, None)
    }

    pub fn import_downloaded_artifact(
        &self,
        selection: &ModelImportSelection,
        metadata: &DownloadedModelMetadata,
    ) -> Result<ModelArtifact, AppError> {
        self.import_artifacts_internal(std::slice::from_ref(selection), Some(metadata))?
            .into_iter()
            .next()
            .ok_or_else(|| AppError::new("MODEL_INSTALL_FAILED", "下载模型没有完成安装", true))
    }

    pub fn download_staging_directory(&self) -> Result<PathBuf, AppError> {
        let directory = self.model_root.join(".downloads");
        fs::create_dir_all(&directory).map_err(|error| {
            AppError::new("MODEL_DIRECTORY_CREATE_FAILED", error.to_string(), true)
        })?;
        Ok(directory)
    }

    pub fn verify_download(
        &self,
        path: &Path,
        expected_sha256: &str,
        expected_size_bytes: u64,
    ) -> Result<(), AppError> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| AppError::new("MODEL_DOWNLOAD_INCOMPLETE", error.to_string(), true))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_INCOMPLETE",
                "下载结果不是拾忆管理的普通文件",
                false,
            ));
        }
        let canonical = fs::canonicalize(path)
            .map_err(|error| AppError::new("MODEL_DOWNLOAD_INCOMPLETE", error.to_string(), true))?;
        let staging = fs::canonicalize(self.download_staging_directory()?)
            .map_err(|error| AppError::new("MODEL_DOWNLOAD_INCOMPLETE", error.to_string(), true))?;
        if !canonical.starts_with(&staging) || metadata.len() != expected_size_bytes {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_SIZE_MISMATCH",
                "模型下载大小与锁定目录不一致",
                true,
            ));
        }
        if expected_sha256.len() != 64 || sha256_file(&canonical)? != expected_sha256 {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_HASH_MISMATCH",
                "模型下载完成但SHA-256校验失败",
                true,
            ));
        }
        Ok(())
    }

    fn import_artifacts_internal(
        &self,
        selections: &[ModelImportSelection],
        metadata: Option<&DownloadedModelMetadata>,
    ) -> Result<Vec<ModelArtifact>, AppError> {
        if selections.is_empty() || selections.len() > 32 {
            return Err(AppError::new(
                "MODEL_IMPORT_REQUEST_INVALID",
                "请选择1到32个模型组件",
                false,
            ));
        }
        let mut registry = self.load_registry()?;
        let mut imported = Vec::new();
        let mut installation_guards = Vec::new();
        for selection in selections {
            let source = fs::canonicalize(&selection.source_path).map_err(|error| {
                AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true)
            })?;
            let candidate = import_candidate(&source)?;
            if let Some(existing) = registry
                .artifacts
                .iter()
                .find(|artifact| {
                    artifact.sha256 == candidate.sha256 && artifact.role == selection.role
                })
                .cloned()
            {
                imported.push(existing);
                continue;
            }
            let artifact_id = Uuid::now_v7();
            let target_directory = self
                .model_root
                .join(selection.role.directory_name())
                .join(artifact_id.to_string());
            let temporary_directory = self
                .model_root
                .join(".installing")
                .join(artifact_id.to_string());
            fs::create_dir_all(&temporary_directory)
                .map_err(|error| AppError::new("MODEL_INSTALL_FAILED", error.to_string(), true))?;
            let mut installation_guard = InstallationGuard::new(temporary_directory.clone());
            let source_parent = source.parent().ok_or_else(|| {
                AppError::new("MODEL_SOURCE_UNAVAILABLE", "模型文件缺少父目录", false)
            })?;
            let mut copied_main_path = None;
            let mut files_to_copy = vec![source.clone()];
            files_to_copy.extend(
                candidate
                    .companion_files
                    .iter()
                    .map(PathBuf::from)
                    .filter(|path| path.parent() == Some(source_parent)),
            );
            files_to_copy.sort();
            files_to_copy.dedup();
            for file in files_to_copy {
                let name = file.file_name().ok_or_else(|| {
                    AppError::new("MODEL_SOURCE_UNAVAILABLE", "模型配套文件名无效", false)
                })?;
                let target = temporary_directory.join(name);
                fs::copy(&file, &target).map_err(|error| {
                    AppError::new("MODEL_INSTALL_FAILED", error.to_string(), true)
                })?;
                if file == source {
                    copied_main_path = Some(target);
                }
            }
            let copied_main_path = copied_main_path.expect("main model file included");
            let copied_hash = sha256_file(&copied_main_path)?;
            if copied_hash != candidate.sha256 {
                return Err(AppError::new(
                    "MODEL_HASH_MISMATCH",
                    "模型复制后的SHA-256与源文件不一致",
                    false,
                ));
            }
            if let Some(parent) = target_directory.parent() {
                fs::create_dir_all(parent).map_err(|error| {
                    AppError::new("MODEL_INSTALL_FAILED", error.to_string(), true)
                })?;
            }
            fs::rename(&temporary_directory, &target_directory)
                .map_err(|error| AppError::new("MODEL_INSTALL_FAILED", error.to_string(), true))?;
            installation_guard.track(target_directory.clone());
            let installed_main = target_directory.join(
                source
                    .file_name()
                    .expect("canonical model source has filename"),
            );
            let artifact = ModelArtifact {
                artifact_id,
                role: selection.role,
                format: candidate.format,
                model_id: source
                    .file_stem()
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_else(|| candidate.display_name.clone()),
                model_version: None,
                source: metadata
                    .map(|value| value.source)
                    .unwrap_or(ModelSource::LocalImport),
                repository_id: metadata.map(|value| value.repository_id.clone()),
                revision: metadata.map(|value| value.revision.clone()),
                sha256: candidate.sha256,
                size_bytes: candidate.size_bytes,
                local_path: installed_main.to_string_lossy().into_owned(),
                quantization: infer_quantization(&candidate.display_name),
                context_length: None,
                embedding_dimension: None,
                license_name: metadata.map(|value| value.license_name.clone()),
                status: "ready".into(),
                imported_at: Utc::now(),
            };
            registry.artifacts.push(artifact.clone());
            imported.push(artifact);
            installation_guards.push(installation_guard);
        }
        registry.updated_at = Utc::now();
        self.save_registry(&registry)?;
        for guard in &mut installation_guards {
            guard.commit();
        }
        Ok(imported)
    }

    pub fn list_artifacts(&self) -> Result<Vec<ModelArtifact>, AppError> {
        Ok(self.load_registry()?.artifacts)
    }

    pub fn registry_state(&self) -> Result<ModelRegistryState, AppError> {
        self.load_registry()
    }

    pub fn artifact_by_id(&self, artifact_id: &Uuid) -> Result<ModelArtifact, AppError> {
        self.load_registry()?
            .artifacts
            .into_iter()
            .find(|artifact| artifact.artifact_id == *artifact_id)
            .ok_or_else(|| AppError::new("MODEL_ARTIFACT_NOT_FOUND", "模型组件不存在", false))
    }

    pub fn active_artifact(&self, role: ModelRole) -> Result<Option<ModelArtifact>, AppError> {
        let registry = self.load_registry()?;
        let Some(artifact_id) = registry.active_artifacts.get(role.directory_name()) else {
            return Ok(None);
        };
        Ok(registry
            .artifacts
            .into_iter()
            .find(|artifact| artifact.artifact_id == *artifact_id))
    }

    pub fn activate_artifact(
        &self,
        artifact_id: &Uuid,
        embedding_dimension: Option<u32>,
    ) -> Result<ModelArtifact, AppError> {
        let mut registry = self.load_registry()?;
        let artifact = registry
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.artifact_id == *artifact_id)
            .ok_or_else(|| AppError::new("MODEL_ARTIFACT_NOT_FOUND", "模型组件不存在", false))?;
        if !Path::new(&artifact.local_path).is_file() {
            return Err(AppError::new(
                "MODEL_SOURCE_UNAVAILABLE",
                "模型组件文件已经离开拾忆管理目录",
                false,
            ));
        }
        if let Some(dimension) = embedding_dimension {
            if artifact.role != ModelRole::Embedding || dimension == 0 {
                return Err(AppError::new(
                    "MODEL_ACTIVATION_INVALID",
                    "向量维度只能写入有效的文本向量组件",
                    false,
                ));
            }
            artifact.embedding_dimension = Some(dimension);
        }
        let activated = artifact.clone();
        registry.active_artifacts.insert(
            activated.role.directory_name().to_owned(),
            activated.artifact_id,
        );
        registry.updated_at = Utc::now();
        self.save_registry(&registry)?;
        Ok(activated)
    }

    fn load_registry(&self) -> Result<ModelRegistryState, AppError> {
        if !self.registry_path.exists() {
            return Ok(ModelRegistryState::default());
        }
        let bytes = fs::read(&self.registry_path).map_err(|error| {
            AppError::new("MODEL_REGISTRY_READ_FAILED", error.to_string(), true)
        })?;
        let registry = serde_json::from_slice::<ModelRegistryState>(&bytes)
            .map_err(|error| AppError::new("MODEL_REGISTRY_INVALID", error.to_string(), false))?;
        if registry.registry_version > REGISTRY_VERSION {
            return Err(AppError::new(
                "MODEL_REGISTRY_TOO_NEW",
                "模型注册表来自更高版本的拾忆",
                false,
            ));
        }
        Ok(registry)
    }

    fn save_registry(&self, registry: &ModelRegistryState) -> Result<(), AppError> {
        let bytes = serde_json::to_vec_pretty(registry)
            .map_err(|error| AppError::new("MODEL_REGISTRY_INVALID", error.to_string(), false))?;
        let temporary_path = self.registry_path.with_extension("json.tmp");
        let mut file = File::create(&temporary_path).map_err(|error| {
            AppError::new("MODEL_REGISTRY_WRITE_FAILED", error.to_string(), true)
        })?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                AppError::new("MODEL_REGISTRY_WRITE_FAILED", error.to_string(), true)
            })?;
        atomic_replace_file(&temporary_path, &self.registry_path)
            .map_err(|error| AppError::new("MODEL_REGISTRY_WRITE_FAILED", error.to_string(), true))
    }
}

#[derive(Debug)]
struct InstallationGuard {
    path: PathBuf,
    committed: bool,
}

impl InstallationGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn track(&mut self, path: PathBuf) {
        self.path = path;
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for InstallationGuard {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(windows)]
fn atomic_replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };

    let mut source_wide = source.as_os_str().encode_wide().collect::<Vec<_>>();
    source_wide.push(0);
    let mut target_wide = target.as_os_str().encode_wide().collect::<Vec<_>>();
    target_wide.push(0);
    unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(target_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(std::io::Error::other)
}

#[cfg(not(windows))]
fn atomic_replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

fn collect_model_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_file() {
        if model_format(path).is_some() {
            output.push(fs::canonicalize(path).map_err(|error| {
                AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true)
            })?);
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(path)
        .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?
    {
        let entry = entry
            .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?;
        collect_model_files(&entry.path(), output)?;
        if output.len() > MAX_IMPORT_FILES {
            break;
        }
    }
    Ok(())
}

fn import_candidate(path: &Path) -> Result<ImportCandidate, AppError> {
    let format = model_format(path)
        .ok_or_else(|| AppError::new("MODEL_FORMAT_UNSUPPORTED", "仅支持GGUF和ONNX模型", false))?;
    let metadata = fs::metadata(path)
        .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?;
    if metadata.len() == 0 {
        return Err(AppError::new("MODEL_FILE_EMPTY", "模型文件为空", false));
    }
    let display_name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let suggested_role = infer_role(path, format);
    let companion_files = if format == ModelFormat::Onnx {
        discover_companion_files(path)?
    } else {
        Vec::new()
    };
    let mut warnings = Vec::new();
    if suggested_role.is_none() {
        warnings.push("无法可靠判断模型用途，请在导入前选择用途".into());
    }
    if format == ModelFormat::Onnx
        && suggested_role == Some(ModelRole::Embedding)
        && !companion_files
            .iter()
            .any(|value| value.ends_with("tokenizer.json"))
    {
        warnings.push("向量模型目录缺少tokenizer.json，运行自检可能失败".into());
    }
    Ok(ImportCandidate {
        candidate_id: Uuid::now_v7(),
        source_path: path.to_string_lossy().into_owned(),
        display_name,
        format,
        suggested_role,
        size_bytes: metadata.len(),
        sha256: sha256_file(path)?,
        companion_files,
        warnings,
    })
}

fn model_format(path: &Path) -> Option<ModelFormat> {
    match path
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase()
        .as_str()
    {
        "gguf" => Some(ModelFormat::Gguf),
        "onnx" => Some(ModelFormat::Onnx),
        _ => None,
    }
}

fn infer_role(path: &Path, format: ModelFormat) -> Option<ModelRole> {
    let value = path.to_string_lossy().to_ascii_lowercase();
    if value.contains("ocr") || value.contains("det_model") || value.contains("rec_model") {
        Some(ModelRole::Ocr)
    } else if value.contains("rerank") || value.contains("cross-encoder") {
        Some(ModelRole::Reranker)
    } else if value.contains("embed") || value.contains("bge-") || value.contains("gte-") {
        Some(ModelRole::Embedding)
    } else if format == ModelFormat::Gguf {
        Some(ModelRole::Generation)
    } else {
        None
    }
}

fn discover_companion_files(path: &Path) -> Result<Vec<String>, AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new("MODEL_SOURCE_UNAVAILABLE", "ONNX模型缺少父目录", false))?;
    let allowed = ["json", "txt", "model", "vocab", "merges", "yaml", "yml"];
    let mut files = Vec::new();
    for entry in fs::read_dir(parent)
        .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?
    {
        let entry = entry
            .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?;
        let metadata = entry
            .metadata()
            .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?;
        if !metadata.is_file() || entry.path() == path {
            continue;
        }
        let extension = entry
            .path()
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase());
        if extension
            .as_deref()
            .is_some_and(|extension| allowed.contains(&extension))
        {
            files.push(entry.path().to_string_lossy().into_owned());
        }
    }
    files.sort();
    Ok(files)
}

fn sha256_file(path: &Path) -> Result<String, AppError> {
    let mut file = File::open(path)
        .map_err(|error| AppError::new("MODEL_HASH_FAILED", error.to_string(), true))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::new("MODEL_HASH_FAILED", error.to_string(), true))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn infer_quantization(name: &str) -> Option<String> {
    let upper = name.to_ascii_uppercase();
    ["Q2_K", "Q3_K", "Q4_K", "Q5_K", "Q6_K", "Q8_0", "F16", "F32"]
        .into_iter()
        .find(|value| upper.contains(value))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_import_is_hash_verified_and_keeps_source_read_only() {
        let data = tempfile::tempdir().expect("data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let model = source.path().join("bge-small-zh-model.onnx");
        let tokenizer = source.path().join("tokenizer.json");
        fs::write(&model, b"fake onnx bytes for contract test").expect("write model");
        fs::write(&tokenizer, b"{}").expect("write tokenizer");
        let before = fs::read(&model).expect("read before");
        let manager = ModelManager::open(data.path()).expect("open manager");
        let candidates = manager
            .scan_import_paths(&[source.path().to_string_lossy().into_owned()])
            .expect("scan import");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].suggested_role, Some(ModelRole::Embedding));
        assert!(
            candidates[0]
                .companion_files
                .iter()
                .any(|path| path.ends_with("tokenizer.json"))
        );
        let imported = manager
            .import_artifacts(&[ModelImportSelection {
                source_path: candidates[0].source_path.clone(),
                role: ModelRole::Embedding,
            }])
            .expect("import artifact");
        assert_eq!(imported.len(), 1);
        assert!(Path::new(&imported[0].local_path).is_file());
        assert_eq!(fs::read(model).expect("read source after"), before);
        assert_eq!(manager.list_artifacts().expect("list artifacts").len(), 1);
        let active = manager
            .activate_artifact(&imported[0].artifact_id, Some(512))
            .expect("activate verified embedding");
        assert_eq!(active.embedding_dimension, Some(512));
        assert_eq!(
            manager
                .active_artifact(ModelRole::Embedding)
                .expect("read active embedding")
                .expect("active embedding")
                .artifact_id,
            imported[0].artifact_id
        );
    }

    #[test]
    fn scripts_are_not_discovered_as_model_components() {
        let data = tempfile::tempdir().expect("data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        fs::write(source.path().join("remote_code.py"), "raise SystemExit").expect("write script");
        let manager = ModelManager::open(data.path()).expect("open manager");
        let error = manager
            .scan_import_paths(&[source.path().to_string_lossy().into_owned()])
            .expect_err("scripts cannot be imported");
        assert_eq!(error.code, "MODEL_FORMAT_UNSUPPORTED");
    }
}
