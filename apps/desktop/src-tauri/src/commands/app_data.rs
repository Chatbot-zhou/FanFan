use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use remin_core::{
    AddRootRequest, AnswerResult, AppError, AskMode, AskRequest, CandidateRoot, CatalogService,
    ChunkEmbeddingInput, CollectionModelReview, CollectionRecord, CollectionRule,
    CollectionSuggestion, CollectionSuggestionPage, CollectionSuggestionQuery,
    CollectionSuggestionRefreshResult, CollectionSuggestionUpdateRequest, CreateCollectionRequest,
    DegradationLevel, DownloadFile, DownloadedModelMetadata, EmbeddingRequest, ExclusionRule,
    ExclusionRuleInput, ExportResult, ExportTableRequest, ExtractionPreset, ExtractionRunRequest,
    ExtractionRunResult, FilePage, FilePreview, FileQuery, FileRecord, ImageUnderstandingResult,
    ImportCandidate, InboxItem, InboxPage, InboxQuery, InboxUpdateRequest, IncrementalWatchManager,
    IndexRebuildResult, JobRecord, KnowledgeSpace, KnowledgeSpaceRequest, LocalGenerationRuntime,
    LogPage, LogQuery, MaintenanceSnapshot, ModelArtifact, ModelDownloadFileProgress,
    ModelDownloadJob, ModelEdition, ModelFormat, ModelImportSelection, ModelManager, ModelRole,
    ModelRoleConfig, ModelSource, ParseMetrics, ParseOutcome, ParseRequest, ParseResult,
    PendingEmbeddingActivation, PlanSkillRequest, RagReadiness, RelationPage, RelationQuery,
    RelationRefreshResult, RelationType, RerankRequest, RootRecord, ScopeFilter, SearchMode,
    SearchRequest, SearchSession, SemanticQuery, SkillDefinition, TaskExecutionResult, TaskPlan,
    TriageStatus, WorkerClient,
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

#[derive(Default)]
pub struct ScanCoordinatorState {
    queue: Mutex<VecDeque<(Uuid, Uuid)>>,
    running: AtomicBool,
}

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
    pub embedding_reschedule: AtomicBool,
    pub vision_running: AtomicBool,
}

pub struct GenerationServiceState(pub Arc<Mutex<LocalGenerationRuntime>>);

const DOWNLOAD_ACTION_RUN: u8 = 0;
const DOWNLOAD_ACTION_PAUSE: u8 = 1;
const DOWNLOAD_ACTION_CANCEL: u8 = 2;

#[derive(Clone, Default)]
pub struct ModelDownloadCoordinatorState {
    controls: Arc<Mutex<HashMap<Uuid, Arc<AtomicU8>>>>,
    running: Arc<Mutex<HashSet<Uuid>>>,
}

impl ModelDownloadCoordinatorState {
    pub fn pause_all(&self) {
        if let Ok(controls) = self.controls.lock() {
            for control in controls.values() {
                control.store(DOWNLOAD_ACTION_PAUSE, Ordering::Release);
            }
        }
    }

    fn begin(&self, job_id: Uuid) -> Result<Option<Arc<AtomicU8>>, AppError> {
        let mut running = self.running.lock().map_err(|_| {
            AppError::new(
                "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                "模型下载协调器暂时不可用",
                true,
            )
        })?;
        if !running.insert(job_id) {
            return Ok(None);
        }
        let control = Arc::new(AtomicU8::new(DOWNLOAD_ACTION_RUN));
        self.controls
            .lock()
            .map_err(|_| {
                AppError::new(
                    "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                    "模型下载协调器暂时不可用",
                    true,
                )
            })?
            .insert(job_id, Arc::clone(&control));
        Ok(Some(control))
    }

    fn set_action(&self, job_id: &Uuid, action: u8) -> Result<bool, AppError> {
        let controls = self.controls.lock().map_err(|_| {
            AppError::new(
                "MODEL_DOWNLOAD_STATE_UNAVAILABLE",
                "模型下载协调器暂时不可用",
                true,
            )
        })?;
        if let Some(control) = controls.get(job_id) {
            control.store(action, Ordering::Release);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn finish(&self, job_id: &Uuid) {
        if let Ok(mut controls) = self.controls.lock() {
            controls.remove(job_id);
        }
        if let Ok(mut running) = self.running.lock() {
            running.remove(job_id);
        }
    }
}

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
    active_profile_id: Option<String>,
    active_profile_name: Option<String>,
    runtime_backend: Option<&'static str>,
    message: String,
    checked_at: String,
    capabilities: ModelCapabilities,
    rag_complete: bool,
    semantic_index_coverage: f64,
    embedding_migration: Option<EmbeddingMigrationState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingMigrationState {
    artifact_id: String,
    status: String,
    error: Option<AppError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelCapabilities {
    generation: bool,
    embedding: bool,
    vision: bool,
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
    disk_space_bytes(path).map(|(_, available)| available / 1024 / 1024 / 1024)
}

#[cfg(windows)]
fn disk_space_bytes(path: &Path) -> Option<(u64, u64)> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut available = 0_u64;
    let mut total = 0_u64;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut available),
            Some(&mut total),
            None,
        )
    }
    .ok()
    .map(|_| (total, available))
}

#[cfg(not(windows))]
fn disk_available_gb(_path: &Path) -> Option<u64> {
    None
}

#[cfg(not(windows))]
fn disk_space_bytes(_path: &Path) -> Option<(u64, u64)> {
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
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
) -> Result<ModelRuntimeState, AppError> {
    let models = models.get()?;
    let catalog = catalog.get()?;
    model_state_from_manager(&models, Some(&catalog))
}

#[derive(Debug, Deserialize)]
pub struct RagReadinessRequest {
    scope: ScopeFilter,
}

#[tauri::command(async)]
pub fn rag_readiness_get(
    request: RagReadinessRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
) -> Result<RagReadiness, AppError> {
    let models = models.get()?;
    let catalog = catalog.get()?;
    let snapshot = catalog.maintenance_snapshot()?;
    let generation_ready = models.active_artifact(ModelRole::Generation)?.is_some();
    let embedding = models.active_artifact(ModelRole::Embedding)?;
    let embedding_ready = embedding.is_some();
    let vision_ready = models.active_artifact(ModelRole::Vision)?.is_some();
    let (image_total, image_ready, pending_image_assets) = catalog.image_understanding_stats()?;
    let image_index_coverage = if image_total == 0 {
        1.0
    } else {
        (image_ready as f64 / image_total as f64).clamp(0.0, 1.0)
    };
    let (coverage, scope_coverage) = match embedding.as_ref() {
        Some(artifact) => {
            catalog.semantic_index_coverage(&request.scope, &artifact.artifact_id.to_string())?
        }
        None => (0.0, 0.0),
    };
    let mut blockers = Vec::new();
    if !generation_ready {
        blockers.push(AppError::new(
            "RAG_GENERATION_MISSING",
            "未配置已通过自检的本地生成模型",
            true,
        ));
    }
    if !embedding_ready {
        blockers.push(AppError::new(
            "RAG_EMBEDDING_MISSING",
            "未配置已通过自检的中文 Embedding 模型",
            true,
        ));
    }
    if scope_coverage <= 0.0 {
        blockers.push(AppError::new(
            "RAG_INDEX_EMPTY",
            "当前资料尚未建立语义索引",
            true,
        ));
    }
    if snapshot.degradation_level == "core" {
        blockers.push(AppError::new(
            "RAG_CORE_MODE",
            "后台资源繁忙，生成任务暂时暂停；搜索和预览仍可继续使用",
            true,
        ));
    }
    Ok(RagReadiness {
        ready: blockers.is_empty(),
        generation_ready,
        embedding_ready,
        vision_ready,
        semantic_index_coverage: coverage,
        scope_index_coverage: scope_coverage,
        image_index_coverage,
        pending_image_assets,
        degradation_level: snapshot.degradation_level,
        background_notice: snapshot.background_notice,
        blockers,
        checked_at: Utc::now(),
    })
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
pub fn model_role_config_list(
    models: State<'_, ModelServiceState>,
) -> Result<Vec<ModelRoleConfig>, AppError> {
    models.get()?.role_configs()
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
pub async fn model_download_start(
    request: ModelDownloadRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    downloads: State<'_, ModelDownloadCoordinatorState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ModelDownloadJob, AppError> {
    if !request.confirmed {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONFIRMATION_REQUIRED",
            "联网下载模型需要用户明确确认",
            false,
        ));
    }
    let edition = remin_core::model_edition_by_id(&request.edition_id, &request.source)?;
    let manager = models.get()?;
    let files = download_file_progress(&edition);
    let job = manager.create_download_job(
        &edition.edition_id,
        &edition.name,
        edition
            .artifacts
            .first()
            .map(|artifact| artifact.source)
            .ok_or_else(|| {
                AppError::new("MODEL_EDITION_INVALID", "模型版本没有可下载组件", false)
            })?,
        files,
    )?;
    if job.phase == "indexing"
        && manager
            .pending_embedding_activation()?
            .is_some_and(|pending| pending.download_job_id == Some(job.job_id))
    {
        spawn_embed_pending(app, catalog.get()?);
        return Ok(job);
    }
    spawn_model_download(
        app,
        catalog.get()?,
        Arc::clone(&manager),
        edition,
        job.job_id,
        downloads.inner().clone(),
        worker.client.isolated(),
        Arc::clone(&generation.0),
    )?;
    manager_download_job(&models, &job.job_id)
}

#[tauri::command(async)]
pub fn model_download_list(
    models: State<'_, ModelServiceState>,
) -> Result<Vec<ModelDownloadJob>, AppError> {
    models.get()?.list_download_jobs()
}

#[derive(Debug, Deserialize)]
pub struct ModelDownloadJobRequest {
    job_id: Uuid,
}

#[tauri::command(async)]
pub fn model_download_get(
    request: ModelDownloadJobRequest,
    models: State<'_, ModelServiceState>,
) -> Result<ModelDownloadJob, AppError> {
    models.get()?.download_job(&request.job_id)
}

#[tauri::command(async)]
pub fn model_download_pause(
    request: ModelDownloadJobRequest,
    app: AppHandle,
    models: State<'_, ModelServiceState>,
    downloads: State<'_, ModelDownloadCoordinatorState>,
) -> Result<ModelDownloadJob, AppError> {
    let manager = models.get()?;
    let mut job = manager.download_job(&request.job_id)?;
    if matches!(job.status.as_str(), "completed" | "failed" | "cancelled") {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONTROL_INVALID",
            "当前模型下载任务不能暂停",
            false,
        ));
    }
    if job.phase == "indexing" {
        manager.pause_embedding_activation(&request.job_id)?;
        job.status = "paused".into();
        job.phase = "paused".into();
        job.bytes_per_second = 0;
        job.eta_seconds = None;
        job = manager.update_download_job(&job)?;
        emit_download_state(&app, &job);
        return Ok(job);
    }
    let running = downloads.set_action(&request.job_id, DOWNLOAD_ACTION_PAUSE)?;
    if !running {
        job.status = "paused".into();
        job.phase = "paused".into();
        job.bytes_per_second = 0;
        job.eta_seconds = None;
        job = manager.update_download_job(&job)?;
        emit_download_state(&app, &job);
    }
    Ok(job)
}

#[tauri::command(async)]
pub fn model_download_cancel(
    request: ModelDownloadJobRequest,
    app: AppHandle,
    models: State<'_, ModelServiceState>,
    downloads: State<'_, ModelDownloadCoordinatorState>,
) -> Result<ModelDownloadJob, AppError> {
    let manager = models.get()?;
    let mut job = manager.download_job(&request.job_id)?;
    if job.status == "completed" {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONTROL_INVALID",
            "已经完成的模型下载不能取消",
            false,
        ));
    }
    if job.phase == "indexing" {
        manager.cancel_embedding_activation(&request.job_id)?;
        job.status = "cancelled".into();
        job.phase = "cancelled".into();
        job.bytes_per_second = 0;
        job.eta_seconds = None;
        job.error = None;
        job = manager.update_download_job(&job)?;
        emit_download_state(&app, &job);
        return Ok(job);
    }
    let running = downloads.set_action(&request.job_id, DOWNLOAD_ACTION_CANCEL)?;
    if !running {
        job.status = "cancelled".into();
        job.phase = "cancelled".into();
        job.bytes_per_second = 0;
        job.eta_seconds = None;
        job.error = None;
        job = manager.update_download_job(&job)?;
        emit_download_state(&app, &job);
    }
    Ok(job)
}

