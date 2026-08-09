use std::{
    collections::HashMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use chrono::Utc;
use remin_core::{
    AddRootRequest, AnswerResult, AppError, AskRequest, CandidateRoot, CatalogService,
    ChunkEmbeddingInput, CollectionRecord, CollectionRule, CreateCollectionRequest,
    DegradationLevel, DownloadedModelMetadata, EmbeddingRequest, ExportResult, ExportTableRequest,
    ExtractionPreset, ExtractionRunRequest, ExtractionRunResult, FilePage, FilePreview, FileQuery,
    FileRecord, ImportCandidate, InboxItem, InboxPage, InboxQuery, InboxUpdateRequest,
    IncrementalWatchManager, IndexRebuildResult, JobRecord, LocalGenerationRuntime, LogPage,
    LogQuery, MaintenanceSnapshot, ModelArtifact, ModelEdition, ModelFormat, ModelImportSelection,
    ModelManager, ModelRole, ParseMetrics, ParseOutcome, ParseRequest, ParseResult,
    PlanSkillRequest, RelationPage, RelationQuery, RelationRefreshResult, RelationType,
    RootDiscoveryResult, RootRecord, SearchMode, SearchRequest, SearchSession, SemanticQuery,
    SkillDefinition, TaskExecutionResult, TaskPlan, TriageStatus, WorkerClient,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use windows::{
    Win32::{
        Storage::FileSystem::GetDiskFreeSpaceExW,
        System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    },
    core::PCWSTR,
};

#[derive(Clone, Default)]
pub struct CatalogServiceState(Arc<OnceLock<Arc<CatalogService>>>);

impl CatalogServiceState {
    pub fn get(&self) -> Result<Arc<CatalogService>, AppError> {
        self.0.get().cloned().ok_or_else(|| {
            AppError::new("STARTUP_NOT_READY", "本地资料库仍在后台打开，请稍候", true)
        })
    }

    pub fn initialize(&self, service: Arc<CatalogService>) -> Result<(), AppError> {
        self.0.set(service).map_err(|_| {
            AppError::new("STARTUP_ALREADY_INITIALIZED", "本地资料库已经初始化", false)
        })
    }
}

#[derive(Default)]
pub struct WatcherServiceState(pub Mutex<Option<IncrementalWatchManager>>);

impl WatcherServiceState {
    pub fn install(&self, watcher: IncrementalWatchManager) -> Result<(), AppError> {
        let mut current = self.0.lock().map_err(|_| {
            AppError::new("WATCHER_STATE_UNAVAILABLE", "目录监听状态已经损坏", true)
        })?;
        *current = Some(watcher);
        Ok(())
    }

    fn with_mut<T>(
        &self,
        action: impl FnOnce(&mut IncrementalWatchManager) -> T,
    ) -> Result<T, AppError> {
        let mut current = self.0.lock().map_err(|_| {
            AppError::new("WATCHER_STATE_UNAVAILABLE", "目录监听状态已经损坏", true)
        })?;
        let watcher = current.as_mut().ok_or_else(|| {
            AppError::new(
                "STARTUP_NOT_READY",
                "目录监听器仍在后台初始化，请稍候",
                true,
            )
        })?;
        Ok(action(watcher))
    }
}

pub struct WorkerServiceState {
    pub client: WorkerClient,
    pub running: AtomicBool,
    pub embedding_running: AtomicBool,
}

pub struct GenerationServiceState(pub Arc<Mutex<LocalGenerationRuntime>>);

#[derive(Debug, Clone, Serialize)]
pub struct OperationHandle {
    operation_id: Uuid,
    kind: &'static str,
    status: &'static str,
    created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AskOperationSnapshot {
    handle: OperationHandle,
    result: Option<AnswerResult>,
    error: Option<AppError>,
}

#[derive(Debug)]
struct AskOperationEntry {
    handle: OperationHandle,
    result: Option<AnswerResult>,
    error: Option<AppError>,
    cancelled: Arc<AtomicBool>,
    worker: WorkerClient,
}

#[derive(Clone, Default)]
pub struct AskCoordinatorState(Arc<Mutex<HashMap<Uuid, AskOperationEntry>>>);

#[derive(Clone, Default)]
pub struct ModelServiceState(Arc<OnceLock<Arc<ModelManager>>>);

impl ModelServiceState {
    pub fn get(&self) -> Result<Arc<ModelManager>, AppError> {
        self.0.get().cloned().ok_or_else(|| {
            AppError::new(
                "STARTUP_NOT_READY",
                "本地模型目录仍在后台打开，请稍候",
                true,
            )
        })
    }

    pub fn initialize(&self, service: Arc<ModelManager>) -> Result<(), AppError> {
        self.0
            .set(service)
            .map_err(|_| AppError::new("STARTUP_ALREADY_INITIALIZED", "模型服务已经初始化", false))
    }
}

pub struct EnvironmentServiceState {
    pub data_directory: PathBuf,
    pub latest: Mutex<Option<EnvironmentCheck>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentCheck {
    status: &'static str,
    memory_total_gb: Option<u64>,
    disk_available_gb: Option<u64>,
    gpu_name: Option<String>,
    gpu_memory_gb: Option<u64>,
    recommended_edition: Option<&'static str>,
    runtime_backend: Option<&'static str>,
    checked_at: String,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRuntimeState {
    status: &'static str,
    runtime_mode: &'static str,
    active_profile_id: Option<String>,
    active_profile_name: Option<String>,
    runtime_backend: Option<&'static str>,
    message: &'static str,
    checked_at: String,
    capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCapabilities {
    generation: bool,
    embedding: bool,
    reranker: bool,
    ocr: bool,
}

fn detect_environment(data_directory: &Path) -> EnvironmentCheck {
    let memory_total_gb = memory_total_gb();
    let disk_available_gb = disk_available_gb(data_directory);
    let (gpu_name, gpu_memory_gb) = gpu_details();
    let mut warnings = Vec::new();
    if memory_total_gb.is_none() {
        warnings.push("无法读取系统内存信息".to_owned());
    }
    if disk_available_gb.is_none() {
        warnings.push("无法读取应用数据磁盘剩余空间".to_owned());
    }
    if let Some(name) = &gpu_name {
        warnings.push(format!(
            "检测到GPU：{name}；当前验收运行时继续使用已验证的CPU后端"
        ));
    }
    let status = if memory_total_gb.is_none() && disk_available_gb.is_none() {
        "failed"
    } else if memory_total_gb.is_some_and(|value| value < 8)
        || disk_available_gb.is_some_and(|value| value < 10)
    {
        warnings.push("资源低于推荐值，已采用轻量基础模式".to_owned());
        "degraded"
    } else {
        "ready"
    };
    EnvironmentCheck {
        status,
        memory_total_gb,
        disk_available_gb,
        gpu_name,
        gpu_memory_gb,
        recommended_edition: memory_total_gb
            .map(|value| if value >= 12 { "standard" } else { "light" }),
        runtime_backend: Some("cpu"),
        checked_at: Utc::now().to_rfc3339(),
        warnings,
    }
}

#[cfg(windows)]
fn memory_total_gb() -> Option<u64> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut status) }
        .ok()
        .map(|_| status.ullTotalPhys / 1024 / 1024 / 1024)
}

#[cfg(not(windows))]
fn memory_total_gb() -> Option<u64> {
    None
}

#[cfg(windows)]
fn disk_available_gb(path: &Path) -> Option<u64> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut available = 0_u64;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None) }
        .ok()
        .map(|_| available / 1024 / 1024 / 1024)
}

#[cfg(not(windows))]
fn disk_available_gb(_path: &Path) -> Option<u64> {
    None
}

