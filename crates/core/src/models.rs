use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AppError, DownloadFile, RuntimeModelPlan, locked_download_artifact, resolve_runtime_model_plan,
};

const REGISTRY_VERSION: u32 = 4;
const DOWNLOAD_REGISTRY_VERSION: u32 = 1;
const MAX_IMPORT_FILES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Generation,
    Embedding,
    Vision,
    Reranker,
    Ocr,
    Asr,
    Router,
}

impl ModelRole {
    /// 角色在注册表中的目录键名（也用于 preset 报告里的人类可读角色标识）。
    pub fn directory_name(self) -> &'static str {
        match self {
            Self::Generation => "generation",
            Self::Embedding => "embedding",
            Self::Vision => "vision",
            Self::Reranker => "reranker",
            Self::Ocr => "ocr",
            Self::Asr => "asr",
            Self::Router => "router",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFormat {
    Gguf,
    Onnx,
    /// Ollama 托管模型（生成 / embedding）：无本地文件，经本机 Ollama pull
    /// 拉取并以 tag 形式注册，不进入 FanFan 的 ModelStore。
    Ollama,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    LocalImport,
    Modelscope,
    Huggingface,
    /// 本机 Ollama（`127.0.0.1:11434`）。只支持本机，不连局域网 / 远程 / 公网。
    Ollama,
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
    pub model_id: Option<String>,
    pub query_prefix: Option<String>,
    pub max_length: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPackageFile {
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub required: bool,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPackageManifest {
    pub manifest_version: u32,
    pub files: Vec<ModelPackageFile>,
    pub integrity_status: String,
    pub self_test_status: String,
    pub verified_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelArtifact {
    pub artifact_id: Uuid,
    pub role: ModelRole,
    pub format: ModelFormat,
    /// 所属 Model Catalog 的 catalog_id（如 `qwen3-5-2b-q4`）。
    /// `selected_preset_id` 运行时接通后，把它作为「该 artifact 归属哪个 Preset
    /// 角色」的唯一依据；旧数据缺少该字段（`None`）时仅作迁移兼容处理。
    #[serde(default)]
    pub catalog_id: Option<String>,
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
    #[serde(default)]
    pub query_prefix: Option<String>,
    #[serde(default)]
    pub max_length: Option<u32>,
    pub license_name: Option<String>,
    pub status: String,
    #[serde(default)]
    pub package_manifest: Option<ModelPackageManifest>,
    pub imported_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRegistryState {
    pub registry_version: u32,
    pub artifacts: Vec<ModelArtifact>,
    pub active_artifacts: BTreeMap<String, Uuid>,
    #[serde(default)]
    pub profiles: Vec<ModelProfile>,
    #[serde(default)]
    pub active_profile_id: Option<Uuid>,
    #[serde(default)]
    pub pending_embedding_activation: Option<PendingEmbeddingActivation>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingEmbeddingActivation {
    pub artifact_id: Uuid,
    pub dimension: u32,
    pub profile_id: Option<Uuid>,
    pub download_job_id: Option<Uuid>,
    pub status: String,
    pub error: Option<AppError>,
    pub requested_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelProfile {
    pub profile_id: Uuid,
    pub edition: String,
    pub name: String,
    pub generation_artifact_id: Uuid,
    pub embedding_artifact_id: Uuid,
    #[serde(default)]
    pub vision_artifact_id: Option<Uuid>,
    pub ocr_artifact_id: Option<Uuid>,
    pub reranker_artifact_id: Option<Uuid>,
    pub status: String,
    pub activated_at: DateTime<Utc>,
}

/// 一次 `apply_runtime_plan` 的结果：每个角色就绪 / 缺失。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PresetPlanReport {
    pub preset_id: String,
    /// 已就绪并激活的角色（catalog_id 已就位）。
    pub ready: Vec<RolePlanItem>,
    /// 缺失、需要下载或本地导入的角色（catalog_id 未就绪）。
    pub missing: Vec<RolePlanItem>,
}

/// plan 中单个角色的型号约束。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolePlanItem {
    #[serde(rename = "role")]
    pub role: ModelRole,
    pub catalog_id: String,
}

/// 由 Ollama 模型 tag 派生确定性 artifact UUID（幂等、可序列化），使
/// `ollama_embedding_ready` 重复注册时始终指向同一个 artifact。
fn ollama_artifact_uuid(tag: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"fanfan-ollama-embedding:");
    hasher.update(tag.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes)
}

/// 把一份 `RuntimeModelPlan` 展开为「（角色，catalog_id）」的有序元组，
/// 供 `apply_runtime_plan` 逐角色对齐 active artifact；已去重、不包含 Router。
fn plan_items(plan: &RuntimeModelPlan) -> Vec<(ModelRole, String)> {
    let mut out: Vec<(ModelRole, String)> = Vec::with_capacity(7);
    out.push((ModelRole::Generation, plan.generation.clone()));
    out.push((ModelRole::Embedding, plan.embedding.clone()));
    if let Some(value) = &plan.reranker {
        out.push((ModelRole::Reranker, value.clone()));
    }
    out.push((ModelRole::Ocr, plan.ocr.clone()));
    if let Some(value) = &plan.asr {
        out.push((ModelRole::Asr, value.clone()));
    }
    if let Some(value) = &plan.vision {
        out.push((ModelRole::Vision, value.clone()));
    }
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelDownloadFileProgress {
    pub role: ModelRole,
    pub file_name: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelDownloadJob {
    pub job_id: Uuid,
    pub edition_id: String,
    pub edition_name: String,
    pub source: ModelSource,
    pub status: String,
    pub phase: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub progress: f64,
    pub bytes_per_second: u64,
    pub eta_seconds: Option<u64>,
    pub retry_count: u32,
    pub current_file: Option<String>,
    pub files: Vec<ModelDownloadFileProgress>,
    pub installed_artifact_ids: Vec<Uuid>,
    pub profile_id: Option<Uuid>,
    pub error: Option<AppError>,
    #[serde(default)]
    pub activation_status: Option<String>,
    #[serde(default)]
    pub activation_error: Option<AppError>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelDownloadRemoval {
    pub job_id: Uuid,
    pub removed: bool,
    pub partial_bytes_removed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelStoreStatus {
    pub store_path: String,
    pub migration_state: String,
    pub installed_artifacts: u64,
    pub installed_bytes: u64,
    pub integrity_status: String,
    /// 已安排迁移的目标目录（重启后执行）；由桌面端配置层填充。
    pub pending_target_directory: Option<String>,
    /// 存在待执行的模型仓库迁移时恒为 true；由桌面端配置层填充。
    pub restart_required: bool,
    /// 上次迁移/位置解析失败的说明；由桌面端配置层填充。
    pub last_error: Option<String>,
    /// 迁移完成前的旧模型仓库；非空表示有待清理的旧仓库。由桌面端配置层填充。
    pub previous_model_store: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelDownloadRegistryState {
    registry_version: u32,
    jobs: Vec<ModelDownloadJob>,
    updated_at: DateTime<Utc>,
}

impl Default for ModelDownloadRegistryState {
    fn default() -> Self {
        Self {
            registry_version: DOWNLOAD_REGISTRY_VERSION,
            jobs: Vec::new(),
            updated_at: Utc::now(),
        }
    }
}

impl Default for ModelRegistryState {
    fn default() -> Self {
        Self {
            registry_version: REGISTRY_VERSION,
            artifacts: Vec::new(),
            active_artifacts: BTreeMap::new(),
            profiles: Vec::new(),
            active_profile_id: None,
            pending_embedding_activation: None,
            updated_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelManager {
    model_root: PathBuf,
    registry_path: PathBuf,
    downloads_path: PathBuf,
    download_lock: Arc<Mutex<()>>,
    registry_lock: Arc<Mutex<()>>,
}

impl ModelManager {
    pub fn open(data_directory: impl Into<PathBuf>) -> Result<Self, AppError> {
        Self::open_store(data_directory.into().join("models"))
    }

    pub fn open_store(model_root: impl Into<PathBuf>) -> Result<Self, AppError> {
        let model_root = model_root.into();
        fs::create_dir_all(&model_root).map_err(|error| {
            AppError::new("MODEL_DIRECTORY_CREATE_FAILED", error.to_string(), true)
        })?;
        let manager = Self {
            registry_path: model_root.join("registry.json"),
            downloads_path: model_root.join("downloads.json"),
            model_root,
            download_lock: Arc::new(Mutex::new(())),
            registry_lock: Arc::new(Mutex::new(())),
        };
        manager.load_registry()?;
        manager.recover_interrupted_downloads()?;
        manager.recover_interrupted_embedding_activation()?;
        manager.refresh_package_manifests()?;
        Ok(manager)
    }

    pub fn store_status(&self) -> Result<ModelStoreStatus, AppError> {
        let registry = self.load_registry()?;
        let missing_or_changed = registry.artifacts.iter().any(|artifact| {
            artifact
                .package_manifest
                .as_ref()
                .is_none_or(|manifest| manifest.integrity_status != "ready")
        });
        Ok(ModelStoreStatus {
            store_path: self.model_root.to_string_lossy().into_owned(),
            migration_state: "ready".into(),
            installed_artifacts: registry.artifacts.len() as u64,
            installed_bytes: registry
                .artifacts
                .iter()
                .map(|artifact| {
                    artifact
                        .package_manifest
                        .as_ref()
                        .map(|manifest| manifest.files.iter().map(|file| file.size_bytes).sum())
                        .unwrap_or(artifact.size_bytes)
                })
                .sum(),
            integrity_status: if missing_or_changed {
                "missing_or_changed_files"
            } else {
                "ready"
            }
            .into(),
            pending_target_directory: None,
            restart_required: false,
            last_error: None,
            previous_model_store: None,
        })
    }

    pub fn model_root(&self) -> &Path {
        &self.model_root
    }

    pub fn restore_locked_companions_from(
        &self,
        legacy_roots: &[PathBuf],
    ) -> Result<u64, AppError> {
        let registry = self.load_registry()?;
        let mut restored = 0_u64;
        for artifact in &registry.artifacts {
            let Some(locked) = locked_download_artifact(&artifact.model_id, artifact.source) else {
                continue;
            };
            let Some(parent) = Path::new(&artifact.local_path).parent() else {
                continue;
            };
            for expected in locked.companion_files {
                let target = parent.join(&expected.file_name);
                // 快速路径：目标已存在且大小与包清单一致 → 无需恢复。包清单是
                // 下载/导入/自检时对实际文件的 SHA 记录，与 refresh_package_manifests
                // 快速路径同一原则；否则每次启动都会对全部配套文件（本机约
                // 3.9GB，含 2.5GB 视觉主模型）做全量 SHA-256，实测卡住启动 21s。
                // 文件被同大小替换的情况由模型加载失败与自检兜底。
                let manifest_matches = artifact.package_manifest.as_ref().is_some_and(|manifest| {
                    manifest.files.iter().any(|file| {
                        file.file_name == expected.file_name
                            && fs::metadata(&target)
                                .is_ok_and(|meta| meta.is_file() && meta.len() == file.size_bytes)
                    })
                });
                if manifest_matches {
                    continue;
                }
                if package_file_matches(&target, &expected)? {
                    continue;
                }
                let mut source = None;
                for legacy_root in legacy_roots.iter().filter(|root| root.is_dir()) {
                    if let Some(found) = find_verified_file(legacy_root, &expected)? {
                        source = Some(found);
                        break;
                    }
                }
                let Some(source) = source else {
                    continue;
                };
                let temporary = parent.join(format!(
                    ".{}.restore-{}.tmp",
                    expected.file_name,
                    Uuid::now_v7()
                ));
                fs::copy(&source, &temporary).map_err(|error| {
                    AppError::new("MODEL_COMPANION_RESTORE_FAILED", error.to_string(), true)
                })?;
                if !package_file_matches(&temporary, &expected)? {
                    let _ = fs::remove_file(&temporary);
                    return Err(AppError::new(
                        "MODEL_COMPANION_RESTORE_VERIFY_FAILED",
                        "旧模型目录中的配套文件校验失败，未启用该文件",
                        false,
                    ));
                }
                fs::rename(&temporary, &target).map_err(|error| {
                    let _ = fs::remove_file(&temporary);
                    AppError::new("MODEL_COMPANION_RESTORE_FAILED", error.to_string(), true)
                })?;
                restored = restored.saturating_add(1);
            }
        }
        if restored > 0 {
            self.refresh_package_manifests()?;
        }
        Ok(restored)
    }

    pub fn refresh_package_manifests(&self) -> Result<(), AppError> {
        let _guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let active = registry
            .active_artifacts
            .values()
            .copied()
            .collect::<Vec<_>>();
        for artifact in &mut registry.artifacts {
            let previous_self_test = artifact
                .package_manifest
                .as_ref()
                .map(|manifest| manifest.self_test_status.clone())
                .unwrap_or_else(|| {
                    if active.contains(&artifact.artifact_id) {
                        "ready".into()
                    } else {
                        "pending".into()
                    }
                });
            // 快速路径：manifest 已 ready 且所有文件大小仍匹配 → 复用，跳过全量
            // SHA-256（5GB 模型每次启动哈希约 55s）。SHA 在导入/下载/自检时强制
            // 校验；文件被替换但大小相同的情况由模型加载失败与自检兜底。
            let already_ready = artifact.package_manifest.as_ref().is_some_and(|manifest| {
                manifest.integrity_status == "ready"
                    && manifest.files.iter().all(|file| {
                        Path::new(&artifact.local_path)
                            .parent()
                            .is_some_and(|parent| {
                                fs::metadata(parent.join(&file.file_name)).is_ok_and(|metadata| {
                                    metadata.is_file() && metadata.len() == file.size_bytes
                                })
                            })
                    })
            });
            let mut manifest = if already_ready {
                artifact.package_manifest.clone().expect("checked above")
            } else {
                build_package_manifest(artifact)?
            };
            manifest.self_test_status = previous_self_test;
            artifact.status = match (
                manifest.integrity_status.as_str(),
                manifest.self_test_status.as_str(),
            ) {
                ("ready", "ready") => "ready",
                ("ready", _) => "pending_self_test",
                _ => "incomplete",
            }
            .into();
            artifact.package_manifest = Some(manifest);
        }
        registry.registry_version = REGISTRY_VERSION;
        registry.updated_at = Utc::now();
        self.save_registry(&registry)
    }

    pub fn rebase_store_paths(&self, previous_root: &Path) -> Result<u64, AppError> {
        let _guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let mut changed = 0_u64;
        for artifact in &mut registry.artifacts {
            let path = PathBuf::from(&artifact.local_path);
            let Ok(relative) = path.strip_prefix(previous_root) else {
                continue;
            };
            artifact.local_path = self
                .model_root
                .join(relative)
                .to_string_lossy()
                .into_owned();
            changed += 1;
        }
        if changed > 0 {
            registry.updated_at = Utc::now();
            self.save_registry(&registry)?;
        }
        Ok(changed)
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
                "下载结果不是翻翻管理的普通文件",
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
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let mut imported = Vec::new();
        let mut installation_guards = Vec::new();
        for selection in selections {
            let source = fs::canonicalize(&selection.source_path).map_err(|error| {
                AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true)
            })?;
            let mut candidate = import_candidate(&source)?;
            if selection.role == ModelRole::Vision
                && candidate.format == ModelFormat::Gguf
                && candidate.companion_files.is_empty()
            {
                candidate.companion_files = discover_gguf_vision_companions(&source)?;
            }
            if selection.role == ModelRole::Vision
                && candidate.format == ModelFormat::Gguf
                && source.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("mmproj")
                })
            {
                return Err(AppError::new(
                    "VISION_MODEL_MAIN_REQUIRED",
                    "请选择视觉语言模型主GGUF文件；mmproj只能作为同目录配套文件导入",
                    false,
                ));
            }
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
            let installation_guard = InstallationGuard::new(temporary_directory.clone());
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
            // 先把 guard 放进 Vec，再 track 到 target_directory。
            // 否则若后续 build_package_manifest 失败，guard 仍停留在局部作用域，
            // Drop 时会按 track 后的 target_directory 删除已成功安装的文件，
            // 而已 push 到 Vec 的前序 guard 又会因未 commit 在函数返回时
            // 一并删除各自 target_directory，造成大面积已安装文件丢失。
            installation_guards.push(installation_guard);
            installation_guards
                .last_mut()
                .expect("just pushed guard")
                .track(target_directory.clone());
            let installed_main = target_directory.join(
                source
                    .file_name()
                    .expect("canonical model source has filename"),
            );
            let mut artifact = ModelArtifact {
                artifact_id,
                role: selection.role,
                catalog_id: None,
                format: candidate.format,
                model_id: metadata
                    .and_then(|value| value.model_id.clone())
                    .unwrap_or_else(|| {
                        source
                            .file_stem()
                            .map(|value| value.to_string_lossy().into_owned())
                            .unwrap_or_else(|| candidate.display_name.clone())
                    }),
                model_version: metadata.map(|value| value.revision.clone()),
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
                query_prefix: metadata.and_then(|value| value.query_prefix.clone()),
                max_length: metadata.and_then(|value| value.max_length),
                license_name: metadata.map(|value| value.license_name.clone()),
                status: "pending_self_test".into(),
                package_manifest: None,
                imported_at: Utc::now(),
            };
            artifact.package_manifest = Some(build_package_manifest(&artifact)?);
            registry.artifacts.push(artifact.clone());
            imported.push(artifact);
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
            .find(|artifact| artifact.artifact_id == *artifact_id && artifact.status == "ready"))
    }

    /// Ollama 专属就绪层：把本机 Ollama 的 embedding 模型（`qwen3-embedding:0.6b`）
    /// 以「合成 ready artifact」形式注册并激活，使既有的
    /// `active_artifact(Embedding)` / 就绪门控 / 索引覆盖计算无需感知后端差异即可
    /// 识别「Ollama embedding 已就绪」。幂等：tag 用确定性 UUID 标识，重复调用
    /// 覆盖同一 artifact 并保持 ready。
    pub fn ollama_embedding_ready(&self, tag: &str) -> Result<ModelArtifact, AppError> {
        let dimension = crate::model_catalog::OLLAMA_EMBEDDING_DIMENSION;
        let catalog_id = crate::model_catalog::OLLAMA_EMBEDDING_CATALOG_ID;
        let artifact_id = ollama_artifact_uuid(tag);
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let now = Utc::now();
        let artifact = ModelArtifact {
            artifact_id,
            role: ModelRole::Embedding,
            format: ModelFormat::Ollama,
            catalog_id: Some(catalog_id.to_owned()),
            model_id: tag.to_owned(),
            model_version: None,
            source: ModelSource::Ollama,
            repository_id: Some("local-ollama".to_owned()),
            revision: None,
            sha256: String::new(), // Ollama 由 /api/pull 托管，无本地文件无 sha256
            size_bytes: 0,
            local_path: tag.to_owned(), // 携带 tag，run_embedding 以此为模型标识
            quantization: None,
            context_length: None,
            embedding_dimension: Some(dimension),
            query_prefix: None,
            max_length: Some(1024),
            license_name: None,
            status: "ready".into(),
            package_manifest: None,
            imported_at: now,
        };
        match registry
            .artifacts
            .iter_mut()
            .find(|existing| existing.artifact_id == artifact_id)
        {
            Some(existing) => *existing = artifact.clone(),
            None => registry.artifacts.push(artifact.clone()),
        }
        registry.active_artifacts.insert(
            ModelRole::Embedding.directory_name().to_owned(),
            artifact_id,
        );
        registry.active_profile_id = None;
        registry.pending_embedding_activation = None;
        registry
            .profiles
            .retain(|profile| profile.embedding_artifact_id != artifact_id);
        registry.updated_at = now;
        self.save_registry(&registry)?;
        Ok(artifact)
    }

    /// Ollama 专属就绪层：模拟 `ollama_embedding_ready`，把本机 Ollama 的文本生成
    /// 模型（如 `qwen3.5:2b`）以「合成 ready artifact」形式注册并激活，使
    /// `collect_plan_report` / `apply_runtime_plan` 能识别「Ollama generation 已就绪」。
    /// `catalog_id` 使用 `built_in_model_catalog` 的连字符 catalog_id（如
    /// `qwen3-5-2b-q4`），与 preset 的 generation 一致，保证精确匹配。幂等：
    /// tag 用确定性 UUID 标识，重复调用覆盖同一 artifact 并保持 ready。
    ///
    /// 该函数同时完成「旧 GGUF active → Ollama active」的数据迁移：覆盖
    /// `active_artifacts[Generation]` 指向本 Ollama artifact，并把同 role 的旧
    /// `format=Gguf` artifact 置为 `inactive`（保留来源证据以利审计）。
    pub fn ollama_generation_ready(
        &self,
        tag: &str,
        catalog_id: &str,
    ) -> Result<ModelArtifact, AppError> {
        let artifact_id = ollama_artifact_uuid(tag);
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let now = Utc::now();
        let artifact = ModelArtifact {
            artifact_id,
            role: ModelRole::Generation,
            format: ModelFormat::Ollama,
            catalog_id: Some(catalog_id.to_owned()),
            model_id: tag.to_owned(),
            model_version: None,
            source: ModelSource::Ollama,
            repository_id: Some("local-ollama".to_owned()),
            revision: None,
            sha256: String::new(), // Ollama 由 /api/pull 托管，无本地文件无 sha256
            size_bytes: 0,
            local_path: tag.to_owned(), // 携带 tag，generation 以此为模型标识
            quantization: None,
            context_length: None,
            embedding_dimension: None,
            query_prefix: None,
            max_length: None,
            license_name: None,
            status: "ready".into(),
            package_manifest: None,
            imported_at: now,
        };
        match registry
            .artifacts
            .iter_mut()
            .find(|existing| existing.artifact_id == artifact_id)
        {
            Some(existing) => *existing = artifact.clone(),
            None => registry.artifacts.push(artifact.clone()),
        }
        // 旧 GGUF generation artifact 置为 inactive，消除「注册表 GGUF 就绪 /
        // 运行走 Ollama」的双轨错位；仅落地数据，不触碰检索业务。
        for existing in registry.artifacts.iter_mut() {
            if existing.role == ModelRole::Generation
                && existing.format == ModelFormat::Gguf
                && existing.status == "ready"
            {
                existing.status = "inactive".into();
            }
        }
        registry.active_artifacts.insert(
            ModelRole::Generation.directory_name().to_owned(),
            artifact_id,
        );
        registry.active_profile_id = None;
        registry.updated_at = now;
        self.save_registry(&registry)?;
        Ok(artifact)
    }

    pub fn active_profile(&self) -> Result<Option<ModelProfile>, AppError> {
        let registry = self.load_registry()?;
        let Some(profile_id) = registry.active_profile_id else {
            return Ok(None);
        };
        Ok(registry
            .profiles
            .into_iter()
            .find(|profile| profile.profile_id == profile_id))
    }

    pub fn activate_profile(
        &self,
        edition: &str,
        edition_name: &str,
        generation_artifact_id: &Uuid,
        embedding_artifact_id: &Uuid,
        embedding_dimension: u32,
        download_job_id: Option<Uuid>,
    ) -> Result<ModelProfile, AppError> {
        if embedding_dimension == 0 {
            return Err(AppError::new(
                "MODEL_ACTIVATION_INVALID",
                "完整RAG配置缺少有效的向量维度",
                false,
            ));
        }
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let generation = registry
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_id == *generation_artifact_id)
            .cloned()
            .ok_or_else(|| {
                AppError::new("MODEL_ARTIFACT_NOT_FOUND", "生成模型组件不存在", false)
            })?;
        let embedding = registry
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_id == *embedding_artifact_id)
            .cloned()
            .ok_or_else(|| {
                AppError::new("MODEL_ARTIFACT_NOT_FOUND", "语义模型组件不存在", false)
            })?;
        // 迁移后容纳 Ollama 后端：generation/embedding 可为 Gguf/Onnx 文件或本机
        // Ollama（format=Ollama，local_path 为 tag）。格式不匹配的旧组合仍拒绝。
        let generation_ok =
            generation.format == ModelFormat::Gguf || generation.format == ModelFormat::Ollama;
        let embedding_ok =
            embedding.format == ModelFormat::Onnx || embedding.format == ModelFormat::Ollama;
        if generation.role != ModelRole::Generation
            || !generation_ok
            || embedding.role != ModelRole::Embedding
            || !embedding_ok
        {
            return Err(AppError::new(
                "MODEL_ACTIVATION_INVALID",
                "完整RAG配置的模型角色或格式不匹配",
                false,
            ));
        }
        let generation_file_ok =
            generation.format == ModelFormat::Ollama || Path::new(&generation.local_path).is_file();
        let embedding_file_ok =
            embedding.format == ModelFormat::Ollama || Path::new(&embedding.local_path).is_file();
        if !generation_file_ok || !embedding_file_ok {
            return Err(AppError::new(
                "MODEL_SOURCE_UNAVAILABLE",
                "完整RAG配置包含不可用的本地模型文件",
                false,
            ));
        }
        // Ollama artifact 无 package_manifest，跳过包完整性校验（由 /api/pull 托管）。
        if generation.package_manifest.is_some() {
            ensure_package_integrity(&generation)?;
        }
        if embedding.package_manifest.is_some() {
            ensure_package_integrity(&embedding)?;
        }
        for artifact_id in [generation_artifact_id, embedding_artifact_id] {
            if let Some(artifact) = registry
                .artifacts
                .iter_mut()
                .find(|artifact| artifact.artifact_id == *artifact_id)
            {
                // Ollama artifact 无 package_manifest，跳过包校验直写就绪。
                let Some(manifest) = artifact.package_manifest.as_mut() else {
                    artifact.status = "ready".into();
                    continue;
                };
                manifest.self_test_status = "ready".into();
                manifest.verified_at = Utc::now();
                artifact.status = "ready".into();
            }
        }
        if let Some(artifact) = registry
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.artifact_id == *embedding_artifact_id)
        {
            artifact.embedding_dimension = Some(embedding_dimension);
        }
        let embedding_already_active = registry
            .active_artifacts
            .get(ModelRole::Embedding.directory_name())
            .is_some_and(|active| *active == *embedding_artifact_id);
        let now = Utc::now();
        let profile = ModelProfile {
            profile_id: Uuid::now_v7(),
            edition: edition.to_owned(),
            name: edition_name.to_owned(),
            generation_artifact_id: *generation_artifact_id,
            embedding_artifact_id: *embedding_artifact_id,
            vision_artifact_id: None,
            ocr_artifact_id: None,
            reranker_artifact_id: None,
            status: if embedding_already_active {
                "ready".into()
            } else {
                "indexing".into()
            },
            activated_at: now,
        };
        supersede_pending_profile(&mut registry);
        registry.profiles.push(profile.clone());
        if embedding_already_active {
            registry.active_artifacts.insert(
                ModelRole::Generation.directory_name().into(),
                *generation_artifact_id,
            );
            registry.active_artifacts.insert(
                ModelRole::Embedding.directory_name().into(),
                *embedding_artifact_id,
            );
            registry.active_profile_id = Some(profile.profile_id);
            registry.pending_embedding_activation = None;
        } else {
            registry.pending_embedding_activation = Some(PendingEmbeddingActivation {
                artifact_id: *embedding_artifact_id,
                dimension: embedding_dimension,
                profile_id: Some(profile.profile_id),
                download_job_id,
                status: "indexing".into(),
                error: None,
                requested_at: now,
                updated_at: now,
            });
        }
        registry.registry_version = REGISTRY_VERSION;
        registry.updated_at = now;
        self.save_registry(&registry)?;
        Ok(profile)
    }

    pub fn begin_embedding_activation(
        &self,
        artifact_id: &Uuid,
        embedding_dimension: u32,
    ) -> Result<Option<PendingEmbeddingActivation>, AppError> {
        self.begin_embedding_activation_with_job(artifact_id, embedding_dimension, None)
    }

    pub fn begin_embedding_activation_with_job(
        &self,
        artifact_id: &Uuid,
        embedding_dimension: u32,
        download_job_id: Option<Uuid>,
    ) -> Result<Option<PendingEmbeddingActivation>, AppError> {
        if embedding_dimension == 0 {
            return Err(AppError::new(
                "MODEL_ACTIVATION_INVALID",
                "Embedding 模型自检没有返回有效的向量维度",
                false,
            ));
        }
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let artifact = registry
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.artifact_id == *artifact_id)
            .ok_or_else(|| AppError::new("MODEL_ARTIFACT_NOT_FOUND", "模型组件不存在", false))?;
        if artifact.role != ModelRole::Embedding
            || (artifact.format != ModelFormat::Onnx && artifact.format != ModelFormat::Ollama)
        {
            return Err(AppError::new(
                "MODEL_ACTIVATION_INVALID",
                "只有通过自检的 ONNX/Ollama Embedding 组件可以建立语义索引",
                false,
            ));
        }
        let file_ok =
            artifact.format == ModelFormat::Ollama || Path::new(&artifact.local_path).is_file();
        if !file_ok {
            return Err(AppError::new(
                "MODEL_SOURCE_UNAVAILABLE",
                "模型组件文件已经离开翻翻管理目录",
                false,
            ));
        }
        artifact.embedding_dimension = Some(embedding_dimension);
        let already_active = registry
            .active_artifacts
            .get(ModelRole::Embedding.directory_name())
            .is_some_and(|active| *active == *artifact_id)
            // 只有当组件确实处于 ready 时才视为“已激活”；若 active 指向的
            // 组件仍停在 pending_self_test（下载后索引构建中断的残留状态），
            // 应重新建立激活任务让索引流程恢复。
            && artifact.status == "ready";
        supersede_pending_profile(&mut registry);
        let pending = if already_active {
            registry.pending_embedding_activation = None;
            None
        } else {
            let now = Utc::now();
            let pending = PendingEmbeddingActivation {
                artifact_id: *artifact_id,
                dimension: embedding_dimension,
                profile_id: None,
                download_job_id,
                status: "indexing".into(),
                error: None,
                requested_at: now,
                updated_at: now,
            };
            registry.pending_embedding_activation = Some(pending.clone());
            Some(pending)
        };
        registry.updated_at = Utc::now();
        self.save_registry(&registry)?;
        Ok(pending)
    }

    pub fn pending_embedding_activation(
        &self,
    ) -> Result<Option<PendingEmbeddingActivation>, AppError> {
        Ok(self.load_registry()?.pending_embedding_activation)
    }

    pub fn complete_embedding_activation(
        &self,
        artifact_id: &Uuid,
        dimension: u32,
    ) -> Result<PendingEmbeddingActivation, AppError> {
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let pending = registry
            .pending_embedding_activation
            .clone()
            .filter(|pending| {
                pending.artifact_id == *artifact_id
                    && pending.dimension == dimension
                    && pending.status == "indexing"
            })
            .ok_or_else(|| {
                AppError::new(
                    "MODEL_ACTIVATION_INVALID",
                    "Embedding 索引完成结果与当前待切换模型不一致",
                    false,
                )
            })?;
        let artifact = registry
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.artifact_id == *artifact_id)
            .ok_or_else(|| AppError::new("MODEL_ARTIFACT_NOT_FOUND", "模型组件不存在", false))?;
        if artifact.role != ModelRole::Embedding || artifact.format != ModelFormat::Onnx {
            return Err(AppError::new(
                "MODEL_ACTIVATION_INVALID",
                "待切换组件不是有效的 ONNX Embedding 模型",
                false,
            ));
        }
        artifact.embedding_dimension = Some(dimension);
        // 索引构建成功即视为自检通过：与 activate_artifact 保持同一状态口径，
        // 否则 artifact 会一直停留在 pending_self_test，与已激活状态不一致
        // （表现为“模型已启用但详情仍显示待自检”）。
        if let Some(manifest) = artifact.package_manifest.as_mut() {
            manifest.self_test_status = "ready".into();
            manifest.verified_at = Utc::now();
        }
        artifact.status = "ready".into();
        registry
            .active_artifacts
            .insert(ModelRole::Embedding.directory_name().into(), *artifact_id);
        if let Some(profile_id) = pending.profile_id {
            let profile = registry
                .profiles
                .iter_mut()
                .find(|profile| profile.profile_id == profile_id)
                .ok_or_else(|| {
                    AppError::new(
                        "MODEL_ACTIVATION_INVALID",
                        "待切换模型配置已经不存在",
                        false,
                    )
                })?;
            profile.status = "ready".into();
            profile.activated_at = Utc::now();
            registry.active_artifacts.insert(
                ModelRole::Generation.directory_name().into(),
                profile.generation_artifact_id,
            );
            registry.active_profile_id = Some(profile_id);
        } else if registry.active_profile_id.is_some_and(|profile_id| {
            registry.profiles.iter().any(|profile| {
                profile.profile_id == profile_id && profile.embedding_artifact_id != *artifact_id
            })
        }) {
            registry.active_profile_id = None;
        }
        registry.pending_embedding_activation = None;
        registry.updated_at = Utc::now();
        self.save_registry(&registry)?;
        Ok(pending)
    }

    pub fn fail_embedding_activation(
        &self,
        artifact_id: &Uuid,
        error: &AppError,
    ) -> Result<PendingEmbeddingActivation, AppError> {
        self.update_pending_embedding_activation(artifact_id, None, "failed", Some(error.clone()))
    }

    pub fn pause_embedding_activation(
        &self,
        download_job_id: &Uuid,
    ) -> Result<PendingEmbeddingActivation, AppError> {
        self.update_pending_embedding_activation_for_job(download_job_id, "paused", None)
    }

    pub fn cancel_embedding_activation(
        &self,
        download_job_id: &Uuid,
    ) -> Result<PendingEmbeddingActivation, AppError> {
        self.update_pending_embedding_activation_for_job(download_job_id, "cancelled", None)
    }

    pub fn resume_embedding_activation(
        &self,
        download_job_id: &Uuid,
    ) -> Result<PendingEmbeddingActivation, AppError> {
        self.update_pending_embedding_activation_for_job(download_job_id, "indexing", None)
    }

    fn update_pending_embedding_activation_for_job(
        &self,
        download_job_id: &Uuid,
        status: &str,
        error: Option<AppError>,
    ) -> Result<PendingEmbeddingActivation, AppError> {
        let pending = self
            .pending_embedding_activation()?
            .filter(|pending| pending.download_job_id == Some(*download_job_id))
            .ok_or_else(|| {
                AppError::new(
                    "MODEL_DOWNLOAD_CONTROL_INVALID",
                    "当前下载任务没有可控制的索引切换阶段",
                    false,
                )
            })?;
        self.update_pending_embedding_activation(
            &pending.artifact_id,
            Some(*download_job_id),
            status,
            error,
        )
    }

    fn update_pending_embedding_activation(
        &self,
        artifact_id: &Uuid,
        download_job_id: Option<Uuid>,
        status: &str,
        error: Option<AppError>,
    ) -> Result<PendingEmbeddingActivation, AppError> {
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let pending = registry
            .pending_embedding_activation
            .as_mut()
            .filter(|pending| {
                pending.artifact_id == *artifact_id
                    && download_job_id
                        .map(|job_id| pending.download_job_id == Some(job_id))
                        .unwrap_or(true)
            })
            .ok_or_else(|| {
                AppError::new(
                    "MODEL_ACTIVATION_INVALID",
                    "待切换的 Embedding 模型已经变化",
                    false,
                )
            })?;
        pending.status = status.to_owned();
        pending.error = error;
        pending.updated_at = Utc::now();
        let updated = pending.clone();
        if let Some(profile_id) = pending.profile_id
            && let Some(profile) = registry
                .profiles
                .iter_mut()
                .find(|profile| profile.profile_id == profile_id)
        {
            profile.status = status.to_owned();
        }
        registry.updated_at = Utc::now();
        self.save_registry(&registry)?;
        Ok(updated)
    }

    pub fn activate_artifact(
        &self,
        artifact_id: &Uuid,
        embedding_dimension: Option<u32>,
    ) -> Result<ModelArtifact, AppError> {
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let artifact = registry
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.artifact_id == *artifact_id)
            .ok_or_else(|| AppError::new("MODEL_ARTIFACT_NOT_FOUND", "模型组件不存在", false))?;
        if !Path::new(&artifact.local_path).is_file() {
            return Err(AppError::new(
                "MODEL_SOURCE_UNAVAILABLE",
                "模型组件文件已经离开翻翻管理目录",
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
        let manifest = artifact.package_manifest.as_mut().ok_or_else(|| {
            AppError::new(
                "MODEL_PACKAGE_NOT_VERIFIED",
                "模型包还没有完成完整性校验",
                true,
            )
        })?;
        if manifest.integrity_status != "ready" {
            return Err(AppError::new(
                "MODEL_PACKAGE_INCOMPLETE",
                "模型包缺少配套文件或文件校验失败",
                true,
            ));
        }
        manifest.self_test_status = "ready".into();
        manifest.verified_at = Utc::now();
        artifact.status = "ready".into();
        let activated = artifact.clone();
        registry.active_artifacts.insert(
            activated.role.directory_name().to_owned(),
            activated.artifact_id,
        );
        registry.updated_at = Utc::now();
        self.save_registry(&registry)?;
        Ok(activated)
    }

    pub fn deactivate_role(&self, role: ModelRole) -> Result<(), AppError> {
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        registry.active_artifacts.remove(role.directory_name());
        if matches!(role, ModelRole::Generation | ModelRole::Embedding) {
            registry.active_profile_id = None;
        }
        if role == ModelRole::Embedding {
            registry.pending_embedding_activation = None;
        }
        registry.updated_at = Utc::now();
        self.save_registry(&registry)
    }

    /// 让 `selected_preset_id` 真正驱动运行时：把 plan 里每个角色的 active
    /// 指向「已就绪且 catalog_id 与预设一致」的本地 artifact；找不到就上报为
    /// 缺失（由下载编排处理）。运行时加载必须经此计划取模型，禁止直接读旧
    /// `model_role_config` 覆盖 preset 的选择。
    ///
    /// 匹配策略：优先用 `ModelArtifact.catalog_id` 精确匹配；旧数据没有该字段
    /// 时不强行猜，仅把该角色按当前 active 保留并记为缺失（迁移兼容、不静默替换）。
    pub fn apply_runtime_plan(&self, preset_id: &str) -> Result<PresetPlanReport, AppError> {
        let plan = resolve_runtime_model_plan(preset_id).ok_or_else(|| {
            AppError::new("PRESET_UNKNOWN", "未知模型预设，拒绝接通运行时", false)
        })?;
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let report = self.collect_plan_report(&registry, preset_id, &plan);
        // 仅对已就绪的角色把 active 指向匹配 artifact；缺失角色保持原样（由下载编排处理）。
        for item in &report.ready {
            if let Some(artifact_id) = registry
                .artifacts
                .iter()
                .find(|artifact| {
                    artifact.role == item.role
                        && artifact.status == "ready"
                        && artifact.catalog_id.as_deref() == Some(item.catalog_id.as_str())
                })
                .map(|artifact| artifact.artifact_id)
            {
                registry
                    .active_artifacts
                    .insert(item.role.directory_name().to_owned(), artifact_id);
            }
        }
        registry.updated_at = Utc::now();
        self.save_registry(&registry)?;
        Ok(report)
    }

    /// 只读地评估「选中某档位后各角色就绪 / 缺失情况」：不持久化 `selected_preset_id`、
    /// 不修改 active artifact、不写库。供前端在选择档位前先弹「下载缺失模型」确认框，
    /// 用户确认后才真正调用 `apply_runtime_plan` 完成切换（先确认下载、再切换）。
    pub fn plan_preset(&self, preset_id: &str) -> Result<PresetPlanReport, AppError> {
        let plan = resolve_runtime_model_plan(preset_id)
            .ok_or_else(|| AppError::new("PRESET_UNKNOWN", "未知模型预设", false))?;
        let _registry_guard = self.lock_registry()?;
        let registry = self.load_registry()?;
        Ok(self.collect_plan_report(&registry, preset_id, &plan))
    }

    /// 依据 registry 中的「已就绪 artifact」比对 plan 各角色的 catalog_id，返回
    /// 就绪 / 缺失清单。Ollama artifact（format=Ollama，local_path 为模型 tag）不受
    /// 本地文件存在性约束；其余角色仍需本地文件存在于 `DATA\FanFanModelStore`。
    /// 纯只读比对，不写 active_artifacts、不保存。
    fn collect_plan_report(
        &self,
        registry: &ModelRegistryState,
        preset_id: &str,
        plan: &RuntimeModelPlan,
    ) -> PresetPlanReport {
        let mut ready = Vec::new();
        let mut missing = Vec::new();
        for (role, catalog_id) in plan_items(plan) {
            let matched = registry.artifacts.iter().any(|artifact| {
                // Ollama 模型的 local_path 存的是模型 tag 而非真实文件路径，
                // 就绪判定不得再用 Path::is_file 校验（Ollama 由本机 pull 托管）。
                let local_ok = artifact.format == ModelFormat::Ollama
                    || Path::new(&artifact.local_path).is_file();
                artifact.role == role
                    && artifact.status == "ready"
                    && local_ok
                    && artifact.catalog_id.as_deref() == Some(catalog_id.as_str())
            });
            if matched {
                ready.push(RolePlanItem { role, catalog_id });
            } else {
                missing.push(RolePlanItem { role, catalog_id });
            }
        }
        PresetPlanReport {
            preset_id: preset_id.to_owned(),
            ready,
            missing,
        }
    }

    /// 把已安装 artifact 绑定到其归属 catalog_id，让 Preset 的白名单匹配与孤立清理
    /// 能可靠区分「当前档位模型」与「旧版遗留模型」。下载/导入完成后的 artifact 缺省
    /// 无 catalog_id，若不回填，`apply_runtime_plan` 永远匹配不到、孤立清理也失效。
    pub fn bind_artifact_catalog_id(
        &self,
        artifact_id: &Uuid,
        catalog_id: &str,
    ) -> Result<(), AppError> {
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        if let Some(artifact) = registry
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.artifact_id == *artifact_id)
            && artifact.catalog_id.as_deref() != Some(catalog_id)
        {
            artifact.catalog_id = Some(catalog_id.to_owned());
            registry.updated_at = Utc::now();
            self.save_registry(&registry)?;
        }
        Ok(())
    }

    pub fn vision_projector_path(&self, artifact: &ModelArtifact) -> Result<PathBuf, AppError> {
        if artifact.role != ModelRole::Vision || artifact.format != ModelFormat::Gguf {
            return Err(AppError::new(
                "VISION_MODEL_INVALID",
                "图片理解模型必须是带mmproj配套文件的GGUF视觉语言模型",
                false,
            ));
        }
        let directory = Path::new(&artifact.local_path)
            .parent()
            .ok_or_else(|| AppError::new("VISION_MODEL_INVALID", "图片理解模型目录无效", false))?;
        let mut projectors = fs::read_dir(directory)
            .map_err(|error| {
                AppError::new("VISION_PROJECTOR_UNAVAILABLE", error.to_string(), true)
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|value| value.eq_ignore_ascii_case("gguf"))
                    && path.file_name().is_some_and(|value| {
                        value
                            .to_string_lossy()
                            .to_ascii_lowercase()
                            .contains("mmproj")
                    })
            })
            .collect::<Vec<_>>();
        projectors.sort();
        match projectors.as_slice() {
            [path] => Ok(path.clone()),
            [] => Err(AppError::new(
                "VISION_PROJECTOR_MISSING",
                "视觉模型目录缺少与主模型匹配的mmproj文件",
                false,
            )),
            _ => Err(AppError::new(
                "VISION_PROJECTOR_AMBIGUOUS",
                "视觉模型目录包含多个mmproj文件，请只保留与主模型匹配的一个",
                false,
            )),
        }
    }

    pub fn create_download_job(
        &self,
        edition_id: &str,
        edition_name: &str,
        source: ModelSource,
        files: Vec<ModelDownloadFileProgress>,
    ) -> Result<ModelDownloadJob, AppError> {
        let _guard = self.download_lock.lock().map_err(|_| {
            AppError::new(
                "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                "模型下载状态暂时不可用",
                true,
            )
        })?;
        let mut registry = self.load_download_registry()?;
        if let Some(existing) = registry.jobs.iter().find(|job| {
            job.edition_id == edition_id
                && matches!(job.status.as_str(), "queued" | "running" | "paused")
        }) {
            return Ok(existing.clone());
        }
        let total_bytes = files.iter().map(|file| file.total_bytes).sum();
        let now = Utc::now();
        let job = ModelDownloadJob {
            job_id: Uuid::now_v7(),
            edition_id: edition_id.to_owned(),
            edition_name: edition_name.to_owned(),
            source,
            status: "queued".into(),
            phase: "queued".into(),
            downloaded_bytes: files.iter().map(|file| file.downloaded_bytes).sum(),
            total_bytes,
            progress: 0.0,
            bytes_per_second: 0,
            eta_seconds: None,
            retry_count: 0,
            current_file: None,
            files,
            installed_artifact_ids: Vec::new(),
            profile_id: None,
            error: None,
            activation_status: Some("pending".into()),
            activation_error: None,
            created_at: now,
            updated_at: now,
        };
        registry.jobs.push(job.clone());
        registry.updated_at = now;
        self.save_download_registry(&registry)?;
        Ok(job)
    }

    pub fn list_download_jobs(&self) -> Result<Vec<ModelDownloadJob>, AppError> {
        let _guard = self.download_lock.lock().map_err(|_| {
            AppError::new(
                "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                "模型下载状态暂时不可用",
                true,
            )
        })?;
        let mut jobs = self.load_download_registry()?.jobs;
        jobs.sort_by_key(|job| std::cmp::Reverse(job.updated_at));
        Ok(jobs)
    }

    pub fn download_job(&self, job_id: &Uuid) -> Result<ModelDownloadJob, AppError> {
        self.list_download_jobs()?
            .into_iter()
            .find(|job| job.job_id == *job_id)
            .ok_or_else(|| {
                AppError::new("MODEL_DOWNLOAD_JOB_NOT_FOUND", "模型下载任务不存在", false)
            })
    }

    pub fn update_download_job(
        &self,
        job: &ModelDownloadJob,
    ) -> Result<ModelDownloadJob, AppError> {
        let _guard = self.download_lock.lock().map_err(|_| {
            AppError::new(
                "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                "模型下载状态暂时不可用",
                true,
            )
        })?;
        let mut registry = self.load_download_registry()?;
        let current = registry
            .jobs
            .iter_mut()
            .find(|current| current.job_id == job.job_id)
            .ok_or_else(|| {
                AppError::new("MODEL_DOWNLOAD_JOB_NOT_FOUND", "模型下载任务不存在", false)
            })?;
        let mut next = job.clone();
        next.downloaded_bytes = next.files.iter().map(|file| file.downloaded_bytes).sum();
        next.total_bytes = next.files.iter().map(|file| file.total_bytes).sum();
        next.progress = if next.total_bytes == 0 {
            0.0
        } else {
            (next.downloaded_bytes as f64 / next.total_bytes as f64).clamp(0.0, 1.0)
        };
        next.updated_at = Utc::now();
        *current = next.clone();
        registry.updated_at = next.updated_at;
        self.save_download_registry(&registry)?;
        Ok(next)
    }

    pub fn remove_download_job(&self, job_id: &Uuid) -> Result<bool, AppError> {
        let _guard = self.download_lock.lock().map_err(|_| {
            AppError::new(
                "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                "模型下载状态暂时不可用",
                true,
            )
        })?;
        let mut registry = self.load_download_registry()?;
        let previous_len = registry.jobs.len();
        registry.jobs.retain(|job| job.job_id != *job_id);
        if registry.jobs.len() == previous_len {
            return Ok(false);
        }
        registry.updated_at = Utc::now();
        self.save_download_registry(&registry)?;
        Ok(true)
    }

    pub fn remove_download_staging_for_edition(&self, edition_id: &str) -> Result<u64, AppError> {
        if edition_id.is_empty()
            || !edition_id.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_JOB_INVALID",
                "模型下载任务包含无效的版本标识",
                false,
            ));
        }
        let root = self.download_staging_directory()?;
        let mut removed_bytes = 0_u64;
        for source in ["huggingface", "modelscope"] {
            let target = root.join(source).join(edition_id);
            if !target.exists() {
                continue;
            }
            removed_bytes = removed_bytes.saturating_add(directory_size_without_links(&target)?);
            fs::remove_dir_all(&target).map_err(|error| {
                AppError::new("MODEL_DOWNLOAD_CLEANUP_FAILED", error.to_string(), true)
            })?;
        }
        Ok(removed_bytes)
    }

    pub fn download_artifact_staging_directory(
        &self,
        source: ModelSource,
        edition_id: &str,
        role: ModelRole,
    ) -> Result<PathBuf, AppError> {
        let source = match source {
            ModelSource::Huggingface => "huggingface",
            ModelSource::Modelscope => "modelscope",
            ModelSource::LocalImport => {
                return Err(AppError::new(
                    "MODEL_DOWNLOAD_SOURCE_UNAVAILABLE",
                    "本地导入不能使用下载暂存目录",
                    false,
                ));
            }
            ModelSource::Ollama => "ollama",
        };
        let directory = self
            .download_staging_directory()?
            .join(source)
            .join(edition_id)
            .join(role.directory_name());
        fs::create_dir_all(&directory).map_err(|error| {
            AppError::new("MODEL_DIRECTORY_CREATE_FAILED", error.to_string(), true)
        })?;
        Ok(directory)
    }

    fn recover_interrupted_downloads(&self) -> Result<(), AppError> {
        let _guard = self.download_lock.lock().map_err(|_| {
            AppError::new(
                "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                "模型下载状态暂时不可用",
                true,
            )
        })?;
        let mut registry = self.load_download_registry()?;
        let mut changed = false;
        for job in &mut registry.jobs {
            if matches!(job.status.as_str(), "queued" | "running") {
                job.status = "paused".into();
                job.phase = "paused".into();
                job.bytes_per_second = 0;
                job.eta_seconds = None;
                job.current_file = None;
                job.error = Some(AppError::new(
                    "MODEL_DOWNLOAD_INTERRUPTED",
                    "应用上次退出时模型尚未下载完成，可以继续下载",
                    true,
                ));
                job.updated_at = Utc::now();
                changed = true;
            }
        }
        let legacy_root = self.model_root.join(".downloads");
        if legacy_root.is_dir() {
            let entries = fs::read_dir(&legacy_root).map_err(|error| {
                AppError::new("MODEL_DOWNLOAD_STATE_READ_FAILED", error.to_string(), true)
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| {
                    AppError::new("MODEL_DOWNLOAD_STATE_READ_FAILED", error.to_string(), true)
                })?;
                let path = entry.path();
                let metadata = match entry.metadata() {
                    Ok(metadata) if metadata.is_file() => metadata,
                    _ => continue,
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some((edition_id, edition_name, role, total_bytes)) =
                    legacy_download_spec(&name)
                else {
                    continue;
                };
                let quarantine = legacy_root.join("quarantine");
                fs::create_dir_all(&quarantine).map_err(|error| {
                    AppError::new("MODEL_DOWNLOAD_QUARANTINE_FAILED", error.to_string(), true)
                })?;
                let quarantined_name =
                    format!("{}.invalid-{}", name, Utc::now().timestamp_millis());
                fs::rename(&path, quarantine.join(quarantined_name)).map_err(|error| {
                    AppError::new("MODEL_DOWNLOAD_QUARANTINE_FAILED", error.to_string(), true)
                })?;
                let now = Utc::now();
                registry.jobs.push(ModelDownloadJob {
                    job_id: Uuid::now_v7(),
                    edition_id: edition_id.into(),
                    edition_name: edition_name.into(),
                    source: ModelSource::Huggingface,
                    status: "failed".into(),
                    phase: "failed".into(),
                    downloaded_bytes: metadata.len().min(total_bytes),
                    total_bytes,
                    progress: if total_bytes == 0 {
                        0.0
                    } else {
                        (metadata.len() as f64 / total_bytes as f64).clamp(0.0, 1.0)
                    },
                    bytes_per_second: 0,
                    eta_seconds: None,
                    retry_count: 0,
                    current_file: Some(name.clone()),
                    files: vec![ModelDownloadFileProgress {
                        role,
                        file_name: name.trim_end_matches(".part").into(),
                        downloaded_bytes: metadata.len().min(total_bytes),
                        total_bytes,
                        status: "failed".into(),
                    }],
                    installed_artifact_ids: Vec::new(),
                    profile_id: None,
                    error: Some(AppError::new(
                        "MODEL_DOWNLOAD_LEGACY_PARTIAL_INVALID",
                        format!(
                            "发现旧版未标记来源的断点文件，已隔离以避免跨来源续传（实际{}字节，预期{}字节）",
                            metadata.len(),
                            total_bytes
                        ),
                        true,
                    )),
                    activation_status: None,
                    activation_error: None,
                    created_at: now,
                    updated_at: now,
                });
                changed = true;
            }
        }
        if changed {
            registry.updated_at = Utc::now();
            self.save_download_registry(&registry)?;
        }
        Ok(())
    }

    fn recover_interrupted_embedding_activation(&self) -> Result<(), AppError> {
        let _registry_guard = self.lock_registry()?;
        let mut registry = self.load_registry()?;
        let Some(pending) = registry.pending_embedding_activation.as_mut() else {
            return Ok(());
        };
        if pending.status != "indexing" || pending.download_job_id.is_none() {
            return Ok(());
        }
        pending.status = "paused".into();
        pending.error = None;
        pending.updated_at = Utc::now();
        if let Some(profile_id) = pending.profile_id
            && let Some(profile) = registry
                .profiles
                .iter_mut()
                .find(|profile| profile.profile_id == profile_id)
        {
            profile.status = "paused".into();
        }
        registry.updated_at = Utc::now();
        self.save_registry(&registry)
    }

    fn load_download_registry(&self) -> Result<ModelDownloadRegistryState, AppError> {
        if !self.downloads_path.exists() {
            return Ok(ModelDownloadRegistryState::default());
        }
        let bytes = fs::read(&self.downloads_path).map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_STATE_READ_FAILED", error.to_string(), true)
        })?;
        let registry =
            serde_json::from_slice::<ModelDownloadRegistryState>(&bytes).map_err(|error| {
                AppError::new("MODEL_DOWNLOAD_STATE_INVALID", error.to_string(), false)
            })?;
        if registry.registry_version > DOWNLOAD_REGISTRY_VERSION {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_STATE_TOO_NEW",
                "模型下载状态来自更高版本的翻翻",
                false,
            ));
        }
        Ok(registry)
    }

    fn save_download_registry(
        &self,
        registry: &ModelDownloadRegistryState,
    ) -> Result<(), AppError> {
        let bytes = serde_json::to_vec_pretty(registry).map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_STATE_INVALID", error.to_string(), false)
        })?;
        let temporary_path = self.downloads_path.with_extension("json.tmp");
        let mut file = File::create(&temporary_path).map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_STATE_WRITE_FAILED", error.to_string(), true)
        })?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                AppError::new("MODEL_DOWNLOAD_STATE_WRITE_FAILED", error.to_string(), true)
            })?;
        atomic_replace_file(&temporary_path, &self.downloads_path).map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_STATE_WRITE_FAILED", error.to_string(), true)
        })
    }

    /// 只读健康校验：registry.json 存在、可解析且版本未超前。
    /// 与 open_store 不同，不创建目录、不恢复中断下载、不刷新清单——
    /// 专供清理前的安全校验使用，避免第二个 ModelManager 实例
    /// 与在役服务并发改写 downloads.json / registry.json。
    pub fn registry_readable(model_root: &Path) -> Result<(), AppError> {
        let registry_path = model_root.join("registry.json");
        // 与 load_registry 语义一致：缺失视为全新空仓库，可读。
        if !registry_path.is_file() {
            return Ok(());
        }
        let bytes = fs::read(&registry_path).map_err(|error| {
            AppError::new(
                "MODEL_REGISTRY_READ_FAILED",
                format!("模型注册表不可读：{error}"),
                true,
            )
        })?;
        let registry = serde_json::from_slice::<ModelRegistryState>(&bytes)
            .map_err(|error| AppError::new("MODEL_REGISTRY_INVALID", error.to_string(), false))?;
        if registry.registry_version > REGISTRY_VERSION {
            return Err(AppError::new(
                "MODEL_REGISTRY_TOO_NEW",
                "模型注册表来自更高版本的翻翻",
                false,
            ));
        }
        Ok(())
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
                "模型注册表来自更高版本的翻翻",
                false,
            ));
        }
        Ok(registry)
    }

    fn lock_registry(&self) -> Result<std::sync::MutexGuard<'_, ()>, AppError> {
        self.registry_lock.lock().map_err(|_| {
            AppError::new(
                "MODEL_REGISTRY_LOCK_FAILED",
                "模型注册表状态已损坏，请重启翻翻后重试",
                true,
            )
        })
    }

    fn save_registry(&self, registry: &ModelRegistryState) -> Result<(), AppError> {
        let mut registry = registry.clone();
        registry.registry_version = REGISTRY_VERSION;
        let bytes = serde_json::to_vec_pretty(&registry)
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

fn supersede_pending_profile(registry: &mut ModelRegistryState) {
    if let Some(profile_id) = registry
        .pending_embedding_activation
        .as_ref()
        .and_then(|pending| pending.profile_id)
        && let Some(profile) = registry
            .profiles
            .iter_mut()
            .find(|profile| profile.profile_id == profile_id)
    {
        profile.status = "superseded".into();
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

fn legacy_download_spec(name: &str) -> Option<(&'static str, &'static str, ModelRole, u64)> {
    match name {
        "Qwen3-0.6B-Q8_0.gguf.part" => {
            Some(("light", "轻量版", ModelRole::Generation, 639_446_688))
        }
        "Qwen3-4B-Q4_K_M.gguf.part" => {
            Some(("standard", "标准版", ModelRole::Generation, 2_497_280_256))
        }
        _ => None,
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

fn build_package_manifest(artifact: &ModelArtifact) -> Result<ModelPackageManifest, AppError> {
    let main_path = Path::new(&artifact.local_path);
    let parent = main_path.parent().ok_or_else(|| {
        AppError::new(
            "MODEL_PACKAGE_PATH_INVALID",
            "模型包缺少有效的安装目录",
            false,
        )
    })?;
    let expected = locked_download_artifact(&artifact.model_id, artifact.source)
        .map(|locked| locked.files())
        .unwrap_or_else(|| {
            vec![DownloadFile {
                file_name: main_path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "model".into()),
                remote_path: String::new(),
                url: String::new(),
                sha256: artifact.sha256.clone(),
                size_bytes: artifact.size_bytes,
            }]
        });
    let mut files = Vec::with_capacity(expected.len());
    for expected_file in expected {
        let path = parent.join(&expected_file.file_name);
        let status = match fs::metadata(&path) {
            Ok(metadata)
                if metadata.is_file()
                    && metadata.len() == expected_file.size_bytes
                    && sha256_file(&path)? == expected_file.sha256 =>
            {
                "ready"
            }
            Ok(_) => "changed",
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing",
            Err(error) => {
                return Err(AppError::new(
                    "MODEL_PACKAGE_VERIFY_FAILED",
                    error.to_string(),
                    true,
                ));
            }
        };
        files.push(ModelPackageFile {
            file_name: expected_file.file_name,
            size_bytes: expected_file.size_bytes,
            sha256: expected_file.sha256,
            required: true,
            status: status.into(),
        });
    }
    let integrity_status = if files.iter().all(|file| file.status == "ready") {
        "ready"
    } else {
        "incomplete"
    };
    Ok(ModelPackageManifest {
        manifest_version: 1,
        files,
        integrity_status: integrity_status.into(),
        self_test_status: "pending".into(),
        verified_at: Utc::now(),
    })
}

fn ensure_package_integrity(artifact: &ModelArtifact) -> Result<(), AppError> {
    if artifact
        .package_manifest
        .as_ref()
        .is_some_and(|manifest| manifest.integrity_status == "ready")
    {
        Ok(())
    } else {
        Err(AppError::new(
            "MODEL_PACKAGE_INCOMPLETE",
            "模型包缺少配套文件或文件校验失败",
            true,
        ))
    }
}

fn package_file_matches(path: &Path, expected: &DownloadFile) -> Result<bool, AppError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(AppError::new(
                "MODEL_PACKAGE_VERIFY_FAILED",
                error.to_string(),
                true,
            ));
        }
    };
    Ok(metadata.len() == expected.size_bytes && sha256_file(path)? == expected.sha256)
}

fn find_verified_file(root: &Path, expected: &DownloadFile) -> Result<Option<PathBuf>, AppError> {
    let mut pending = vec![root.to_path_buf()];
    let mut visited = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(|error| {
            AppError::new("MODEL_COMPANION_RESTORE_FAILED", error.to_string(), true)
        })? {
            let entry = entry.map_err(|error| {
                AppError::new("MODEL_COMPANION_RESTORE_FAILED", error.to_string(), true)
            })?;
            let metadata = entry.metadata().map_err(|error| {
                AppError::new("MODEL_COMPANION_RESTORE_FAILED", error.to_string(), true)
            })?;
            if metadata.is_dir() {
                pending.push(entry.path());
            } else if metadata.is_file()
                && entry.file_name().to_string_lossy() == expected.file_name
                && package_file_matches(&entry.path(), expected)?
            {
                return Ok(Some(entry.path()));
            }
            visited = visited.saturating_add(1);
            if visited > MAX_IMPORT_FILES.saturating_mul(8) {
                return Ok(None);
            }
        }
    }
    Ok(None)
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
    } else if suggested_role == Some(ModelRole::Vision) {
        discover_gguf_vision_companions(path)?
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
    if format == ModelFormat::Gguf
        && suggested_role == Some(ModelRole::Vision)
        && !path.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .to_ascii_lowercase()
                .contains("mmproj")
        })
        && companion_files.is_empty()
    {
        warnings.push("视觉模型目录缺少匹配的mmproj GGUF文件，无法启用图片理解".into());
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
    } else if value.contains("vision")
        || value.contains("vl-")
        || value.contains("-vl")
        || value.contains("multimodal")
        || value.contains("mmproj")
    {
        Some(ModelRole::Vision)
    } else if value.contains("rerank") || value.contains("cross-encoder") {
        Some(ModelRole::Reranker)
    } else if value.contains("embed") || value.contains("bge-") || value.contains("gte-") {
        Some(ModelRole::Embedding)
    } else if value.contains("asr")
        || value.contains("whisper")
        || value.contains("paraformer")
        || value.contains("sense-voice")
    {
        Some(ModelRole::Asr)
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
    let allowed = [
        "json",
        "txt",
        "model",
        "vocab",
        "merges",
        "yaml",
        "yml",
        "onnx_data",
        // OCR 等多文件 ONNX 模型：同目录下的 det/cls 等配套 ONNX 文件也需安装。
        "onnx",
    ];
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

fn discover_gguf_vision_companions(path: &Path) -> Result<Vec<String>, AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::new("MODEL_SOURCE_UNAVAILABLE", "GGUF模型缺少父目录", false))?;
    let mut files = fs::read_dir(parent)
        .map_err(|error| AppError::new("MODEL_SOURCE_UNAVAILABLE", error.to_string(), true))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|candidate| {
            candidate != path
                && candidate.is_file()
                && candidate
                    .extension()
                    .is_some_and(|value| value.eq_ignore_ascii_case("gguf"))
                && candidate.file_name().is_some_and(|value| {
                    value
                        .to_string_lossy()
                        .to_ascii_lowercase()
                        .contains("mmproj")
                })
        })
        .map(|candidate| candidate.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
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

fn directory_size_without_links(path: &Path) -> Result<u64, AppError> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(path)
        .map_err(|error| AppError::new("MODEL_DOWNLOAD_CLEANUP_FAILED", error.to_string(), true))?
    {
        let entry = entry.map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_CLEANUP_FAILED", error.to_string(), true)
        })?;
        let metadata = entry.metadata().map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_CLEANUP_FAILED", error.to_string(), true)
        })?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        total = total.saturating_add(if metadata.is_dir() {
            directory_size_without_links(&entry.path())?
        } else if metadata.is_file() {
            metadata.len()
        } else {
            0
        });
    }
    Ok(total)
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

    fn import_fake_model(
        manager: &ModelManager,
        source_root: &Path,
        name: &str,
        role: ModelRole,
    ) -> ModelArtifact {
        let directory = source_root.join(name);
        fs::create_dir_all(&directory).expect("create model source");
        let extension = if role == ModelRole::Embedding {
            "onnx"
        } else {
            "gguf"
        };
        let model = directory.join(format!("{name}.{extension}"));
        fs::write(&model, format!("fake {name} model bytes")).expect("write fake model");
        if role == ModelRole::Embedding {
            fs::write(directory.join("tokenizer.json"), b"{}").expect("write tokenizer");
        }
        manager
            .import_artifacts(&[ModelImportSelection {
                source_path: model.to_string_lossy().into_owned(),
                role,
            }])
            .expect("import fake model")
            .remove(0)
    }

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
    fn embedding_profile_switch_keeps_old_model_until_new_index_is_verified() {
        let data = tempfile::tempdir().expect("data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let manager = ModelManager::open(data.path()).expect("open manager");
        let generation =
            import_fake_model(&manager, source.path(), "generation", ModelRole::Generation);
        let old_embedding = import_fake_model(
            &manager,
            source.path(),
            "embedding-old",
            ModelRole::Embedding,
        );
        let new_embedding = import_fake_model(
            &manager,
            source.path(),
            "embedding-new",
            ModelRole::Embedding,
        );
        manager
            .activate_artifact(&old_embedding.artifact_id, Some(2))
            .expect("activate existing embedding baseline");
        let old_profile = manager
            .activate_profile(
                "old",
                "旧配置",
                &generation.artifact_id,
                &old_embedding.artifact_id,
                2,
                None,
            )
            .expect("activate old profile");
        assert_eq!(old_profile.status, "ready");

        let job_id = Uuid::now_v7();
        let staged = manager
            .activate_profile(
                "new",
                "新配置",
                &generation.artifact_id,
                &new_embedding.artifact_id,
                3,
                Some(job_id),
            )
            .expect("stage new profile");
        assert_eq!(staged.status, "indexing");
        assert_eq!(
            manager
                .active_artifact(ModelRole::Embedding)
                .expect("read old active embedding")
                .expect("old embedding remains active")
                .artifact_id,
            old_embedding.artifact_id
        );
        assert_eq!(
            manager
                .active_profile()
                .expect("read old active profile")
                .expect("old profile remains active")
                .profile_id,
            old_profile.profile_id
        );
        let pending = manager
            .pending_embedding_activation()
            .expect("read pending migration")
            .expect("pending migration");
        assert_eq!(pending.artifact_id, new_embedding.artifact_id);
        assert_eq!(pending.download_job_id, Some(job_id));

        drop(manager);
        let manager = ModelManager::open(data.path()).expect("reopen manager after interruption");
        assert_eq!(
            manager
                .pending_embedding_activation()
                .expect("read recovered migration")
                .expect("recovered migration")
                .status,
            "paused"
        );
        assert_eq!(
            manager
                .active_artifact(ModelRole::Embedding)
                .expect("read active after restart")
                .expect("old embedding preserved after restart")
                .artifact_id,
            old_embedding.artifact_id
        );
        manager
            .resume_embedding_activation(&job_id)
            .expect("resume migration after restart");

        let failure = AppError::new(
            "VECTOR_INDEX_INCOMPLETE",
            "synthetic incomplete index",
            true,
        );
        manager
            .fail_embedding_activation(&new_embedding.artifact_id, &failure)
            .expect("persist failed migration");
        assert_eq!(
            manager
                .active_artifact(ModelRole::Embedding)
                .expect("read active after failure")
                .expect("old embedding preserved after failure")
                .artifact_id,
            old_embedding.artifact_id
        );
        manager
            .resume_embedding_activation(&job_id)
            .expect("resume persisted migration");
        let completed = manager
            .complete_embedding_activation(&new_embedding.artifact_id, 3)
            .expect("complete verified migration");
        assert_eq!(completed.download_job_id, Some(job_id));
        assert!(
            manager
                .pending_embedding_activation()
                .expect("read completed migration")
                .is_none()
        );
        assert_eq!(
            manager
                .active_artifact(ModelRole::Embedding)
                .expect("read new active embedding")
                .expect("new embedding active")
                .artifact_id,
            new_embedding.artifact_id
        );
        assert_eq!(
            manager
                .active_profile()
                .expect("read new active profile")
                .expect("new profile active")
                .profile_id,
            staged.profile_id
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

    #[test]
    fn legacy_partial_download_is_quarantined_and_reported_as_failed() {
        let data = tempfile::tempdir().expect("data tempdir");
        let downloads = data.path().join("models").join(".downloads");
        fs::create_dir_all(&downloads).expect("create legacy downloads");
        let partial = downloads.join("Qwen3-0.6B-Q8_0.gguf.part");
        fs::write(&partial, b"invalid interrupted model bytes").expect("write partial");

        let manager = ModelManager::open(data.path()).expect("open manager");
        let jobs = manager.list_download_jobs().expect("list recovered jobs");

        assert!(!partial.exists());
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].status, "failed");
        assert_eq!(jobs[0].phase, "failed");
        assert_eq!(
            jobs[0].error.as_ref().map(|error| error.code.as_str()),
            Some("MODEL_DOWNLOAD_LEGACY_PARTIAL_INVALID")
        );
        assert!(
            downloads
                .join("quarantine")
                .read_dir()
                .expect("quarantine")
                .next()
                .is_some()
        );
    }

    #[test]
    fn durable_store_removes_download_task_without_touching_installed_models() {
        let store = tempfile::tempdir().expect("model store tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let manager = ModelManager::open_store(store.path()).expect("open durable store");
        let installed = import_fake_model(
            &manager,
            source.path(),
            "installed-generation",
            ModelRole::Generation,
        );
        let job = manager
            .create_download_job(
                "test-edition",
                "测试模型",
                ModelSource::Huggingface,
                Vec::new(),
            )
            .expect("create download job");
        let staging = manager
            .download_artifact_staging_directory(
                ModelSource::Huggingface,
                "test-edition",
                ModelRole::Generation,
            )
            .expect("create staging");
        fs::write(staging.join("model.gguf.part"), b"partial bytes").expect("write partial");

        assert!(
            manager
                .remove_download_job(&job.job_id)
                .expect("remove task")
        );
        assert!(
            manager
                .remove_download_staging_for_edition("test-edition")
                .expect("remove staging")
                > 0
        );
        assert!(Path::new(&installed.local_path).is_file());
        assert!(manager.list_download_jobs().expect("list jobs").is_empty());
        let status = manager.store_status().expect("store status");
        assert_eq!(status.installed_artifacts, 1);
        assert_eq!(status.integrity_status, "ready");
    }

    #[test]
    fn edition_has_one_stable_download_job_across_sources() {
        let store = tempfile::tempdir().expect("model store tempdir");
        let manager = ModelManager::open_store(store.path()).expect("open durable store");
        let first = manager
            .create_download_job(
                "same-edition",
                "同一模型",
                ModelSource::Huggingface,
                Vec::new(),
            )
            .expect("create first task");
        let second = manager
            .create_download_job(
                "same-edition",
                "同一模型",
                ModelSource::Modelscope,
                Vec::new(),
            )
            .expect("reuse task");

        assert_eq!(second.job_id, first.job_id);
        assert_eq!(second.source, ModelSource::Huggingface);
        assert_eq!(manager.list_download_jobs().expect("list jobs").len(), 1);
    }

    #[test]
    fn vision_import_copies_matching_mmproj_and_rejects_projector_as_main() {
        let data = tempfile::tempdir().expect("data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let model = source.path().join("Qwen3-VL-2B-Q4_K_M.gguf");
        let projector = source.path().join("mmproj-Qwen3-VL-2B-F16.gguf");
        fs::write(&model, b"vision-main").expect("write vision model");
        fs::write(&projector, b"vision-projector").expect("write projector");
        let manager = ModelManager::open(data.path()).expect("open manager");

        let imported = manager
            .import_artifacts(&[ModelImportSelection {
                source_path: model.to_string_lossy().into_owned(),
                role: ModelRole::Vision,
            }])
            .expect("import vision model");
        let copied_projector = manager
            .vision_projector_path(&imported[0])
            .expect("resolve copied projector");
        assert_eq!(
            fs::read(copied_projector).expect("read copied projector"),
            b"vision-projector"
        );

        let error = manager
            .import_artifacts(&[ModelImportSelection {
                source_path: projector.to_string_lossy().into_owned(),
                role: ModelRole::Vision,
            }])
            .expect_err("projector cannot be imported as main model");
        assert_eq!(error.code, "VISION_MODEL_MAIN_REQUIRED");
    }
}