#[derive(Debug, Deserialize)]
pub struct ModelDownloadRetryRequest {
    job_id: Uuid,
    source: Option<String>,
}

#[tauri::command]
pub async fn model_download_retry(
    request: ModelDownloadRetryRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    downloads: State<'_, ModelDownloadCoordinatorState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ModelDownloadJob, AppError> {
    let manager = models.get()?;
    let previous = manager.download_job(&request.job_id)?;
    let source = request.source.unwrap_or_else(|| match previous.source {
        ModelSource::Modelscope => "modelscope".into(),
        _ => "huggingface".into(),
    });
    let edition = remin_core::model_edition_by_id(&previous.edition_id, &source)?;
    let selected_source = edition.artifacts[0].source;
    if selected_source == previous.source
        && manager
            .pending_embedding_activation()?
            .is_some_and(|pending| pending.download_job_id == Some(request.job_id))
    {
        manager.resume_embedding_activation(&request.job_id)?;
        let mut job = previous;
        job.status = "running".into();
        job.phase = "indexing".into();
        job.bytes_per_second = 0;
        job.eta_seconds = None;
        job.error = None;
        let job = manager.update_download_job(&job)?;
        emit_download_state(&app, &job);
        spawn_embed_pending(app, catalog.get()?);
        return Ok(job);
    }
    let job = if selected_source == previous.source {
        let mut job = previous;
        job.status = "queued".into();
        job.phase = "queued".into();
        job.retry_count = job.retry_count.saturating_add(1);
        job.bytes_per_second = 0;
        job.eta_seconds = None;
        job.current_file = None;
        job.error = None;
        manager.update_download_job(&job)?
    } else {
        manager.create_download_job(
            &edition.edition_id,
            &edition.name,
            selected_source,
            download_file_progress(&edition),
        )?
    };
    spawn_model_download(
        app,
        catalog.get()?,
        Arc::clone(&manager),
        edition,
        job.job_id,
        downloads.inner().clone(),
        worker.client.isolated(),
        Arc::clone(&generation.0),
    )?;
    manager.download_job(&job.job_id)
}

fn manager_download_job(
    models: &State<'_, ModelServiceState>,
    job_id: &Uuid,
) -> Result<ModelDownloadJob, AppError> {
    models.get()?.download_job(job_id)
}