#[cfg(windows)]
fn gpu_details() -> (Option<String>, Option<u64>) {
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        "$gpu=Get-CimInstance Win32_VideoController | Where-Object {$_.Name -notmatch 'Microsoft Basic'} | Sort-Object AdapterRAM -Descending | Select-Object -First 1 Name,AdapterRAM; if($gpu){$gpu | ConvertTo-Json -Compress}",
    ]);
    hide_process_window(&mut command);
    let Ok(output) = command.output() else {
        return (None, None);
    };
    if !output.status.success() {
        return (None, None);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
        return (None, None);
    };
    let name = value
        .get("Name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let memory = value
        .get("AdapterRAM")
        .and_then(Value::as_u64)
        .map(|bytes| bytes / 1024 / 1024 / 1024)
        .filter(|value| *value > 0);
    (name, memory)
}

#[cfg(not(windows))]
fn gpu_details() -> (Option<String>, Option<u64>) {
    (None, None)
}

#[tauri::command(async)]
pub fn environment_get_latest(
    environment: State<'_, EnvironmentServiceState>,
) -> Option<EnvironmentCheck> {
    environment
        .latest
        .lock()
        .expect("environment state poisoned")
        .clone()
}

#[tauri::command(async)]
pub fn environment_detect(environment: State<'_, EnvironmentServiceState>) -> EnvironmentCheck {
    let check = detect_environment(&environment.data_directory);
    *environment
        .latest
        .lock()
        .expect("environment state poisoned") = Some(check.clone());
    check
}

#[tauri::command(async)]
pub fn model_state_get(
    models: State<'_, ModelServiceState>,
) -> Result<ModelRuntimeState, AppError> {
    let models = models.get()?;
    model_state_from_manager(&models)
}

#[derive(Debug, Deserialize)]
pub struct ModelImportScanRequest {
    paths: Vec<String>,
}

#[tauri::command(async)]
pub fn model_import_scan(
    request: ModelImportScanRequest,
    models: State<'_, ModelServiceState>,
) -> Result<Vec<ImportCandidate>, AppError> {
    models.get()?.scan_import_paths(&request.paths)
}

#[derive(Debug, Deserialize)]
pub struct ModelImportConfirmRequest {
    selections: Vec<ModelImportSelection>,
}

#[tauri::command(async)]
pub fn model_import_confirm(
    request: ModelImportConfirmRequest,
    models: State<'_, ModelServiceState>,
) -> Result<Vec<ModelArtifact>, AppError> {
    models.get()?.import_artifacts(&request.selections)
}

#[tauri::command(async)]
pub fn model_artifact_list(
    models: State<'_, ModelServiceState>,
) -> Result<Vec<ModelArtifact>, AppError> {
    models.get()?.list_artifacts()
}

#[tauri::command(async)]
pub fn model_catalog_list() -> Vec<ModelEdition> {
    remin_core::built_in_model_editions()
}

#[derive(Debug, Deserialize)]
pub struct ModelDownloadRequest {
    edition_id: String,
    source: String,
    confirmed: bool,
}

#[tauri::command]
pub async fn model_download_install(
    request: ModelDownloadRequest,
    app: AppHandle,
    models: State<'_, ModelServiceState>,
) -> Result<ModelArtifact, AppError> {
    if !request.confirmed {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONFIRMATION_REQUIRED",
            "联网下载模型需要用户明确确认",
            false,
        ));
    }
    let edition = remin_core::model_edition_by_id(&request.edition_id, &request.source)?;
    let manager = models.get()?;
    tauri::async_runtime::spawn_blocking(move || {
        download_and_install_model(&manager, &edition, &app)
    })
    .await
    .map_err(|error| AppError::new("MODEL_DOWNLOAD_TASK_FAILED", error.to_string(), true))?
}

fn download_and_install_model(
    models: &ModelManager,
    edition: &ModelEdition,
    app: &AppHandle,
) -> Result<ModelArtifact, AppError> {
    let artifact = &edition.artifact;
    let staging = models.download_staging_directory()?;
    let completed_path = staging.join(&artifact.file_name);
    let partial_path = staging.join(format!("{}.part", artifact.file_name));
    if completed_path.is_file()
        && models
            .verify_download(&completed_path, &artifact.sha256, artifact.size_bytes)
            .is_err()
    {
        fs::remove_file(&completed_path).map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_CLEANUP_FAILED", error.to_string(), true)
        })?;
    }
    if !completed_path.is_file() {
        if partial_path.exists()
            && fs::symlink_metadata(&partial_path)
                .map(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
                .unwrap_or(true)
        {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_INCOMPLETE",
                "模型断点文件不是普通文件",
                false,
            ));
        }
        let _ = app.emit(
            "model.download_started",
            json!({"edition_id": edition.edition_id, "model_id": artifact.model_id, "size_bytes": artifact.size_bytes}),
        );
        let mut command = Command::new("curl.exe");
        command
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--proto",
                "=https",
                "--tlsv1.2",
                "--connect-timeout",
                "30",
                "--retry",
                "3",
                "--retry-all-errors",
                "--continue-at",
                "-",
                "--user-agent",
                "Remin/0.1",
                "--output",
            ])
            .arg(&partial_path)
            .arg(&artifact.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        hide_process_window(&mut command);
        let mut child = command.spawn().map_err(|error| {
            AppError::new("MODEL_DOWNLOADER_UNAVAILABLE", error.to_string(), true)
        })?;
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| AppError::new("MODEL_DOWNLOAD_FAILED", error.to_string(), true))?
            {
                break status;
            }
            let downloaded_bytes = fs::metadata(&partial_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            let _ = app.emit(
                "model.download_progress",
                json!({
                    "edition_id": edition.edition_id,
                    "downloaded_bytes": downloaded_bytes,
                    "total_bytes": artifact.size_bytes,
                    "progress": if artifact.size_bytes == 0 { 0.0 } else { downloaded_bytes as f64 / artifact.size_bytes as f64 }
                }),
            );
            thread::sleep(std::time::Duration::from_millis(500));
        };
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        if !status.success() {
            let detail = stderr.chars().take(800).collect::<String>();
            return Err(AppError::new(
                "MODEL_DOWNLOAD_FAILED",
                if detail.trim().is_empty() {
                    format!("模型下载进程退出：{status}")
                } else {
                    detail
                },
                true,
            ));
        }
        fs::rename(&partial_path, &completed_path).map_err(|error| {
            AppError::new("MODEL_DOWNLOAD_FINALIZE_FAILED", error.to_string(), true)
        })?;
    }
    if let Err(error) =
        models.verify_download(&completed_path, &artifact.sha256, artifact.size_bytes)
    {
        let _ = fs::remove_file(&completed_path);
        return Err(error);
    }
    let installed = models.import_downloaded_artifact(
        &ModelImportSelection {
            source_path: completed_path.to_string_lossy().into_owned(),
            role: artifact.role,
        },
        &DownloadedModelMetadata {
            source: artifact.source,
            repository_id: artifact.repository_id.clone(),
            revision: artifact.revision.clone(),
            license_name: artifact.license_name.clone(),
        },
    )?;
    fs::remove_file(&completed_path)
        .map_err(|error| AppError::new("MODEL_DOWNLOAD_CLEANUP_FAILED", error.to_string(), true))?;
    let _ = app.emit(
        "model.download_completed",
        json!({"edition_id": edition.edition_id, "artifact_id": installed.artifact_id}),
    );
    Ok(installed)
}

#[cfg(windows)]
fn hide_process_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_process_window(_command: &mut Command) {}

#[derive(Debug, Deserialize)]
pub struct ModelArtifactActionRequest {
    artifact_id: String,
}