fn download_file_progress(edition: &ModelEdition) -> Vec<ModelDownloadFileProgress> {
    edition
        .artifacts
        .iter()
        .flat_map(|artifact| {
            artifact
                .files()
                .into_iter()
                .map(|file| ModelDownloadFileProgress {
                    role: artifact.role,
                    file_name: file.file_name,
                    downloaded_bytes: 0,
                    total_bytes: file.size_bytes,
                    status: "queued".into(),
                })
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn spawn_model_download(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    models: Arc<ModelManager>,
    edition: ModelEdition,
    job_id: Uuid,
    coordinator: ModelDownloadCoordinatorState,
    worker: WorkerClient,
    generation: Arc<Mutex<LocalGenerationRuntime>>,
) -> Result<(), AppError> {
    let Some(control) = coordinator.begin(job_id)? else {
        return Ok(());
    };
    thread::spawn(move || {
        run_model_download(
            &app,
            &catalog,
            &models,
            &edition,
            job_id,
            &control,
            &worker,
            &generation,
        );
        coordinator.finish(&job_id);
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_model_download(
    app: &AppHandle,
    catalog: &Arc<CatalogService>,
    models: &ModelManager,
    edition: &ModelEdition,
    job_id: Uuid,
    control: &AtomicU8,
    worker: &WorkerClient,
    generation: &Arc<Mutex<LocalGenerationRuntime>>,
) {
    let result = (|| {
        let mut job = models.download_job(&job_id)?;
        job.status = "running".into();
        job.phase = "downloading".into();
        job.error = None;
        persist_download_job(app, models, &mut job)?;
        let _ = app.emit("model.download_started", &job);

        for artifact in &edition.artifacts {
            let staging = models.download_artifact_staging_directory(
                artifact.source,
                &edition.edition_id,
                artifact.role,
            )?;
            for file in artifact.files() {
                download_model_file(
                    app,
                    models,
                    &mut job,
                    artifact.role,
                    &file,
                    &staging,
                    control,
                )?;
            }
        }

        job.phase = "verifying".into();
        job.current_file = None;
        persist_download_job(app, models, &mut job)?;
        for artifact in &edition.artifacts {
            let staging = models.download_artifact_staging_directory(
                artifact.source,
                &edition.edition_id,
                artifact.role,
            )?;
            for file in artifact.files() {
                models.verify_download(
                    &staging.join(&file.file_name),
                    &file.sha256,
                    file.size_bytes,
                )?;
            }
        }

        job.phase = "installing".into();
        persist_download_job(app, models, &mut job)?;
        let mut installed = Vec::new();
        for artifact in &edition.artifacts {
            check_download_control(control)?;
            let staging = models.download_artifact_staging_directory(
                artifact.source,
                &edition.edition_id,
                artifact.role,
            )?;
            let installed_artifact = models.import_downloaded_artifact(
                &ModelImportSelection {
                    source_path: staging
                        .join(&artifact.file_name)
                        .to_string_lossy()
                        .into_owned(),
                    role: artifact.role,
                },
                &DownloadedModelMetadata {
                    source: artifact.source,
                    repository_id: artifact.repository_id.clone(),
                    revision: artifact.revision.clone(),
                    license_name: artifact.license_name.clone(),
                    model_id: Some(artifact.model_id.clone()),
                    query_prefix: artifact.query_prefix.clone(),
                    max_length: artifact.max_length,
                },
            )?;
            installed.push(installed_artifact);
            job.installed_artifact_ids = installed
                .iter()
                .map(|artifact: &ModelArtifact| artifact.artifact_id)
                .collect();
            persist_download_job(app, models, &mut job)?;
        }

        job.phase = "self_testing".into();
        persist_download_job(app, models, &mut job)?;
        let embedding = installed
            .iter()
            .find(|artifact| artifact.role == ModelRole::Embedding)
            .ok_or_else(|| {
                AppError::new("MODEL_PROFILE_INCOMPLETE", "完整RAG缺少语义模型", false)
            })?;
        let generation_artifact = installed
            .iter()
            .find(|artifact| artifact.role == ModelRole::Generation)
            .ok_or_else(|| {
                AppError::new("MODEL_PROFILE_INCOMPLETE", "完整RAG缺少生成模型", false)
            })?;
        let tokenizer = PathBuf::from(&embedding.local_path)
            .parent()
            .map(|parent| parent.join("tokenizer.json"))
            .ok_or_else(|| {
                AppError::new("EMBEDDING_TOKENIZER_UNAVAILABLE", "语义模型目录无效", false)
            })?;
        let embedding_test = worker.encode_embeddings(&EmbeddingRequest {
            model_path: embedding.local_path.clone(),
            tokenizer_path: Some(tokenizer.to_string_lossy().into_owned()),
            texts: vec!["拾起散落的信息，连接过去的自己".into()],
            max_length: embedding.max_length.unwrap_or(512),
            threads: 2,
        })?;
        let embedding_valid = embedding_test.dimension > 0
            && embedding_test.vectors.len() == 1
            && embedding_test.vectors[0].len() == embedding_test.dimension as usize
            && embedding_test.vectors[0]
                .iter()
                .all(|value| value.is_finite());
        if !embedding_valid {
            return Err(AppError::new(
                "MODEL_SELF_TEST_FAILED",
                "语义模型自检返回了无效维度或向量",
                true,
            ));
        }
        let threads = std::thread::available_parallelism()
            .map(|value| value.get() as u32)
            .unwrap_or(2)
            .clamp(1, 8);
        {
            let mut runtime = generation.lock().map_err(|_| {
                AppError::new(
                    "GENERATION_RUNTIME_LOCK_FAILED",
                    "生成运行时状态已损坏",
                    true,
                )
            })?;
            runtime.activate(&generation_artifact.local_path, 4096, threads)?;
            let generated = match runtime.complete(
                "只根据给定证据回答；每个事实句必须保留证据编号[S1]，不得补充证据外事实。",
                "证据[S1]：拾忆在本地处理资料。请用一句完整中文事实句复述并引用。",
                64,
            ) {
                Ok(generated) => generated,
                Err(error) => {
                    runtime.stop();
                    return Err(error);
                }
            };
            if generated.trim().is_empty()
                || !generated.contains("[S1]")
                || !(generated.contains("本地") && generated.contains("资料"))
            {
                runtime.stop();
                return Err(AppError::new(
                    "MODEL_SELF_TEST_FAILED",
                    "生成模型未通过最小严格引文问答自检",
                    true,
                ));
            }
        }

        job.phase = "activating".into();
        persist_download_job(app, models, &mut job)?;
        let profile = match models.activate_profile(
            &edition.edition_id,
            &edition.name,
            &generation_artifact.artifact_id,
            &embedding.artifact_id,
            embedding_test.dimension,
            Some(job_id),
        ) {
            Ok(profile) => profile,
            Err(error) => {
                if let Ok(mut runtime) = generation.lock() {
                    runtime.stop();
                }
                return Err(error);
            }
        };
        job.profile_id = Some(profile.profile_id);
        if profile.status == "indexing" {
            job.status = "running".into();
            job.phase = "indexing".into();
            job.progress = 1.0;
            job.current_file = None;
            job.bytes_per_second = 0;
            job.eta_seconds = None;
            job.error = None;
            persist_download_job(app, models, &mut job)?;
            for artifact in &edition.artifacts {
                if let Ok(staging) = models.download_artifact_staging_directory(
                    artifact.source,
                    &edition.edition_id,
                    artifact.role,
                ) {
                    let _ = fs::remove_dir_all(staging);
                }
            }
            spawn_embed_pending(app.clone(), Arc::clone(catalog));
            let _ = app.emit("model.download_indexing", &job);
            return Ok::<(), AppError>(());
        }
        job.status = "completed".into();
        job.phase = "completed".into();
        job.current_file = None;
        job.bytes_per_second = 0;
        job.eta_seconds = Some(0);
        job.error = None;
        for file in &mut job.files {
            file.status = "completed".into();
            file.downloaded_bytes = file.total_bytes;
        }
        persist_download_job(app, models, &mut job)?;
        for artifact in &edition.artifacts {
            if let Ok(staging) = models.download_artifact_staging_directory(
                artifact.source,
                &edition.edition_id,
                artifact.role,
            ) {
                let _ = fs::remove_dir_all(staging);
            }
        }
        spawn_embed_pending(app.clone(), Arc::clone(catalog));
        let _ = app.emit("model.download_completed", &job);
        Ok::<(), AppError>(())
    })();

    if let Err(error) = result {
        let action = control.load(Ordering::Acquire);
        if let Ok(mut job) = models.download_job(&job_id) {
            match action {
                DOWNLOAD_ACTION_PAUSE => {
                    job.status = "paused".into();
                    job.phase = "paused".into();
                    job.error = None;
                }
                DOWNLOAD_ACTION_CANCEL => {
                    job.status = "cancelled".into();
                    job.phase = "cancelled".into();
                    job.error = None;
                }
                _ => {
                    job.status = "failed".into();
                    job.phase = "failed".into();
                    job.error = Some(error.clone());
                }
            }
            job.bytes_per_second = 0;
            job.eta_seconds = None;
            job.current_file = None;
            if let Ok(job) = models.update_download_job(&job) {
                emit_download_state(app, &job);
            }
        }
        if action == DOWNLOAD_ACTION_RUN {
            let _ = app.emit("model.download_failed", error);
        }
    }
}

fn download_model_file(
    app: &AppHandle,
    models: &ModelManager,
    job: &mut ModelDownloadJob,
    role: ModelRole,
    file: &DownloadFile,
    staging: &Path,
    control: &AtomicU8,
) -> Result<(), AppError> {
    let completed_path = staging.join(&file.file_name);
    let partial_path = staging.join(format!("{}.part", file.file_name));
    if completed_path.is_file()
        && models
            .verify_download(&completed_path, &file.sha256, file.size_bytes)
            .is_ok()
    {
        update_download_file(job, role, file, file.size_bytes, "completed");
        persist_download_job(app, models, job)?;
        return Ok(());
    }
    if completed_path.exists() {
        quarantine_download_file(&completed_path)?;
    }
    for attempt in 0..=1 {
        check_download_control(control)?;
        if partial_path.exists() {
            let metadata = fs::symlink_metadata(&partial_path).map_err(|error| {
                AppError::new("MODEL_DOWNLOAD_INCOMPLETE", error.to_string(), true)
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(AppError::new(
                    "MODEL_DOWNLOAD_INCOMPLETE",
                    "模型断点不是拾忆管理的普通文件",
                    false,
                ));
            }
            if metadata.len() > file.size_bytes {
                quarantine_download_file(&partial_path)?;
            }
        }
        job.current_file = Some(file.file_name.clone());
        update_download_file(
            job,
            role,
            file,
            fs::metadata(&partial_path)
                .map(|value| value.len())
                .unwrap_or(0),
            "downloading",
        );
        persist_download_job(app, models, job)?;
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
            .arg(&file.url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        hide_process_window(&mut command);
        let mut child = command.spawn().map_err(|error| {
            AppError::new("MODEL_DOWNLOADER_UNAVAILABLE", error.to_string(), true)
        })?;
        let started = Instant::now();
        let initial_bytes = fs::metadata(&partial_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let status = loop {
            if control.load(Ordering::Acquire) != DOWNLOAD_ACTION_RUN {
                let _ = child.kill();
                let _ = child.wait();
                check_download_control(control)?;
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|error| AppError::new("MODEL_DOWNLOAD_FAILED", error.to_string(), true))?
            {
                break status;
            }
            let downloaded = fs::metadata(&partial_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
            if downloaded > file.size_bytes {
                let _ = child.kill();
                break child.wait().map_err(|error| {
                    AppError::new("MODEL_DOWNLOAD_FAILED", error.to_string(), true)
                })?;
            }
            let elapsed = started.elapsed().as_secs_f64().max(0.25);
            job.bytes_per_second =
                ((downloaded.saturating_sub(initial_bytes)) as f64 / elapsed).max(0.0) as u64;
            update_download_file(job, role, file, downloaded, "downloading");
            persist_download_job(app, models, job)?;
            thread::sleep(Duration::from_millis(500));
        };
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        if status.success() {
            fs::rename(&partial_path, &completed_path).map_err(|error| {
                AppError::new("MODEL_DOWNLOAD_FINALIZE_FAILED", error.to_string(), true)
            })?;
            if models
                .verify_download(&completed_path, &file.sha256, file.size_bytes)
                .is_ok()
            {
                update_download_file(job, role, file, file.size_bytes, "completed");
                persist_download_job(app, models, job)?;
                return Ok(());
            }
            quarantine_download_file(&completed_path)?;
        } else if partial_path.exists()
            && fs::metadata(&partial_path).is_ok_and(|metadata| metadata.len() > file.size_bytes)
        {
            quarantine_download_file(&partial_path)?;
        }
        if attempt == 1 {
            let detail = stderr.chars().take(800).collect::<String>();
            return Err(AppError::new(
                "MODEL_DOWNLOAD_FAILED",
                if detail.trim().is_empty() {
                    format!("模型组件 {} 下载或校验失败", file.file_name)
                } else {
                    detail
                },
                true,
            ));
        }
        job.retry_count = job.retry_count.saturating_add(1);
    }
    unreachable!("download attempts always return")
}

fn update_download_file(
    job: &mut ModelDownloadJob,
    role: ModelRole,
    file: &DownloadFile,
    downloaded_bytes: u64,
    status: &str,
) {
    if let Some(progress) = job
        .files
        .iter_mut()
        .find(|progress| progress.role == role && progress.file_name == file.file_name)
    {
        progress.downloaded_bytes = downloaded_bytes.min(progress.total_bytes);
        progress.status = status.into();
    }
}

fn persist_download_job(
    app: &AppHandle,
    models: &ModelManager,
    job: &mut ModelDownloadJob,
) -> Result<(), AppError> {
    job.downloaded_bytes = job.files.iter().map(|file| file.downloaded_bytes).sum();
    job.total_bytes = job.files.iter().map(|file| file.total_bytes).sum();
    job.progress = if job.total_bytes == 0 {
        0.0
    } else {
        (job.downloaded_bytes as f64 / job.total_bytes as f64).clamp(0.0, 1.0)
    };
    job.eta_seconds = if job.bytes_per_second == 0 {
        None
    } else {
        Some(
            job.total_bytes
                .saturating_sub(job.downloaded_bytes)
                .div_ceil(job.bytes_per_second),
        )
    };
    *job = models.update_download_job(job)?;
    emit_download_state(app, job);
    Ok(())
}

fn emit_download_state(app: &AppHandle, job: &ModelDownloadJob) {
    let _ = app.emit("model.download_state", job);
    let _ = app.emit(
        "model.download_progress",
        json!({
            "job_id": job.job_id,
            "edition_id": job.edition_id,
            "downloaded_bytes": job.downloaded_bytes,
            "total_bytes": job.total_bytes,
            "progress": job.progress,
            "phase": job.phase,
            "status": job.status,
        }),
    );
}

fn check_download_control(control: &AtomicU8) -> Result<(), AppError> {
    match control.load(Ordering::Acquire) {
        DOWNLOAD_ACTION_PAUSE => Err(AppError::new(
            "MODEL_DOWNLOAD_PAUSED",
            "模型下载已暂停",
            true,
        )),
        DOWNLOAD_ACTION_CANCEL => Err(AppError::new(
            "MODEL_DOWNLOAD_CANCELLED",
            "模型下载已取消",
            false,
        )),
        _ => Ok(()),
    }
}

fn quarantine_download_file(path: &Path) -> Result<(), AppError> {
    if !path.exists() {
        return Ok(());
    }
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model.part".into());
    let quarantine = path.with_file_name(format!(
        "{file_name}.invalid-{}",
        Utc::now().timestamp_millis()
    ));
    fs::rename(path, quarantine)
        .map_err(|error| AppError::new("MODEL_DOWNLOAD_QUARANTINE_FAILED", error.to_string(), true))
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
            if response.dimension == 0
                || response.vectors.len() != 1
                || response.vectors[0].len() != response.dimension as usize
                || response.vectors[0].iter().any(|value| !value.is_finite())
            {
                return Err(AppError::new(
                    "MODEL_SELF_TEST_FAILED",
                    "语义模型自检返回了无效维度或向量",
                    true,
                ));
            }
            models.begin_embedding_activation(&artifact_id, response.dimension)?;
            spawn_embed_pending(app.clone(), Arc::clone(&catalog));
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
        (ModelRole::Vision, ModelFormat::Gguf) => {
            let projector = models.vision_projector_path(&artifact)?;
            let threads = std::thread::available_parallelism()
                .map(|value| value.get() as u32)
                .unwrap_or(2)
                .clamp(1, 8);
            generation
                .0
                .lock()
                .map_err(|_| {
                    AppError::new(
                        "VISION_RUNTIME_LOCK_FAILED",
                        "图片理解运行时状态已损坏",
                        true,
                    )
                })?
                .activate_multimodal(
                    &artifact.local_path,
                    &projector.to_string_lossy(),
                    4096,
                    threads,
                )?;
            models.activate_artifact(&artifact_id, None)?;
            spawn_image_understanding_pending(app.clone(), Arc::clone(&catalog));
        }
        (ModelRole::Reranker, ModelFormat::Onnx) => {
            let tokenizer_path = PathBuf::from(&artifact.local_path)
                .parent()
                .map(|parent| parent.join("tokenizer.json"))
                .ok_or_else(|| {
                    AppError::new("RERANK_TOKENIZER_UNAVAILABLE", "模型目录无效", false)
                })?;
            let response = worker.client.rerank(&RerankRequest {
                model_path: artifact.local_path.clone(),
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                query: "哪段资料描述了本地知识库？".into(),
                documents: vec![
                    "拾忆在本地建立可检索的资料知识库。".into(),
                    "今天窗外天气晴朗。".into(),
                ],
                max_length: artifact.max_length.unwrap_or(512),
                threads: 2,
            })?;
            if response.scores.len() != 2 || response.scores.iter().any(|score| !score.is_finite())
            {
                return Err(AppError::new(
                    "MODEL_SELF_TEST_FAILED",
                    "重排模型自检返回了无效分数",
                    true,
                ));
            }
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
    model_state_from_manager(&models, Some(&catalog))
}

fn model_state_from_manager(
    models: &ModelManager,
    catalog: Option<&CatalogService>,
) -> Result<ModelRuntimeState, AppError> {
    let artifacts = models.list_artifacts()?;
    let active_profile = models.active_profile()?;
    let pending_embedding = models.pending_embedding_activation()?;
    let active_embedding = models.active_artifact(ModelRole::Embedding)?;
    let capabilities = ModelCapabilities {
        generation: models.active_artifact(ModelRole::Generation)?.is_some(),
        embedding: active_embedding.is_some(),
        vision: models.active_artifact(ModelRole::Vision)?.is_some(),
        reranker: models.active_artifact(ModelRole::Reranker)?.is_some(),
        ocr: models.active_artifact(ModelRole::Ocr)?.is_some(),
    };
    let any_active = capabilities.generation
        || capabilities.embedding
        || capabilities.vision
        || capabilities.reranker
        || capabilities.ocr;
    let semantic_index_coverage = match (catalog, active_embedding.as_ref()) {
        (Some(catalog), Some(embedding)) => catalog
            .active_vector_generation(&embedding.artifact_id.to_string())?
            .map(|generation| generation.coverage)
            .unwrap_or(0.0),
        _ => 0.0,
    };
    let rag_complete =
        capabilities.generation && capabilities.embedding && semantic_index_coverage >= 0.999_999;
    let message = match pending_embedding
        .as_ref()
        .map(|pending| pending.status.as_str())
    {
        Some("indexing") => "正在为新 Embedding 构建并校验语义索引，完成前继续使用原索引".into(),
        Some("paused") => "新 Embedding 索引构建已暂停，当前仍使用原索引".into(),
        Some("cancelled") => "新 Embedding 索引切换已取消，当前仍使用原索引".into(),
        Some("failed") => "新 Embedding 索引构建失败，当前仍使用原索引；可重试模型任务".into(),
        _ if capabilities.generation && capabilities.embedding && !rag_complete => format!(
            "模型已就绪，语义索引覆盖率 {}%",
            (semantic_index_coverage * 100.0).round() as u32
        ),
        _ if capabilities.generation => "本地生成模型已配置，将在提问时按需加载".into(),
        _ if capabilities.embedding => "语义检索已就绪，问资料仍需生成模型".into(),
        _ if capabilities.vision => "本地多模态模型已配置，图片理解将在后台串行处理".into(),
        _ if artifacts.is_empty() => "未配置本地模型".into(),
        _ => "模型已导入，等待运行自检".into(),
    };
    Ok(ModelRuntimeState {
        status: if any_active {
            "ready"
        } else if pending_embedding
            .as_ref()
            .is_some_and(|pending| pending.status == "indexing")
        {
            "checking"
        } else if artifacts.is_empty() {
            "unconfigured"
        } else {
            "unavailable"
        },
        active_profile_id: active_profile
            .as_ref()
            .map(|profile| profile.profile_id.to_string()),
        active_profile_name: active_profile.as_ref().map(|profile| profile.name.clone()),
        runtime_backend: if any_active { Some("cpu") } else { None },
        message,
        checked_at: Utc::now().to_rfc3339(),
        capabilities,
        rag_complete,
        semantic_index_coverage,
        embedding_migration: pending_embedding.map(|pending| EmbeddingMigrationState {
            artifact_id: pending.artifact_id.to_string(),
            status: pending.status,
            error: pending.error,
        }),
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
    let catalog = catalog.get()?;
    let root = catalog.add_root(request)?;
    if let Err(error) = watcher
        .with_mut(|watcher| watcher.watch_root(&root))
        .and_then(|result| result)
    {
        let _ = app.emit("catalog.watch_degraded", &error);
    }
    let prepared = catalog.prepare_scan(&root.root_id, "user_authorized")?;
    if prepared.should_start {
        spawn_scan(app, Arc::clone(&catalog), root.root_id, prepared.job.job_id);
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

#[derive(Debug, Deserialize)]
pub struct ImageUnderstandingActionRequest {
    asset_id: Uuid,
}

#[tauri::command(async)]
pub fn image_understanding_retry(
    request: ImageUnderstandingActionRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let catalog = catalog.get()?;
    catalog.retry_image_understanding(&request.asset_id)?;
    spawn_image_understanding_pending(app, catalog);
    Ok(true)
}

#[derive(Debug, Deserialize)]
pub struct ImageDeepAnalyzeRequest {
    asset_id: Uuid,
    question: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImageDeepAnalysis {
    asset_id: Uuid,
    question: String,
    answer: String,
    observations: Vec<String>,
    uncertainties: Vec<String>,
    model_artifact_id: Uuid,
    analyzed_at: String,
}

#[tauri::command]
pub async fn image_deep_analyze(
    request: ImageDeepAnalyzeRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ImageDeepAnalysis, AppError> {
    let question = request.question.trim().to_owned();
    if question.is_empty() || question.chars().count() > 2_000 {
        return Err(AppError::new(
            "VISION_REQUEST_INVALID",
            "原图深度分析需要1到2000个字符的问题",
            false,
        ));
    }
    let asset_id = request.asset_id;
    let catalog = catalog.get()?;
    let models = models.get()?;
    let generation = Arc::clone(&generation.0);
    tauri::async_runtime::spawn_blocking(move || {
        let (image_path, mime_type, _) = catalog.authorized_image_asset_path(&asset_id)?;
        let artifact = models
            .active_artifact(ModelRole::Vision)?
            .ok_or_else(|| {
                AppError::new(
                    "VISION_MODEL_INVALID",
                    "原图深度分析需要先配置并自检本地多模态模型",
                    true,
                )
            })?;
        let projector = models.vision_projector_path(&artifact)?;
        let threads = std::thread::available_parallelism()
            .map(|value| value.get() as u32)
            .unwrap_or(2)
            .clamp(1, 8);
        let mut runtime = generation.lock().map_err(|_| {
            AppError::new(
                "VISION_RUNTIME_LOCK_FAILED",
                "图片理解运行时状态已损坏",
                true,
            )
        })?;
        let projector_path = projector.to_string_lossy();
        if runtime.active_model_path() != Some(artifact.local_path.as_str())
            || runtime.active_mmproj_path() != Some(projector_path.as_ref())
            || !runtime.is_active()
        {
            runtime.activate_multimodal(
                &artifact.local_path,
                projector_path.as_ref(),
                4096,
                threads,
            )?;
        }
        let cancelled = AtomicBool::new(false);
        let response = runtime.describe_image_cancellable(
            "你是拾忆的本地图片证据分析器。只能根据当前图片中可验证的内容回答，不得补充外部知识；看不清或图片不支持的问题必须明确说明。",
            &format!(
                "用户问题：{question}\n只输出一个JSON对象，不要Markdown：{{\"answer\":\"基于图片的中文回答\",\"observations\":[\"支持回答的可见细节\"],\"uncertainties\":[\"看不清或无法确认的内容\"]}}。"
            ),
            &image_path,
            &mime_type,
            512,
            &cancelled,
        )?;
        let payload = parse_vision_question_answer(&response)?;
        Ok(ImageDeepAnalysis {
            asset_id,
            question,
            answer: payload.answer,
            observations: payload.observations,
            uncertainties: payload.uncertainties,
            model_artifact_id: artifact.artifact_id,
            analyzed_at: Utc::now().to_rfc3339(),
        })
    })
    .await
    .map_err(|error| AppError::new("VISION_REQUEST_FAILED", error.to_string(), true))?
}

#[tauri::command(async)]
pub fn collection_list(
    catalog: State<'_, CatalogServiceState>,
) -> Result<Vec<CollectionRecord>, AppError> {
    catalog.get()?.list_collections()
}

#[tauri::command(async)]
pub fn knowledge_space_list(
    catalog: State<'_, CatalogServiceState>,
) -> Result<Vec<KnowledgeSpace>, AppError> {
    catalog.get()?.list_knowledge_spaces()
}

#[tauri::command(async)]
pub fn knowledge_space_create(
    request: KnowledgeSpaceRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<KnowledgeSpace, AppError> {
    catalog.get()?.create_knowledge_space(&request)
}

#[derive(Debug, Deserialize)]
pub struct KnowledgeSpaceUpdateRequest {
    space_id: String,
    space: KnowledgeSpaceRequest,
}

#[tauri::command(async)]
pub fn knowledge_space_update(
    request: KnowledgeSpaceUpdateRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<KnowledgeSpace, AppError> {
    let space_id = Uuid::parse_str(&request.space_id).map_err(|error| {
        AppError::new("KNOWLEDGE_SPACE_REQUEST_INVALID", error.to_string(), false)
    })?;
    catalog
        .get()?
        .update_knowledge_space(&space_id, &request.space)
}

#[derive(Debug, Deserialize)]
pub struct KnowledgeSpaceIdRequest {
    space_id: String,
}

#[tauri::command(async)]
pub fn knowledge_space_delete(
    request: KnowledgeSpaceIdRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    let space_id = Uuid::parse_str(&request.space_id).map_err(|error| {
        AppError::new("KNOWLEDGE_SPACE_REQUEST_INVALID", error.to_string(), false)
    })?;
    catalog.get()?.delete_knowledge_space(&space_id)
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
            ..FileQuery::default()
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
pub struct CollectionSuggestionRefreshRequest {
    #[serde(default = "default_suggestion_refresh_limit")]
    max_files: u32,
}

fn default_suggestion_refresh_limit() -> u32 {
    500
}

#[tauri::command(async)]
pub fn collection_suggestion_refresh(
    request: CollectionSuggestionRefreshRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<CollectionSuggestionRefreshResult, AppError> {
    let catalog = catalog.get()?;
    if catalog.maintenance_snapshot()?.degradation_level == "core" {
        return Err(AppError::new(
            "COLLECTION_AI_PAUSED_CORE_MODE",
            "后台资源繁忙，AI集合分析已暂停；已有虚拟集合仍可使用",
            true,
        ));
    }
    let models = models.get()?;
    let embedding = models
        .active_artifact(ModelRole::Embedding)?
        .ok_or_else(|| {
            AppError::new(
                "COLLECTION_AI_EMBEDDING_MISSING",
                "AI智能集合需要已通过自检的Embedding模型",
                true,
            )
        })?;
    let generation_artifact = models
        .active_artifact(ModelRole::Generation)?
        .ok_or_else(|| {
            AppError::new(
                "COLLECTION_AI_GENERATION_MISSING",
                "AI智能集合需要已通过自检的本地生成模型完成候选组复核",
                true,
            )
        })?;
    let mut result = catalog
        .refresh_collection_suggestions(&embedding.artifact_id.to_string(), request.max_files)?;
    if !result.suggestion_ids.is_empty() {
        let new_ids = result
            .suggestion_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let candidates = catalog.query_collection_suggestions(&CollectionSuggestionQuery {
            cursor: None,
            page_size: 100,
            status: Some("suggested".into()),
        })?;
        let mut reviewed_ids = Vec::new();
        for suggestion in candidates
            .items
            .into_iter()
            .filter(|item| new_ids.contains(&item.suggestion_id))
        {
            let _ = app.emit(
                "collection.suggestion_phase",
                json!({"suggestion_id": suggestion.suggestion_id, "phase": "model_review"}),
            );
            let candidate_json = serde_json::to_string(
                &suggestion
                    .members
                    .iter()
                    .map(|member| {
                        json!({
                            "file_id": member.file.file_id,
                            "title": member.file.display_name,
                            "semantic_reason": member.rationale,
                            "confidence": member.confidence,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .map_err(|error| {
                AppError::new("COLLECTION_MODEL_REVIEW_INVALID", error.to_string(), false)
            })?;
            let cancelled = AtomicBool::new(false);
            let reviewed = complete_with_model(
                generation.0.as_ref(),
                &generation_artifact,
                "你是本地文档分类复核器。只复核给定候选，不得添加候选外文件。只输出JSON对象：{\"suggested_name\":\"不超过40字\",\"description\":\"说明共同主题和判断依据\",\"members\":[{\"file_id\":\"原UUID\",\"rationale\":\"该成员与主题的具体联系\"}]}。不相关成员应省略；少于2个相关成员也必须按原格式返回空members。",
                &format!("请结构化复核这个Embedding候选组：\n{candidate_json}"),
                640,
                &cancelled,
            ).and_then(|value| parse_collection_model_review(&value));
            match reviewed.and_then(|review| {
                catalog.apply_collection_model_review(
                    &suggestion.suggestion_id,
                    &review,
                    &generation_artifact.artifact_id.to_string(),
                )
            }) {
                Ok(_) => reviewed_ids.push(suggestion.suggestion_id),
                Err(_) => catalog.reject_collection_suggestion(&suggestion.suggestion_id)?,
            }
        }
        if reviewed_ids.is_empty() {
            return Err(AppError::new(
                "COLLECTION_MODEL_REVIEW_FAILED",
                "候选关系已计算，但没有候选组通过本地模型结构化复核",
                true,
            ));
        }
        result.created_suggestions = reviewed_ids.len() as u64;
        result.suggestion_ids = reviewed_ids;
        result.model_version = generation_artifact.artifact_id.to_string();
    }
    let _ = app.emit("collection.suggestions_changed", &result);
    Ok(result)
}

fn parse_collection_model_review(value: &str) -> Result<CollectionModelReview, AppError> {
    let trimmed = value.trim();
    let parsed = serde_json::from_str(trimmed)
        .or_else(|_| {
            let start = trimmed
                .find('{')
                .ok_or(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing JSON object",
                )))?;
            let end = trimmed
                .rfind('}')
                .ok_or(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing JSON object",
                )))?;
            serde_json::from_str(&trimmed[start..=end])
        })
        .map_err(|error| {
            AppError::new(
                "COLLECTION_MODEL_REVIEW_INVALID",
                format!("本地模型没有返回有效的集合复核JSON：{error}"),
                true,
            )
        })?;
    Ok(parsed)
}

#[tauri::command(async)]
pub fn collection_suggestion_query(
    request: CollectionSuggestionQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<CollectionSuggestionPage, AppError> {
    catalog.get()?.query_collection_suggestions(&request)
}

#[derive(Debug, Deserialize)]
pub struct CollectionSuggestionUpdateCommandRequest {
    suggestion_id: Uuid,
    suggestion: CollectionSuggestionUpdateRequest,
}

#[tauri::command(async)]
pub fn collection_suggestion_update(
    request: CollectionSuggestionUpdateCommandRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<CollectionSuggestion, AppError> {
    catalog
        .get()?
        .update_collection_suggestion(&request.suggestion_id, &request.suggestion)
}

#[derive(Debug, Deserialize)]
pub struct CollectionSuggestionActionRequest {
    suggestion_id: Uuid,
}

#[tauri::command(async)]
pub fn collection_suggestion_confirm(
    request: CollectionSuggestionActionRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
) -> Result<CollectionRecord, AppError> {
    let collection = catalog
        .get()?
        .confirm_collection_suggestion(&request.suggestion_id)?;
    let _ = app.emit("collection.suggestions_changed", &collection);
    let _ = app.emit("catalog.changed", &collection);
    Ok(collection)
}

#[tauri::command(async)]
pub fn collection_suggestion_reject(
    request: CollectionSuggestionActionRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    catalog
        .get()?
        .reject_collection_suggestion(&request.suggestion_id)?;
    let _ = app.emit(
        "collection.suggestions_changed",
        json!({"suggestion_id": request.suggestion_id, "status": "rejected"}),
    );
    Ok(())
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
pub fn exclusion_rule_list(
    catalog: State<'_, CatalogServiceState>,
) -> Result<Vec<ExclusionRule>, AppError> {
    catalog.get()?.list_exclusion_rules()
}

#[tauri::command(async)]
pub fn exclusion_rule_upsert(
    request: ExclusionRuleInput,
    catalog: State<'_, CatalogServiceState>,
) -> Result<ExclusionRule, AppError> {
    catalog.get()?.upsert_exclusion_rule(&request)
}

#[derive(Debug, Deserialize)]
pub struct ExclusionRuleDeleteRequest {
    rule_id: Uuid,
}

#[tauri::command(async)]
pub fn exclusion_rule_delete(
    request: ExclusionRuleDeleteRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    catalog.get()?.delete_exclusion_rule(&request.rule_id)
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
    app: AppHandle,
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
                asset_cache_dir: image_asset_cache_dir(&app, &revision_id),
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

#[derive(Debug, Clone, Serialize)]
pub struct StorageUsageCategory {
    key: String,
    label: String,
    size_bytes: u64,
    clearable: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageUsageSnapshot {
    categories: Vec<StorageUsageCategory>,
    total_bytes: u64,
    data_directory: String,
    disk_capacity_bytes: Option<u64>,
    disk_available_bytes: Option<u64>,
    soft_quota_bytes: u64,
    soft_quota_is_custom: bool,
    over_soft_quota: bool,
    background_tasks_paused: bool,
    notice: Option<String>,
    measured_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CacheClearRequest {
    category: String,
    confirmation: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheClearResult {
    category: String,
    removed_entries: u64,
    freed_bytes: u64,
}

#[tauri::command(async)]
pub fn storage_usage_get(
    environment: State<'_, EnvironmentServiceState>,
    catalog: State<'_, CatalogServiceState>,
) -> Result<StorageUsageSnapshot, AppError> {
    storage_usage_snapshot(
        &environment.data_directory,
        catalog.get()?.storage_quota_override()?,
    )
}

#[derive(Debug, Deserialize)]
pub struct StoragePolicySetRequest {
    quota_bytes: u64,
    confirmation: String,
}

#[tauri::command(async)]
pub fn storage_policy_set(
    request: StoragePolicySetRequest,
    environment: State<'_, EnvironmentServiceState>,
    catalog: State<'_, CatalogServiceState>,
) -> Result<StorageUsageSnapshot, AppError> {
    if request.confirmation != "SET_STORAGE_QUOTA" {
        return Err(AppError::new(
            "STORAGE_POLICY_CONFIRMATION_REQUIRED",
            "调整存储软配额前需要明确确认",
            false,
        ));
    }
    let catalog = catalog.get()?;
    let quota = catalog.set_storage_quota_override(request.quota_bytes)?;
    storage_usage_snapshot(&environment.data_directory, Some(quota))
}

#[tauri::command(async)]
pub fn cache_clear(
    request: CacheClearRequest,
    environment: State<'_, EnvironmentServiceState>,
) -> Result<CacheClearResult, AppError> {
    if request.confirmation != "CLEAR_CACHE" {
        return Err(AppError::new(
            "CACHE_CLEAR_CONFIRMATION_REQUIRED",
            "清理缓存前需要明确确认",
            false,
        ));
    }
    let target = match request.category.as_str() {
        "temporary_cache" => environment.data_directory.join("cache"),
        "failed_downloads" => environment
            .data_directory
            .join("models")
            .join(".downloads")
            .join("quarantine"),
        _ => {
            return Err(AppError::new(
                "CACHE_CATEGORY_INVALID",
                "只允许清理临时缓存或失败下载隔离区",
                false,
            ));
        }
    };
    let freed_bytes = directory_size(&target)?;
    let removed_entries = clear_directory_contents(&target)?;
    Ok(CacheClearResult {
        category: request.category,
        removed_entries,
        freed_bytes,
    })
}

#[derive(Debug, Deserialize)]
pub struct AppDataResetRequest {
    confirmation: String,
}

#[tauri::command(async)]
pub fn app_data_reset_schedule(
    request: AppDataResetRequest,
    app: AppHandle,
) -> Result<(), AppError> {
    if request.confirmation != "RESET_APPLICATION_DATA" {
        return Err(AppError::new(
            "APP_DATA_RESET_CONFIRMATION_REQUIRED",
            "重置应用数据前需要输入完整确认短语",
            false,
        ));
    }
    let config_dir = app
        .path()
        .app_config_dir()
        .map_err(|error| AppError::new("APP_DATA_RESET_PATH_INVALID", error.to_string(), false))?;
    let parent = config_dir.parent().ok_or_else(|| {
        AppError::new(
            "APP_DATA_RESET_PATH_INVALID",
            "无法确定应用配置目录的父目录",
            false,
        )
    })?;
    let marker = parent.join(".com.remin.desktop-reset-request");
    fs::write(&marker, "RESET_APPLICATION_DATA")
        .map_err(|error| AppError::new("APP_DATA_RESET_MARKER_FAILED", error.to_string(), true))?;
    app.restart();
}

fn storage_usage_snapshot(
    data_directory: &Path,
    quota_override: Option<u64>,
) -> Result<StorageUsageSnapshot, AppError> {
    let database_size = ["remin.db", "remin.db-wal", "remin.db-shm"]
        .iter()
        .try_fold(0_u64, |total, name| {
            Ok::<u64, AppError>(total + file_size(&data_directory.join(name))?)
        })?;
    let model_root = data_directory.join("models");
    let installed_models = ["generation", "embedding", "vision", "reranker", "ocr"]
        .iter()
        .try_fold(0_u64, |total, name| {
            Ok::<u64, AppError>(total + directory_size(&model_root.join(name))?)
        })?;
    let downloads = model_root.join(".downloads");
    let failed_downloads = directory_size(&downloads.join("quarantine"))?;
    let download_staging = directory_size(&downloads)?.saturating_sub(failed_downloads);
    let temporary_cache = directory_size(&data_directory.join("cache"))?;
    let vector_indexes = directory_size(&data_directory.join("vector-indexes"))?;
    let categories = vec![
        StorageUsageCategory {
            key: "database".into(),
            label: "资料索引数据库".into(),
            size_bytes: database_size,
            clearable: false,
            detail: "元数据、全文索引和语义向量；请使用重建索引维护".into(),
        },
        StorageUsageCategory {
            key: "vector_indexes".into(),
            label: "语义向量索引".into(),
            size_bytes: vector_indexes,
            clearable: false,
            detail: "由SQLite真值构建的USearch索引；切换Embedding时按世代原子替换".into(),
        },
        StorageUsageCategory {
            key: "installed_models".into(),
            label: "已安装模型".into(),
            size_bytes: installed_models,
            clearable: false,
            detail: "已经校验并激活的本地模型组件".into(),
        },
        StorageUsageCategory {
            key: "resumable_downloads".into(),
            label: "可续传模型下载".into(),
            size_bytes: download_staging,
            clearable: false,
            detail: "保留暂停任务的断点，避免重新下载".into(),
        },
        StorageUsageCategory {
            key: "temporary_cache".into(),
            label: "解析与预览临时缓存".into(),
            size_bytes: temporary_cache,
            clearable: true,
            detail: "可安全重建，不含源文件与索引数据库".into(),
        },
        StorageUsageCategory {
            key: "failed_downloads".into(),
            label: "失败下载隔离区".into(),
            size_bytes: failed_downloads,
            clearable: true,
            detail: "大小或哈希异常的下载副本，不用于续传".into(),
        },
    ];
    let total_bytes = categories.iter().map(|item| item.size_bytes).sum();
    let (disk_capacity_bytes, disk_available_bytes) = disk_space_bytes(data_directory)
        .map(|(total, available)| (Some(total), Some(available)))
        .unwrap_or((None, None));
    const GIB: u64 = 1024 * 1024 * 1024;
    let default_quota = disk_capacity_bytes
        .map(|capacity| (capacity / 10).clamp(10 * GIB, 50 * GIB))
        .unwrap_or(10 * GIB);
    let soft_quota_bytes = quota_override.unwrap_or(default_quota);
    let over_soft_quota = total_bytes >= soft_quota_bytes;
    Ok(StorageUsageSnapshot {
        total_bytes,
        categories,
        data_directory: data_directory.to_string_lossy().into_owned(),
        disk_capacity_bytes,
        disk_available_bytes,
        soft_quota_bytes,
        soft_quota_is_custom: quota_override.is_some(),
        over_soft_quota,
        background_tasks_paused: over_soft_quota,
        notice: over_soft_quota
            .then(|| "存储已达到软配额，暂停图片缓存、OCR和语义索引；搜索与预览继续可用".into()),
        measured_at: Utc::now().to_rfc3339(),
    })
}

fn background_storage_budget_allows(app: &AppHandle, catalog: &CatalogService) -> bool {
    let environment = app.state::<EnvironmentServiceState>();
    let quota = match catalog.storage_quota_override() {
        Ok(value) => value,
        Err(error) => {
            let _ = app.emit("background.paused", error);
            return false;
        }
    };
    match storage_usage_snapshot(&environment.data_directory, quota) {
        Ok(snapshot) if snapshot.background_tasks_paused => {
            let _ = app.emit(
                "background.paused",
                json!({
                    "reason": "storage_soft_quota",
                    "notice": snapshot.notice,
                    "used_bytes": snapshot.total_bytes,
                    "soft_quota_bytes": snapshot.soft_quota_bytes,
                }),
            );
            false
        }
        Ok(_) => true,
        Err(error) => {
            let _ = app.emit("background.paused", error);
            false
        }
    }
}

fn file_size(path: &Path) -> Result<u64, AppError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        Ok(_) => Ok(0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(AppError::new(
            "STORAGE_USAGE_READ_FAILED",
            error.to_string(),
            true,
        )),
    }
}

fn directory_size(path: &Path) -> Result<u64, AppError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(AppError::new(
                "STORAGE_USAGE_READ_FAILED",
                error.to_string(),
                true,
            ));
        }
    };
    let mut total = 0_u64;
    for entry in entries {
        let entry = entry
            .map_err(|error| AppError::new("STORAGE_USAGE_READ_FAILED", error.to_string(), true))?;
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::new("STORAGE_USAGE_READ_FAILED", error.to_string(), true))?;
        if file_type.is_symlink() {
            continue;
        }
        total = total.saturating_add(if file_type.is_dir() {
            directory_size(&entry.path())?
        } else if file_type.is_file() {
            entry
                .metadata()
                .map_err(|error| {
                    AppError::new("STORAGE_USAGE_READ_FAILED", error.to_string(), true)
                })?
                .len()
        } else {
            0
        });
    }
    Ok(total)
}

fn clear_directory_contents(path: &Path) -> Result<u64, AppError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(AppError::new("CACHE_CLEAR_FAILED", error.to_string(), true));
        }
    };
    let mut removed = 0_u64;
    for entry in entries {
        let entry =
            entry.map_err(|error| AppError::new("CACHE_CLEAR_FAILED", error.to_string(), true))?;
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::new("CACHE_CLEAR_FAILED", error.to_string(), true))?;
        let result = if file_type.is_dir() && !file_type.is_symlink() {
            fs::remove_dir_all(entry.path())
        } else {
            fs::remove_file(entry.path())
        };
        result.map_err(|error| AppError::new("CACHE_CLEAR_FAILED", error.to_string(), true))?;
        removed += 1;
    }
    Ok(removed)
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
                    .unwrap_or("环境信息不完整，已降低后台任务并发")
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
    request.validate()?;
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
        let phase_app = app.clone();
        let phase = |name: &str, progress: f64| {
            let _ = phase_app.emit(
                "ask.phase",
                json!({"operation_id": operation_id, "phase": name, "progress": progress}),
            );
        };
        let result = compute_answer(
            &request,
            &catalog,
            &models,
            &worker,
            &generation,
            &cancelled,
            &phase,
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
                for claim in &answer.claims {
                    if cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let _ = app.emit(
                        "ask.token",
                        json!({"operation_id": operation_id, "token": format!("{}\n", claim.text), "verified": true}),
                    );
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
    phase: &dyn Fn(&str, f64),
) -> Result<AnswerResult, AppError> {
    if cancelled.load(Ordering::Acquire) {
        return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
    }
    phase("understanding", 0.08);
    let embedding = models.active_artifact(ModelRole::Embedding)?;
    let generation_artifact = models.active_artifact(ModelRole::Generation)?;
    let maintenance = catalog.maintenance_snapshot()?;
    let index_coverage = if let Some(embedding) = embedding.as_ref() {
        catalog
            .semantic_index_coverage(&request.scope, &embedding.artifact_id.to_string())?
            .1
    } else {
        0.0
    };
    let degradation_reason = if generation_artifact.is_none() {
        Some("缺少已自检的本地生成模型".to_owned())
    } else if embedding.is_none() {
        Some("缺少已自检的中文 Embedding 模型".to_owned())
    } else if index_coverage <= 0.0 {
        Some("当前检索范围尚未建立语义索引".to_owned())
    } else if maintenance.degradation_level == "core" {
        Some("后台资源繁忙，语义检索与生成暂时暂停；搜索和预览仍可继续使用".to_owned())
    } else {
        None
    };
    let explicit_extracts = request.mode == AskMode::EvidenceExtracts;
    if !explicit_extracts && degradation_reason.is_some() && !request.allow_degraded_extractive {
        return Err(AppError::new(
            "RAG_DEGRADED_CONFIRMATION_REQUIRED",
            format!(
                "完整 RAG 暂不可用：{}。如需继续，请明确切换到证据摘录模式。",
                degradation_reason.as_deref().unwrap_or("组件未就绪")
            ),
            true,
        ));
    }
    if explicit_extracts || degradation_reason.is_some() {
        phase("evidence_retrieval", 0.35);
        let mut result = catalog.answer_extractively(request, None)?;
        result.degradation_reason =
            degradation_reason.or_else(|| Some("用户明确选择证据摘录模式".into()));
        result.index_coverage = index_coverage;
        result.retrieval_channels = vec!["filename".into(), "fts".into()];
        catalog.validate_answer_evidence(&result)?;
        catalog.record_ask_exchange(request, &result)?;
        phase("completed", 1.0);
        return Ok(result);
    }
    let embedding = embedding.expect("RAG readiness checked embedding");
    let generation_artifact = generation_artifact.expect("RAG readiness checked generation");
    let history = request
        .session_id
        .map(|session_id| catalog.load_ask_history(&session_id, 8))
        .transpose()?
        .unwrap_or_default();
    let retrieval_question = if history.is_empty() {
        request.question.trim().to_owned()
    } else {
        let history_text = history
            .iter()
            .map(|message| {
                format!(
                    "{}：{}",
                    message.role,
                    compact_for_prompt(&message.content, 500)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = format!(
            "对话历史：\n{history_text}\n\n当前追问：{}\n\n把当前追问改写成可独立检索的问题。只输出改写后的问题，不回答。",
            request.question.trim()
        );
        let rewritten = complete_with_model(
            generation,
            &generation_artifact,
            "你负责将连续追问改写为独立的中文检索问题，不得引入历史中不存在的事实。",
            &prompt,
            160,
            cancelled,
        )?;
        let rewritten = rewritten.trim();
        if rewritten.is_empty() {
            return Err(AppError::new(
                "RAG_QUERY_REWRITE_FAILED",
                "追问改写没有产生有效检索问题",
                true,
            ));
        }
        compact_for_prompt(rewritten, 2_000)
    };
    if cancelled.load(Ordering::Acquire) {
        return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
    }
    phase("hybrid_retrieval", 0.25);
    let tokenizer_path = PathBuf::from(&embedding.local_path)
        .parent()
        .map(|parent| parent.join("tokenizer.json"))
        .ok_or_else(|| {
            AppError::new(
                "EMBEDDING_TOKENIZER_MISSING",
                "Embedding 模型目录无效",
                false,
            )
        })?;
    if !tokenizer_path.is_file() {
        return Err(AppError::new(
            "EMBEDDING_TOKENIZER_MISSING",
            "Embedding tokenizer 不存在，完整 RAG 已停止",
            true,
        ));
    }
    let embedding_text = format!(
        "{}{}",
        embedding.query_prefix.as_deref().unwrap_or(""),
        retrieval_question
    );
    let response = worker.encode_embeddings(&EmbeddingRequest {
        model_path: embedding.local_path.clone(),
        tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
        texts: vec![embedding_text],
        max_length: embedding.max_length.unwrap_or(512),
        threads: 2,
    })?;
    let vector = response.vectors.first().ok_or_else(|| {
        AppError::new("EMBEDDING_EMPTY", "Embedding 运行时没有返回查询向量", true)
    })?;
    let mut retrieval_request = request.clone();
    retrieval_request.question = retrieval_question;
    let artifact_id = embedding.artifact_id.to_string();
    let mut extractive = catalog.answer_extractively(
        &retrieval_request,
        Some(SemanticQuery {
            model_artifact_id: &artifact_id,
            vector,
        }),
    )?;
    extractive.index_coverage = index_coverage;
    extractive.retrieval_channels = vec![
        "filename".into(),
        "fts".into(),
        "embedding".into(),
        "rrf".into(),
    ];
    if extractive.insufficient_evidence {
        extractive.answer_mode = "rag_refusal".into();
        catalog.record_ask_exchange(request, &extractive)?;
        phase("completed", 1.0);
        return Ok(extractive);
    }
    if maintenance.degradation_level == "full"
        && let Some(reranker) = models.active_artifact(ModelRole::Reranker)?
        && reranker.format == ModelFormat::Onnx
        && !extractive.claims.is_empty()
    {
        phase("reranking", 0.42);
        let tokenizer_path = PathBuf::from(&reranker.local_path)
            .parent()
            .map(|parent| parent.join("tokenizer.json"));
        if let Some(tokenizer_path) = tokenizer_path.filter(|path| path.is_file()) {
            let documents = extractive
                .claims
                .iter()
                .map(|claim| compact_for_prompt(&claim.text, 12_000))
                .collect::<Vec<_>>();
            if let Ok(response) = worker.rerank(&RerankRequest {
                model_path: reranker.local_path,
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                query: retrieval_request.question.clone(),
                documents,
                max_length: reranker.max_length.unwrap_or(512),
                threads: 2,
            }) && apply_rerank_scores(&mut extractive, &response.scores).is_ok()
            {
                extractive.retrieval_channels.push("reranker".into());
            }
        }
    }
    phase("evidence_selection", 0.48);
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
    if (runtime.active_model_path() != Some(generation_artifact.local_path.as_str())
        || !runtime.is_active())
        && runtime
            .activate(&generation_artifact.local_path, 4096, threads)
            .is_err()
    {
        return Err(AppError::new(
            "GENERATION_ACTIVATION_FAILED",
            "本地生成模型加载失败，未降级为关键词答案",
            true,
        ));
    }
    phase("generating", 0.62);
    let prompt = remin_core::generation_prompt(request, &extractive);
    let generated = runtime.complete_cancellable(
        "你是拾忆的本地资料回答器。只能使用用户提供的证据，严格保留[S数字]引用，不得补充外部知识。",
        &prompt,
        512,
        cancelled,
    )
    .inspect_err(|error| {
        if error.code == "OPERATION_CANCELLED" {
            runtime.stop();
        }
    })?;
    drop(runtime);
    phase("citation_validation", 0.88);
    let mut grounded =
        remin_core::apply_grounded_generation(&extractive, &generated).ok_or_else(|| {
            AppError::new(
                "RAG_CITATION_VALIDATION_FAILED",
                "生成内容未通过逐句引用校验，回答已拒绝显示；不会回退为伪装的关键词答案",
                true,
            )
        })?;
    for claim in &grounded.claims {
        let evidence = claim
            .citations
            .iter()
            .enumerate()
            .map(|(index, citation)| format!("[E{}] {}", index + 1, citation.quote))
            .collect::<Vec<_>>()
            .join("\n");
        let verification = complete_with_model(
            generation,
            &generation_artifact,
            "你是严格的中文证据核验器。判断事实句是否完全由给定原文证据支持，只输出SUPPORTED或UNSUPPORTED。",
            &format!("事实句：{}\n\n原文证据：\n{}", claim.text, evidence),
            32,
            cancelled,
        )?;
        if !claim_support_is_verified(&verification) {
            return Err(AppError::new(
                "RAG_CLAIM_UNSUPPORTED",
                "至少一个事实句未通过原文支持性校验，整条回答已拒绝显示",
                true,
            ));
        }
    }
    grounded.index_coverage = index_coverage;
    grounded.retrieval_channels = extractive.retrieval_channels;
    catalog.validate_answer_evidence(&grounded)?;
    catalog.record_ask_exchange(request, &grounded)?;
    phase("completed", 1.0);
    Ok(grounded)
}

fn apply_rerank_scores(result: &mut AnswerResult, scores: &[f32]) -> Result<(), AppError> {
    if scores.len() != result.claims.len() || scores.iter().any(|score| !score.is_finite()) {
        return Err(AppError::new(
            "RERANK_OUTPUT_INVALID",
            "重排分数数量或数值无效，已保留融合检索顺序",
            false,
        ));
    }
    let mut ranked = result
        .claims
        .drain(..)
        .zip(scores.iter().copied())
        .collect::<Vec<_>>();
    for (claim, score) in &mut ranked {
        for citation in &mut claim.citations {
            citation.retrieval_score = *score;
        }
    }
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
    result.claims = ranked.into_iter().map(|(claim, _)| claim).collect();
    Ok(())
}

fn compact_for_prompt(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(limit).collect()
}

fn complete_with_model(
    generation: &Mutex<LocalGenerationRuntime>,
    artifact: &ModelArtifact,
    system_prompt: &str,
    prompt: &str,
    max_tokens: u32,
    cancelled: &AtomicBool,
) -> Result<String, AppError> {
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
    if runtime.active_model_path() != Some(artifact.local_path.as_str()) || !runtime.is_active() {
        runtime.activate(&artifact.local_path, 4096, threads)?;
    }
    runtime.complete_cancellable(system_prompt, prompt, max_tokens, cancelled)
}

fn claim_support_is_verified(value: &str) -> bool {
    let normalized = value.to_ascii_uppercase();
    !normalized.contains("UNSUPPORTED") && normalized.contains("SUPPORTED")
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
    enqueue_scans(app, catalog, [(root_id, job_id)]);
}

pub(crate) fn spawn_scan_queue(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    recovered: Vec<(Uuid, JobRecord)>,
) {
    if recovered.is_empty() {
        return;
    }
    enqueue_scans(
        app,
        catalog,
        recovered
            .into_iter()
            .map(|(root_id, job)| (root_id, job.job_id)),
    );
}

fn enqueue_scans(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    scans: impl IntoIterator<Item = (Uuid, Uuid)>,
) {
    let state = app.state::<ScanCoordinatorState>();
    if let Ok(mut queue) = state.queue.lock() {
        for scan in scans {
            if !queue.iter().any(|queued| queued.1 == scan.1) {
                queue.push_back(scan);
            }
        }
    } else {
        let _ = app.emit(
            "catalog.watch_degraded",
            AppError::new("SCAN_QUEUE_UNAVAILABLE", "扫描调度队列暂时不可用", true),
        );
        return;
    }
    if state.running.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(move || {
        loop {
            let next = app
                .state::<ScanCoordinatorState>()
                .queue
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front());
            if let Some((root_id, job_id)) = next {
                run_scan(&app, &catalog, root_id, job_id);
                continue;
            }
            let state = app.state::<ScanCoordinatorState>();
            state.running.store(false, Ordering::Release);
            let has_more = state
                .queue
                .lock()
                .map(|queue| !queue.is_empty())
                .unwrap_or(false);
            if has_more && !state.running.swap(true, Ordering::AcqRel) {
                continue;
            }
            break;
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
        let worker = app.state::<WorkerServiceState>();
        if worker.running.swap(true, Ordering::AcqRel) {
            return;
        }
        let _running_reset = RunningReset(&worker.running);
        loop {
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
                break;
            }
            let pending = match catalog.list_pending_parse_files(batch_size) {
                Ok(files) => files,
                Err(error) => {
                    let _ = app.emit("index.failed", error);
                    break;
                }
            };
            if pending.is_empty() {
                break;
            }
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
                    asset_cache_dir: image_asset_cache_dir(&app, &revision_id),
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
                        image_assets: vec![],
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
            thread::yield_now();
        }
        drop(_running_reset);
        spawn_image_understanding_pending(app.clone(), Arc::clone(&catalog));
        spawn_embed_pending(app, catalog);
    });
}

fn image_asset_cache_dir(app: &AppHandle, revision_id: &Uuid) -> Option<String> {
    app.path()
        .app_cache_dir()
        .ok()
        .map(|path| path.join("image-assets").join(revision_id.to_string()))
        .map(|path| path.to_string_lossy().into_owned())
}

#[derive(Debug, Deserialize)]
struct VisionDescriptionPayload {
    summary: String,
    #[serde(default)]
    visible_text: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    entities: Vec<String>,
    #[serde(default)]
    chart_summary: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VisionQuestionPayload {
    answer: String,
    #[serde(default)]
    observations: Vec<String>,
    #[serde(default)]
    uncertainties: Vec<String>,
}

fn parse_vision_question_answer(value: &str) -> Result<VisionQuestionPayload, AppError> {
    let trimmed = value.trim();
    let mut parsed = serde_json::from_str::<VisionQuestionPayload>(trimmed)
        .or_else(|_| {
            let start = trimmed.find('{').ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing JSON object",
                ))
            })?;
            let end = trimmed.rfind('}').ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing JSON object",
                ))
            })?;
            serde_json::from_str(&trimmed[start..=end])
        })
        .map_err(|error| {
            AppError::new(
                "VISION_RESPONSE_SCHEMA_INVALID",
                format!("本地多模态模型没有返回有效的原图分析JSON：{error}"),
                true,
            )
        })?;
    parsed.answer = parsed.answer.trim().chars().take(6_000).collect();
    if parsed.answer.is_empty() {
        return Err(AppError::new(
            "VISION_RESPONSE_SCHEMA_INVALID",
            "本地多模态模型返回的原图分析为空",
            true,
        ));
    }
    let normalize = |values: Vec<String>| {
        values
            .into_iter()
            .map(|value| value.trim().chars().take(500).collect::<String>())
            .filter(|value| !value.is_empty())
            .take(32)
            .collect::<Vec<_>>()
    };
    parsed.observations = normalize(parsed.observations);
    parsed.uncertainties = normalize(parsed.uncertainties);
    Ok(parsed)
}

fn parse_vision_description(value: &str) -> Result<VisionDescriptionPayload, AppError> {
    let trimmed = value.trim();
    let mut parsed = serde_json::from_str::<VisionDescriptionPayload>(trimmed)
        .or_else(|_| {
            let start = trimmed.find('{').ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing JSON object",
                ))
            })?;
            let end = trimmed.rfind('}').ok_or_else(|| {
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing JSON object",
                ))
            })?;
            serde_json::from_str(&trimmed[start..=end])
        })
        .map_err(|error| {
            AppError::new(
                "VISION_RESPONSE_SCHEMA_INVALID",
                format!("本地多模态模型没有返回有效的图片说明JSON：{error}"),
                true,
            )
        })?;
    parsed.summary = parsed.summary.trim().chars().take(6_000).collect();
    if parsed.summary.is_empty() {
        return Err(AppError::new(
            "VISION_RESPONSE_SCHEMA_INVALID",
            "本地多模态模型返回的图片摘要为空",
            true,
        ));
    }
    parsed.visible_text = parsed
        .visible_text
        .take()
        .map(|value| value.trim().chars().take(12_000).collect::<String>())
        .filter(|value| !value.is_empty());
    parsed.chart_summary = parsed
        .chart_summary
        .take()
        .map(|value| value.trim().chars().take(4_000).collect::<String>())
        .filter(|value| !value.is_empty());
    let normalize_list = |values: Vec<String>| {
        values
            .into_iter()
            .map(|value| value.trim().chars().take(80).collect::<String>())
            .filter(|value| !value.is_empty())
            .take(32)
            .collect::<Vec<_>>()
    };
    parsed.keywords = normalize_list(parsed.keywords);
    parsed.entities = normalize_list(parsed.entities);
    Ok(parsed)
}

pub(crate) fn spawn_image_understanding_pending(app: AppHandle, catalog: Arc<CatalogService>) {
    thread::spawn(move || {
        struct RunningReset<'a>(&'a AtomicBool);
        impl Drop for RunningReset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }

        if !background_storage_budget_allows(&app, &catalog) {
            return;
        }
        let models = app.state::<ModelServiceState>();
        let models = match models.get() {
            Ok(models) => models,
            Err(_) => return,
        };
        let artifact = match models.active_artifact(ModelRole::Vision) {
            Ok(Some(artifact)) => artifact,
            Ok(None) => return,
            Err(error) => {
                let _ = app.emit("vision.failed", error);
                return;
            }
        };
        let projector = match models.vision_projector_path(&artifact) {
            Ok(path) => path,
            Err(error) => {
                let _ = app.emit("vision.failed", error);
                return;
            }
        };
        let worker = app.state::<WorkerServiceState>();
        if worker.vision_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let _running_reset = RunningReset(&worker.vision_running);
        let generation = app.state::<GenerationServiceState>();
        let model_artifact_id = artifact.artifact_id.to_string();
        let projector_path = projector.to_string_lossy().into_owned();
        let threads = std::thread::available_parallelism()
            .map(|value| value.get() as u32)
            .unwrap_or(2)
            .clamp(1, 8);
        let mut committed = 0_u64;
        loop {
            let degradation = catalog
                .maintenance_snapshot()
                .map(|snapshot| snapshot.degradation_level)
                .unwrap_or_else(|_| "balanced".to_owned());
            if degradation == "core" {
                break;
            }
            let pending = match catalog.claim_pending_image_understanding(&model_artifact_id) {
                Ok(Some(pending)) => pending,
                Ok(None) => match catalog.image_understanding_stats() {
                    Ok((_, _, pending)) if pending > 0 => {
                        thread::yield_now();
                        continue;
                    }
                    _ => break,
                },
                Err(error) => {
                    let _ = app.emit("vision.failed", error);
                    break;
                }
            };
            let _ = app.emit(
                "vision.progress",
                json!({
                    "asset_id": pending.asset_id,
                    "revision_id": pending.revision_id,
                    "stage": "understanding",
                    "attempt": pending.attempt_count,
                }),
            );
            let operation = (|| {
                let mut runtime = generation.0.lock().map_err(|_| {
                    AppError::new(
                        "VISION_RUNTIME_LOCK_FAILED",
                        "图片理解运行时状态已损坏",
                        true,
                    )
                })?;
                if runtime.active_model_path() != Some(artifact.local_path.as_str())
                    || runtime.active_mmproj_path() != Some(projector_path.as_str())
                    || !runtime.is_active()
                {
                    runtime.activate_multimodal(
                        &artifact.local_path,
                        &projector_path,
                        4096,
                        threads,
                    )?;
                }
                let location = serde_json::to_string(&pending.locator).map_err(|error| {
                    AppError::new("IMAGE_ASSET_INVALID", error.to_string(), false)
                })?;
                let ocr_hint = pending
                    .ocr_text
                    .as_deref()
                    .map(|value| compact_for_prompt(value, 4_000))
                    .unwrap_or_default();
                let prompt = format!(
                    "请理解这张本地资料图片。位置元数据：{location}\n已有OCR（可能为空或有误）：{ocr_hint}\n只输出一个JSON对象，不要Markdown：{{\"summary\":\"客观完整的中文摘要\",\"visible_text\":\"可辨认文字或null\",\"keywords\":[\"关键词\"],\"entities\":[\"人名/机构/地点/产品\"],\"chart_summary\":\"若为图表则描述坐标、趋势与关键数值，否则null\"}}。不得猜测看不清的内容。"
                );
                let cancelled = AtomicBool::new(false);
                let response = runtime.describe_image_cancellable(
                    "你是拾忆的本地图片与图表理解器。只描述图中可验证内容，并严格输出指定JSON。",
                    &prompt,
                    Path::new(&pending.cache_path),
                    &pending.mime_type,
                    768,
                    &cancelled,
                )?;
                let description = parse_vision_description(&response)?;
                Ok::<_, AppError>(ImageUnderstandingResult {
                    asset_id: pending.asset_id,
                    revision_id: pending.revision_id,
                    model_artifact_id: model_artifact_id.clone(),
                    summary: description.summary,
                    visible_text: description.visible_text,
                    keywords: description.keywords,
                    entities: description.entities,
                    chart_summary: description.chart_summary,
                    idempotency_key: pending.idempotency_key.clone(),
                })
            })();
            match operation.and_then(|result| catalog.commit_image_understanding(&result)) {
                Ok(()) => {
                    committed = committed.saturating_add(1);
                    let _ = app.emit(
                        "vision.completed",
                        json!({"asset_id": pending.asset_id, "revision_id": pending.revision_id}),
                    );
                    let _ = app.emit("index.changed", pending.file_id.to_string());
                    if committed.is_multiple_of(8) {
                        spawn_embed_pending(app.clone(), Arc::clone(&catalog));
                    }
                }
                Err(error) => {
                    let _ = catalog.fail_image_understanding(&pending.asset_id, &error);
                    let _ = app.emit(
                        "vision.failed",
                        json!({"asset_id": pending.asset_id, "error": error}),
                    );
                }
            }
            thread::yield_now();
        }
        if let Ok(mut runtime) = generation.0.lock()
            && runtime.active_model_path() == Some(artifact.local_path.as_str())
            && runtime.active_mmproj_path() == Some(projector_path.as_str())
        {
            runtime.stop();
        }
        drop(_running_reset);
        if committed > 0 {
            spawn_embed_pending(app, catalog);
        }
    });
}

pub(crate) fn spawn_embed_pending(app: AppHandle, catalog: Arc<CatalogService>) {
    let worker = app.state::<WorkerServiceState>();
    worker.embedding_reschedule.store(true, Ordering::Release);
    if worker.embedding_running.swap(true, Ordering::AcqRel) {
        return;
    }
    thread::spawn(move || {
        struct RunningReset<'a>(&'a AtomicBool);
        impl Drop for RunningReset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let worker = app.state::<WorkerServiceState>();
        let _running_reset = RunningReset(&worker.embedding_running);
        loop {
            worker.embedding_reschedule.store(false, Ordering::Release);
            run_embedding_cycle(&app, &catalog, &worker);
            if !worker.embedding_reschedule.swap(false, Ordering::AcqRel) {
                break;
            }
        }
    });
}

fn run_embedding_cycle(app: &AppHandle, catalog: &CatalogService, worker: &WorkerServiceState) {
    if !background_storage_budget_allows(app, catalog) {
        return;
    }
    let models_state = app.state::<ModelServiceState>();
    let models = match models_state.get() {
        Ok(models) => models,
        Err(error) => {
            let _ = app.emit("embedding.failed", error);
            return;
        }
    };
    let pending = match models.pending_embedding_activation() {
        Ok(Some(pending)) if pending.status == "indexing" => Some(pending),
        Ok(_) => None,
        Err(error) => {
            let _ = app.emit("embedding.failed", error);
            return;
        }
    };
    let artifact = match pending.as_ref() {
        Some(pending) => models.artifact_by_id(&pending.artifact_id),
        None => models
            .active_artifact(ModelRole::Embedding)
            .and_then(|artifact| {
                artifact.ok_or_else(|| {
                    AppError::new(
                        "RAG_EMBEDDING_MISSING",
                        "尚未启用可用于后台索引的 Embedding 模型",
                        true,
                    )
                })
            }),
    };
    let artifact = match artifact {
        Ok(artifact) => artifact,
        Err(error) => {
            if pending.is_some() {
                record_embedding_activation_failure(
                    app,
                    catalog,
                    &models,
                    pending.as_ref(),
                    &error,
                );
            }
            return;
        }
    };
    let tokenizer_path = match PathBuf::from(&artifact.local_path)
        .parent()
        .map(|parent| parent.join("tokenizer.json"))
        .filter(|path| path.is_file())
    {
        Some(path) => path,
        None => {
            let error = AppError::new(
                "EMBEDDING_TOKENIZER_UNAVAILABLE",
                "Embedding 模型缺少受管理的 tokenizer.json",
                false,
            );
            record_embedding_activation_failure(app, catalog, &models, pending.as_ref(), &error);
            return;
        }
    };
    let model_artifact_id = artifact.artifact_id.to_string();
    let expected_dimension = pending
        .as_ref()
        .map(|pending| pending.dimension)
        .or(artifact.embedding_dimension);
    let mut committed_total = 0_u64;
    let mut completed_dimension = expected_dimension;
    let result = (|| -> Result<bool, AppError> {
        loop {
            if let Some(expected) = pending.as_ref() {
                let still_current = models
                    .pending_embedding_activation()?
                    .is_some_and(|current| {
                        current.artifact_id == expected.artifact_id
                            && current.dimension == expected.dimension
                            && current.status == "indexing"
                    });
                if !still_current {
                    return Ok(false);
                }
            }
            let degradation = catalog
                .maintenance_snapshot()
                .map(|snapshot| snapshot.degradation_level)
                .unwrap_or_else(|_| "balanced".to_owned());
            if degradation == "core" {
                return Ok(false);
            }
            let chunks = catalog.list_pending_embedding_chunks(&model_artifact_id, 16)?;
            if chunks.is_empty() {
                break;
            }
            let response = worker.client.encode_embeddings(&EmbeddingRequest {
                model_path: artifact.local_path.clone(),
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                texts: chunks.iter().map(|chunk| chunk.text.clone()).collect(),
                max_length: artifact.max_length.unwrap_or(512),
                threads: 2,
            })?;
            if response.dimension == 0
                || expected_dimension.is_some_and(|dimension| dimension != response.dimension)
                || response.vectors.len() != chunks.len()
                || response
                    .vectors
                    .iter()
                    .any(|vector| vector.len() != response.dimension as usize)
            {
                return Err(AppError::new(
                    "EMBEDDING_OUTPUT_INVALID",
                    "向量数量或维度与模型自检结果不一致",
                    false,
                ));
            }
            let inputs = chunks
                .iter()
                .zip(response.vectors)
                .map(|(chunk, vector)| ChunkEmbeddingInput {
                    chunk_id: chunk.chunk_id,
                    vector,
                })
                .collect::<Vec<_>>();
            let committed =
                catalog.commit_chunk_embeddings(&model_artifact_id, response.dimension, &inputs)?;
            if committed == 0 {
                break;
            }
            committed_total = committed_total.saturating_add(committed);
            completed_dimension = Some(response.dimension);
            let _ = app.emit("embedding.changed", committed);
            thread::yield_now();
        }

        let searchable_chunks = catalog.maintenance_snapshot()?.searchable_chunks;
        let Some(dimension) = completed_dimension else {
            if searchable_chunks == 0 {
                return Ok(true);
            }
            return Err(AppError::new(
                "EMBEDDING_OUTPUT_INVALID",
                "Embedding 模型没有已验证的向量维度",
                false,
            ));
        };
        let existing_generation = catalog.active_vector_generation(&model_artifact_id)?;
        let needs_rebuild = searchable_chunks > 0
            && (committed_total > 0
                || existing_generation
                    .as_ref()
                    .is_none_or(|generation| generation.dimension != dimension));
        if needs_rebuild {
            let _ = app.emit("embedding.index_phase", "building");
            let generation = catalog.rebuild_vector_generation(&model_artifact_id, dimension)?;
            if generation.status != "active"
                || generation.dimension != dimension
                || generation.item_count == 0
                || generation.coverage < 0.999_999
            {
                return Err(AppError::new(
                    "VECTOR_INDEX_INCOMPLETE",
                    "新语义索引未覆盖当前全部有效分块，未切换活动 Embedding",
                    true,
                ));
            }
            let _ = app.emit("embedding.index_phase", "active");
            let _ = app.emit("embedding.index_changed", generation);
        } else if searchable_chunks > 0 {
            let generation = existing_generation.ok_or_else(|| {
                AppError::new(
                    "VECTOR_INDEX_INCOMPLETE",
                    "新语义索引尚未建立，未切换活动 Embedding",
                    true,
                )
            })?;
            if generation.dimension != dimension
                || generation.item_count == 0
                || generation.coverage < 0.999_999
            {
                return Err(AppError::new(
                    "VECTOR_INDEX_INCOMPLETE",
                    "新语义索引尚未通过覆盖率校验，未切换活动 Embedding",
                    true,
                ));
            }
        }
        Ok(true)
    })();

    match result {
        Ok(true) => {
            if let Some(pending) = pending {
                match models.complete_embedding_activation(&pending.artifact_id, pending.dimension)
                {
                    Ok(completed) => {
                        finalize_embedding_download(app, &models, &completed);
                        if let Ok(state) = model_state_from_manager(&models, Some(catalog)) {
                            let _ = app.emit("model.state", state);
                        }
                    }
                    Err(error) => {
                        record_embedding_activation_failure(
                            app,
                            catalog,
                            &models,
                            Some(&pending),
                            &error,
                        );
                    }
                }
            }
        }
        Ok(false) => {}
        Err(error) => {
            let _ = app.emit("embedding.index_phase", "fallback");
            record_embedding_activation_failure(app, catalog, &models, pending.as_ref(), &error);
        }
    }
}

fn record_embedding_activation_failure(
    app: &AppHandle,
    catalog: &CatalogService,
    models: &ModelManager,
    pending: Option<&PendingEmbeddingActivation>,
    error: &AppError,
) {
    let mut download_failed = false;
    if let Some(pending) = pending {
        let _ = models.fail_embedding_activation(&pending.artifact_id, error);
        if let Some(job_id) = pending.download_job_id
            && let Ok(mut job) = models.download_job(&job_id)
        {
            download_failed = true;
            job.status = "failed".into();
            job.phase = "failed".into();
            job.bytes_per_second = 0;
            job.eta_seconds = None;
            job.error = Some(error.clone());
            if let Ok(job) = models.update_download_job(&job) {
                emit_download_state(app, &job);
            }
        }
    }
    let _ = app.emit("embedding.failed", error.clone());
    if download_failed {
        let _ = app.emit("model.download_failed", error.clone());
    }
    if let Ok(state) = model_state_from_manager(models, Some(catalog)) {
        let _ = app.emit("model.state", state);
    }
}

fn finalize_embedding_download(
    app: &AppHandle,
    models: &ModelManager,
    completed: &PendingEmbeddingActivation,
) {
    if let Some(job_id) = completed.download_job_id
        && let Ok(mut job) = models.download_job(&job_id)
    {
        job.status = "completed".into();
        job.phase = "completed".into();
        job.current_file = None;
        job.bytes_per_second = 0;
        job.eta_seconds = Some(0);
        job.error = None;
        for file in &mut job.files {
            file.status = "completed".into();
            file.downloaded_bytes = file.total_bytes;
        }
        if let Ok(job) = models.update_download_job(&job) {
            emit_download_state(app, &job);
            let _ = app.emit("model.download_completed", &job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remin_core::{
        AnswerClaim, AnswerSourceFile, EvidenceRef, GroundingStatus, SourceLocator, SupportStatus,
    };

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

    #[test]
    fn vision_description_requires_structured_bounded_json() {
        let parsed = parse_vision_description(
            r#"```json
            {"summary":"柱状图显示收入增长","visible_text":"收入 128 万元","keywords":["收入","增长"],"entities":["第二季度"],"chart_summary":"第二季度最高"}
            ```"#,
        )
        .expect("parse fenced vision JSON");
        assert_eq!(parsed.summary, "柱状图显示收入增长");
        assert_eq!(parsed.keywords, vec!["收入", "增长"]);
        assert_eq!(
            parse_vision_description(r#"{"summary":""}"#)
                .expect_err("empty summary must fail")
                .code,
            "VISION_RESPONSE_SCHEMA_INVALID"
        );
    }

    #[test]
    fn vision_question_answer_requires_grounded_structured_output() {
        let parsed = parse_vision_question_answer(
            r#"回答如下：{"answer":"图中第二季度柱形最高。","observations":["第二季度柱形高于第一季度"],"uncertainties":["纵轴单位看不清"]}"#,
        )
        .expect("parse bounded image answer");
        assert_eq!(parsed.answer, "图中第二季度柱形最高。");
        assert_eq!(parsed.observations.len(), 1);
        assert_eq!(parsed.uncertainties, vec!["纵轴单位看不清"]);
        assert_eq!(
            parse_vision_question_answer(r#"{"answer":""}"#)
                .expect_err("empty image answer must fail")
                .code,
            "VISION_RESPONSE_SCHEMA_INVALID"
        );
    }

    #[test]
    fn rerank_scores_reorder_claims_and_update_all_citations() {
        let first_file = Uuid::now_v7();
        let second_file = Uuid::now_v7();
        let claim = |file_id, text: &str| AnswerClaim {
            claim_id: Uuid::now_v7(),
            text: text.into(),
            support_status: SupportStatus::Supported,
            citations: vec![EvidenceRef {
                evidence_id: Uuid::now_v7(),
                file_id,
                revision_id: Uuid::now_v7(),
                node_id: Uuid::now_v7(),
                chunk_id: Uuid::now_v7(),
                image_asset_id: None,
                quote: text.into(),
                locator: SourceLocator::default(),
                retrieval_score: 0.0,
            }],
        };
        let mut result = AnswerResult {
            session_id: Uuid::now_v7(),
            message_id: Uuid::now_v7(),
            answer: String::new(),
            grounding_status: GroundingStatus::Grounded,
            insufficient_evidence: false,
            claims: vec![claim(first_file, "低相关"), claim(second_file, "高相关")],
            source_files: vec![
                AnswerSourceFile {
                    file_id: first_file,
                    display_name: "低相关.txt".into(),
                    canonical_path: "资料/低相关.txt".into(),
                },
                AnswerSourceFile {
                    file_id: second_file,
                    display_name: "高相关.txt".into(),
                    canonical_path: "资料/高相关.txt".into(),
                },
            ],
            used_file_ids: vec![first_file, second_file],
            elapsed_ms: 1,
            answer_mode: "extractive".into(),
            retrieval_channels: vec!["fts".into()],
            index_coverage: 1.0,
            degradation_reason: None,
        };

        apply_rerank_scores(&mut result, &[0.1, 0.9]).expect("apply valid rerank scores");

        assert_eq!(result.claims[0].text, "高相关");
        assert_eq!(result.claims[0].citations[0].retrieval_score, 0.9);
        assert_eq!(result.claims[1].citations[0].retrieval_score, 0.1);
        assert_eq!(
            apply_rerank_scores(&mut result, &[f32::NAN])
                .expect_err("invalid score count and value must fail")
                .code,
            "RERANK_OUTPUT_INVALID"
        );
    }
}