#[tauri::command(async)]
pub fn model_artifact_activate(
    request: ModelArtifactActionRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ModelRuntimeState, AppError> {
    let models = models.get()?;
    let catalog = catalog.get()?;
    let artifact_id = Uuid::parse_str(&request.artifact_id)
        .map_err(|error| AppError::new("MODEL_ACTIVATION_INVALID", error.to_string(), false))?;
    let artifact = models.artifact_by_id(&artifact_id)?;
    match (artifact.role, artifact.format) {
        (ModelRole::Embedding, ModelFormat::Onnx) => {
            let tokenizer_path = PathBuf::from(&artifact.local_path)
                .parent()
                .map(|parent| parent.join("tokenizer.json"))
                .ok_or_else(|| {
                    AppError::new("EMBEDDING_TOKENIZER_UNAVAILABLE", "模型目录无效", false)
                })?;
            let response = worker.client.encode_embeddings(&EmbeddingRequest {
                model_path: artifact.local_path.clone(),
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                texts: vec!["拾起散落的信息，连接过去的自己".into()],
                max_length: 128,
                threads: 2,
            })?;
            models.activate_artifact(&artifact_id, Some(response.dimension))?;
            spawn_embed_pending(app, catalog);
        }
        (ModelRole::Generation, ModelFormat::Gguf) => {
            let threads = std::thread::available_parallelism()
                .map(|value| value.get() as u32)
                .unwrap_or(2)
                .clamp(1, 8);
            generation
                .0
                .lock()
                .map_err(|_| {
                    AppError::new(
                        "GENERATION_RUNTIME_LOCK_FAILED",
                        "生成运行时状态已损坏",
                        true,
                    )
                })?
                .activate(&artifact.local_path, 4096, threads)?;
            models.activate_artifact(&artifact_id, None)?;
        }
        _ => {
            return Err(AppError::new(
                "MODEL_RUNTIME_UNSUPPORTED",
                "当前模型角色或格式尚未接入本地运行时",
                false,
            ));
        }
    }
    model_state_from_manager(&models)
}

fn model_state_from_manager(models: &ModelManager) -> Result<ModelRuntimeState, AppError> {
    let artifacts = models.list_artifacts()?;
    let capabilities = ModelCapabilities {
        generation: models.active_artifact(ModelRole::Generation)?.is_some(),
        embedding: models.active_artifact(ModelRole::Embedding)?.is_some(),
        reranker: models.active_artifact(ModelRole::Reranker)?.is_some(),
        ocr: models.active_artifact(ModelRole::Ocr)?.is_some(),
    };
    let any_active = capabilities.generation
        || capabilities.embedding
        || capabilities.reranker
        || capabilities.ocr;
    Ok(ModelRuntimeState {
        status: if any_active {
            "ready"
        } else if artifacts.is_empty() {
            "unconfigured"
        } else {
            "unavailable"
        },
        runtime_mode: "basic",
        active_profile_id: None,
        active_profile_name: None,
        runtime_backend: if any_active { Some("cpu") } else { None },
        message: if capabilities.generation {
            "本地生成模型已配置，将在提问时按需加载"
        } else if capabilities.embedding {
            "语义检索已就绪，问资料仍需生成模型"
        } else if artifacts.is_empty() {
            "未配置本地模型"
        } else {
            "模型已导入，等待运行自检"
        },
        checked_at: Utc::now().to_rfc3339(),
        capabilities,
    })
}

fn privacy_safe_display_path(path: &str) -> String {
    let normalized = path.replace('/', "\\");
    let absolute = normalized.as_bytes().get(1) == Some(&b':') || normalized.starts_with("\\\\");
    if !absolute {
        return normalized;
    }
    let parts = normalized
        .split('\\')
        .filter(|part| !part.is_empty() && !part.ends_with(':'))
        .collect::<Vec<_>>();
    let start = parts.len().saturating_sub(3);
    let visible = parts[start..].join("\\");
    if start > 0 {
        format!("…\\{visible}")
    } else {
        visible
    }
}

#[derive(Debug, Deserialize)]
pub struct DateRequest {
    local_date: String,
}

#[tauri::command(async)]
pub fn home_get_summary(
    request: DateRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<Value, AppError> {
    let catalog = catalog.get()?;
    let (today_added, recent) = catalog.home_file_summary(&request.local_date)?;
    let candidates = catalog.discover_candidate_roots()?;
    let active_scan = catalog.latest_active_scan_job()?;
    let new_inbox = catalog.query_inbox(&InboxQuery {
        status: TriageStatus::New,
        event_types: vec![],
        root_ids: vec![],
        date_from: None,
        date_to: None,
        cursor: None,
        page_size: 200,
    })?;
    let error_inbox = catalog.query_inbox(&InboxQuery {
        status: TriageStatus::Error,
        event_types: vec![],
        root_ids: vec![],
        date_from: None,
        date_to: None,
        cursor: None,
        page_size: 200,
    })?;
    let relations = catalog.list_file_relations(500)?;
    let collections = catalog.list_collections()?;
    let index_stats = catalog.index_activity_stats()?;
    let failed = error_inbox.items.len();
    let awaiting_confirmation = new_inbox.items.len();
    let possible_duplicates = relations
        .iter()
        .filter(|relation| relation.relation_type == RelationType::ExactDuplicate)
        .count();
    let recent_files = recent
        .iter()
        .map(|file| {
            json!({
                "file_id": file.file_id,
                "name": file.display_name,
                "extension": file.extension,
                "subtitle": privacy_safe_display_path(&file.canonical_path),
                "modified_at": file.fs_modified_at,
            })
        })
        .collect::<Vec<_>>();
    let scan_progress = active_scan.map(|job| {
        json!({
            "scan_job_id": job.job_id,
            "status": job.status,
            "discovered_files": index_stats.discovered_files,
            "searchable_files": index_stats.searchable_files,
            "parsed_files": index_stats.parsed_files,
            "embedded_files": index_stats.embedded_files,
            "ocr_pages": index_stats.ocr_pages,
            "progress": job.progress,
        })
    });
    Ok(json!({
        "local_date": request.local_date,
        "metrics": [
            { "key": "today_added", "label": "今日新增", "value": today_added },
            { "key": "awaiting_confirmation", "label": "待确认", "value": awaiting_confirmation },
            { "key": "possible_duplicates", "label": "可能重复", "value": possible_duplicates },
            { "key": "processing_failed", "label": "处理失败", "value": failed }
        ],
        "scan_progress": scan_progress,
        "recent_files": recent_files,
        "favorite_files": [],
        "collections": collections.into_iter().take(6).enumerate().map(|(index, collection)| {
            let tone = ["purple", "green", "pink", "blue"][index % 4];
            json!({
                "collection_id": collection.collection_id,
                "name": collection.name,
                "item_count": collection.file_count,
                "tone": tone
            })
        }).collect::<Vec<_>>(),
        "candidate_roots": candidates
    }))
}

#[derive(Debug, Deserialize)]
pub struct CandidateActionRequest {
    candidate_id: String,
    action: String,
}

#[tauri::command(async)]
pub fn candidate_root_action(
    request: CandidateActionRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    watcher: State<'_, WatcherServiceState>,
) -> Result<CandidateRoot, AppError> {
    let catalog = catalog.get()?;
    let candidate_id = Uuid::parse_str(&request.candidate_id)
        .map_err(|error| AppError::new("CANDIDATE_ID_INVALID", error.to_string(), false))?;
    let outcome = catalog.candidate_root_action(&candidate_id, &request.action)?;
    if let Some(root) = &outcome.root
        && let Err(error) = watcher
            .with_mut(|watcher| watcher.watch_root(root))
            .and_then(|result| result)
    {
        let _ = app.emit("catalog.watch_degraded", &error);
    }
    Ok(outcome.candidate)
}

#[tauri::command(async)]
pub fn root_discover_defaults(
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    watcher: State<'_, WatcherServiceState>,
) -> Result<RootDiscoveryResult, AppError> {
    let catalog = catalog.get()?;
    let result = catalog.discover_default_roots();
    for root in &result.roots {
        if let Err(error) = watcher
            .with_mut(|watcher| watcher.watch_root(root))
            .and_then(|result| result)
        {
            let _ = app.emit("catalog.watch_degraded", &error);
        }
        if root.last_scan_at.is_none()
            && let Ok(prepared) = catalog.prepare_scan(&root.root_id, "first_launch")
            && prepared.should_start
        {
            spawn_scan(
                app.clone(),
                Arc::clone(&catalog),
                root.root_id,
                prepared.job.job_id,
            );
        }
    }
    Ok(result)
}

#[tauri::command(async)]
pub fn root_list(catalog: State<'_, CatalogServiceState>) -> Result<Vec<RootRecord>, AppError> {
    catalog.get()?.list_roots()
}

#[tauri::command(async)]
pub fn root_add(
    request: AddRootRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    watcher: State<'_, WatcherServiceState>,
) -> Result<RootRecord, AppError> {
    let root = catalog.get()?.add_root(request)?;
    if let Err(error) = watcher
        .with_mut(|watcher| watcher.watch_root(&root))
        .and_then(|result| result)
    {
        let _ = app.emit("catalog.watch_degraded", &error);
    }
    Ok(root)
}

#[derive(Debug, Deserialize)]
pub struct RootDisableRequest {
    root_id: String,
}

#[tauri::command(async)]
pub fn root_disable(
    request: RootDisableRequest,
    catalog: State<'_, CatalogServiceState>,
    watcher: State<'_, WatcherServiceState>,
) -> Result<(), AppError> {
    let root_id = Uuid::parse_str(&request.root_id)
        .map_err(|error| AppError::new("ROOT_ID_INVALID", error.to_string(), false))?;
    catalog.get()?.disable_root(&root_id)?;
    watcher.with_mut(|watcher| watcher.unwatch_root(&root_id))?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ScanStartRequest {
    root_id: String,
    reason: String,
}

#[tauri::command(async)]
pub fn scan_start(
    request: ScanStartRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
) -> Result<JobRecord, AppError> {
    let root_id = Uuid::parse_str(&request.root_id)
        .map_err(|error| AppError::new("ROOT_ID_INVALID", error.to_string(), false))?;
    let catalog = catalog.get()?;
    let prepared = catalog.prepare_scan(&root_id, &request.reason)?;
    if prepared.should_start {
        spawn_scan(app, catalog, root_id, prepared.job.job_id);
    }
    Ok(prepared.job)
}

#[derive(Debug, Deserialize)]
pub struct ScanControlRequest {
    job_id: String,
}

fn parse_job_id(request: &ScanControlRequest) -> Result<Uuid, AppError> {
    Uuid::parse_str(&request.job_id)
        .map_err(|error| AppError::new("JOB_ID_INVALID", error.to_string(), false))
}

#[tauri::command(async)]
pub fn scan_pause(
    request: ScanControlRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<JobRecord, AppError> {
    catalog.get()?.pause_scan(&parse_job_id(&request)?)
}

#[tauri::command(async)]
pub fn scan_resume(
    request: ScanControlRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<JobRecord, AppError> {
    catalog.get()?.resume_scan(&parse_job_id(&request)?)
}

#[tauri::command(async)]
pub fn scan_cancel(
    request: ScanControlRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<JobRecord, AppError> {
    catalog.get()?.cancel_scan(&parse_job_id(&request)?)
}

#[tauri::command(async)]
pub fn inbox_query(
    request: InboxQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<InboxPage, AppError> {
    catalog.get()?.query_inbox(&request)
}

#[tauri::command(async)]
pub fn inbox_update(
    request: InboxUpdateRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<InboxItem, AppError> {
    catalog.get()?.update_inbox_item(&request)
}

#[tauri::command(async)]
pub fn ocr_retry(
    request: FileIdRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let file_id = parse_file_id(&request)?;
    let catalog = catalog.get()?;
    catalog.retry_ocr(&file_id)?;
    spawn_parse_pending(app, catalog);
    Ok(true)
}

#[tauri::command(async)]
pub fn collection_list(
    catalog: State<'_, CatalogServiceState>,
) -> Result<Vec<CollectionRecord>, AppError> {
    catalog.get()?.list_collections()
}

#[tauri::command(async)]
pub fn collection_create(
    request: CreateCollectionRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<CollectionRecord, AppError> {
    catalog.get()?.create_collection(&request)
}

#[derive(Debug, Deserialize)]
pub struct CollectionUpdateRequest {
    collection_id: String,
    collection: CreateCollectionRequest,
}

#[tauri::command(async)]
pub fn collection_update(
    request: CollectionUpdateRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<CollectionRecord, AppError> {
    let collection_id = Uuid::parse_str(&request.collection_id)
        .map_err(|error| AppError::new("COLLECTION_REQUEST_INVALID", error.to_string(), false))?;
    catalog
        .get()?
        .update_collection(&collection_id, &request.collection)
}

#[derive(Debug, Deserialize)]
pub struct CollectionRulePreviewRequest {
    rule: CollectionRule,
    limit: u32,
}

#[tauri::command(async)]
pub fn collection_rule_preview(
    request: CollectionRulePreviewRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<Vec<FileRecord>, AppError> {
    catalog
        .get()?
        .preview_collection_rule(&request.rule, request.limit)
}

#[derive(Debug, Deserialize)]
pub struct CollectionIdRequest {
    collection_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CollectionFileQueryRequest {
    collection_id: String,
    cursor: Option<String>,
    page_size: u32,
}

#[tauri::command(async)]
pub fn collection_file_query(
    request: CollectionFileQueryRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<FilePage, AppError> {
    let collection_id = Uuid::parse_str(&request.collection_id)
        .map_err(|error| AppError::new("COLLECTION_REQUEST_INVALID", error.to_string(), false))?;
    catalog.get()?.query_collection_files(
        &collection_id,
        &FileQuery {
            cursor: request.cursor,
            page_size: request.page_size,
        },
    )
}

#[tauri::command(async)]
pub fn collection_delete(
    request: CollectionIdRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    let collection_id = Uuid::parse_str(&request.collection_id)
        .map_err(|error| AppError::new("COLLECTION_REQUEST_INVALID", error.to_string(), false))?;
    catalog.get()?.delete_collection(&collection_id)
}

#[derive(Debug, Deserialize)]
pub struct CollectionMembershipRequest {
    collection_id: String,
    file_id: String,
}

#[tauri::command(async)]
pub fn collection_add_file(
    request: CollectionMembershipRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    let collection_id = Uuid::parse_str(&request.collection_id)
        .map_err(|error| AppError::new("COLLECTION_REQUEST_INVALID", error.to_string(), false))?;
    let file_id = Uuid::parse_str(&request.file_id)
        .map_err(|error| AppError::new("FILE_ID_INVALID", error.to_string(), false))?;
    catalog
        .get()?
        .add_file_to_collection(&collection_id, &file_id)
}

#[tauri::command(async)]
pub fn collection_remove_file(
    request: CollectionMembershipRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    let collection_id = Uuid::parse_str(&request.collection_id)
        .map_err(|error| AppError::new("COLLECTION_REQUEST_INVALID", error.to_string(), false))?;
    let file_id = Uuid::parse_str(&request.file_id)
        .map_err(|error| AppError::new("FILE_ID_INVALID", error.to_string(), false))?;
    catalog
        .get()?
        .remove_file_from_collection(&collection_id, &file_id)
}

#[derive(Debug, Deserialize)]
pub struct RelationRefreshRequest {
    max_files: u32,
}

#[tauri::command(async)]
pub fn relation_refresh(
    request: RelationRefreshRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<RelationRefreshResult, AppError> {
    catalog.get()?.refresh_file_relations(request.max_files)
}

#[tauri::command(async)]
pub fn relation_query(
    request: RelationQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<RelationPage, AppError> {
    catalog.get()?.query_file_relations(&request)
}

#[derive(Debug, Deserialize)]
pub struct RelationReviewRequest {
    relation_id: Uuid,
    action: String,
}

#[tauri::command(async)]
pub fn relation_review(
    request: RelationReviewRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    catalog
        .get()?
        .review_file_relation(&request.relation_id, &request.action)
}

#[tauri::command(async)]
pub fn file_query(
    request: FileQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<FilePage, AppError> {
    catalog.get()?.query_files(&request)
}

#[tauri::command(async)]
pub fn extraction_preset_list() -> Vec<ExtractionPreset> {
    remin_core::extraction_presets()
}

#[tauri::command(async)]
pub fn extraction_run(
    request: ExtractionRunRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<ExtractionRunResult, AppError> {
    catalog.get()?.run_extraction(&request)
}

#[tauri::command(async)]
pub fn skill_list() -> Vec<SkillDefinition> {
    remin_core::registered_skills()
}

#[tauri::command(async)]
pub fn task_plan(request: PlanSkillRequest) -> Result<TaskPlan, AppError> {
    remin_core::plan_skill(&request)
}

#[tauri::command(async)]
pub fn task_execute(
    request: PlanSkillRequest,
    catalog: State<'_, CatalogServiceState>,
    worker: State<'_, WorkerServiceState>,
) -> Result<TaskExecutionResult, AppError> {
    if request.skill_id == "rerun_ocr" {
        let files = catalog.get()?.authorized_files_by_ids(&request.file_ids)?;
        let supported = ["pdf", "jpg", "jpeg", "png", "tif", "tiff", "bmp", "webp"];
        let mut parsed = Vec::with_capacity(files.len());
        for file in &files {
            if !supported.contains(&file.extension.to_ascii_lowercase().as_str()) {
                return Err(AppError::new(
                    "OCR_FORMAT_UNSUPPORTED",
                    format!("{}不是可重新OCR的图片或PDF", file.display_name),
                    false,
                ));
            }
            let revision_id = file
                .current_revision_id
                .ok_or_else(|| AppError::new("OCR_FILE_NOT_INDEXED", "文件缺少当前修订", true))?;
            let parse_request = ParseRequest {
                job_id: Uuid::now_v7(),
                file_id: file.file_id,
                revision_id,
                source_path: file.canonical_path.clone(),
                format: file.extension.clone(),
                ocr_policy: "force".to_owned(),
                language_hints: vec!["zh".to_owned(), "en".to_owned()],
                max_pages: None,
                parser_version: "0.1.0".to_owned(),
            };
            let result = worker.client.parse_document(&parse_request)?;
            if matches!(
                result.status,
                ParseOutcome::Failed | ParseOutcome::Unsupported | ParseOutcome::Encrypted
            ) || result.metrics.ocr_page_count == 0
            {
                return Err(result.error.clone().unwrap_or_else(|| {
                    AppError::new(
                        "OCR_RERUN_EMPTY",
                        format!("{}没有产生可验证的OCR结果，原索引已保留", file.display_name),
                        true,
                    )
                }));
            }
            parsed.push((file.file_id, result));
        }
        for (file_id, result) in &parsed {
            catalog.get()?.commit_parse_result(file_id, result)?;
        }
    }
    catalog.get()?.execute_task(&request)
}

#[tauri::command(async)]
pub fn task_recoverable(
    catalog: State<'_, CatalogServiceState>,
) -> Result<Option<TaskPlan>, AppError> {
    catalog.get()?.latest_recoverable_task_plan()
}

#[derive(Debug, Deserialize)]
pub struct TaskResumeRequest {
    task_id: String,
}

#[tauri::command(async)]
pub fn task_resume(
    request: TaskResumeRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<TaskExecutionResult, AppError> {
    let task_id = Uuid::parse_str(&request.task_id)
        .map_err(|_| AppError::new("TASK_ID_INVALID", "任务标识无效", false))?;
    catalog.get()?.resume_task_execution(&task_id)
}

#[derive(Debug, Deserialize)]
pub struct ExtractionExportRequest {
    run: ExtractionRunResult,
    format: String,
    target_path: String,
}

#[tauri::command(async)]
pub fn extraction_export(
    request: ExtractionExportRequest,
    worker: State<'_, WorkerServiceState>,
) -> Result<ExportResult, AppError> {
    let mut headers = vec!["文件名".to_owned(), "资料路径".to_owned()];
    headers.extend(
        request
            .run
            .preset
            .fields
            .iter()
            .map(|field| field.label.clone()),
    );
    let rows = request
        .run
        .rows
        .iter()
        .map(|row| {
            let mut values = vec![
                json!(row.file.display_name),
                json!(privacy_safe_display_path(&row.file.canonical_path)),
            ];
            for field in &request.run.preset.fields {
                values.push(
                    row.values
                        .iter()
                        .find(|value| value.field_key == field.key)
                        .map(|value| value.normalized_value.clone())
                        .unwrap_or(Value::Null),
                );
            }
            values
        })
        .collect();
    worker.client.export_table(&ExportTableRequest {
        target_path: request.target_path,
        format: request.format,
        headers,
        rows,
    })
}

#[tauri::command(async)]
pub fn maintenance_get(
    catalog: State<'_, CatalogServiceState>,
    environment: State<'_, EnvironmentServiceState>,
) -> Result<MaintenanceSnapshot, AppError> {
    if let Some(check) = environment
        .latest
        .lock()
        .expect("environment state poisoned")
        .clone()
        && let Some((level, triggers)) = environment_degradation(&check)
    {
        catalog
            .get()?
            .reconcile_degradation_state(level, triggers)?;
    }
    catalog.get()?.maintenance_snapshot()
}

#[derive(Debug, Deserialize)]
pub struct MaintenanceCheckRequest {
    level: String,
}

#[tauri::command(async)]
pub fn maintenance_check(
    request: MaintenanceCheckRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<remin_core::MaintenanceCheckResult, AppError> {
    catalog.get()?.maintenance_check(&request.level)
}

fn environment_degradation(check: &EnvironmentCheck) -> Option<(DegradationLevel, Vec<String>)> {
    let severe = check.memory_total_gb.is_some_and(|value| value < 4)
        || check.disk_available_gb.is_some_and(|value| value < 2);
    if severe {
        return Some((
            DegradationLevel::Core,
            vec!["资源: 内存或磁盘空间低于核心处理阈值".to_owned()],
        ));
    }
    if check.status != "ready" {
        return Some((
            DegradationLevel::Balanced,
            vec![format!(
                "资源: {}",
                check
                    .warnings
                    .first()
                    .map(String::as_str)
                    .unwrap_or("环境信息不完整，暂用均衡模式")
            )],
        ));
    }
    None
}

#[tauri::command(async)]
pub fn maintenance_log_query(
    request: LogQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<LogPage, AppError> {
    catalog.get()?.query_logs(&request)
}

#[tauri::command(async)]
pub fn maintenance_logs_clear(catalog: State<'_, CatalogServiceState>) -> Result<u64, AppError> {
    catalog.get()?.clear_logs()
}

#[derive(Debug, Deserialize)]
pub struct DiagnosticExportRequest {
    target_path: String,
    confirmed: bool,
}

#[tauri::command(async)]
pub fn diagnostic_export(
    request: DiagnosticExportRequest,
    catalog: State<'_, CatalogServiceState>,
    environment: State<'_, EnvironmentServiceState>,
    worker: State<'_, WorkerServiceState>,
) -> Result<ExportResult, AppError> {
    if !request.confirmed {
        return Err(AppError::new(
            "EXPORT_CONFIRMATION_REQUIRED",
            "导出诊断包前需要明确确认",
            false,
        ));
    }
    let catalog = catalog.get()?;
    let snapshot = catalog.maintenance_snapshot()?;
    let logs = catalog.list_logs(200)?;
    let latest_environment = environment
        .latest
        .lock()
        .map_err(|_| AppError::new("ENVIRONMENT_STATE_UNAVAILABLE", "环境状态不可用", true))?
        .clone();
    worker.client.export_table(&ExportTableRequest {
        target_path: request.target_path,
        format: "json".to_owned(),
        headers: vec![
            "generated_at".to_owned(),
            "app_version".to_owned(),
            "maintenance".to_owned(),
            "environment".to_owned(),
            "recent_logs".to_owned(),
        ],
        rows: vec![vec![
            json!(Utc::now().to_rfc3339()),
            json!(env!("CARGO_PKG_VERSION")),
            serde_json::to_value(snapshot).map_err(|error| {
                AppError::new("DIAGNOSTIC_SERIALIZE_FAILED", error.to_string(), false)
            })?,
            serde_json::to_value(latest_environment).map_err(|error| {
                AppError::new("DIAGNOSTIC_SERIALIZE_FAILED", error.to_string(), false)
            })?,
            serde_json::to_value(logs).map_err(|error| {
                AppError::new("DIAGNOSTIC_SERIALIZE_FAILED", error.to_string(), false)
            })?,
        ]],
    })
}

#[derive(Debug, Deserialize)]
pub struct IndexRebuildRequest {
    confirmation: String,
}

#[tauri::command(async)]
pub fn index_rebuild(
    request: IndexRebuildRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
) -> Result<IndexRebuildResult, AppError> {
    let catalog = catalog.get()?;
    let result = catalog.rebuild_index(&request.confirmation)?;
    spawn_parse_pending(app, catalog);
    Ok(result)
}

#[tauri::command(async)]
pub fn search_start(
    request: SearchRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    worker: State<'_, WorkerServiceState>,
) -> Result<SearchSession, AppError> {
    let catalog = catalog.get()?;
    let models = models.get()?;
    if matches!(request.mode, SearchMode::Semantic | SearchMode::Hybrid)
        && let Some(artifact) = models.active_artifact(ModelRole::Embedding)?
    {
        let tokenizer_path = PathBuf::from(&artifact.local_path)
            .parent()
            .map(|parent| parent.join("tokenizer.json"));
        if let Some(tokenizer_path) = tokenizer_path
            && let Ok(response) = worker.client.encode_embeddings(&EmbeddingRequest {
                model_path: artifact.local_path,
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                texts: vec![request.query.clone()],
                max_length: 512,
                threads: 2,
            })
            && let Some(vector) = response.vectors.first()
        {
            return catalog.search_with_semantic(
                &request,
                Some(SemanticQuery {
                    model_artifact_id: &artifact.artifact_id.to_string(),
                    vector,
                }),
            );
        }
    }
    catalog.search(&request)
}

#[tauri::command(async)]
pub fn ask_start(
    request: AskRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<OperationHandle, AppError> {
    let catalog = catalog.get()?;
    let models = models.get()?;
    let operation_id = Uuid::now_v7();
    let cancelled = Arc::new(AtomicBool::new(false));
    let ask_worker = worker.client.isolated();
    let handle = OperationHandle {
        operation_id,
        kind: "ask",
        status: "queued",
        created_at: Utc::now().to_rfc3339(),
    };
    let operations = app.state::<AskCoordinatorState>().inner().clone();
    let mut entries = operations
        .0
        .lock()
        .map_err(|_| AppError::new("OPERATION_NOT_FOUND", "问答操作状态不可用", true))?;
    if entries.len() >= 128 {
        let mut finished = entries
            .iter()
            .filter(|(_, entry)| {
                matches!(entry.handle.status, "completed" | "failed" | "cancelled")
            })
            .map(|(id, entry)| (*id, entry.handle.created_at.clone()))
            .collect::<Vec<_>>();
        finished.sort_by(|left, right| left.1.cmp(&right.1));
        let remove_count = entries.len().saturating_sub(96);
        for (id, _) in finished.into_iter().take(remove_count) {
            entries.remove(&id);
        }
    }
    if entries.len() >= 128 {
        return Err(AppError::new(
            "OPERATION_QUEUE_FULL",
            "正在进行的问答过多，请取消旧操作后重试",
            true,
        ));
    }
    entries.insert(
        operation_id,
        AskOperationEntry {
            handle: handle.clone(),
            result: None,
            error: None,
            cancelled: Arc::clone(&cancelled),
            worker: ask_worker.clone(),
        },
    );
    drop(entries);
    let worker = ask_worker;
    let generation = Arc::clone(&generation.0);
    tauri::async_runtime::spawn_blocking(move || {
        if let Ok(mut entries) = operations.0.lock()
            && let Some(entry) = entries.get_mut(&operation_id)
        {
            entry.handle.status = "running";
        }
        let result = compute_answer(
            &request,
            &catalog,
            &models,
            &worker,
            &generation,
            &cancelled,
        );
        if cancelled.load(Ordering::Acquire) {
            let error = AppError::new("OPERATION_CANCELLED", "问答已取消", false);
            if let Ok(mut entries) = operations.0.lock()
                && let Some(entry) = entries.get_mut(&operation_id)
            {
                entry.handle.status = "cancelled";
                entry.error = Some(error.clone());
            }
            let _ = app.emit(
                "ask.cancelled",
                json!({"operation_id": operation_id, "error": error}),
            );
            return;
        }
        match result {
            Ok(answer) => {
                for claim in &answer.claims {
                    for citation in &claim.citations {
                        let _ = app.emit(
                            "ask.citation",
                            json!({"operation_id": operation_id, "claim_id": claim.claim_id, "citation": citation}),
                        );
                    }
                    let _ = app.emit(
                        "ask.claim",
                        json!({"operation_id": operation_id, "claim": claim}),
                    );
                }
                let characters = answer.answer.chars().collect::<Vec<_>>();
                for chunk in characters.chunks(24) {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let token = chunk.iter().collect::<String>();
                    let _ = app.emit(
                        "ask.token",
                        json!({"operation_id": operation_id, "token": token}),
                    );
                    std::thread::sleep(Duration::from_millis(8));
                }
                if cancelled.load(Ordering::Acquire) {
                    let error = AppError::new("OPERATION_CANCELLED", "问答已取消", false);
                    if let Ok(mut entries) = operations.0.lock()
                        && let Some(entry) = entries.get_mut(&operation_id)
                    {
                        entry.handle.status = "cancelled";
                        entry.error = Some(error.clone());
                    }
                    let _ = app.emit(
                        "ask.cancelled",
                        json!({"operation_id": operation_id, "error": error}),
                    );
                    return;
                }
                if let Ok(mut entries) = operations.0.lock()
                    && let Some(entry) = entries.get_mut(&operation_id)
                {
                    entry.handle.status = "completed";
                    entry.result = Some(answer.clone());
                }
                let _ = app.emit(
                    "ask.completed",
                    json!({"operation_id": operation_id, "result": answer}),
                );
            }
            Err(error) => {
                if let Ok(mut entries) = operations.0.lock()
                    && let Some(entry) = entries.get_mut(&operation_id)
                {
                    entry.handle.status = "failed";
                    entry.error = Some(error.clone());
                }
                let _ = app.emit(
                    "ask.failed",
                    json!({"operation_id": operation_id, "error": error}),
                );
            }
        }
    });
    Ok(handle)
}

fn compute_answer(
    request: &AskRequest,
    catalog: &CatalogService,
    models: &ModelManager,
    worker: &WorkerClient,
    generation: &Mutex<LocalGenerationRuntime>,
    cancelled: &AtomicBool,
) -> Result<AnswerResult, AppError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
    }
    let mut semantic_result = None;
    if let Some(artifact) = models.active_artifact(ModelRole::Embedding)? {
        let tokenizer_path = PathBuf::from(&artifact.local_path)
            .parent()
            .map(|parent| parent.join("tokenizer.json"));
        if let Some(tokenizer_path) = tokenizer_path
            && let Ok(response) = worker.encode_embeddings(&EmbeddingRequest {
                model_path: artifact.local_path,
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                texts: vec![request.question.clone()],
                max_length: 512,
                threads: 2,
            })
            && let Some(vector) = response.vectors.first()
        {
            let artifact_id = artifact.artifact_id.to_string();
            semantic_result = Some(catalog.answer_extractively(
                request,
                Some(SemanticQuery {
                    model_artifact_id: &artifact_id,
                    vector,
                }),
            )?);
        }
    }
    let extractive = match semantic_result {
        Some(result) => result,
        None => catalog.answer_extractively(request, None)?,
    };
    if extractive.insufficient_evidence {
        return Ok(extractive);
    }
    let Some(artifact) = models.active_artifact(ModelRole::Generation)? else {
        return Ok(extractive);
    };
    let mut runtime = generation.lock().map_err(|_| {
        AppError::new(
            "GENERATION_RUNTIME_LOCK_FAILED",
            "生成运行时状态已损坏",
            true,
        )
    })?;
    let threads = std::thread::available_parallelism()
        .map(|value| value.get() as u32)
        .unwrap_or(2)
        .clamp(1, 8);
    if (runtime.active_model_path() != Some(artifact.local_path.as_str()) || !runtime.is_active())
        && runtime
            .activate(&artifact.local_path, 4096, threads)
            .is_err()
    {
        return Ok(extractive);
    }
    let prompt = remin_core::generation_prompt(request, &extractive);
    let generated = match runtime.complete_cancellable(
        "你是拾忆的本地资料回答器。只能使用用户提供的证据，严格保留[S数字]引用，不得补充外部知识。",
        &prompt,
        512,
        cancelled,
    ) {
        Ok(answer) => answer,
        Err(error) if error.code == "OPERATION_CANCELLED" => {
            runtime.stop();
            return Err(error);
        }
        Err(_) => return Ok(extractive),
    };
    Ok(remin_core::apply_grounded_generation(&extractive, &generated).unwrap_or(extractive))
}

#[derive(Debug, Deserialize)]
pub struct OperationRequest {
    operation_id: Uuid,
}

#[tauri::command(async)]
pub fn ask_operation_get(
    request: OperationRequest,
    operations: State<'_, AskCoordinatorState>,
) -> Result<AskOperationSnapshot, AppError> {
    let entries = operations
        .0
        .lock()
        .map_err(|_| AppError::new("OPERATION_NOT_FOUND", "问答操作状态不可用", true))?;
    let entry = entries
        .get(&request.operation_id)
        .ok_or_else(|| AppError::new("OPERATION_NOT_FOUND", "问答操作不存在或已经过期", false))?;
    Ok(AskOperationSnapshot {
        handle: entry.handle.clone(),
        result: entry.result.clone(),
        error: entry.error.clone(),
    })
}

#[tauri::command(async)]
pub fn ask_cancel(
    request: OperationRequest,
    operations: State<'_, AskCoordinatorState>,
) -> Result<AskOperationSnapshot, AppError> {
    let mut entries = operations
        .0
        .lock()
        .map_err(|_| AppError::new("OPERATION_NOT_FOUND", "问答操作状态不可用", true))?;
    let entry = entries
        .get_mut(&request.operation_id)
        .ok_or_else(|| AppError::new("OPERATION_NOT_FOUND", "问答操作不存在或已经过期", false))?;
    if matches!(entry.handle.status, "completed" | "failed" | "cancelled") {
        return Ok(AskOperationSnapshot {
            handle: entry.handle.clone(),
            result: entry.result.clone(),
            error: entry.error.clone(),
        });
    }
    entry.cancelled.store(true, Ordering::Release);
    entry.handle.status = "cancelled";
    entry.error = Some(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
    entry.worker.cancel_active();
    Ok(AskOperationSnapshot {
        handle: entry.handle.clone(),
        result: None,
        error: entry.error.clone(),
    })
}

#[derive(Debug, Deserialize)]
pub struct FileIdRequest {
    file_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PreviewRequest {
    file_id: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_preview_limit")]
    limit: usize,
    #[serde(default)]
    anchor_node_id: Option<String>,
}

fn default_preview_limit() -> usize {
    80
}

fn parse_file_id(request: &FileIdRequest) -> Result<Uuid, AppError> {
    Uuid::parse_str(&request.file_id)
        .map_err(|error| AppError::new("FILE_ID_INVALID", error.to_string(), false))
}

#[tauri::command(async)]
pub fn preview_get(
    request: PreviewRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<FilePreview, AppError> {
    let file_id = Uuid::parse_str(&request.file_id)
        .map_err(|_| AppError::new("FILE_ID_INVALID", "文件标识无效", false))?;
    let anchor_node_id = request
        .anchor_node_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| AppError::new("PREVIEW_ANCHOR_INVALID", "引用节点标识无效", false))?;
    catalog.get()?.file_preview_page(
        &file_id,
        request.offset,
        request.limit,
        anchor_node_id.as_ref(),
    )
}

#[tauri::command(async)]
pub fn file_open(
    request: FileIdRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    let path = catalog
        .get()?
        .authorized_file_path(&parse_file_id(&request)?)?;
    #[cfg(windows)]
    {
        let operation = wide_null("open");
        let file = wide_null(path.as_os_str());
        let result = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(operation.as_ptr()),
                PCWSTR(file.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as isize <= 32 {
            return Err(AppError::new(
                "FILE_OPEN_FAILED",
                "Windows无法使用默认程序打开此文件",
                true,
            ));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Err(AppError::new(
            "FILE_OPEN_FAILED",
            "当前平台不支持打开原文件",
            false,
        ))
    }
}

#[tauri::command(async)]
pub fn file_reveal(
    request: FileIdRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    let path = catalog
        .get()?
        .authorized_file_path(&parse_file_id(&request)?)?;
    Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .map_err(|error| AppError::new("FILE_REVEAL_FAILED", error.to_string(), true))?;
    Ok(())
}

#[cfg(windows)]
fn wide_null(value: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

pub(crate) fn spawn_scan(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    root_id: Uuid,
    job_id: Uuid,
) {
    thread::spawn(move || {
        run_scan(&app, &catalog, root_id, job_id);
    });
}

pub(crate) fn spawn_scan_queue(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    recovered: Vec<(Uuid, JobRecord)>,
) {
    if recovered.is_empty() {
        return;
    }
    thread::spawn(move || {
        for (root_id, job) in recovered {
            run_scan(&app, &catalog, root_id, job.job_id);
        }
    });
}

fn run_scan(app: &AppHandle, catalog: &Arc<CatalogService>, root_id: Uuid, job_id: Uuid) {
    match catalog.execute_scan(root_id, job_id) {
        Ok(job) => {
            let _ = app.emit("job.progress", &job);
            let _ = app.emit("catalog.changed", root_id.to_string());
            if matches!(
                job.status,
                remin_core::JobStatus::Succeeded | remin_core::JobStatus::Partial
            ) {
                spawn_parse_pending(app.clone(), Arc::clone(catalog));
            }
        }
        Err(error) => {
            let _ = app.emit("job.progress", &error);
        }
    }
}

pub(crate) fn spawn_parse_pending(app: AppHandle, catalog: Arc<CatalogService>) {
    thread::spawn(move || {
        struct RunningReset<'a>(&'a AtomicBool);
        impl Drop for RunningReset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let should_continue = {
            let worker = app.state::<WorkerServiceState>();
            if worker.running.swap(true, Ordering::AcqRel) {
                return;
            }
            let _running_reset = RunningReset(&worker.running);
            let degradation = catalog
                .maintenance_snapshot()
                .map(|snapshot| snapshot.degradation_level)
                .unwrap_or_else(|_| "balanced".to_owned());
            let batch_size = match degradation.as_str() {
                "core" => 0,
                "balanced" => 4,
                _ => 16,
            };
            if batch_size == 0 {
                false
            } else {
                let pending = match catalog.list_pending_parse_files(batch_size) {
                    Ok(files) => files,
                    Err(error) => {
                        let _ = app.emit("index.failed", error);
                        return;
                    }
                };
                for file in pending {
                    let Some(revision_id) = file.current_revision_id else {
                        continue;
                    };
                    if let Err(error) = catalog.mark_file_parsing(&file.file_id, &revision_id) {
                        let _ = app.emit("index.failed", error);
                        continue;
                    }
                    let request = ParseRequest {
                        job_id: Uuid::now_v7(),
                        file_id: file.file_id,
                        revision_id,
                        source_path: file.canonical_path.clone(),
                        format: file.extension.clone(),
                        ocr_policy: "auto".to_owned(),
                        language_hints: vec!["zh".to_owned()],
                        max_pages: None,
                        parser_version: "0.1.0".to_owned(),
                    };
                    let result = worker
                        .client
                        .parse_document(&request)
                        .unwrap_or_else(|error| ParseResult {
                            revision_id,
                            status: ParseOutcome::Failed,
                            parser_name: "none".to_owned(),
                            parser_version: request.parser_version.clone(),
                            nodes: vec![],
                            warnings: vec![],
                            metrics: ParseMetrics {
                                page_count: 0,
                                node_count: 0,
                                character_count: 0,
                                ocr_page_count: 0,
                                elapsed_ms: 0,
                            },
                            error: Some(error),
                        });
                    match catalog.commit_parse_result(&file.file_id, &result) {
                        Ok(()) => {
                            let _ = app.emit("index.changed", file.file_id.to_string());
                        }
                        Err(error) => {
                            let _ = app.emit("index.failed", error);
                        }
                    }
                }
                spawn_embed_pending(app.clone(), Arc::clone(&catalog));
                catalog
                    .list_pending_parse_files(1)
                    .is_ok_and(|files| !files.is_empty())
            }
        };
        if should_continue {
            thread::sleep(std::time::Duration::from_millis(250));
            spawn_parse_pending(app, catalog);
        }
    });
}

pub(crate) fn spawn_embed_pending(app: AppHandle, catalog: Arc<CatalogService>) {
    thread::spawn(move || {
        struct RunningReset<'a>(&'a AtomicBool);
        impl Drop for RunningReset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let should_continue = {
            let models = app.state::<ModelServiceState>();
            let models = match models.get() {
                Ok(models) => models,
                Err(_) => return,
            };
            let artifact = match models.active_artifact(ModelRole::Embedding) {
                Ok(Some(artifact)) => artifact,
                Ok(None) => return,
                Err(error) => {
                    let _ = app.emit("embedding.failed", error);
                    return;
                }
            };
            let Some(tokenizer_path) = PathBuf::from(&artifact.local_path)
                .parent()
                .map(|parent| parent.join("tokenizer.json"))
            else {
                return;
            };
            let worker = app.state::<WorkerServiceState>();
            if worker.embedding_running.swap(true, Ordering::AcqRel) {
                return;
            }
            let _running_reset = RunningReset(&worker.embedding_running);
            let model_artifact_id = artifact.artifact_id.to_string();
            let chunks = match catalog.list_pending_embedding_chunks(&model_artifact_id, 16) {
                Ok(chunks) if chunks.is_empty() => return,
                Ok(chunks) => chunks,
                Err(error) => {
                    let _ = app.emit("embedding.failed", error);
                    return;
                }
            };
            let response = match worker.client.encode_embeddings(&EmbeddingRequest {
                model_path: artifact.local_path.clone(),
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                texts: chunks.iter().map(|chunk| chunk.text.clone()).collect(),
                max_length: 512,
                threads: 2,
            }) {
                Ok(response) => response,
                Err(error) => {
                    let _ = app.emit("embedding.failed", error);
                    return;
                }
            };
            if response.vectors.len() != chunks.len() {
                let _ = app.emit(
                    "embedding.failed",
                    AppError::new(
                        "EMBEDDING_OUTPUT_INVALID",
                        "向量数量与输入分块数量不一致",
                        false,
                    ),
                );
                return;
            }
            let inputs = chunks
                .iter()
                .zip(response.vectors)
                .map(|(chunk, vector)| ChunkEmbeddingInput {
                    chunk_id: chunk.chunk_id,
                    vector,
                })
                .collect::<Vec<_>>();
            match catalog.commit_chunk_embeddings(&model_artifact_id, response.dimension, &inputs) {
                Ok(0) => false,
                Ok(committed) => {
                    let _ = app.emit("embedding.changed", committed);
                    catalog
                        .list_pending_embedding_chunks(&model_artifact_id, 1)
                        .is_ok_and(|chunks| !chunks.is_empty())
                }
                Err(error) => {
                    let _ = app.emit("embedding.failed", error);
                    false
                }
            }
        };
        if should_continue {
            thread::sleep(std::time::Duration::from_millis(250));
            spawn_embed_pending(app, catalog);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn environment_detection_reads_real_memory_and_disk() {
        let check = detect_environment(Path::new("."));

        assert!(check.memory_total_gb.is_some_and(|value| value > 0));
        assert!(check.disk_available_gb.is_some());
        assert!(matches!(check.status, "ready" | "degraded"));
        assert!(check.recommended_edition.is_some());
    }

    #[test]
    fn environment_pressure_selects_a_progressive_degradation_floor() {
        let balanced = EnvironmentCheck {
            status: "degraded",
            memory_total_gb: Some(6),
            disk_available_gb: Some(20),
            gpu_name: None,
            gpu_memory_gb: None,
            recommended_edition: Some("light"),
            runtime_backend: Some("cpu"),
            checked_at: Utc::now().to_rfc3339(),
            warnings: vec!["内存低于推荐值".to_owned()],
        };
        assert_eq!(
            environment_degradation(&balanced).map(|value| value.0),
            Some(DegradationLevel::Balanced)
        );

        let core = EnvironmentCheck {
            status: "degraded",
            memory_total_gb: Some(3),
            disk_available_gb: Some(1),
            ..balanced
        };
        assert_eq!(
            environment_degradation(&core).map(|value| value.0),
            Some(DegradationLevel::Core)
        );
    }
}
