use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use fanfan_core::{
    AddRootRequest, AiRuntimeSnapshot, AnswerClaim, AnswerResult, AppError, AskMessagePage,
    AskRequest, AskSessionPage, CandidateRoot, CatalogService, ChunkEmbeddingInput,
    CollectionModelReview, CollectionRecord, CollectionRule, CollectionSuggestion,
    CollectionSuggestionPage, CollectionSuggestionQuery, CollectionSuggestionRefreshResult,
    CollectionSuggestionUpdateRequest, CreateCollectionRequest, DegradationLevel, DownloadFile,
    DownloadedModelMetadata, EmbeddingRequest, ExclusionRule, ExclusionRuleInput, ExportResult,
    FilePage, FilePreview, FileQuery, FileRecord, ImageUnderstandingResult, ImportCandidate,
    InboxItem, InboxPage, InboxQuery, InboxUpdateRequest, IncrementalWatchManager,
    IndexActivityStats, Intent,
    JobRecord, LocalGenerationRuntime, LogPage, LogQuery, MaintenanceSnapshot, ModelArtifact,
    ModelCatalogEntry, ModelDownloadFileProgress, ModelDownloadJob, ModelDownloadRemoval,
    ModelEdition, ModelFormat, ModelImportSelection, ModelManager, ModelPreset, ModelRole,
    ModelRoleConfig, ModelSource, ModelStoreStatus, NodeTracePage, NodeTraceQuery, OcrRuntimeConfig,
    ParseMetrics, ParseOutcome, ParseRequest, ParseResult, PendingEmbeddingActivation,
    RagReadiness, RelationGroupPage, RelationGroupQuery, RelationPage, RelationQuery,
    RelationRefreshResult, RerankRequest,
    RootRecord, RouteDecision,
    RuntimeBackendKind, RuntimeCapability, RuntimeManager, RuntimeResourceBudget, RuntimeTaskKind,
    RuntimeTaskRequest, ScopeFilter, SearchMode, SearchRequest, SearchSession, SemanticQuery,
    SpeechRecognitionRequest, SpeechRecognitionResult, SpeechSynthesisRequest,
    SpeechSynthesisResult, TriageStatus, WorkerClient, AskMessage, chat_prompt,
    chat_prompt_mini, intent_routing_prompt, intent_routing_prompt_mini,
    is_natural_language_query,
    parse_intent_verdict, parse_query_intent, parse_rewritten_queries,
    query_rewrite_prompt, query_understanding_prompt, strip_long_path_prefix,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use windows::{
    Win32::{
        Storage::FileSystem::GetDiskFreeSpaceExW,
        System::SystemInformation::{
            GetLogicalProcessorInformationEx, GlobalMemoryStatusEx, MEMORYSTATUSEX,
            RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
        },
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
    pub foreground_activity: AtomicU32,
}

#[derive(Clone)]
pub struct SpeechWorkerState(pub WorkerClient);

#[derive(Debug, Clone, Serialize)]
pub struct SpeechRecognitionSession {
    session_id: Uuid,
    status: &'static str,
    result: SpeechRecognitionResult,
    completed_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpeechSynthesisSession {
    session_id: Uuid,
    status: &'static str,
    message_id: Uuid,
    result: SpeechSynthesisResult,
    completed_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeechRecognitionInput {
    samples: Vec<f32>,
    sample_rate: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeechSynthesisInput {
    message_id: Uuid,
    #[serde(default)]
    speaker_id: u32,
    speed: f32,
}

struct ForegroundActivityGuard<'a>(&'a AtomicU32);

impl ForegroundActivityGuard<'_> {
    fn begin(counter: &AtomicU32) -> ForegroundActivityGuard<'_> {
        counter.fetch_add(1, Ordering::AcqRel);
        ForegroundActivityGuard(counter)
    }
}

impl Drop for ForegroundActivityGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct GenerationServiceState(pub Arc<Mutex<LocalGenerationRuntime>>);

#[derive(Clone)]
pub struct RuntimeManagerState(pub RuntimeManager);

pub fn create_runtime_manager_state() -> RuntimeManagerState {
    let total_memory = memory_status_bytes()
        .map(|(total, _)| total)
        .unwrap_or(8 * 1024 * 1024 * 1024);
    RuntimeManagerState(RuntimeManager::new(RuntimeResourceBudget::conservative(
        physical_core_count(),
        total_memory,
    )))
}

const DOWNLOAD_ACTION_RUN: u8 = 0;
const DOWNLOAD_ACTION_PAUSE: u8 = 1;
const DOWNLOAD_ACTION_CANCEL: u8 = 2;
type DownloadLogCheckpoint = (String, String, u8);
type DownloadLogCheckpoints = Mutex<HashMap<Uuid, DownloadLogCheckpoint>>;
static DOWNLOAD_LOG_CHECKPOINTS: OnceLock<DownloadLogCheckpoints> = OnceLock::new();
static PHYSICAL_CORE_COUNT: OnceLock<u32> = OnceLock::new();
static MEMORY_PRESSURE_ACTIVE: AtomicBool = AtomicBool::new(false);

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

    fn is_running(&self, job_id: &Uuid) -> bool {
        self.running
            .lock()
            .is_ok_and(|running| running.contains(job_id))
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
pub struct AppStatusScanProgress {
    scan_job_id: Uuid,
    status: fanfan_core::JobStatus,
    discovered_files: u64,
    searchable_files: u64,
    parsed_files: u64,
    embedded_files: u64,
    ocr_pages: u64,
    progress: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct AppStatusSnapshot {
    local_only: bool,
    source_files_readonly: bool,
    roots: Vec<RootRecord>,
    scan_progress: Option<AppStatusScanProgress>,
    maintenance: MaintenanceSnapshot,
    inference_runtime: InferenceRuntimeState,
    ai_runtime: AiRuntimeSnapshot,
    recovery_actions: Vec<&'static str>,
    checked_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AskOperationSnapshot {
    handle: OperationHandle,
    result: Option<AnswerResult>,
    error: Option<AppError>,
}

type AskProgressCallbacks<'a> = (&'a dyn Fn(&str, f64), &'a dyn Fn(&AnswerClaim));

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
    pub config_directory: PathBuf,
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
    runtime_backend: Option<String>,
    runtime_devices: Vec<String>,
    gpu_runtime_available: bool,
    checked_at: String,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelRuntimeState {
    status: &'static str,
    active_profile_id: Option<String>,
    active_profile_name: Option<String>,
    runtime_backend: Option<String>,
    inference_runtime: InferenceRuntimeState,
    checked_at: String,
    capabilities: ModelCapabilities,
    rag_complete: bool,
    semantic_index_coverage: f64,
    embedding_migration: Option<EmbeddingMigrationState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InferenceRuntimeState {
    backend: String,
    device_names: Vec<String>,
    gpu_available: bool,
    gpu_offload_layers: Option<u32>,
    gpu_offload_mode: String,
    thread_budget: u32,
    batch_thread_budget: u32,
    active: bool,
    pressure_reason: Option<String>,
    hardware: HardwareProfile,
    runtime_package: RuntimeBackendPackage,
    budget: InferenceBudget,
}

#[derive(Debug, Clone, Serialize)]
pub struct HardwareProfile {
    physical_core_count: u32,
    logical_thread_count: u32,
    memory_total_bytes: Option<u64>,
    memory_available_bytes: Option<u64>,
    gpu_name: Option<String>,
    gpu_memory_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeBackendPackage {
    backend: String,
    device_count: u32,
    gpu_capable: bool,
    cpu_fallback_available: bool,
    validated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InferenceBudget {
    foreground_threads: u32,
    background_threads: u32,
    batch_size: u32,
    ubatch_size: u32,
    gpu_reserve_bytes: u64,
    system_memory_reserve_bytes: u64,
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
    tts: bool,
    asr: bool,
}

fn detect_environment(
    data_directory: &Path,
    runtime_capability: Option<&RuntimeCapability>,
    cached_gpu: Option<(Option<String>, Option<u64>)>,
) -> EnvironmentCheck {
    let memory_total_gb = memory_total_gb();
    let disk_available_gb = disk_available_gb(data_directory);
    // 显卡信息优先取 llama.cpp --list-devices 探测结果（比 PowerShell WMI 更接近
    // 运行时实际能力）；后台探测未完成时回退到落盘缓存（上次成功结果）。
    let (gpu_name, gpu_memory_gb) =
        match gpu_details_from_devices(runtime_capability.map(|c| c.devices.as_slice())) {
            Some(details) => details,
            None => cached_gpu.unwrap_or((None, None)),
        };
    let mut warnings = Vec::new();
    if memory_total_gb.is_none() {
        warnings.push("无法读取系统内存信息".to_owned());
    }
    if disk_available_gb.is_none() {
        warnings.push("无法读取应用数据磁盘剩余空间".to_owned());
    }
    if let Some(name) = &gpu_name {
        if runtime_capability.is_some_and(|capability| capability.gpu_available) {
            warnings.push(format!("检测到GPU：{name}；本地运行时已启用GPU后端"));
        } else {
            warnings.push(format!(
                "检测到GPU：{name}；当前安装的llama.cpp运行时未识别GPU，将安全回退到CPU"
            ));
        }
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
        runtime_backend: runtime_capability.map(|capability| capability.backend.clone()),
        runtime_devices: runtime_capability
            .map(|capability| capability.devices.clone())
            .unwrap_or_default(),
        gpu_runtime_available: runtime_capability
            .is_some_and(|capability| capability.gpu_available),
        checked_at: Utc::now().to_rfc3339(),
        warnings,
    }
}

#[cfg(windows)]
fn memory_total_gb() -> Option<u64> {
    memory_status_bytes().map(|(total, _)| total / 1024 / 1024 / 1024)
}

#[cfg(windows)]
fn memory_status_bytes() -> Option<(u64, u64)> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut status) }
        .ok()
        .map(|_| (status.ullTotalPhys, status.ullAvailPhys))
}

#[cfg(not(windows))]
fn memory_total_gb() -> Option<u64> {
    None
}

#[cfg(not(windows))]
fn memory_status_bytes() -> Option<(u64, u64)> {
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

/// 从 llama.cpp `--list-devices` 探测结果解析 GPU 名称与显存（替代 PowerShell
/// WMI 探测：每次启动跑 powershell.exe 1-3s，发布后用户机器上一样执行）。
/// 设备行形如 `CUDA0: NVIDIA GeForce RTX 3060 Laptop GPU: 6144 MiB`。
fn gpu_details_from_devices(devices: Option<&[String]>) -> Option<(Option<String>, Option<u64>)> {
    let line = devices?
        .iter()
        .find(|line| {
            let normalized = line.to_ascii_lowercase();
            normalized.contains("cuda")
                || normalized.contains("vulkan")
                || normalized.contains("metal")
                || normalized.contains("nvidia")
        })?
        .trim();
    // 行尾显存：" 6144 MiB" / " 6.0 GB" / " 6.0 GiB"；解析失败仍保留名称。
    let mut name = line;
    let mut memory_gb = None;
    for (unit, multiplier_gb) in [
        (" MiB", 1.0 / 1024.0),
        (" MB", 1.0 / 1024.0),
        (" GiB", 1.0),
        (" GB", 1.0),
    ] {
        if let Some(pos) = line.rfind(unit) {
            let amount = &line[..pos];
            let digit_start = amount
                .rfind(|character: char| !character.is_ascii_digit() && character != '.')
                .map(|index| index + 1)
                .unwrap_or(0);
            if let Ok(value) = amount[digit_start..].trim().parse::<f64>() {
                name = amount[..digit_start].trim_end_matches([' ', ':', '\t']);
                memory_gb = Some((value * multiplier_gb) as u64);
            }
            break;
        }
    }
    // 剥掉 backend 前缀："CUDA0: " → 纯显卡名
    let name = name.split_once(':').map(|(_, rest)| rest.trim()).unwrap_or(name);
    Some(((!name.is_empty()).then(|| name.to_owned()), memory_gb))
}

const ENVIRONMENT_CACHE_FILE: &str = "environment-cache.json";
/// 显卡型号/显存几乎不变；llama 探测失败时回退到上次成功结果，7 天内不重测。
const ENVIRONMENT_CACHE_TTL: chrono::Duration = chrono::Duration::days(7);

fn environment_cache_path(data_directory: &Path) -> PathBuf {
    data_directory.join(ENVIRONMENT_CACHE_FILE)
}

fn read_environment_cache(data_directory: &Path) -> Option<(Option<String>, Option<u64>)> {
    let content = fs::read_to_string(environment_cache_path(data_directory)).ok()?;
    let value = serde_json::from_str::<Value>(&content).ok()?;
    let checked_at = chrono::DateTime::parse_from_rfc3339(value.get("checked_at")?.as_str()?)
        .ok()?
        .with_timezone(&Utc);
    if Utc::now().signed_duration_since(checked_at) > ENVIRONMENT_CACHE_TTL {
        return None;
    }
    Some((
        value.get("gpu_name").and_then(Value::as_str).map(str::to_owned),
        value.get("gpu_memory_gb").and_then(Value::as_u64),
    ))
}

fn write_environment_cache(data_directory: &Path, check: &EnvironmentCheck) {
    let path = environment_cache_path(data_directory);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = serde_json::to_string_pretty(&json!(check)) {
        let _ = fs::write(path, content);
    }
}

/// 后台 GPU 探测完成后刷新环境状态并落盘：环境页与模型推荐立即拿到探测
/// 结果（探测完成前 model_state/environment 如实显示 CPU 状态，前后端一致）。
pub(crate) fn refresh_environment_after_probe(
    app: &AppHandle,
    runtime_capability: &RuntimeCapability,
) {
    let environment = app.state::<EnvironmentServiceState>();
    let cached_gpu = read_environment_cache(&environment.data_directory);
    let check = detect_environment(
        &environment.data_directory,
        Some(runtime_capability),
        cached_gpu,
    );
    if let Ok(mut latest) = environment.latest.lock() {
        *latest = Some(check.clone());
    }
    write_environment_cache(&environment.data_directory, &check);
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
pub fn environment_detect(
    environment: State<'_, EnvironmentServiceState>,
    catalog: State<'_, CatalogServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<EnvironmentCheck, AppError> {
    let mut latest = environment
        .latest
        .lock()
        .map_err(|_| AppError::new("ENVIRONMENT_STATE_UNAVAILABLE", "环境检测状态不可用", true))?;
    if let Some(cached) = latest.as_ref()
        && chrono::DateTime::parse_from_rfc3339(&cached.checked_at)
            .ok()
            .is_some_and(|checked_at| {
                Utc::now()
                    .signed_duration_since(checked_at.with_timezone(&Utc))
                    .num_seconds()
                    < 30
            })
    {
        return Ok(cached.clone());
    }
    // 探测由启动阶段的后台线程负责（lib.rs background_probe_generation_runtime），
    // 完成前如实返回当前 runtime 状态，绝不在此同步启动 llama-server --list-devices
    //（冷 GPU 可达数十秒）；显卡信息回退到落盘缓存（上次成功结果）。
    let runtime_capability = generation
        .0
        .lock()
        .map_err(|_| {
            AppError::new(
                "GENERATION_RUNTIME_LOCK_FAILED",
                "生成运行时状态已损坏",
                true,
            )
        })?
        .current_capability()
        .cloned();
    let cached_gpu = read_environment_cache(&environment.data_directory);
    let check = detect_environment(
        &environment.data_directory,
        runtime_capability.as_ref(),
        cached_gpu,
    );
    crate::runtime_log::event(
        if check.status == "ready" {
            "info"
        } else {
            "warning"
        },
        "environment",
        "environment.detected",
        None,
        &json!({
            "status": check.status,
            "memory_total_gb": check.memory_total_gb,
            "disk_available_gb": check.disk_available_gb,
            "gpu_name": check.gpu_name,
            "gpu_memory_gb": check.gpu_memory_gb,
            "runtime_backend": check.runtime_backend,
            "warning_count": check.warnings.len(),
        }),
    );
    *latest = Some(check.clone());
    drop(latest);
    write_environment_cache(&environment.data_directory, &check);
    if let Some((level, triggers)) = environment_degradation(&check) {
        catalog
            .get()?
            .reconcile_degradation_state(level, triggers)?;
    }
    Ok(check)
}

#[tauri::command(async)]
pub fn model_state_get(
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    generation: State<'_, GenerationServiceState>,
    worker: State<'_, WorkerServiceState>,
) -> Result<ModelRuntimeState, AppError> {
    let models = models.get()?;
    let catalog = catalog.get()?;
    let mut inference_runtime = inference_runtime_state(&generation)?;
    if worker.foreground_activity.load(Ordering::Acquire) > 0 {
        inference_runtime.pressure_reason =
            Some("正在优先处理搜索或问答，后台模型任务已让出".into());
    }
    model_state_from_manager(&models, Some(&catalog), Some(inference_runtime))
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
pub fn model_catalog_list(source: String) -> Result<Vec<ModelEdition>, AppError> {
    ["light", "standard"]
        .into_iter()
        .map(|edition_id| fanfan_core::model_edition_by_id(edition_id, &source))
        .collect()
}

#[tauri::command(async)]
pub fn model_role_catalog_list(
    environment: State<'_, EnvironmentServiceState>,
) -> Vec<ModelCatalogEntry> {
    let mut catalog = fanfan_core::built_in_model_catalog();
    let cached = environment
        .latest
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let check = match cached {
        Some(check) => check,
        None => {
            let detected = detect_environment(
                &environment.data_directory,
                None,
                read_environment_cache(&environment.data_directory),
            );
            if let Ok(mut latest) = environment.latest.lock() {
                *latest = Some(detected.clone());
            }
            write_environment_cache(&environment.data_directory, &detected);
            detected
        }
    };
    let recommended =
        fanfan_core::recommended_catalog_ids(&catalog, check.memory_total_gb, check.gpu_memory_gb);
    for entry in &mut catalog {
        entry.recommended = recommended.contains(&entry.catalog_id);
    }
    catalog
}

#[tauri::command(async)]
pub fn model_preset_list() -> Vec<ModelPreset> {
    fanfan_core::built_in_model_presets()
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
    begin_model_download(
        app,
        catalog.get()?,
        models.get()?,
        downloads.inner().clone(),
        worker.client.isolated(),
        Arc::clone(&generation.0),
        &request.edition_id,
        &request.source,
    )
}

fn begin_model_download(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    manager: Arc<ModelManager>,
    downloads: ModelDownloadCoordinatorState,
    worker: WorkerClient,
    generation: Arc<Mutex<LocalGenerationRuntime>>,
    edition_id: &str,
    source: &str,
) -> Result<ModelDownloadJob, AppError> {
    let edition = fanfan_core::model_edition_by_id(edition_id, source)?;
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
        spawn_embed_pending(app, catalog);
        return Ok(job);
    }
    spawn_model_download(
        app.clone(),
        catalog,
        Arc::clone(&manager),
        edition,
        job.job_id,
        downloads,
        worker,
        generation,
    )?;
    let updated = manager.download_job(&job.job_id)?;
    emit_download_state(&app, &updated);
    Ok(updated)
}

#[cfg(debug_assertions)]
pub(crate) fn queue_evaluation_model_download(
    app: &AppHandle,
    catalog: Arc<CatalogService>,
    edition_id: &str,
    source: &str,
) -> Result<ModelDownloadJob, AppError> {
    let manager = app.state::<ModelServiceState>().get()?;
    let downloads = app.state::<ModelDownloadCoordinatorState>().inner().clone();
    let worker = app.state::<WorkerServiceState>().client.isolated();
    let generation = Arc::clone(&app.state::<GenerationServiceState>().0);
    begin_model_download(
        app.clone(),
        catalog,
        manager,
        downloads,
        worker,
        generation,
        edition_id,
        source,
    )
}

#[tauri::command(async)]
pub fn model_download_list(
    models: State<'_, ModelServiceState>,
) -> Result<Vec<ModelDownloadJob>, AppError> {
    models.get()?.list_download_jobs()
}

#[tauri::command(async)]
pub fn model_store_status_get(
    models: State<'_, ModelServiceState>,
) -> Result<ModelStoreStatus, AppError> {
    models.get()?.store_status()
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
) -> Result<ModelDownloadRemoval, AppError> {
    let manager = models.get()?;
    let job = manager.download_job(&request.job_id)?;
    if job.status == "completed" {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONTROL_INVALID",
            "已经完成的模型下载不能取消",
            false,
        ));
    }
    if job.phase == "indexing" {
        manager.cancel_embedding_activation(&request.job_id)?;
    }
    let running = downloads.set_action(&request.job_id, DOWNLOAD_ACTION_CANCEL)?;
    let removed = manager.remove_download_job(&request.job_id)?;
    let mut partial_bytes_removed = 0_u64;
    if running {
        let cleanup_app = app.clone();
        let cleanup_models = Arc::clone(&manager);
        let cleanup_downloads = downloads.inner().clone();
        let cleanup_job_id = request.job_id;
        let cleanup_edition_id = job.edition_id.clone();
        thread::spawn(move || {
            let cleanup_job_id_text = cleanup_job_id.to_string();
            for _ in 0..100 {
                if !cleanup_downloads.is_running(&cleanup_job_id) {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            match cleanup_models.remove_download_staging_for_edition(&cleanup_edition_id) {
                Ok(bytes) => {
                    let _ = cleanup_app.emit(
                        "model:download_removed",
                        ModelDownloadRemoval {
                            job_id: cleanup_job_id,
                            removed: true,
                            partial_bytes_removed: bytes,
                        },
                    );
                }
                Err(error) => crate::runtime_log::event(
                    "warning",
                    "model_download",
                    "model_download.cleanup_failed",
                    Some(&cleanup_job_id_text),
                    &json!({ "error_code": error.code, "retryable": error.retryable }),
                ),
            }
        });
    } else {
        partial_bytes_removed = manager.remove_download_staging_for_edition(&job.edition_id)?;
    }
    let removal = ModelDownloadRemoval {
        job_id: request.job_id,
        removed,
        partial_bytes_removed,
    };
    let _ = app.emit("model:download_removed", &removal);
    Ok(removal)
}

#[derive(Debug, Deserialize)]
pub struct ModelDownloadSwitchSourceRequest {
    job_id: Uuid,
    source: String,
}

#[tauri::command]
pub async fn model_download_resume(
    request: ModelDownloadJobRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    downloads: State<'_, ModelDownloadCoordinatorState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ModelDownloadJob, AppError> {
    let manager = models.get()?;
    let previous = manager.download_job(&request.job_id)?;
    if previous.status != "paused" {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONTROL_INVALID",
            "只有已暂停的模型下载可以继续",
            false,
        ));
    }
    if manager
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
    restart_existing_download(
        app,
        catalog.get()?,
        manager,
        downloads.inner().clone(),
        worker.client.isolated(),
        Arc::clone(&generation.0),
        previous,
        None,
        false,
    )
}

#[tauri::command]
pub async fn model_download_retry(
    request: ModelDownloadJobRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    downloads: State<'_, ModelDownloadCoordinatorState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ModelDownloadJob, AppError> {
    let manager = models.get()?;
    let previous = manager.download_job(&request.job_id)?;
    if previous.status != "failed" && previous.activation_status.as_deref() != Some("failed") {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONTROL_INVALID",
            "只有失败的模型任务可以重试",
            false,
        ));
    }
    restart_existing_download(
        app,
        catalog.get()?,
        manager,
        downloads.inner().clone(),
        worker.client.isolated(),
        Arc::clone(&generation.0),
        previous,
        None,
        true,
    )
}

#[tauri::command]
pub async fn model_download_switch_source(
    request: ModelDownloadSwitchSourceRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    downloads: State<'_, ModelDownloadCoordinatorState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ModelDownloadJob, AppError> {
    let manager = models.get()?;
    let previous = manager.download_job(&request.job_id)?;
    if !matches!(previous.status.as_str(), "paused" | "failed")
        && previous.activation_status.as_deref() != Some("failed")
    {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONTROL_INVALID",
            "当前模型任务不能切换来源",
            false,
        ));
    }
    let target_source = parse_download_source(&request.source)?;
    if target_source == previous.source {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_SOURCE_UNCHANGED",
            "请选择另一个模型下载来源",
            false,
        ));
    }
    if manager
        .pending_embedding_activation()?
        .is_some_and(|pending| pending.download_job_id == Some(request.job_id))
    {
        manager.cancel_embedding_activation(&request.job_id)?;
    }
    restart_existing_download(
        app,
        catalog.get()?,
        manager,
        downloads.inner().clone(),
        worker.client.isolated(),
        Arc::clone(&generation.0),
        previous,
        Some(target_source),
        true,
    )
}

#[tauri::command(async)]
pub fn model_download_remove(
    request: ModelDownloadJobRequest,
    app: AppHandle,
    models: State<'_, ModelServiceState>,
) -> Result<ModelDownloadRemoval, AppError> {
    let manager = models.get()?;
    let job = manager.download_job(&request.job_id)?;
    if !matches!(job.status.as_str(), "failed" | "paused" | "cancelled")
        && job.activation_status.as_deref() != Some("failed")
    {
        return Err(AppError::new(
            "MODEL_DOWNLOAD_CONTROL_INVALID",
            "只有暂停或失败的模型任务可以移除",
            false,
        ));
    }
    if job.phase == "indexing" {
        manager.cancel_embedding_activation(&request.job_id)?;
    }
    let removed = manager.remove_download_job(&request.job_id)?;
    let partial_bytes_removed = manager.remove_download_staging_for_edition(&job.edition_id)?;
    let removal = ModelDownloadRemoval {
        job_id: request.job_id,
        removed,
        partial_bytes_removed,
    };
    let _ = app.emit("model:download_removed", &removal);
    Ok(removal)
}

#[allow(clippy::too_many_arguments)]
fn restart_existing_download(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    manager: Arc<ModelManager>,
    downloads: ModelDownloadCoordinatorState,
    worker: WorkerClient,
    generation: Arc<Mutex<LocalGenerationRuntime>>,
    mut job: ModelDownloadJob,
    source: Option<ModelSource>,
    increment_retry: bool,
) -> Result<ModelDownloadJob, AppError> {
    let selected_source = source.unwrap_or(job.source);
    let source_name = match selected_source {
        ModelSource::Huggingface => "huggingface",
        ModelSource::Modelscope => "modelscope",
        ModelSource::LocalImport => {
            return Err(AppError::new(
                "MODEL_DOWNLOAD_SOURCE_UNAVAILABLE",
                "本地导入不能作为联网下载来源",
                false,
            ));
        }
    };
    let edition = fanfan_core::model_edition_by_id(&job.edition_id, source_name)?;
    if selected_source != job.source {
        job.source = selected_source;
        job.files = download_file_progress(&edition);
        job.downloaded_bytes = 0;
        job.total_bytes = job.files.iter().map(|file| file.total_bytes).sum();
        job.progress = 0.0;
        job.installed_artifact_ids.clear();
        job.profile_id = None;
    }
    job.status = "queued".into();
    job.phase = "queued".into();
    if increment_retry {
        job.retry_count = job.retry_count.saturating_add(1);
    }
    job.bytes_per_second = 0;
    job.eta_seconds = None;
    job.current_file = None;
    job.error = None;
    job.activation_status = Some("pending".into());
    job.activation_error = None;
    let job = manager.update_download_job(&job)?;
    emit_download_state(&app, &job);
    spawn_model_download(
        app,
        catalog,
        Arc::clone(&manager),
        edition,
        job.job_id,
        downloads,
        worker,
        generation,
    )?;
    manager.download_job(&job.job_id)
}

fn parse_download_source(source: &str) -> Result<ModelSource, AppError> {
    match source {
        "huggingface" => Ok(ModelSource::Huggingface),
        "modelscope" => Ok(ModelSource::Modelscope),
        _ => Err(AppError::new(
            "MODEL_DOWNLOAD_SOURCE_UNAVAILABLE",
            "不支持的模型下载来源",
            false,
        )),
    }
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
    generation: &Mutex<LocalGenerationRuntime>,
) {
    let result = (|| {
        let mut job = models.download_job(&job_id)?;
        job.status = "running".into();
        job.phase = "downloading".into();
        job.error = None;
        persist_download_job(app, models, &mut job)?;
        let _ = app.emit("model:download_started", &job);

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
            .find(|artifact| artifact.role == ModelRole::Embedding);
        let generation_artifact = installed
            .iter()
            .find(|artifact| artifact.role == ModelRole::Generation);
        if embedding.is_none() || generation_artifact.is_none() {
            job.phase = "activating".into();
            persist_download_job(app, models, &mut job)?;
            let embedding_indexing = self_test_and_activate_downloaded_roles(
                app, catalog, models, worker, generation, &installed, job.job_id,
            )?;
            for artifact in &edition.artifacts {
                if let Ok(staging) = models.download_artifact_staging_directory(
                    artifact.source,
                    &edition.edition_id,
                    artifact.role,
                ) {
                    let _ = fs::remove_dir_all(staging);
                }
            }
            job.current_file = None;
            job.bytes_per_second = 0;
            job.eta_seconds = Some(0);
            job.error = None;
            for file in &mut job.files {
                file.status = "completed".into();
                file.downloaded_bytes = file.total_bytes;
            }
            if embedding_indexing {
                job.status = "running".into();
                job.phase = "indexing".into();
                persist_download_job(app, models, &mut job)?;
                spawn_embed_pending(app.clone(), Arc::clone(catalog));
                let _ = app.emit("model:download_indexing", &job);
            } else {
                job.status = "completed".into();
                job.phase = "completed".into();
                persist_download_job(app, models, &mut job)?;
                let _ = app.emit("model:download_completed", &job);
            }
            return Ok::<(), AppError>(());
        }
        let embedding = embedding.expect("checked embedding");
        let generation_artifact = generation_artifact.expect("checked generation");
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
        let threads = interactive_inference_threads();
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
                "证据[S1]：翻翻在本地处理资料。请用一句完整中文事实句复述并引用。",
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
            let _ = app.emit("model:download_indexing", &job);
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
        let _ = app.emit("model:download_completed", &job);
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
            let _ = app.emit("model:download_failed", error);
        }
    }
}

fn self_test_and_activate_downloaded_roles(
    app: &AppHandle,
    catalog: &Arc<CatalogService>,
    models: &ModelManager,
    worker: &WorkerClient,
    generation: &Mutex<LocalGenerationRuntime>,
    installed: &[ModelArtifact],
    download_job_id: Uuid,
) -> Result<bool, AppError> {
    let mut embedding_indexing = false;
    for artifact in installed {
        match (artifact.role, artifact.format) {
            (ModelRole::Generation, ModelFormat::Gguf) => {
                let mut runtime = generation.lock().map_err(|_| {
                    AppError::new(
                        "GENERATION_RUNTIME_LOCK_FAILED",
                        "生成运行时状态已损坏",
                        true,
                    )
                })?;
                runtime.activate(&artifact.local_path, 4096, interactive_inference_threads())?;
                let generated = runtime.complete(
                    "你正在执行本地模型健康检查。只需用一句简短中文确认可以回答。",
                    "请回复：翻翻本地模型可以工作。",
                    32,
                )?;
                if generated.trim().chars().count() < 4 {
                    runtime.stop();
                    return Err(AppError::new(
                        "MODEL_SELF_TEST_FAILED",
                        "生成模型没有通过最小本地推理自检，已回滚",
                        true,
                    ));
                }
                models.activate_artifact(&artifact.artifact_id, None)?;
            }
            (ModelRole::Embedding, ModelFormat::Onnx) => {
                let tokenizer = PathBuf::from(&artifact.local_path)
                    .parent()
                    .map(|parent| parent.join("tokenizer.json"))
                    .ok_or_else(|| {
                        AppError::new("EMBEDDING_TOKENIZER_UNAVAILABLE", "语义模型目录无效", false)
                    })?;
                let response = worker.encode_embeddings(&EmbeddingRequest {
                    model_path: artifact.local_path.clone(),
                    tokenizer_path: Some(tokenizer.to_string_lossy().into_owned()),
                    texts: vec!["拾起散落的信息，连接过去的自己".into()],
                    max_length: artifact.max_length.unwrap_or(512),
                    threads: background_inference_threads(),
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
                models.begin_embedding_activation_with_job(
                    &artifact.artifact_id,
                    response.dimension,
                    Some(download_job_id),
                )?;
                embedding_indexing = true;
            }
            (ModelRole::Vision, ModelFormat::Gguf) => {
                let projector = models.vision_projector_path(artifact)?;
                generation
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
                        interactive_inference_threads(),
                    )?;
                models.activate_artifact(&artifact.artifact_id, None)?;
                spawn_image_understanding_pending(app.clone(), Arc::clone(catalog));
            }
            (ModelRole::Reranker, ModelFormat::Onnx) => {
                let tokenizer = PathBuf::from(&artifact.local_path)
                    .parent()
                    .map(|parent| parent.join("tokenizer.json"))
                    .ok_or_else(|| {
                        AppError::new("RERANK_TOKENIZER_UNAVAILABLE", "模型目录无效", false)
                    })?;
                let response = worker.rerank(&RerankRequest {
                    model_path: artifact.local_path.clone(),
                    tokenizer_path: Some(tokenizer.to_string_lossy().into_owned()),
                    query: "哪段资料描述了本地知识库？".into(),
                    documents: vec![
                        "翻翻在本地建立可检索的资料知识库。".into(),
                        "今天窗外天气晴朗。".into(),
                    ],
                    max_length: artifact.max_length.unwrap_or(512),
                    threads: background_inference_threads(),
                })?;
                if response.scores.len() != 2
                    || response.scores.iter().any(|score| !score.is_finite())
                    || response.scores[0] <= response.scores[1]
                {
                    return Err(AppError::new(
                        "MODEL_SELF_TEST_FAILED",
                        "重排模型自检返回了无效分数",
                        true,
                    ));
                }
                models.activate_artifact(&artifact.artifact_id, None)?;
            }
            (ModelRole::Ocr, ModelFormat::Onnx) => {
                worker.self_test_ocr(
                    artifact.local_path.clone(),
                    model_companion_path(artifact, "ch_PP-OCRv5_mobile_det.onnx")?,
                    model_companion_path(artifact, "ch_ppocr_mobile_v2.0_cls_infer.onnx")?,
                    model_companion_path(artifact, "ppocrv5_dict.txt")?,
                    1,
                )?;
                models.activate_artifact(&artifact.artifact_id, None)?;
            }
            (ModelRole::Tts, ModelFormat::Onnx) => {
                worker.self_test_tts(
                    artifact.local_path.clone(),
                    model_companion_path(artifact, "tokens.txt")?,
                    model_companion_path(artifact, "lexicon.txt")?,
                    1,
                )?;
                models.activate_artifact(&artifact.artifact_id, None)?;
            }
            (ModelRole::Asr, ModelFormat::Onnx) => {
                worker.self_test_asr(
                    artifact.local_path.clone(),
                    model_companion_path(artifact, "tokens.txt")?,
                    model_companion_path(artifact, "silero_vad.onnx")?,
                    1,
                )?;
                models.activate_artifact(&artifact.artifact_id, None)?;
            }
            _ => {
                return Err(AppError::new(
                    "MODEL_RUNTIME_UNSUPPORTED",
                    "当前模型角色或格式尚未接入本地运行时",
                    false,
                ));
            }
        }
    }
    Ok(embedding_indexing)
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
                    "模型断点不是翻翻管理的普通文件",
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
                "FanFan/0.1",
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
    let _ = app.emit("model:download_state", job);
    let _ = app.emit(
        "model:download_progress",
        json!({
            "job_id": job.job_id,
            "edition_id": job.edition_id,
            "downloaded_bytes": job.downloaded_bytes,
            "total_bytes": job.total_bytes,
            "progress": job.progress,
            "phase": job.phase,
            "status": job.status,
            "source": job.source,
        }),
    );
    let progress_bucket = (job.progress.clamp(0.0, 1.0) * 20.0).floor() as u8;
    let checkpoint = (job.phase.clone(), job.status.clone(), progress_bucket);
    let should_log = DOWNLOAD_LOG_CHECKPOINTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map(|mut checkpoints| {
            if checkpoints.get(&job.job_id) == Some(&checkpoint) {
                false
            } else {
                checkpoints.insert(job.job_id, checkpoint);
                true
            }
        })
        .unwrap_or(true);
    if should_log {
        crate::runtime_log::event(
            if job.status == "failed" {
                "error"
            } else {
                "info"
            },
            "model.download",
            "download.state_changed",
            Some(&job.job_id.to_string()),
            &json!({
                "job_id": job.job_id,
                "edition_id": job.edition_id,
                "phase": job.phase,
                "status": job.status,
                "progress": job.progress,
                "downloaded_bytes": job.downloaded_bytes,
                "total_bytes": job.total_bytes,
                "bytes_per_second": job.bytes_per_second,
                "eta_seconds": job.eta_seconds,
                "retry_count": job.retry_count,
                "error_code": job.error.as_ref().map(|error| error.code.as_str()),
                "retryable": job.error.as_ref().map(|error| error.retryable),
            }),
        );
    }
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

#[derive(Debug, Deserialize)]
pub struct ModelRoleActionRequest {
    role: ModelRole,
}

#[tauri::command(async)]
pub fn model_role_disable(
    request: ModelRoleActionRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    generation: State<'_, GenerationServiceState>,
) -> Result<ModelRuntimeState, AppError> {
    let started = Instant::now();
    let models = models.get()?;
    let catalog = catalog.get()?;
    if request.role == ModelRole::Generation {
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
            .stop();
    }
    models.deactivate_role(request.role)?;
    crate::runtime_log::event(
        "info",
        "model.runtime",
        "model.role_disabled",
        None,
        &json!({
            "role": request.role,
            "duration_ms": started.elapsed().as_millis(),
        }),
    );
    model_state_from_manager(
        &models,
        Some(&catalog),
        Some(inference_runtime_state(&generation)?),
    )
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
    let started = Instant::now();
    crate::runtime_log::event(
        "info",
        "model.runtime",
        "model.activation_started",
        Some(&artifact_id.to_string()),
        &json!({
            "artifact_id": artifact_id,
            "role": artifact.role,
            "format": artifact.format,
        }),
    );
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
            let threads = interactive_inference_threads();
            let mut runtime = generation.0.lock().map_err(|_| {
                AppError::new(
                    "GENERATION_RUNTIME_LOCK_FAILED",
                    "生成运行时状态已损坏",
                    true,
                )
            })?;
            runtime.activate(&artifact.local_path, 4096, threads)?;
            let self_test = runtime.complete(
                "你正在执行本地模型健康检查。只需用一句简短中文确认可以回答。",
                "请回复：翻翻本地模型可以工作。",
                32,
            );
            let self_test_failed = match &self_test {
                Ok(value) => value.trim().chars().count() < 4,
                Err(_) => true,
            };
            if self_test_failed {
                runtime.stop();
                return Err(AppError::new(
                    "MODEL_SELF_TEST_FAILED",
                    "生成模型没有通过最小本地推理自检，已回滚",
                    true,
                ));
            }
            drop(runtime);
            models.activate_artifact(&artifact_id, None)?;
        }
        (ModelRole::Vision, ModelFormat::Gguf) => {
            let projector = models.vision_projector_path(&artifact)?;
            let threads = interactive_inference_threads();
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
                    "翻翻在本地建立可检索的资料知识库。".into(),
                    "今天窗外天气晴朗。".into(),
                ],
                max_length: artifact.max_length.unwrap_or(512),
                threads: 2,
            })?;
            if response.scores.len() != 2
                || response.scores.iter().any(|score| !score.is_finite())
                || response.scores[0] <= response.scores[1]
            {
                return Err(AppError::new(
                    "MODEL_SELF_TEST_FAILED",
                    "重排模型自检返回了无效分数",
                    true,
                ));
            }
            models.activate_artifact(&artifact_id, None)?;
        }
        (ModelRole::Ocr, ModelFormat::Onnx) => {
            worker.client.self_test_ocr(
                artifact.local_path.clone(),
                model_companion_path(&artifact, "ch_PP-OCRv5_mobile_det.onnx")?,
                model_companion_path(&artifact, "ch_ppocr_mobile_v2.0_cls_infer.onnx")?,
                model_companion_path(&artifact, "ppocrv5_dict.txt")?,
                1,
            )?;
            models.activate_artifact(&artifact_id, None)?;
        }
        (ModelRole::Tts, ModelFormat::Onnx) => {
            worker.client.self_test_tts(
                artifact.local_path.clone(),
                model_companion_path(&artifact, "tokens.txt")?,
                model_companion_path(&artifact, "lexicon.txt")?,
                1,
            )?;
            models.activate_artifact(&artifact_id, None)?;
        }
        (ModelRole::Asr, ModelFormat::Onnx) => {
            worker.client.self_test_asr(
                artifact.local_path.clone(),
                model_companion_path(&artifact, "tokens.txt")?,
                model_companion_path(&artifact, "silero_vad.onnx")?,
                1,
            )?;
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
    let state = model_state_from_manager(
        &models,
        Some(&catalog),
        Some(inference_runtime_state(&generation)?),
    )?;
    crate::runtime_log::event(
        "info",
        "model.runtime",
        "model.activation_completed",
        Some(&artifact_id.to_string()),
        &json!({
            "artifact_id": artifact_id,
            "role": artifact.role,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    );
    Ok(state)
}

fn default_inference_runtime_state() -> InferenceRuntimeState {
    let hardware = current_hardware_profile(&[]);
    let budget = current_inference_budget(hardware.memory_total_bytes);
    InferenceRuntimeState {
        backend: "unavailable".into(),
        device_names: Vec::new(),
        gpu_available: false,
        gpu_offload_layers: Some(0),
        gpu_offload_mode: "disabled".into(),
        thread_budget: interactive_inference_threads(),
        batch_thread_budget: background_inference_threads(),
        active: false,
        pressure_reason: None,
        hardware,
        runtime_package: RuntimeBackendPackage {
            backend: "unavailable".into(),
            device_count: 0,
            gpu_capable: false,
            cpu_fallback_available: false,
            validated: false,
        },
        budget,
    }
}

pub(crate) fn inference_runtime_state(
    generation: &GenerationServiceState,
) -> Result<InferenceRuntimeState, AppError> {
    let mut runtime = generation.0.lock().map_err(|_| {
        AppError::new(
            "GENERATION_RUNTIME_LOCK_FAILED",
            "生成运行时状态已损坏",
            true,
        )
    })?;
    let active = runtime.is_active();
    // 探测由启动阶段的后台线程负责；探测完成前如实返回当前 runtime 状态
    //（CPU 生效中），绝不在此同步启动 llama-server --list-devices——冷 GPU
    // 可达数十秒，曾导致标题栏/模型页整窗卡死。后台探测完成会发 model:state
    // 事件驱动前端刷新，前后端状态始终一致。
    let capability = runtime.current_capability().cloned().unwrap_or_else(|| {
        RuntimeCapability {
            executable_available: true,
            backend: "cpu".into(),
            devices: Vec::new(),
            gpu_available: false,
            checked_at: chrono::Utc::now(),
            error_code: None,
        }
    });
    let device_names = runtime
        .active_device()
        .map(|device| vec![device.to_owned()])
        .unwrap_or_else(|| capability.devices.clone());
    let backend = runtime
        .active_backend()
        .map(str::to_owned)
        .unwrap_or_else(|| capability.backend.clone());
    let hardware = current_hardware_profile(&device_names);
    let budget = current_inference_budget(hardware.memory_total_bytes);
    Ok(InferenceRuntimeState {
        backend: backend.clone(),
        device_names: device_names.clone(),
        gpu_available: capability.gpu_available,
        gpu_offload_layers: runtime.active_gpu_layers(),
        gpu_offload_mode: if capability.gpu_available {
            "automatic".into()
        } else {
            "disabled".into()
        },
        thread_budget: runtime
            .active_threads()
            .unwrap_or_else(interactive_inference_threads),
        batch_thread_budget: background_inference_threads(),
        active,
        pressure_reason: None,
        hardware,
        runtime_package: RuntimeBackendPackage {
            backend,
            device_count: device_names.len() as u32,
            gpu_capable: capability.gpu_available,
            cpu_fallback_available: runtime.cpu_fallback_available(),
            validated: capability.executable_available
                && (!capability.gpu_available || !device_names.is_empty()),
        },
        budget,
    })
}

fn current_hardware_profile(device_names: &[String]) -> HardwareProfile {
    let (memory_total_bytes, memory_available_bytes) = memory_status_bytes()
        .map(|(total, available)| (Some(total), Some(available)))
        .unwrap_or((None, None));
    let logical_thread_count = std::thread::available_parallelism()
        .map(|value| value.get() as u32)
        .unwrap_or(1);
    let gpu_name = device_names.first().cloned();
    let gpu_memory_bytes = device_names.first().and_then(|device| {
        let marker = " MiB";
        let end = device.find(marker)?;
        let start = device[..end].rfind(|character: char| !character.is_ascii_digit())? + 1;
        device[start..end]
            .parse::<u64>()
            .ok()
            .map(|mib| mib * 1024 * 1024)
    });
    HardwareProfile {
        physical_core_count: physical_core_count(),
        logical_thread_count,
        memory_total_bytes,
        memory_available_bytes,
        gpu_name,
        gpu_memory_bytes,
    }
}

fn current_inference_budget(memory_total_bytes: Option<u64>) -> InferenceBudget {
    const GIB: u64 = 1024 * 1024 * 1024;
    InferenceBudget {
        foreground_threads: interactive_inference_threads(),
        background_threads: background_inference_threads(),
        batch_size: 256,
        ubatch_size: 128,
        gpu_reserve_bytes: 512 * 1024 * 1024,
        system_memory_reserve_bytes: memory_total_bytes
            .map(|total| (total / 5).max(2 * GIB))
            .unwrap_or(2 * GIB),
    }
}

pub(crate) fn model_state_from_manager(
    models: &ModelManager,
    catalog: Option<&CatalogService>,
    inference_runtime: Option<InferenceRuntimeState>,
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
        tts: models.active_artifact(ModelRole::Tts)?.is_some(),
        asr: models.active_artifact(ModelRole::Asr)?.is_some(),
    };
    let any_active = capabilities.generation
        || capabilities.embedding
        || capabilities.vision
        || capabilities.reranker
        || capabilities.ocr
        || capabilities.tts
        || capabilities.asr;
    let semantic_index_coverage = match (catalog, active_embedding.as_ref()) {
        (Some(catalog), Some(embedding)) => catalog
            .active_vector_generation(&embedding.artifact_id.to_string())?
            .map(|generation| generation.coverage)
            .unwrap_or(0.0),
        _ => 0.0,
    };
    let inference_runtime = inference_runtime.unwrap_or_else(default_inference_runtime_state);
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
        runtime_backend: any_active.then(|| inference_runtime.backend.clone()),
        inference_runtime,
        checked_at: Utc::now().to_rfc3339(),
        rag_complete: capabilities.generation
            && capabilities.embedding
            && semantic_index_coverage >= 0.999_999,
        capabilities,
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
    // 扫描进行中必须直查（进度环接近实时）；无扫描时命中缓存则整体跳过 8 个查询。
    let active_scan = catalog.latest_active_scan_job()?;
    if active_scan.is_none() {
        let hit = {
            let cache = HOME_SUMMARY_CACHE
                .get_or_init(|| Mutex::new((Instant::now(), String::new(), Value::Null)))
                .lock()
                .ok();
            cache
                .as_deref()
                .filter(|(cached_at, cached_date, _)| {
                    *cached_date == request.local_date
                        && cached_at.elapsed() < HOME_SUMMARY_CACHE_TTL
                })
                .map(|(_, _, summary)| summary.clone())
        };
        if let Some(summary) = hit {
            return Ok(summary);
        }
    }
    let (today_added, recent) = catalog.home_file_summary(&request.local_date)?;
    let candidates = catalog.list_candidate_roots()?;
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
    let collections = catalog.list_collections()?;
    // 与状态栏共享 10s TTL 缓存，避免首页轮询重复触发 5 个 COUNT(DISTINCT) 全表扫描
    let index_stats = cached_index_activity_stats(&catalog)?;
    let failed = error_inbox.items.len();
    let awaiting_confirmation = new_inbox.items.len();
    // SQL COUNT 替代拉 500 行再在内存里数
    let possible_duplicates = catalog.count_exact_duplicate_relations()?;
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
    let scan_progress = active_scan.as_ref().map(|job| {
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
    let summary = json!({
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
    });
    if active_scan.is_none() {
        if let Ok(mut cache) = HOME_SUMMARY_CACHE
            .get_or_init(|| Mutex::new((Instant::now(), String::new(), Value::Null)))
            .lock()
        {
            *cache = (Instant::now(), request.local_date.clone(), summary.clone());
        }
    }
    Ok(summary)
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
        let _ = app.emit("catalog:watch_degraded", &error);
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
        let _ = app.emit("catalog:watch_degraded", &error);
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
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    watcher: State<'_, WatcherServiceState>,
) -> Result<(), AppError> {
    let root_id = Uuid::parse_str(&request.root_id)
        .map_err(|error| AppError::new("ROOT_ID_INVALID", error.to_string(), false))?;
    let catalog = catalog.get()?;
    watcher.with_mut(|watcher| watcher.unwatch_root(&root_id))?;
    catalog.disable_root(&root_id)?;
    crate::runtime_log::event(
        "info",
        "catalog",
        "root.authorization_revoked",
        None,
        &json!({ "root_id": root_id }),
    );
    tauri::async_runtime::spawn_blocking(move || match catalog.cleanup_disabled_root(&root_id) {
        Ok(removed) => {
            crate::runtime_log::event(
                "info",
                "catalog",
                "root.background_cleanup_completed",
                None,
                &json!({ "root_id": root_id, "removed_memberships": removed }),
            );
            let _ = app.emit("catalog:changed", root_id.to_string());
        }
        Err(error) => {
            crate::runtime_log::event(
                "error",
                "catalog",
                "root.background_cleanup_failed",
                None,
                &json!({
                    "root_id": root_id,
                    "error_code": error.code,
                    "retryable": error.retryable,
                }),
            );
            let _ = app.emit("index:failed", error);
        }
    });
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

#[derive(Debug, Deserialize)]
pub struct InboxRetryRequest {
    inbox_id: Uuid,
}

#[tauri::command(async)]
pub fn inbox_retry(
    request: InboxRetryRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
) -> Result<InboxItem, AppError> {
    let started = Instant::now();
    crate::runtime_log::event(
        "info",
        "inbox",
        "inbox.retry_started",
        Some(&request.inbox_id.to_string()),
        &json!({ "inbox_id": request.inbox_id }),
    );
    let catalog = catalog.get()?;
    match catalog.retry_inbox_item(&request.inbox_id) {
        Ok(item) => {
            spawn_parse_pending(app, Arc::clone(&catalog));
            crate::runtime_log::event(
                "info",
                "inbox",
                "inbox.retry_scheduled",
                Some(&request.inbox_id.to_string()),
                &json!({
                    "inbox_id": request.inbox_id,
                    "file_id": item.file_id,
                    "attempt_count": item.attempt_count,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            Ok(item)
        }
        Err(error) => {
            crate::runtime_log::event(
                "error",
                "inbox",
                "inbox.retry_failed",
                Some(&request.inbox_id.to_string()),
                &json!({
                    "inbox_id": request.inbox_id,
                    "error_code": error.code,
                    "retryable": error.retryable,
                    "error_details": error.details,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            Err(error)
        }
    }
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
        let cancelled = AtomicBool::new(false);
        run_image_deep_analysis(
            &catalog,
            &models,
            &generation,
            asset_id,
            &question,
            &cancelled,
        )
    })
    .await
    .map_err(|error| AppError::new("VISION_REQUEST_FAILED", error.to_string(), true))?
}

fn run_image_deep_analysis(
    catalog: &CatalogService,
    models: &ModelManager,
    generation: &Mutex<LocalGenerationRuntime>,
    asset_id: Uuid,
    question: &str,
    cancelled: &AtomicBool,
) -> Result<ImageDeepAnalysis, AppError> {
    let (image_path, mime_type, _) = catalog.authorized_image_asset_path(&asset_id)?;
    let artifact = models.active_artifact(ModelRole::Vision)?.ok_or_else(|| {
        AppError::new(
            "VISION_MODEL_INVALID",
            "原图深度分析需要先配置并自检本地多模态模型",
            true,
        )
    })?;
    let projector = models.vision_projector_path(&artifact)?;
    let threads = interactive_inference_threads();
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
    let response = runtime.describe_image_cancellable(
        "你是翻翻的本地图片证据分析器。只能根据当前图片中可验证的内容回答，不得补充外部知识；看不清或图片不支持的问题必须明确说明。",
        &format!(
            "用户问题：{question}\n只输出一个JSON对象，不要Markdown：{{\"answer\":\"基于图片的中文回答\",\"observations\":[\"支持回答的可见细节\"],\"uncertainties\":[\"看不清或无法确认的内容\"]}}。"
        ),
        &image_path,
        &mime_type,
        512,
        cancelled,
    )?;
    let payload = parse_vision_question_answer(&response)?;
    Ok(ImageDeepAnalysis {
        asset_id,
        question: question.to_owned(),
        answer: payload.answer,
        observations: payload.observations,
        uncertainties: payload.uncertainties,
        model_artifact_id: artifact.artifact_id,
        analyzed_at: Utc::now().to_rfc3339(),
    })
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
    let correlation_id = Uuid::now_v7().to_string();
    let started = Instant::now();
    crate::runtime_log::event(
        "info",
        "collections.ai",
        "suggestion.refresh_started",
        Some(&correlation_id),
        &json!({ "max_files": request.max_files }),
    );
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
    // 生成模型可选：有则给集合命名润色；没有则用规则名，成员分组不受影响。
    let generation_artifact = models.active_artifact(ModelRole::Generation)?;
    let mut result = catalog
        .refresh_collection_suggestions(&embedding.artifact_id.to_string(), request.max_files)?;
    trace_node(
        &catalog,
        "collection",
        "candidates",
        &correlation_id,
        None,
        None,
        &json!({ "max_files": request.max_files }),
        &json!({
            "profiled_files": result.profiled_files,
            "candidate_edges": result.candidate_edges,
            "created_suggestions": result.created_suggestions,
            "suggestion_ids": result.suggestion_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        }),
        "ok",
        None,
    );
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
        if let Some(generation_artifact) = generation_artifact {
            for suggestion in candidates
                .items
                .into_iter()
                .filter(|item| new_ids.contains(&item.suggestion_id))
            {
                let _ = app.emit(
                    "collection:suggestion_phase",
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
                                "confidence": member.confidence,
                            })
                        })
                        .collect::<Vec<_>>(),
                )
                .map_err(|error| {
                    AppError::new("COLLECTION_MODEL_REVIEW_INVALID", error.to_string(), false)
                })?;
                let cancelled = AtomicBool::new(false);
                let review_started = Instant::now();
                let raw_review = complete_with_model(
                    generation.0.as_ref(),
                    &generation_artifact,
                    "你是本地文档集合命名助手。只根据给定分组内的资料命名，不得增删成员。名称必须概括成员的共同主题、文档类型和用途，禁止直接复制任一文件名，也禁止使用‘相关资料’‘文档集合’等空泛名称。只输出JSON对象：{\"suggested_name\":\"不超过40字\",\"description\":\"不超过200字的说明\"}。",
                    &format!("请给这个同主题资料分组起名并写说明：\n{candidate_json}"),
                    320,
                    &cancelled,
                );
                // 命名只是润色：任何失败都保留规则名，不丢弃建议、不阻塞主流程。
                let (decision, parsed) = match raw_review.as_deref().map(parse_collection_model_review) {
                    Ok(Ok(review)) => match catalog.apply_collection_model_naming(
                        &suggestion.suggestion_id,
                        &review,
                        &generation_artifact.artifact_id.to_string(),
                    ) {
                        Ok(_) => ("applied".into(), true),
                        Err(error) => (format!("kept_rule_name:{error}"), true),
                    },
                    Ok(Err(_)) => ("kept_rule_name:parse_failed".into(), false),
                    Err(error) => (format!("kept_rule_name:model_error:{error}"), false),
                };
                trace_node(
                    &catalog,
                    "collection",
                    "model_review",
                    &correlation_id,
                    None,
                    Some(&suggestion.suggestion_id.to_string()),
                    &json!({
                        "suggestion_id": suggestion.suggestion_id,
                        "candidate_json": candidate_json,
                    }),
                    &json!({
                        "raw": raw_review.unwrap_or_default(),
                        "parsed": parsed,
                        "decision": decision,
                    }),
                    "ok",
                    Some(review_started.elapsed().as_millis() as u64),
                );
            }
        }
        // 分组判断完全由 Embedding 聚类完成，建议一经写入即为创建成功。
        result.created_suggestions = result.suggestion_ids.len() as u64;
    }
    let _ = app.emit("collection:suggestions_changed", &result);
    crate::runtime_log::event(
        "info",
        "collections.ai",
        "suggestion.refresh_completed",
        Some(&correlation_id),
        &json!({
            "profiled_files": result.profiled_files,
            "candidate_edges": result.candidate_edges,
            "created_suggestions": result.created_suggestions,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    );
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
    let _ = app.emit("collection:suggestions_changed", &collection);
    let _ = app.emit("catalog:changed", &collection);
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
        "collection:suggestions_changed",
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
    models: State<'_, ModelServiceState>,
) -> Result<RelationRefreshResult, AppError> {
    let correlation_id = Uuid::now_v7().to_string();
    let started = Instant::now();
    crate::runtime_log::event(
        "info",
        "relations",
        "relation.refresh_started",
        Some(&correlation_id),
        &json!({ "max_files": request.max_files }),
    );
    let catalog = catalog.get()?;
    let exact_started = Instant::now();
    let mut result = catalog.refresh_file_relations(request.max_files)?;
    trace_node(
        &catalog,
        "relation",
        "exact",
        &correlation_id,
        None,
        None,
        &json!({ "max_files": request.max_files }),
        &json!({
            "hashed_files": result.hashed_files,
            "exact_duplicate_pairs": result.exact_duplicate_pairs,
            "version_candidate_pairs": result.version_candidate_pairs,
        }),
        "ok",
        Some(exact_started.elapsed().as_millis() as u64),
    );
    let semantic_started = Instant::now();
    let semantic_found = if let Some(embedding) = models.get()?.active_artifact(ModelRole::Embedding)? {
        let (semantic, contains) = catalog.refresh_semantic_file_relations(
            &embedding.artifact_id.to_string(),
            request.max_files,
        )?;
        result.semantic_related_pairs = semantic;
        result.contains_or_summarizes_pairs = contains;
        Some((semantic, contains, embedding.artifact_id.to_string()))
    } else {
        None
    };
    // 聚类：把成对的边按连通分量聚成组（重复组/版本族/同主题组/摘要组）
    result.groups_created = catalog.refresh_relation_groups(
        semantic_found.as_ref().map(|(_, _, artifact_id)| artifact_id.as_str()),
    )?;
    let (semantic_related, contains_pairs) = match semantic_found {
        Some((semantic, contains, _)) => (Some(semantic), Some(contains)),
        None => (None, None),
    };
    trace_node(
        &catalog,
        "relation",
        "semantic",
        &correlation_id,
        None,
        None,
        &json!({ "max_files": request.max_files }),
        &json!({
            "semantic_related_pairs": semantic_related,
            "contains_or_summarizes_pairs": contains_pairs,
        }),
        "ok",
        Some(semantic_started.elapsed().as_millis() as u64),
    );
    crate::runtime_log::event(
        "info",
        "relations",
        "relation.refresh_completed",
        Some(&correlation_id),
        &json!({
            "hashed_files": result.hashed_files,
            "exact_duplicate_pairs": result.exact_duplicate_pairs,
            "version_candidate_pairs": result.version_candidate_pairs,
            "semantic_related_pairs": result.semantic_related_pairs,
            "contains_or_summarizes_pairs": result.contains_or_summarizes_pairs,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    );
    Ok(result)
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

#[derive(Debug, Deserialize)]
pub struct RelationBatchReviewRequest {
    relation_ids: Vec<Uuid>,
    action: String,
}

#[tauri::command(async)]
pub fn relation_batch_review(
    request: RelationBatchReviewRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<u64, AppError> {
    catalog
        .get()?
        .review_file_relations(&request.relation_ids, &request.action)
}

#[tauri::command(async)]
pub fn relation_group_query(
    request: RelationGroupQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<RelationGroupPage, AppError> {
    catalog.get()?.query_relation_groups(&request)
}

#[derive(Debug, Deserialize)]
pub struct RelationGroupReviewRequest {
    group_id: Uuid,
    action: String,
}

#[tauri::command(async)]
pub fn relation_group_review(
    request: RelationGroupReviewRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    catalog
        .get()?
        .review_relation_group(&request.group_id, &request.action)
}

#[derive(Debug, Deserialize)]
pub struct RelationGroupBatchReviewRequest {
    group_ids: Vec<Uuid>,
    action: String,
}

#[tauri::command(async)]
pub fn relation_group_batch_review(
    request: RelationGroupBatchReviewRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<u64, AppError> {
    catalog
        .get()?
        .review_relation_groups(&request.group_ids, &request.action)
}

#[tauri::command(async)]
pub fn file_query(
    request: FileQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<FilePage, AppError> {
    catalog.get()?.query_files(&request)
}

#[derive(Debug, Deserialize)]
pub struct AnswerExportRequest {
    message_id: Uuid,
    target_path: String,
    format: String,
    confirmation: String,
}

#[tauri::command]
pub async fn answer_export(
    request: AnswerExportRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<ExportResult, AppError> {
    if request.confirmation != "EXPORT_NEW_FILE" {
        return Err(AppError::new(
            "EXPORT_CONFIRMATION_REQUIRED",
            "导出需要用户明确确认只新建文件",
            false,
        ));
    }
    if !matches!(request.format.as_str(), "md" | "txt") {
        return Err(AppError::new(
            "EXPORT_FORMAT_UNSUPPORTED",
            "问答结果只支持导出为Markdown或纯文本",
            false,
        ));
    }
    let target_path = PathBuf::from(request.target_path);
    let expected_extension = request.format.as_str();
    if !target_path.is_absolute()
        || target_path.extension().and_then(|value| value.to_str()) != Some(expected_extension)
    {
        return Err(AppError::new(
            "EXPORT_TARGET_INVALID",
            "导出位置必须是带正确扩展名的绝对路径",
            false,
        ));
    }
    let catalog = catalog.get()?;
    tauri::async_runtime::spawn_blocking(move || {
        let answer = catalog.answer_result(&request.message_id)?;
        catalog.validate_answer_evidence(&answer)?;
        let content = render_answer_export(&answer, &request.format)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target_path)
            .map_err(|error| {
                let code = if error.kind() == std::io::ErrorKind::AlreadyExists {
                    "EXPORT_TARGET_EXISTS"
                } else {
                    "EXPORT_CREATE_FAILED"
                };
                AppError::new(
                    code,
                    if code == "EXPORT_TARGET_EXISTS" {
                        "目标文件已存在，翻翻不会覆盖"
                    } else {
                        "无法在所选位置新建导出文件"
                    },
                    code != "EXPORT_TARGET_EXISTS",
                )
            })?;
        if let Err(error) = file
            .write_all(content.as_bytes())
            .and_then(|_| file.sync_all())
        {
            drop(file);
            let _ = fs::remove_file(&target_path);
            return Err(AppError::new(
                "EXPORT_WRITE_FAILED",
                format!("导出写入失败：{error}"),
                true,
            ));
        }
        let mut digest = Sha256::new();
        digest.update(content.as_bytes());
        Ok(ExportResult {
            target_path: target_path.to_string_lossy().into_owned(),
            format: request.format,
            row_count: answer.claims.len() as u64,
            size_bytes: content.len() as u64,
            sha256: format!("{:x}", digest.finalize()),
        })
    })
    .await
    .map_err(|error| AppError::new("EXPORT_WRITE_FAILED", error.to_string(), true))?
}

fn render_answer_export(answer: &AnswerResult, format: &str) -> Result<String, AppError> {
    let mut output = String::new();
    if format == "md" {
        output.push_str("# 翻翻问答结果\n\n");
    }
    output.push_str(answer.answer.trim());
    output.push_str(if format == "md" {
        "\n\n## 引用依据\n"
    } else {
        "\n\n引用依据\n"
    });
    for (claim_index, claim) in answer.claims.iter().enumerate() {
        output.push_str(&format!("\n{}. {}\n", claim_index + 1, claim.text.trim()));
        for citation in &claim.citations {
            let source_name = answer
                .source_files
                .iter()
                .find(|source| source.file_id == citation.file_id)
                .map(|source| source.display_name.as_str())
                .unwrap_or("本地资料");
            let locator = serde_json::to_string(&citation.locator)
                .map_err(|error| AppError::new("EXPORT_DATA_INVALID", error.to_string(), false))?;
            if format == "md" {
                output.push_str(&format!(
                    "   - **{source_name}** · `{locator}`\n     > {}\n",
                    citation.quote.trim()
                ));
            } else {
                output.push_str(&format!(
                    "   - {source_name} · {locator}\n     {}\n",
                    citation.quote.trim()
                ));
            }
        }
    }
    output.push_str("\n由翻翻在本地依据已验证资料生成。\n");
    Ok(output)
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

#[tauri::command]
pub async fn speech_recognize(
    request: SpeechRecognitionInput,
    app: AppHandle,
    models: State<'_, ModelServiceState>,
    worker: State<'_, SpeechWorkerState>,
    runtime_manager: State<'_, RuntimeManagerState>,
) -> Result<SpeechRecognitionSession, AppError> {
    if !(8_000..=96_000).contains(&request.sample_rate)
        || request.samples.is_empty()
        || request.samples.len() > request.sample_rate as usize * 120
        || request
            .samples
            .iter()
            .any(|sample| !sample.is_finite() || !(-1.0..=1.0).contains(sample))
    {
        return Err(AppError::new(
            "SPEECH_AUDIO_INVALID",
            "录音数据无效或超过两分钟限制",
            false,
        ));
    }
    let artifact = models
        .get()?
        .active_artifact(ModelRole::Asr)?
        .ok_or_else(|| AppError::new("ASR_MODEL_UNAVAILABLE", "请先配置语音识别模型", false))?;
    let tokens_path = model_companion_path(&artifact, "tokens.txt")?;
    let vad_model_path = model_companion_path(&artifact, "silero_vad.onnx")?;
    let speech_worker = worker.0.clone();
    let runtime = runtime_manager.0.clone();
    let session_id = Uuid::now_v7();
    let threads = runtime
        .snapshot()?
        .budget
        .foreground_cpu_threads
        .clamp(1, 2);
    let _ = app.emit(
        "speech:partial",
        json!({ "session_id": session_id, "status": "recognizing" }),
    );
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut runtime_request = RuntimeTaskRequest::interactive(
            RuntimeTaskKind::SpeechRecognition,
            RuntimeBackendKind::SherpaOnnx,
        );
        runtime_request.model_id = Some(artifact.artifact_id.to_string());
        runtime_request.cpu_threads = threads;
        runtime_request.memory_bytes = 512 * 1024 * 1024;
        runtime_request.timeout = Duration::from_secs(30);
        runtime_request.idempotency_key = Some(format!("speech:asr:{session_id}"));
        let lease = runtime.acquire(runtime_request)?;
        match speech_worker.recognize_speech(&SpeechRecognitionRequest {
            model_path: artifact.local_path,
            tokens_path,
            vad_model_path,
            samples: request.samples,
            sample_rate: request.sample_rate,
            threads,
        }) {
            Ok(result) => {
                lease.complete();
                Ok(SpeechRecognitionSession {
                    session_id,
                    status: "completed",
                    result,
                    completed_at: Utc::now().to_rfc3339(),
                })
            }
            Err(error) => {
                lease.fail(error.code.clone());
                Err(error)
            }
        }
    })
    .await
    .map_err(|error| AppError::new("ASR_TASK_FAILED", error.to_string(), true))??;
    let _ = app.emit("speech:final", &result);
    Ok(result)
}

#[tauri::command]
pub async fn speech_synthesize_answer(
    request: SpeechSynthesisInput,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    worker: State<'_, SpeechWorkerState>,
    runtime_manager: State<'_, RuntimeManagerState>,
) -> Result<SpeechSynthesisSession, AppError> {
    if !(0.5..=2.0).contains(&request.speed) || request.speaker_id > 10_000 {
        return Err(AppError::new(
            "TTS_OPTIONS_INVALID",
            "语速或音色参数无效",
            false,
        ));
    }
    let catalog = catalog.get()?;
    let answer = catalog.answer_result(&request.message_id)?;
    catalog.validate_answer_evidence(&answer)?;
    if answer.answer.trim().is_empty() || answer.answer.chars().count() > 4_000 {
        return Err(AppError::new(
            "TTS_TEXT_INVALID",
            "当前回答为空或过长，无法安全朗读",
            false,
        ));
    }
    let artifact = models
        .get()?
        .active_artifact(ModelRole::Tts)?
        .ok_or_else(|| AppError::new("TTS_MODEL_UNAVAILABLE", "请先配置语音合成模型", false))?;
    let tokens_path = model_companion_path(&artifact, "tokens.txt")?;
    let lexicon_path = model_companion_path(&artifact, "lexicon.txt")?;
    let speech_worker = worker.0.clone();
    let runtime = runtime_manager.0.clone();
    let session_id = Uuid::now_v7();
    let message_id = request.message_id;
    let threads = runtime
        .snapshot()?
        .budget
        .foreground_cpu_threads
        .clamp(1, 2);
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut runtime_request = RuntimeTaskRequest::interactive(
            RuntimeTaskKind::SpeechSynthesis,
            RuntimeBackendKind::SherpaOnnx,
        );
        runtime_request.model_id = Some(artifact.artifact_id.to_string());
        runtime_request.cpu_threads = threads;
        runtime_request.memory_bytes = 768 * 1024 * 1024;
        runtime_request.timeout = Duration::from_secs(45);
        runtime_request.idempotency_key = Some(format!("speech:tts:{message_id}:{session_id}"));
        let lease = runtime.acquire(runtime_request)?;
        match speech_worker.synthesize_speech(&SpeechSynthesisRequest {
            model_path: artifact.local_path,
            tokens_path,
            lexicon_path,
            text: answer.answer,
            speaker_id: request.speaker_id,
            speed: request.speed,
            threads,
        }) {
            Ok(result) => {
                lease.complete();
                Ok(SpeechSynthesisSession {
                    session_id,
                    status: "completed",
                    message_id,
                    result,
                    completed_at: Utc::now().to_rfc3339(),
                })
            }
            Err(error) => {
                lease.fail(error.code.clone());
                Err(error)
            }
        }
    })
    .await
    .map_err(|error| AppError::new("TTS_TASK_FAILED", error.to_string(), true))??;
    let _ = app.emit("tts:chunk", &result);
    Ok(result)
}

fn model_companion_path(artifact: &ModelArtifact, file_name: &str) -> Result<String, AppError> {
    let parent = Path::new(&artifact.local_path)
        .parent()
        .ok_or_else(|| AppError::new("MODEL_COMPANION_MISSING", "模型组件目录无效", false))?;
    let candidate = parent.join(file_name);
    if candidate.is_symlink() || !candidate.is_file() {
        return Err(AppError::new(
            "MODEL_COMPANION_MISSING",
            format!("模型缺少配套文件 {file_name}"),
            false,
        ));
    }
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| AppError::new("MODEL_COMPANION_MISSING", error.to_string(), false))?;
    let canonical_candidate = fs::canonicalize(&candidate)
        .map_err(|error| AppError::new("MODEL_COMPANION_MISSING", error.to_string(), false))?;
    if !canonical_candidate.starts_with(canonical_parent) {
        return Err(AppError::new(
            "MODEL_COMPANION_INVALID",
            "模型配套文件超出受管模型目录",
            false,
        ));
    }
    Ok(canonical_candidate.to_string_lossy().into_owned())
}

fn active_ocr_runtime(app: &AppHandle, threads: u32) -> Result<Option<OcrRuntimeConfig>, AppError> {
    let manager = app.state::<ModelServiceState>().get()?;
    let Some(artifact) = manager.active_artifact(ModelRole::Ocr)? else {
        return Ok(None);
    };
    Ok(Some(OcrRuntimeConfig {
        model_path: artifact.local_path.clone(),
        det_model_path: model_companion_path(&artifact, "ch_PP-OCRv5_mobile_det.onnx")?,
        cls_model_path: model_companion_path(&artifact, "ch_ppocr_mobile_v2.0_cls_infer.onnx")?,
        dictionary_path: model_companion_path(&artifact, "ppocrv5_dict.txt")?,
        threads: threads.clamp(1, 2),
    }))
}

#[tauri::command(async)]
pub fn maintenance_get(
    catalog: State<'_, CatalogServiceState>,
) -> Result<MaintenanceSnapshot, AppError> {
    let mut snapshot = catalog.get()?.maintenance_snapshot()?;
    if let Ok(runtime_events) = crate::runtime_log::count() {
        snapshot.log_events = runtime_events;
    }
    Ok(snapshot)
}

/// index_activity_stats 的 TTL 缓存：状态栏 1.5s 轮询会触发 3 个 COUNT(DISTINCT)
/// 全表扫描（~0.5s/次），扫描/维护进行时前端退化为 1.5s 轮询会持续占满一个核。
/// 索引内容分钟级才变化，10s 缓存足够，把真实查询频次降到 1/6。
static INDEX_STATS_CACHE: OnceLock<Mutex<(Instant, IndexActivityStats)>> = OnceLock::new();
const INDEX_STATS_CACHE_TTL: Duration = Duration::from_secs(10);

/// home_get_summary 的进程内缓存：前端无扫描时 30s 兜底轮询 + 事件驱动刷新，
/// 命中缓存时 8 个查询整体跳过。扫描进行中跳过缓存直查（进度环要接近实时）。
/// TTL 保持 5s：事件 invalidate 后的下一次请求最多迟到 5s 旧值。
static HOME_SUMMARY_CACHE: OnceLock<Mutex<(Instant, String, Value)>> = OnceLock::new();
const HOME_SUMMARY_CACHE_TTL: Duration = Duration::from_secs(5);

/// Rerank 重排后只把相关性最高的前 N 条证据片段交给生成模型；
/// 未配置 Rerank 时不截断，保持全部证据进生成的既有行为。
const RERANK_TOP_EVIDENCE: usize = 3;

/// 证据门控阈值：rerank（sigmoid 0-1 概率）top-1 低于此值视为候选证据与
/// 用户原始问题无关，判定为路由误判（闲聊被判检索）→ 转闲聊直接回复，
/// 杜绝「检索答错把资料库内容当答案」。阈值可依据 trace 中 reranking
/// 节点记录的每候选 score 实测调优。
const RERANK_CHAT_FALLBACK_THRESHOLD: f32 = 0.1;

fn cached_index_activity_stats(
    catalog: &CatalogService,
) -> Result<IndexActivityStats, AppError> {
    let cache = INDEX_STATS_CACHE.get_or_init(|| {
        Mutex::new((
            Instant::now() - INDEX_STATS_CACHE_TTL,
            IndexActivityStats {
                discovered_files: 0,
                searchable_files: 0,
                parsed_files: 0,
                embedded_files: 0,
                ocr_pages: 0,
            },
        ))
    });
    let mut guard = cache
        .lock()
        .map_err(|_| AppError::new("STATUS_CACHE_LOCK", "状态统计缓存锁定失败", false))?;
    if guard.0.elapsed() >= INDEX_STATS_CACHE_TTL {
        *guard = (Instant::now(), catalog.index_activity_stats()?);
    }
    Ok(guard.1.clone())
}

#[tauri::command(async)]
pub fn app_status_get(
    catalog: State<'_, CatalogServiceState>,
    generation: State<'_, GenerationServiceState>,
    worker: State<'_, WorkerServiceState>,
    runtime_manager: State<'_, RuntimeManagerState>,
) -> Result<AppStatusSnapshot, AppError> {
    let catalog = catalog.get()?;
    let roots = catalog.list_roots()?;
    let active_scan = catalog.latest_active_scan_job()?;
    let index_stats = cached_index_activity_stats(&catalog)?;
    let mut maintenance = catalog.maintenance_snapshot()?;
    if let Ok(runtime_events) = crate::runtime_log::count() {
        maintenance.log_events = runtime_events;
    }
    let scan_progress = active_scan.map(|job| AppStatusScanProgress {
        scan_job_id: job.job_id,
        status: job.status,
        discovered_files: index_stats.discovered_files,
        searchable_files: index_stats.searchable_files,
        parsed_files: index_stats.parsed_files,
        embedded_files: index_stats.embedded_files,
        ocr_pages: index_stats.ocr_pages,
        progress: job.progress,
    });
    let mut recovery_actions = Vec::new();
    if maintenance.failed_files > 0 {
        recovery_actions.push("view_inbox");
    }
    if maintenance.pending_files > 0 || maintenance.active_jobs > 0 {
        recovery_actions.push("view_maintenance");
    }
    recovery_actions.push("view_models");
    let checked_at = maintenance.checked_at.to_rfc3339();
    let mut inference_runtime = inference_runtime_state(&generation)?;
    if worker.foreground_activity.load(Ordering::Acquire) > 0 {
        inference_runtime.pressure_reason =
            Some("正在优先处理搜索或问答，后台模型任务已让出".into());
    }
    Ok(AppStatusSnapshot {
        local_only: true,
        source_files_readonly: true,
        roots,
        scan_progress,
        maintenance,
        inference_runtime,
        ai_runtime: runtime_manager.0.snapshot()?,
        recovery_actions,
        checked_at,
    })
}

#[tauri::command(async)]
pub fn runtime_state_get(
    runtime_manager: State<'_, RuntimeManagerState>,
) -> Result<AiRuntimeSnapshot, AppError> {
    runtime_manager.0.snapshot()
}

#[derive(Debug, Deserialize)]
pub struct MaintenanceCheckRequest {
    level: String,
}

#[tauri::command(async)]
pub fn maintenance_check(
    request: MaintenanceCheckRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<fanfan_core::MaintenanceCheckResult, AppError> {
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
    models: State<'_, ModelServiceState>,
) -> Result<StorageUsageSnapshot, AppError> {
    storage_usage_snapshot(&environment.data_directory, models.get()?.model_root())
}

#[tauri::command(async)]
pub fn storage_location_get(
    environment: State<'_, EnvironmentServiceState>,
) -> crate::StorageLocationStatus {
    crate::storage_location_status(&environment.config_directory, &environment.data_directory)
}

#[derive(Debug, Deserialize)]
pub struct StorageMigrationScheduleRequest {
    selected_directory: String,
    confirmation: String,
}

#[tauri::command]
pub async fn storage_migration_schedule(
    request: StorageMigrationScheduleRequest,
    environment: State<'_, EnvironmentServiceState>,
    catalog: State<'_, CatalogServiceState>,
) -> Result<crate::StorageLocationStatus, AppError> {
    if request.confirmation != "MIGRATE_APPLICATION_STORAGE" {
        return Err(AppError::new(
            "STORAGE_MIGRATION_CONFIRMATION_REQUIRED",
            "迁移应用数据需要再次明确确认",
            false,
        ));
    }
    let config_directory = environment.config_directory.clone();
    let data_directory = environment.data_directory.clone();
    let selected_directory = PathBuf::from(request.selected_directory);
    let roots = catalog
        .get()?
        .list_roots()?
        .into_iter()
        .filter(|root| root.enabled)
        .map(|root| PathBuf::from(root.canonical_path))
        .collect::<Vec<_>>();
    tauri::async_runtime::spawn_blocking(move || {
        crate::schedule_storage_migration(
            &config_directory,
            &data_directory,
            &selected_directory,
            &roots,
        )
    })
    .await
    .map_err(|error| AppError::new("STORAGE_MIGRATION_SCHEDULE_FAILED", error.to_string(), true))?
}

#[tauri::command(async)]
pub fn cache_clear(
    request: CacheClearRequest,
    environment: State<'_, EnvironmentServiceState>,
    models: State<'_, ModelServiceState>,
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
        "failed_downloads" => models
            .get()?
            .model_root()
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
    let marker = parent.join(".com.fanfan.desktop-reset-request");
    fs::write(&marker, "RESET_APPLICATION_DATA")
        .map_err(|error| AppError::new("APP_DATA_RESET_MARKER_FAILED", error.to_string(), true))?;
    app.restart();
}

fn storage_usage_snapshot(
    data_directory: &Path,
    model_root: &Path,
) -> Result<StorageUsageSnapshot, AppError> {
    let database_size = ["fanfan.db", "fanfan.db-wal", "fanfan.db-shm"]
        .iter()
        .try_fold(0_u64, |total, name| {
            Ok::<u64, AppError>(total + file_size(&data_directory.join(name))?)
        })?;
    let installed_models = [
        "generation",
        "embedding",
        "vision",
        "reranker",
        "ocr",
        "tts",
        "asr",
    ]
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
    let soft_quota_bytes = default_quota;
    let over_soft_quota = total_bytes >= soft_quota_bytes;
    Ok(StorageUsageSnapshot {
        total_bytes,
        categories,
        data_directory: data_directory.to_string_lossy().into_owned(),
        disk_capacity_bytes,
        disk_available_bytes,
        soft_quota_bytes,
        over_soft_quota,
        background_tasks_paused: over_soft_quota,
        notice: over_soft_quota
            .then(|| "存储已达到软配额，暂停图片缓存、OCR和语义索引；搜索与预览继续可用".into()),
        measured_at: Utc::now().to_rfc3339(),
    })
}

fn background_storage_budget_allows(app: &AppHandle) -> bool {
    const GIB: u64 = 1024 * 1024 * 1024;
    if let Some((total, available)) = memory_status_bytes() {
        let reserve = (total / 5).max(2 * GIB);
        if available < reserve {
            if !MEMORY_PRESSURE_ACTIVE.swap(true, Ordering::AcqRel) {
                crate::runtime_log::event(
                    "warning",
                    "background",
                    "memory_budget.paused",
                    None,
                    &json!({
                        "available_bytes": available,
                        "reserve_bytes": reserve,
                    }),
                );
                let _ = app.emit(
                    "background:paused",
                    json!({
                        "reason": "system_memory_reserve",
                        "notice": "系统可用内存较低，已暂停新的后台模型任务",
                    }),
                );
            }
            return false;
        }
        if MEMORY_PRESSURE_ACTIVE.swap(false, Ordering::AcqRel) {
            crate::runtime_log::event(
                "info",
                "background",
                "memory_budget.resumed",
                None,
                &json!({ "available_bytes": available, "reserve_bytes": reserve }),
            );
        }
    }
    let environment = app.state::<EnvironmentServiceState>();
    let models = app.state::<ModelServiceState>();
    let Ok(models) = models.get() else {
        return false;
    };
    match storage_usage_snapshot(&environment.data_directory, models.model_root()) {
        Ok(snapshot) if snapshot.background_tasks_paused => {
            crate::runtime_log::event(
                "warning",
                "background",
                "storage_budget.paused",
                None,
                &json!({
                    "used_bytes": snapshot.total_bytes,
                    "soft_quota_bytes": snapshot.soft_quota_bytes,
                    "disk_available_bytes": snapshot.disk_available_bytes,
                }),
            );
            let _ = app.emit(
                "background:paused",
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
            crate::runtime_log::event(
                "error",
                "background",
                "storage_budget.measure_failed",
                None,
                &json!({ "error_code": error.code, "retryable": error.retryable }),
            );
            let _ = app.emit("background:paused", error);
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
    let runtime_page = crate::runtime_log::query(&request)?;
    if runtime_page.total > 0 {
        return Ok(runtime_page);
    }
    catalog.get()?.query_logs(&request)
}

#[tauri::command(async)]
pub fn maintenance_logs_clear(catalog: State<'_, CatalogServiceState>) -> Result<u64, AppError> {
    let runtime_removed = crate::runtime_log::clear()?;
    let database_removed = catalog.get()?.clear_logs()?;
    Ok(runtime_removed.saturating_add(database_removed))
}

#[tauri::command(async)]
pub fn node_trace_query(
    request: NodeTraceQuery,
    catalog: State<'_, CatalogServiceState>,
) -> Result<NodeTracePage, AppError> {
    catalog.get()?.query_node_traces(&request)
}

#[tauri::command(async)]
pub fn node_trace_clear(catalog: State<'_, CatalogServiceState>) -> Result<u64, AppError> {
    catalog.get()?.clear_node_traces()
}

#[derive(Debug, Deserialize)]
pub struct DiagnosticEventInput {
    level: String,
    component: String,
    event_name: String,
    #[serde(default)]
    correlation_id: Option<String>,
    #[serde(default = "empty_json_object")]
    fields: Value,
}

fn empty_json_object() -> Value {
    json!({})
}

#[tauri::command(async)]
pub fn diagnostic_event_append(request: DiagnosticEventInput) -> Result<(), AppError> {
    validate_diagnostic_identifier("component", &request.component, 80)?;
    validate_diagnostic_identifier("event_name", &request.event_name, 120)?;
    if let Some(correlation_id) = request.correlation_id.as_deref() {
        validate_diagnostic_identifier("correlation_id", correlation_id, 128)?;
    }
    if !matches!(
        request.level.as_str(),
        "debug" | "info" | "warning" | "warn" | "error"
    ) {
        return Err(AppError::new(
            "DIAGNOSTIC_LEVEL_INVALID",
            "诊断事件级别无效",
            false,
        ));
    }
    if !request.fields.is_object() {
        return Err(AppError::new(
            "DIAGNOSTIC_FIELDS_INVALID",
            "诊断事件字段必须是对象",
            false,
        ));
    }
    let field_bytes = serde_json::to_vec(&request.fields)
        .map_err(|error| AppError::new("DIAGNOSTIC_SERIALIZE_FAILED", error.to_string(), false))?;
    if field_bytes.len() > 16 * 1024 {
        return Err(AppError::new(
            "DIAGNOSTIC_FIELDS_TOO_LARGE",
            "单条诊断事件不能超过16KB",
            false,
        ));
    }
    crate::runtime_log::event(
        &request.level,
        &request.component,
        &request.event_name,
        request.correlation_id.as_deref(),
        &request.fields,
    );
    Ok(())
}

fn validate_diagnostic_identifier(
    field: &str,
    value: &str,
    max_length: usize,
) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > max_length
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._-".contains(character))
    {
        return Err(AppError::new(
            "DIAGNOSTIC_IDENTIFIER_INVALID",
            format!("诊断事件字段 {field} 无效"),
            false,
        ));
    }
    Ok(())
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
    models: State<'_, ModelServiceState>,
    startup: State<'_, crate::commands::startup::StartupServiceState>,
) -> Result<ExportResult, AppError> {
    if !request.confirmed {
        return Err(AppError::new(
            "EXPORT_CONFIRMATION_REQUIRED",
            "导出诊断包前需要明确确认",
            false,
        ));
    }
    let target = PathBuf::from(request.target_path.trim());
    if !target.is_absolute() || target.extension().and_then(|value| value.to_str()) != Some("json")
    {
        return Err(AppError::new(
            "EXPORT_TARGET_INVALID",
            "诊断包必须导出到绝对路径的JSON新文件",
            false,
        ));
    }
    let catalog = catalog.get()?;
    let mut snapshot = catalog.maintenance_snapshot()?;
    snapshot.log_events = crate::runtime_log::count().unwrap_or(snapshot.log_events);
    let runtime_logs = crate::runtime_log::recent_values(2_000)?;
    let database_logs = catalog.list_logs(500)?;
    let latest_environment = environment
        .latest
        .lock()
        .map_err(|_| AppError::new("ENVIRONMENT_STATE_UNAVAILABLE", "环境状态不可用", true))?
        .clone();
    let startup_state = startup
        .0
        .lock()
        .map_err(|_| AppError::new("STARTUP_STATE_UNAVAILABLE", "启动状态不可用", true))?
        .clone();
    let model_downloads = models
        .get()
        .and_then(|manager| manager.list_download_jobs())
        .unwrap_or_default();
    let payload = fanfan_core::sanitize_log_value(
        &json!({
            "schema_version": 1,
            "generated_at": Utc::now().to_rfc3339(),
            "app_version": env!("CARGO_PKG_VERSION"),
            "session_id": crate::runtime_log::session_id(),
            "startup": startup_state,
            "maintenance": snapshot,
            "environment": latest_environment,
            "model_downloads": model_downloads,
            "runtime_logs": runtime_logs,
            "catalog_logs": database_logs,
        }),
        None,
    );
    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|error| AppError::new("DIAGNOSTIC_SERIALIZE_FAILED", error.to_string(), false))?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| AppError::new("EXPORT_DIRECTORY_FAILED", error.to_string(), true))?;
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|error| {
            let code = if error.kind() == std::io::ErrorKind::AlreadyExists {
                "EXPORT_TARGET_EXISTS"
            } else {
                "EXPORT_CREATE_FAILED"
            };
            AppError::new(code, error.to_string(), true)
        })?;
    if let Err(error) = output.write_all(&bytes).and_then(|_| output.flush()) {
        drop(output);
        let _ = fs::remove_file(&target);
        return Err(AppError::new(
            "EXPORT_WRITE_FAILED",
            error.to_string(),
            true,
        ));
    }
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    Ok(ExportResult {
        target_path: target.to_string_lossy().into_owned(),
        format: "json".to_owned(),
        row_count: runtime_logs.len() as u64 + database_logs.len() as u64,
        size_bytes: bytes.len() as u64,
        sha256,
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
) -> Result<OperationHandle, AppError> {
    fanfan_core::validate_rebuild_confirmation(&request.confirmation)?;
    let catalog = catalog.get()?;
    let operation_id = Uuid::now_v7();
    let handle = OperationHandle {
        operation_id,
        kind: "index_rebuild",
        status: "queued",
        created_at: Utc::now().to_rfc3339(),
    };
    let completion_handle = handle.clone();
    crate::runtime_log::event(
        "info",
        "index",
        "index.rebuild_started",
        Some(&operation_id.to_string()),
        &json!({ "operation_id": operation_id }),
    );
    let _ = app.emit("index:rebuild_started", &handle);
    thread::spawn(move || match catalog.rebuild_index("REBUILD_INDEX") {
        Ok(result) => {
            crate::runtime_log::event(
                "info",
                "index",
                "index.rebuild_prepared",
                Some(&operation_id.to_string()),
                &json!({ "operation_id": operation_id, "reset_files": result.reset_files, "source_files_modified": false }),
            );
            let _ = app.emit("index:changed", json!({ "operation": { "operation_id": completion_handle.operation_id, "kind": completion_handle.kind, "status": "completed", "created_at": completion_handle.created_at }, "result": result }));
            spawn_parse_pending(app, catalog);
        }
        Err(error) => {
            crate::runtime_log::event(
                "error",
                "index",
                "index.rebuild_failed",
                Some(&operation_id.to_string()),
                &json!({ "operation_id": operation_id, "error_code": error.code, "retryable": error.retryable }),
            );
            let _ = app.emit("index:failed", &error);
        }
    });
    Ok(handle)
}

#[tauri::command(async)]
pub fn search_start(
    mut request: SearchRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
    runtime_manager: State<'_, RuntimeManagerState>,
) -> Result<SearchSession, AppError> {
    let _foreground_guard = ForegroundActivityGuard::begin(&worker.foreground_activity);
    let correlation_id = Uuid::now_v7().to_string();
    let started = Instant::now();
    crate::runtime_log::event(
        "info",
        "search",
        "search.started",
        Some(&correlation_id),
        &json!({
            "mode": request.mode,
            "query_length": request.query.chars().count(),
            "page_size": request.page_size,
            "has_cursor": request.cursor.is_some(),
            "root_scope_count": request.scope.root_ids.len(),
            "collection_scope_count": request.scope.collection_ids.len(),
            "file_scope_count": request.scope.file_ids.len(),
        }),
    );
    let catalog = catalog.get()?;
    let models = models.get()?;
    let original_query = request.query.clone();
    // 查询理解：将自然语言转换为结构化检索参数
    let query_runtime_lease = if is_natural_language_query(&request.query) {
        let mut runtime_request =
            RuntimeTaskRequest::interactive(RuntimeTaskKind::Search, RuntimeBackendKind::LlamaCpp);
        runtime_request.cpu_threads = background_inference_threads();
        runtime_request.timeout = Duration::from_secs(3);
        runtime_manager.0.acquire(runtime_request).ok()
    } else {
        None
    };
    let intent: Option<fanfan_core::QueryIntent> = if query_runtime_lease.is_some() {
        models
            .active_artifact(ModelRole::Generation)
            .ok()
            .flatten()
            .and_then(|artifact| {
                generation.0.lock().ok().and_then(|mut runtime| {
                    let threads = background_inference_threads();
                    let _ = runtime
                        .activate(&artifact.local_path, 4096, threads)
                        .or_else(|_| runtime.activate(&artifact.local_path, 2048, threads));
                    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                    let (system, user) = query_understanding_prompt(&request.query, &today);
                    let cancelled = std::sync::atomic::AtomicBool::new(false);
                    let generated: String = runtime
                        .complete_cancellable(&system, &user, 256, &cancelled)
                        .ok()?;
                    Some(parse_query_intent(&generated, &request.query))
                })
            })
    } else {
        None
    };
    if let Some(lease) = query_runtime_lease {
        lease.complete();
    }
    let intent_used = intent.is_some();
    let time_hint = intent.as_ref().and_then(|intent| {
        intent
            .time_hint
            .as_ref()
            .map(|hint| json!({ "from": hint.from, "to": hint.to }))
    });
    if let Some(intent) = intent {
        request.query = intent.rewritten_query;
        if let Some(hint) = intent.time_hint {
            if request.scope.modified_from.is_none() {
                request.scope.modified_from =
                    chrono::NaiveDate::parse_from_str(&hint.from, "%Y-%m-%d")
                        .ok()
                        .and_then(|d| d.and_hms_opt(0, 0, 0))
                        .map(|t| chrono::DateTime::<Utc>::from_naive_utc_and_offset(t, Utc));
            }
            if request.scope.modified_to.is_none() {
                request.scope.modified_to = chrono::NaiveDate::parse_from_str(&hint.to, "%Y-%m-%d")
                    .ok()
                    .and_then(|d| d.and_hms_opt(23, 59, 59))
                    .map(|t| chrono::DateTime::<Utc>::from_naive_utc_and_offset(t, Utc));
            }
        }
        if !intent.extension_hints.is_empty() && request.scope.extensions.is_empty() {
            request.scope.extensions = intent
                .extension_hints
                .into_iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .collect();
        }
    }
    trace_node(
        &catalog,
        "search",
        "understanding",
        &correlation_id,
        None,
        None,
        &json!({ "query": original_query }),
        &json!({
            "rewritten_query": request.query.clone(),
            "time_hint": time_hint,
            "extensions": request.scope.extensions,
            "used": intent_used,
        }),
        "ok",
        None,
    );
    if matches!(request.mode, SearchMode::Semantic | SearchMode::Hybrid)
        && let Some(artifact) = models.active_artifact(ModelRole::Embedding)?
    {
        let mut runtime_request = RuntimeTaskRequest::interactive(
            RuntimeTaskKind::Search,
            RuntimeBackendKind::OnnxRuntime,
        );
        runtime_request.cpu_threads = 2;
        runtime_request.timeout = Duration::from_secs(5);
        let mut embedding_runtime_lease = runtime_manager.0.acquire(runtime_request).ok();
        let tokenizer_path = PathBuf::from(&artifact.local_path)
            .parent()
            .map(|parent| parent.join("tokenizer.json"));
        if embedding_runtime_lease.is_some()
            && let Some(tokenizer_path) = tokenizer_path
            && let Ok(response) = worker.client.encode_embeddings(&EmbeddingRequest {
                model_path: artifact.local_path,
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                texts: vec![request.query.clone()],
                max_length: 512,
                threads: 2,
            })
            && let Some(vector) = response.vectors.first()
        {
            let result = catalog.search_with_semantic(
                &request,
                Some(SemanticQuery {
                    model_artifact_id: &artifact.artifact_id.to_string(),
                    vector,
                }),
            )?;
            crate::runtime_log::event(
                "info",
                "search",
                "search.completed",
                Some(&correlation_id),
                &json!({
                    "search_id": result.search_id,
                    "result_count": result.results.len(),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                    "semantic_used": true,
                    "has_more": result.next_cursor.is_some(),
                }),
            );
            trace_node(
                &catalog,
                "search",
                "retrieval",
                &correlation_id,
                None,
                None,
                &json!({
                    "query": request.query,
                    "mode": format!("{:?}", request.mode),
                    "scope": json!({
                        "roots": request.scope.root_ids.len(),
                        "collections": request.scope.collection_ids.len(),
                        "files": request.scope.file_ids.len(),
                        "extensions": request.scope.extensions,
                    }),
                }),
                &json!({
                    "result_count": result.results.len(),
                    "semantic_used": true,
                    "top": result.results.iter().take(10).map(|hit| json!({
                        "file_name": hit.name,
                        "locator": hit.locator,
                        "snippet": compact_for_prompt(&hit.snippet, 200),
                    })).collect::<Vec<_>>(),
                }),
                "ok",
                Some(started.elapsed().as_millis() as u64),
            );
            if let Some(lease) = embedding_runtime_lease.take() {
                lease.complete();
            }
            return Ok(result);
        }
        if let Some(lease) = embedding_runtime_lease {
            lease.complete();
        }
    }
    let result = catalog.search(&request)?;
    crate::runtime_log::event(
        "info",
        "search",
        "search.completed",
        Some(&correlation_id),
        &json!({
            "search_id": result.search_id,
            "result_count": result.results.len(),
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "semantic_used": false,
            "has_more": result.next_cursor.is_some(),
        }),
    );
    trace_node(
        &catalog,
        "search",
        "retrieval",
        &correlation_id,
        None,
        None,
        &json!({
            "query": request.query,
            "mode": format!("{:?}", request.mode),
            "scope": json!({
                "roots": request.scope.root_ids.len(),
                "collections": request.scope.collection_ids.len(),
                "files": request.scope.file_ids.len(),
                "extensions": request.scope.extensions,
            }),
        }),
        &json!({
            "result_count": result.results.len(),
            "semantic_used": false,
            "top": result.results.iter().take(10).map(|hit| json!({
                "file_name": hit.name,
                "locator": hit.locator,
                "snippet": compact_for_prompt(&hit.snippet, 200),
            })).collect::<Vec<_>>(),
        }),
        "ok",
        Some(started.elapsed().as_millis() as u64),
    );
    Ok(result)
}

#[tauri::command(async)]
pub fn ask_start(
    mut request: AskRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    worker: State<'_, WorkerServiceState>,
    generation: State<'_, GenerationServiceState>,
    runtime_manager: State<'_, RuntimeManagerState>,
) -> Result<OperationHandle, AppError> {
    if request.session_id.is_none() {
        request.session_id = Some(Uuid::now_v7());
    }
    request.validate()?;
    let catalog = catalog.get()?;
    let models = models.get()?;
    let operation_id = Uuid::now_v7();
    crate::runtime_log::event(
        "info",
        "rag",
        "ask.queued",
        Some(&operation_id.to_string()),
        &json!({
            "operation_id": operation_id,
            "session_id": request.session_id,
            "mode": "rag",
            "question_length": request.question.chars().count(),
            "strict_evidence": request.strict_evidence,
            "retrieval_limit": request.retrieval_limit,
            "max_source_files": request.max_source_files,
            "root_scope_count": request.scope.root_ids.len(),
            "collection_scope_count": request.scope.collection_ids.len(),
            "file_scope_count": request.scope.file_ids.len(),
        }),
    );
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
    let runtime_manager = runtime_manager.0.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let foreground_worker = app.state::<WorkerServiceState>();
        let _foreground_guard =
            ForegroundActivityGuard::begin(&foreground_worker.foreground_activity);
        let operation_started = Instant::now();
        let generation_artifact_id = models
            .active_artifact(ModelRole::Generation)
            .ok()
            .flatten()
            .map(|artifact| artifact.artifact_id.to_string());
        let mut runtime_request =
            RuntimeTaskRequest::interactive(RuntimeTaskKind::Ask, RuntimeBackendKind::LlamaCpp);
        runtime_request.cpu_threads = interactive_inference_threads();
        runtime_request.timeout = Duration::from_secs(45);
        runtime_request.model_id = generation_artifact_id;
        runtime_request.idempotency_key = Some(format!("ask:{operation_id}"));
        let runtime_lease = match runtime_manager.acquire(runtime_request) {
            Ok(lease) => lease,
            Err(error) => {
                if let Ok(mut entries) = operations.0.lock()
                    && let Some(entry) = entries.get_mut(&operation_id)
                {
                    entry.handle.status = "failed";
                    entry.error = Some(error.clone());
                }
                let _ = app.emit(
                    "ask:failed",
                    json!({"operation_id": operation_id, "error": error}),
                );
                crate::runtime_log::event(
                    "error",
                    "runtime",
                    "runtime.lease_failed",
                    Some(&operation_id.to_string()),
                    &json!({"task_kind": "ask", "error_code": error.code}),
                );
                return;
            }
        };
        if let Ok(mut entries) = operations.0.lock()
            && let Some(entry) = entries.get_mut(&operation_id)
        {
            entry.handle.status = "running";
        }
        let phase_app = app.clone();
        let claim_app = app.clone();
        let phase = |name: &str, progress: f64| {
            crate::runtime_log::event(
                "info",
                "rag",
                "ask.phase_changed",
                Some(&operation_id.to_string()),
                &json!({
                    "operation_id": operation_id,
                    "phase": name,
                    "progress": progress,
                }),
            );
            let _ = phase_app.emit(
                "ask:phase",
                json!({"operation_id": operation_id, "phase": name, "progress": progress}),
            );
        };
        let verified_claim = |claim: &AnswerClaim| {
            crate::runtime_log::event(
                "info",
                "rag",
                "ask.claim_verified",
                Some(&operation_id.to_string()),
                &json!({
                    "operation_id": operation_id,
                    "claim_id": claim.claim_id,
                    "citation_count": claim.citations.len(),
                }),
            );
            for citation in &claim.citations {
                let _ = claim_app.emit(
                    "ask:citation",
                    json!({"operation_id": operation_id, "claim_id": claim.claim_id, "citation": citation}),
                );
            }
            let _ = claim_app.emit(
                "ask:claim",
                json!({"operation_id": operation_id, "claim": claim}),
            );
            let _ = claim_app.emit(
                "ask:token",
                json!({"operation_id": operation_id, "token": format!("{}\n", claim.text), "verified": true}),
            );
        };
        let result = compute_answer(
            &request,
            &catalog,
            &models,
            &worker,
            &generation,
            &runtime_manager,
            operation_id,
            &cancelled,
            (&phase, &verified_claim),
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
                "ask:cancelled",
                json!({"operation_id": operation_id, "error": error}),
            );
            crate::runtime_log::event(
                "warning",
                "rag",
                "ask.cancelled",
                Some(&operation_id.to_string()),
                &json!({
                    "operation_id": operation_id,
                    "elapsed_ms": operation_started.elapsed().as_millis() as u64,
                }),
            );
            runtime_lease.fail("OPERATION_CANCELLED");
            return;
        }
        match result {
            Ok(answer) => {
                if cancelled.load(Ordering::Acquire) {
                    let error = AppError::new("OPERATION_CANCELLED", "问答已取消", false);
                    if let Ok(mut entries) = operations.0.lock()
                        && let Some(entry) = entries.get_mut(&operation_id)
                    {
                        entry.handle.status = "cancelled";
                        entry.error = Some(error.clone());
                    }
                    let _ = app.emit(
                        "ask:cancelled",
                        json!({"operation_id": operation_id, "error": error}),
                    );
                    crate::runtime_log::event(
                        "warning",
                        "rag",
                        "ask.cancelled",
                        Some(&operation_id.to_string()),
                        &json!({
                            "operation_id": operation_id,
                            "elapsed_ms": operation_started.elapsed().as_millis() as u64,
                        }),
                    );
                    runtime_lease.fail("OPERATION_CANCELLED");
                    return;
                }
                if let Ok(mut entries) = operations.0.lock()
                    && let Some(entry) = entries.get_mut(&operation_id)
                {
                    entry.handle.status = "completed";
                    entry.result = Some(answer.clone());
                }
                let _ = app.emit(
                    "ask:completed",
                    json!({"operation_id": operation_id, "result": answer}),
                );
                crate::runtime_log::event(
                    "info",
                    "rag",
                    "ask:completed",
                    Some(&operation_id.to_string()),
                    &json!({
                        "operation_id": operation_id,
                        "session_id": answer.session_id,
                        "answer_mode": answer.answer_mode,
                        "claim_count": answer.claims.len(),
                        "source_file_count": answer.source_files.len(),
                        "citation_count": answer.claims.iter().map(|claim| claim.citations.len()).sum::<usize>(),
                        "insufficient_evidence": answer.insufficient_evidence,
                        "index_coverage": answer.index_coverage,
                        "retrieval_channels": answer.retrieval_channels,
                        "elapsed_ms": operation_started.elapsed().as_millis() as u64,
                    }),
                );
                runtime_lease.complete();
            }
            Err(error) => {
                if error.code != "OPERATION_CANCELLED" {
                    let _ = catalog.record_ask_failure(&request, &error);
                }
                if let Ok(mut entries) = operations.0.lock()
                    && let Some(entry) = entries.get_mut(&operation_id)
                {
                    entry.handle.status = "failed";
                    entry.error = Some(error.clone());
                }
                let _ = app.emit(
                    "ask:failed",
                    json!({"operation_id": operation_id, "error": error}),
                );
                crate::runtime_log::event(
                    "error",
                    "rag",
                    "ask:failed",
                    Some(&operation_id.to_string()),
                    &json!({
                        "operation_id": operation_id,
                        "error_code": error.code,
                        "retryable": error.retryable,
                        "elapsed_ms": operation_started.elapsed().as_millis() as u64,
                    }),
                );
                runtime_lease.fail(error.code.clone());
            }
        }
    });
    Ok(handle)
}

/// 节点追踪：一条链路节点的输入输出快照，明文落库（失败静默，绝不影响主链路）。
fn trace_node(
    catalog: &CatalogService,
    flow: &str,
    node: &str,
    correlation_id: &str,
    session_id: Option<&str>,
    entity_id: Option<&str>,
    input_json: &Value,
    output_json: &Value,
    status: &str,
    elapsed_ms: Option<u64>,
) {
    let _ = catalog.record_node_trace(
        flow,
        node,
        correlation_id,
        session_id,
        entity_id,
        &truncate_for_trace(input_json),
        &truncate_for_trace(output_json),
        status,
        elapsed_ms,
    );
}

/// 追踪字段容量控制：单个字符串字段截 8KB；序列化后仍超 8KB 则整体降级为截断预览。
fn truncate_for_trace(value: &Value) -> Value {
    const NODE_TRACE_LIMIT: usize = 8000;
    fn cap_strings(value: &mut Value, limit: usize) {
        match value {
            Value::String(text) if text.chars().count() > limit => {
                let mut kept = text.chars().take(limit).collect::<String>();
                kept.push_str("\n…[已截断]");
                *text = kept;
            }
            Value::Array(items) => items.iter_mut().for_each(|item| cap_strings(item, limit)),
            Value::Object(map) => map
                .values_mut()
                .for_each(|item| cap_strings(item, limit)),
            _ => {}
        }
    }
    let mut capped = value.clone();
    cap_strings(&mut capped, NODE_TRACE_LIMIT);
    let serialized = capped.to_string();
    if serialized.len() <= NODE_TRACE_LIMIT {
        return capped;
    }
    let mut end = NODE_TRACE_LIMIT;
    while !serialized.is_char_boundary(end) {
        end -= 1;
    }
    json!({ "__truncated": true, "preview": &serialized[..end] })
}

#[allow(clippy::too_many_arguments)]
fn compute_answer(
    request: &AskRequest,
    catalog: &CatalogService,
    models: &ModelManager,
    worker: &WorkerClient,
    generation: &Mutex<LocalGenerationRuntime>,
    runtime_manager: &RuntimeManager,
    operation_id: Uuid,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
) -> Result<AnswerResult, AppError> {
    let (phase, verified_claim) = progress;
    if cancelled.load(Ordering::Acquire) {
        return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
    }
    phase("intent_routing", 0.05);
    let generation_artifact = models.active_artifact(ModelRole::Generation)?;
    let maintenance = catalog.maintenance_snapshot()?;
    let generation_artifact = generation_artifact.ok_or_else(|| {
        AppError::new(
            "RAG_GENERATION_MODEL_REQUIRED",
            "问资料需要先配置并通过自检的本地生成模型",
            false,
        )
    })?;
    // 路由前加载一次会话历史（最多 20 条，覆盖路由/闲聊/检索生成的 5+5 需要），
    // 三个环节共用同一份；当前轮次问答在 record_ask_exchange 之后才落库，不含本轮。
    let history = request
        .session_id
        .map(|session_id| catalog.load_ask_history(&session_id, 20))
        .transpose()?
        .unwrap_or_default();
    let routing_started = Instant::now();
    let (routing, routing_raw) = route_question(
        request,
        &history,
        generation,
        &generation_artifact,
        cancelled,
    )?;
    let session_id = request.session_id.map(|id| id.to_string());
    trace_node(
        &catalog,
        "ask",
        "routing",
        &operation_id.to_string(),
        session_id.as_deref(),
        None,
        &json!({ "question": request.question }),
        &json!({
            "router": "llm",
            "intent": format!("{:?}", routing.intent),
            "top_category": format!("{:?}", routing.top_category),
            "top_score": routing.top_score,
            "margin": routing.margin,
            "router_active": routing.top_score > 0.0,
            "arbitration_raw": routing_raw,
        }),
        "ok",
        Some(routing_started.elapsed().as_millis() as u64),
    );
    match routing.intent {
        Intent::Chat => run_chat_answer(
            request,
            catalog,
            generation,
            &generation_artifact,
            &maintenance,
            &history,
            operation_id,
            cancelled,
            phase,
        ),
        _ => {
            // Embedding 只被检索分支需要：闲聊在缺 Embedding 时正常工作。
            let embedding = models.active_artifact(ModelRole::Embedding)?;
            run_retrieval_answer(
                request,
                catalog,
                models,
                worker,
                runtime_manager,
                generation,
                generation_artifact,
                embedding,
                maintenance,
                &history,
                operation_id,
                cancelled,
                (phase, verified_claim),
            )
        }
    }
}

/// 意图路由：LLM 直路由。生成模型按 5+5 对话历史判断走检索还是闲聊
/// （≤32 token 的 JSON 补全，模型在 ask 租约阶段已加载）。
/// 解析失败或调用失败默认 Chat——闲聊答错无副作用，检索答错会把资料库内容
/// 当答案胡说（0.6B 仲裁失败的教训）。返回路由 LLM 原始输出供节点追踪复盘。
/// 回复内容一律由模型生成，不做任何关键词/白名单规则分流。
fn route_question(
    request: &AskRequest,
    history: &[AskMessage],
    generation: &Mutex<LocalGenerationRuntime>,
    generation_artifact: &ModelArtifact,
    cancelled: &AtomicBool,
) -> Result<(RouteDecision, Option<String>), AppError> {
    let fallback_chat = RouteDecision {
        intent: Intent::Chat,
        top_category: Intent::Chat,
        top_score: 0.0,
        margin: 0.0,
    };
    // 档位自适应：0.6B 用词级两字输出的迷你版 prompt（词级续写是它的强项，
    // JSON 输出与长 prompt 分类是它误判的放大器）；更强模型用完整定义版。
    let (system, user) = if generation_artifact
        .model_id
        .to_lowercase()
        .contains("0.6b")
    {
        intent_routing_prompt_mini(request.question.trim(), history)
    } else {
        intent_routing_prompt(request.question.trim(), history)
    };
    let raw = match complete_with_model(generation, generation_artifact, &system, &user, 32, cancelled)
    {
        Ok(raw) => raw,
        Err(_) => return Ok((fallback_chat, None)),
    };
    let decision = match parse_intent_verdict(&raw) {
        Some(intent) => RouteDecision {
            intent,
            top_category: intent,
            top_score: 0.0,
            margin: 0.0,
        },
        None => fallback_chat,
    };
    Ok((decision, Some(raw)))
}

/// 闲聊分支：跳过检索/索引 gate，直接用生成模型对话（带会话历史）。
fn run_chat_answer(
    request: &AskRequest,
    catalog: &CatalogService,
    generation: &Mutex<LocalGenerationRuntime>,
    generation_artifact: &ModelArtifact,
    maintenance: &MaintenanceSnapshot,
    history: &[AskMessage],
    operation_id: Uuid,
    cancelled: &AtomicBool,
    phase: &dyn Fn(&str, f64),
) -> Result<AnswerResult, AppError> {
    if maintenance.degradation_level == "core" {
        return Err(AppError::new(
            "RAG_RESOURCE_PRESSURE",
            "当前资源压力较高，暂未启动回答；请稍后重试",
            true,
        ));
    }
    phase("chat_generating", 0.7);
    // 档位自适应：0.6B 用示例复读版 prompt + 更高采样温度（0.1 下只会复读
    // system 自我介绍或套「你好！…？」模板复读问题，日志实测）；强模型
    // 维持完整 prompt + 0.1 不变。
    let is_mini = generation_artifact
        .model_id
        .to_lowercase()
        .contains("0.6b");
    let (system, user) = if is_mini {
        chat_prompt_mini(request.question.trim(), history)
    } else {
        chat_prompt(request, history)
    };
    let started_at = Instant::now();
    let answer = if is_mini {
        complete_chat_with_model(
            generation,
            generation_artifact,
            &system,
            &user,
            512,
            0.6,
            cancelled,
        )?
    } else {
        complete_with_model(
            generation,
            generation_artifact,
            &system,
            &user,
            512,
            cancelled,
        )?
    };
    let result = AnswerResult {
        session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
        message_id: Uuid::now_v7(),
        answer: answer.trim().to_owned(),
        grounding_status: fanfan_core::GroundingStatus::Insufficient,
        insufficient_evidence: false,
        claims: Vec::new(),
        source_files: Vec::new(),
        used_file_ids: Vec::new(),
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        answer_mode: "chat".into(),
        retrieval_channels: Vec::new(),
        index_coverage: 0.0,
        degradation_reason: None,
    };
    let session_id = request.session_id.map(|id| id.to_string());
    trace_node(
        catalog,
        "ask",
        "completed",
        &operation_id.to_string(),
        session_id.as_deref(),
        None,
        &json!({}),
        &json!({
            "answer_mode": result.answer_mode,
            "answer": result.answer,
            "claim_count": result.claims.len(),
            "grounding_status": format!("{:?}", result.grounding_status),
            "insufficient_evidence": result.insufficient_evidence,
            "degradation_reason": result.degradation_reason,
            "elapsed_ms": result.elapsed_ms,
        }),
        "ok",
        Some(result.elapsed_ms),
    );
    catalog.record_ask_exchange(request, &result)?;
    phase("completed", 1.0);
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn run_retrieval_answer(
    request: &AskRequest,
    catalog: &CatalogService,
    models: &ModelManager,
    worker: &WorkerClient,
    runtime_manager: &RuntimeManager,
    generation: &Mutex<LocalGenerationRuntime>,
    generation_artifact: ModelArtifact,
    embedding: Option<ModelArtifact>,
    maintenance: MaintenanceSnapshot,
    history: &[AskMessage],
    operation_id: Uuid,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
) -> Result<AnswerResult, AppError> {
    let (phase, verified_claim) = progress;
    let correlation_id = operation_id.to_string();
    let session_id = request.session_id.map(|id| id.to_string());
    let session_id_ref = session_id.as_deref();
    phase("understanding", 0.08);
    let index_coverage = if let Some(embedding) = embedding.as_ref() {
        catalog
            .semantic_index_coverage(&request.scope, &embedding.artifact_id.to_string())?
            .1
    } else {
        0.0
    };
    let embedding = embedding.ok_or_else(|| {
        AppError::new(
            "RAG_EMBEDDING_MODEL_REQUIRED",
            "问资料需要先配置并通过自检的中文 Embedding 模型",
            false,
        )
    })?;
    if index_coverage <= 0.0 {
        return Err(AppError::new(
            "RAG_SEMANTIC_INDEX_REQUIRED",
            "当前检索范围尚未建立语义索引，完整 RAG 已停止",
            true,
        ));
    }
    if maintenance.degradation_level == "core" {
        return Err(AppError::new(
            "RAG_RESOURCE_PRESSURE",
            "当前资源压力较高，完整 RAG 暂未启动；请稍后重试",
            true,
        ));
    }
    // 改写：无条件尝试（空泛/一题多问/指代上文都交给模型决定，行式输出
    // 每行一个问题；已明确单一的问题模型会原样复读）。解析失败/为空 →
    // 回退用户原始问题，不中断检索（0.6B 改写质量波动时保证不劣于现状）。
    let rewritten_queries = {
        let (system, user) = query_rewrite_prompt(request.question.trim(), history);
        let rewritten = complete_with_model(
            generation,
            &generation_artifact,
            &system,
            &user,
            160,
            cancelled,
        )?;
        parse_rewritten_queries(&rewritten)
    };
    let retrieval_questions = if rewritten_queries.is_empty() {
        vec![request.question.trim().to_owned()]
    } else {
        rewritten_queries
    };
    let history_count = history.len();
    let rewritten_marked = retrieval_questions.len() > 1
        || retrieval_questions
            .first()
            .is_some_and(|question| question != request.question.trim());
    trace_node(
        catalog,
        "ask",
        "understanding",
        &correlation_id,
        session_id_ref,
        None,
        &json!({ "question": request.question, "history_count": history_count }),
        &json!({
            "rewritten_queries": &retrieval_questions,
            "rewritten": rewritten_marked,
        }),
        "ok",
        None,
    );
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
    // 路由改为 LLM 直路由后不再产生问题向量，恒按检索问题（改写拆分后的
    // 多个问题批量编码一次，分别检索后合并）自行编码。
    let embedding_texts = retrieval_questions
        .iter()
        .map(|question| {
            format!(
                "{}{}",
                embedding.query_prefix.as_deref().unwrap_or(""),
                question
            )
        })
        .collect::<Vec<_>>();
    let mut embedding_runtime_request = RuntimeTaskRequest::interactive(
        RuntimeTaskKind::Embedding,
        RuntimeBackendKind::OnnxRuntime,
    );
    embedding_runtime_request.cpu_threads = 2;
    embedding_runtime_request.timeout = Duration::from_secs(10);
    embedding_runtime_request.model_id = Some(embedding.artifact_id.to_string());
    let embedding_runtime_lease = runtime_manager.acquire(embedding_runtime_request)?;
    let response = worker.encode_embeddings(&EmbeddingRequest {
        model_path: embedding.local_path.clone(),
        tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
        texts: embedding_texts,
        max_length: embedding.max_length.unwrap_or(512),
        threads: 2,
    })?;
    embedding_runtime_lease.complete();
    if response.vectors.len() != retrieval_questions.len() {
        return Err(AppError::new(
            "EMBEDDING_EMPTY",
            "Embedding 运行时没有返回全部查询向量",
            true,
        ));
    }
    let artifact_id = embedding.artifact_id.to_string();
    let mut sub_results = Vec::with_capacity(retrieval_questions.len());
    for (question, vector) in retrieval_questions.iter().zip(response.vectors.iter()) {
        let mut sub_request = request.clone();
        sub_request.question = question.clone();
        sub_request.retrieval_limit = sub_request.retrieval_limit.min(10);
        sub_request.max_source_files = sub_request.max_source_files.min(6);
        sub_results.push(catalog.answer_extractively(
            &sub_request,
            Some(SemanticQuery {
                model_artifact_id: &artifact_id,
                vector,
            }),
        )?);
    }
    let mut extractive = merge_extractive_results(sub_results);
    extractive.index_coverage = index_coverage;
    extractive.retrieval_channels = vec![
        "filename".into(),
        "fts".into(),
        "embedding".into(),
        "rrf".into(),
        "mmr".into(),
    ];
    trace_node(
        catalog,
        "ask",
        "retrieval",
        &correlation_id,
        session_id_ref,
        None,
        &json!({ "questions": &retrieval_questions, "sub_query_count": retrieval_questions.len() }),
        &json!({
            "channels": extractive.retrieval_channels,
            "insufficient_evidence": extractive.insufficient_evidence,
            "candidates": extractive.claims.iter().take(10).map(|claim| json!({
                "file_id": claim.citations.first().map(|citation| citation.file_id.to_string()),
                "quote": compact_for_prompt(&claim.text, 500),
                "citations": claim.citations.len(),
            })).collect::<Vec<_>>(),
        }),
        "ok",
        None,
    );
    if extractive.insufficient_evidence {
        extractive.answer_mode = "rag_refusal".into();
        trace_node(
            catalog,
            "ask",
            "completed",
            &correlation_id,
            session_id_ref,
            None,
            &json!({}),
            &json!({
                "answer_mode": "rag_refusal",
                "answer": extractive.answer,
                "claim_count": 0,
                "grounding_status": format!("{:?}", extractive.grounding_status),
                "insufficient_evidence": true,
                "degradation_reason": extractive.degradation_reason,
            }),
            "ok",
            Some(extractive.elapsed_ms),
        );
        catalog.record_ask_exchange(request, &extractive)?;
        phase("completed", 1.0);
        return Ok(extractive);
    }
    let mut reranker_applied = false;
    let mut claims_before_truncate = 0usize;
    // rerank 的完整候选文档（截断前构建，节点追踪要看全部输入）
    let mut rerank_documents: Vec<String> = Vec::new();
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
            rerank_documents = extractive
                .claims
                .iter()
                .map(|claim| compact_for_prompt(&claim.text, 12_000))
                .collect::<Vec<_>>();
            let mut rerank_runtime_request = RuntimeTaskRequest::interactive(
                RuntimeTaskKind::Rerank,
                RuntimeBackendKind::OnnxRuntime,
            );
            rerank_runtime_request.cpu_threads = 2;
            rerank_runtime_request.timeout = Duration::from_secs(10);
            rerank_runtime_request.model_id = Some(reranker.artifact_id.to_string());
            if let Ok(rerank_runtime_lease) = runtime_manager.acquire(rerank_runtime_request)
                && let Ok(response) = worker.rerank(&RerankRequest {
                    model_path: reranker.local_path,
                    tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                    // rerank 一律用用户原始问题排序（改写只服务检索召回，
                    // 排序要贴近用户真实意图，与最终生成的依据一致）。
                    query: request.question.trim().to_owned(),
                    documents: rerank_documents.clone(),
                    max_length: reranker.max_length.unwrap_or(512),
                    threads: 2,
                })
                && apply_rerank_scores(&mut extractive, &response.scores).is_ok()
            {
                rerank_runtime_lease.complete();
                // 证据门控：rerank 后 top-1 分数过低 → 候选证据与用户原始
                // 问题无关，大概率路由误判（闲聊被判检索）→ 转闲聊直接回复，
                // 杜绝「检索答错把资料库内容当答案」。阈值据 trace 实测调优。
                let top_score = extractive
                    .claims
                    .first()
                    .and_then(|claim| claim.citations.first())
                    .map(|citation| citation.retrieval_score)
                    .unwrap_or(0.0);
                if top_score < RERANK_CHAT_FALLBACK_THRESHOLD {
                    trace_node(
                        catalog,
                        "ask",
                        "reranking",
                        &correlation_id,
                        session_id_ref,
                        None,
                        &json!({
                            "query": request.question,
                            "document_count": rerank_documents.len(),
                            "documents": rerank_documents
                                .iter()
                                .map(|document| compact_for_prompt(document, 600))
                                .collect::<Vec<_>>(),
                        }),
                        &json!({
                            "fallback": "rerank_chat",
                            "top_score": top_score,
                            "threshold": RERANK_CHAT_FALLBACK_THRESHOLD,
                        }),
                        "ok",
                        None,
                    );
                    return run_chat_answer(
                        request,
                        catalog,
                        generation,
                        &generation_artifact,
                        &maintenance,
                        history,
                        operation_id,
                        cancelled,
                        phase,
                    );
                }
                // 重排后只把相关性最高的前 N 条证据片段交给生成模型，
                // 其余丢弃（生成 prompt 只能引用保留的 S1..SN）。
                claims_before_truncate = extractive.claims.len();
                extractive.claims.truncate(RERANK_TOP_EVIDENCE);
                extractive.retrieval_channels.push("reranker".into());
                reranker_applied = true;
            }
        }
    }
    trace_node(
        catalog,
        "ask",
        "reranking",
        &correlation_id,
        session_id_ref,
        None,
        &json!({
            "query": request.question,
            "document_count": rerank_documents.len(),
            "documents": rerank_documents
                .iter()
                .map(|document| compact_for_prompt(document, 600))
                .collect::<Vec<_>>(),
        }),
        &json!({
            "applied": reranker_applied,
            "claims_before_truncate": claims_before_truncate,
            "claims_kept": extractive.claims.len(),
            "scores": extractive.claims.iter().take(10).map(|claim| json!({
                "text": compact_for_prompt(&claim.text, 120),
                "score": claim.citations.first().map(|citation| citation.retrieval_score),
            })).collect::<Vec<_>>(),
        }),
        "ok",
        None,
    );
    phase("evidence_selection", 0.48);
    let mut image_analysis_context = Vec::new();
    if reranker_applied && models.active_artifact(ModelRole::Vision)?.is_some() {
        let image_assets = extractive
            .claims
            .iter()
            .flat_map(|claim| claim.citations.iter())
            .filter_map(|citation| citation.image_asset_id)
            .collect::<HashSet<_>>();
        if !image_assets.is_empty() {
            phase("image_reanalysis", 0.54);
            for asset_id in image_assets.into_iter().take(3) {
                if cancelled.load(Ordering::Acquire) {
                    return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
                }
                if let Ok(analysis) = run_image_deep_analysis(
                    catalog,
                    models,
                    generation,
                    asset_id,
                    request.question.trim(),
                    cancelled,
                ) {
                    image_analysis_context.push(format!(
                        "图片资产 {} 的当前问题复核：{}；可见依据：{}；不确定项：{}",
                        analysis.asset_id,
                        analysis.answer,
                        analysis.observations.join("；"),
                        analysis.uncertainties.join("；")
                    ));
                }
            }
            if !image_analysis_context.is_empty() {
                extractive
                    .retrieval_channels
                    .push("vision_question_reanalysis".into());
            }
        }
    }
    let mut runtime = generation.lock().map_err(|_| {
        AppError::new(
            "GENERATION_RUNTIME_LOCK_FAILED",
            "生成运行时状态已损坏",
            true,
        )
    })?;
    let threads = interactive_inference_threads();
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
    let mut prompt = fanfan_core::generation_prompt(request, &extractive, &history);
    if !image_analysis_context.is_empty() {
        prompt.push_str(
            "\n\n以下是针对当前问题重新查看候选原图得到的辅助观察。它不能替代[S数字]原始引用；只有同时受到原始引用支持的事实才能写入答案：\n",
        );
        prompt.push_str(&image_analysis_context.join("\n"));
    }
    let answer_schema = fanfan_core::grounded_answer_json_schema();
    let mut generated = runtime.complete_json_cancellable(
        "你是翻翻的本地资料回答器。只能使用用户提供的证据；每个事实必须通过citation_ids关联证据，不得补充外部知识。",
        &prompt,
        768,
        &answer_schema,
        cancelled,
    )
    .inspect_err(|error| {
        if error.code == "OPERATION_CANCELLED" {
            runtime.stop();
        }
    })?;
    drop(runtime);
    trace_node(
        catalog,
        "ask",
        "generation",
        &correlation_id,
        session_id_ref,
        None,
        &json!({ "prompt": prompt }),
        &json!({ "raw": generated }),
        "ok",
        None,
    );
    phase("citation_validation", 0.88);
    let mut grounded = fanfan_core::apply_grounded_generation(&extractive, &generated);
    if grounded.is_none() {
        phase("citation_structure_repair", 0.9);
        let repair_prompt = format!(
            "下面的输出没有满足结构约束。请保持原有事实、措辞与S编号完全不变，只修正JSON结构和citation_ids的格式，使输出符合指定JSON Schema。\n\n原输出：\n{}",
            compact_for_prompt(&generated, 12_000)
        );
        let repaired = complete_json_with_model(
            generation,
            &generation_artifact,
            "你是结构化引用修复器。保持原有事实、措辞与S编号不变，只修正JSON结构和citation_ids格式。",
            &repair_prompt,
            640,
            &answer_schema,
            cancelled,
        );
        trace_node(
            catalog,
            "ask",
            "repair",
            &correlation_id,
            session_id_ref,
            None,
            &json!({ "broken": generated }),
            &json!({
                "repaired": repaired.as_ref().ok().map(|value| fanfan_core::apply_grounded_generation(&extractive, value).is_some()),
                "raw": repaired.as_ref().map(String::as_str),
            }),
            if repaired.is_ok() { "ok" } else { "error" },
            None,
        );
        if let Ok(repaired) = repaired {
            grounded = fanfan_core::apply_grounded_generation(&extractive, &repaired);
            if grounded.is_some() {
                generated = repaired;
            }
        }
    }
    let Some(mut grounded) = grounded else {
        // 引用核验失败降级：尽量把已生成内容展示给用户，标记未通过核验
        let fallback_text = fanfan_core::extract_unverified_text(&generated);
        if fallback_text.is_empty() {
            return Err(AppError::new(
                "RAG_CITATION_VALIDATION_FAILED",
                "本次生成结果未通过引用核验且没有可展示的内容，可以重试",
                true,
            ));
        }
        let degraded = fanfan_core::unverified_answer(
            &extractive,
            extractive.session_id,
            fallback_text,
            extractive.elapsed_ms,
        );
        trace_node(
            catalog,
            "ask",
            "completed",
            &correlation_id,
            session_id_ref,
            None,
            &json!({}),
            &json!({
                "answer_mode": degraded.answer_mode,
                "answer": degraded.answer,
                "claim_count": 0,
                "grounding_status": format!("{:?}", degraded.grounding_status),
                "insufficient_evidence": degraded.insufficient_evidence,
                "degradation_reason": degraded.degradation_reason,
            }),
            "ok",
            Some(degraded.elapsed_ms),
        );
        catalog.record_ask_exchange(request, &degraded)?;
        phase("completed", 1.0);
        return Ok(degraded);
    };
    let candidates = std::mem::take(&mut grounded.claims);
    let candidate_count = candidates.len().max(1);
    let mut rejected_claims = 0_usize;
    for (index, claim) in candidates.into_iter().enumerate() {
        if cancelled.load(Ordering::Acquire) {
            return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
        }
        let evidence = claim
            .citations
            .iter()
            .enumerate()
            .map(|(index, citation)| format!("[E{}] {}", index + 1, citation.quote))
            .collect::<Vec<_>>()
            .join("\n");
        let deterministically_supported = fanfan_core::claim_has_deterministic_support(
            &claim.text,
            claim
                .citations
                .iter()
                .map(|citation| citation.quote.as_str()),
        );
        let mut llm_verdict = None;
        let supported = if deterministically_supported {
            true
        } else {
            let verification = complete_with_model(
                generation,
                &generation_artifact,
                "你是严格的中文证据核验员。判断「事实句」是否完全由「原文证据」支持：事实句的每个要点都要能在证据中找到对应文字，证据里没有的内容不能算支持。只输出一个词：SUPPORTED 或 UNSUPPORTED，不要输出解释、标点或多余文字。",
                &format!(
                    "【示例一】\n事实句：公司的报销流程是先填单再审批\n原文证据：\n[E1] 报销需先填写报销单，经部门主管审批后方可发放\n输出：SUPPORTED\n\n【示例二】\n事实句：公司的报销上限是五千元\n原文证据：\n[E1] 员工报销需提供正规发票\n输出：UNSUPPORTED\n\n【正式任务】\n事实句：{}\n\n原文证据：\n{}",
                    claim.text, evidence
                ),
                32,
                cancelled,
            )?;
            llm_verdict = Some(verification.clone());
            claim_support_is_verified(&verification)
        };
        trace_node(
            catalog,
            "ask",
            "verification",
            &correlation_id,
            session_id_ref,
            Some(&index.to_string()),
            &json!({
                "claim_index": index,
                "claim_text": claim.text,
                "evidence_count": claim.citations.len(),
            }),
            &json!({
                "deterministic": deterministically_supported,
                "llm_verdict": llm_verdict,
                "supported": supported,
            }),
            "ok",
            None,
        );
        if !supported {
            rejected_claims = rejected_claims.saturating_add(1);
            continue;
        }
        let mut single_claim_result = grounded.clone();
        single_claim_result.claims = vec![claim.clone()];
        catalog.validate_answer_evidence(&single_claim_result)?;
        grounded.claims.push(claim);
        verified_claim(grounded.claims.last().expect("verified claim appended"));
        phase(
            "citation_validation",
            0.88 + 0.1 * ((index + 1) as f64 / candidate_count as f64),
        );
    }
    if grounded.claims.is_empty() {
        // 全部候选事实句未通过支持性校验：降级展示原始生成内容，标记未通过核验
        let fallback_text = fanfan_core::extract_unverified_text(&generated);
        if fallback_text.is_empty() {
            return Err(AppError::new(
                "RAG_CLAIM_UNSUPPORTED",
                "生成内容没有任何事实句通过原文支持性校验，且没有可展示的内容，回答已拒绝显示",
                true,
            ));
        }
        let degraded = fanfan_core::unverified_answer(
            &extractive,
            extractive.session_id,
            fallback_text,
            extractive.elapsed_ms,
        );
        trace_node(
            catalog,
            "ask",
            "completed",
            &correlation_id,
            session_id_ref,
            None,
            &json!({}),
            &json!({
                "answer_mode": degraded.answer_mode,
                "answer": degraded.answer,
                "claim_count": 0,
                "grounding_status": format!("{:?}", degraded.grounding_status),
                "insufficient_evidence": degraded.insufficient_evidence,
                "degradation_reason": degraded.degradation_reason,
            }),
            "ok",
            Some(degraded.elapsed_ms),
        );
        catalog.record_ask_exchange(request, &degraded)?;
        phase("completed", 1.0);
        return Ok(degraded);
    }
    // 润色后的回答：多条 claim 按自然段拼接，不带 [S#] 内联标记（引用通过
    // 前端文件标签表达，claims/citations 结构保留给前端做引文详情）。
    grounded.answer = grounded
        .claims
        .iter()
        .map(|claim| claim.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let verified_file_ids = grounded
        .claims
        .iter()
        .flat_map(|claim| claim.citations.iter().map(|citation| citation.file_id))
        .collect::<HashSet<_>>();
    grounded
        .source_files
        .retain(|source| verified_file_ids.contains(&source.file_id));
    grounded.used_file_ids = verified_file_ids.into_iter().collect();
    if rejected_claims > 0 {
        grounded.grounding_status = fanfan_core::GroundingStatus::Partial;
        grounded.answer_mode = "generated".into();
        grounded.degradation_reason = Some(format!(
            "有{rejected_claims}个候选事实句未通过原文支持性校验，已自动隐藏"
        ));
    }
    grounded.index_coverage = index_coverage;
    grounded.retrieval_channels = extractive.retrieval_channels;
    trace_node(
        catalog,
        "ask",
        "completed",
        &correlation_id,
        session_id_ref,
        None,
        &json!({}),
        &json!({
            "answer_mode": grounded.answer_mode,
            "answer": grounded.answer,
            "claim_count": grounded.claims.len(),
            "grounding_status": format!("{:?}", grounded.grounding_status),
            "insufficient_evidence": grounded.insufficient_evidence,
            "degradation_reason": grounded.degradation_reason,
            "used_file_ids": grounded.used_file_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        }),
        "ok",
        Some(grounded.elapsed_ms),
    );
    catalog.validate_answer_evidence(&grounded)?;
    catalog.record_ask_exchange(request, &grounded)?;
    phase("completed", 1.0);
    Ok(grounded)
}

/// 合并多个子查询的检索结果（一题多问改写拆分后的分别检索）：
/// claims 按 (text, file_id) 去重保留首个；insufficient_evidence 为所有
/// 子查询均无证据；elapsed_ms 求和；来源文件合并。results 至少含一项。
fn merge_extractive_results(mut results: Vec<AnswerResult>) -> AnswerResult {
    let mut merged = results.remove(0);
    for mut other in results {
        let mut seen = HashSet::new();
        for claim in &merged.claims {
            seen.insert((
                claim.text.clone(),
                claim.citations.first().map(|citation| citation.file_id),
            ));
        }
        for claim in other.claims.drain(..) {
            let key = (
                claim.text.clone(),
                claim.citations.first().map(|citation| citation.file_id),
            );
            if seen.insert(key) {
                merged.claims.push(claim);
            }
        }
        merged.insufficient_evidence = merged.insufficient_evidence && other.insufficient_evidence;
        merged.elapsed_ms = merged.elapsed_ms.saturating_add(other.elapsed_ms);
        merged.source_files.extend(other.source_files);
    }
    merged
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
    let threads = interactive_inference_threads();
    if runtime.active_model_path() != Some(artifact.local_path.as_str()) || !runtime.is_active() {
        runtime.activate(&artifact.local_path, 4096, threads)?;
    }
    runtime.complete_cancellable(system_prompt, prompt, max_tokens, cancelled)
}

/// 闲聊专用：允许更高的采样温度（0.6B 在 0.1 下只会复读模板开场白）。
/// 路由/改写/检索等稳定输出场景继续走 complete_with_model（0.1）。
fn complete_chat_with_model(
    generation: &Mutex<LocalGenerationRuntime>,
    artifact: &ModelArtifact,
    system_prompt: &str,
    prompt: &str,
    max_tokens: u32,
    temperature: f32,
    cancelled: &AtomicBool,
) -> Result<String, AppError> {
    let mut runtime = generation.lock().map_err(|_| {
        AppError::new(
            "GENERATION_RUNTIME_LOCK_FAILED",
            "生成运行时状态已损坏",
            true,
        )
    })?;
    let threads = interactive_inference_threads();
    if runtime.active_model_path() != Some(artifact.local_path.as_str()) || !runtime.is_active() {
        runtime.activate(&artifact.local_path, 4096, threads)?;
    }
    runtime.complete_chat_cancellable(system_prompt, prompt, max_tokens, temperature, cancelled)
}

fn complete_json_with_model(
    generation: &Mutex<LocalGenerationRuntime>,
    artifact: &ModelArtifact,
    system_prompt: &str,
    prompt: &str,
    max_tokens: u32,
    schema: &serde_json::Value,
    cancelled: &AtomicBool,
) -> Result<String, AppError> {
    let mut runtime = generation.lock().map_err(|_| {
        AppError::new(
            "GENERATION_RUNTIME_LOCK_FAILED",
            "生成运行时状态已损坏",
            true,
        )
    })?;
    let threads = interactive_inference_threads();
    if runtime.active_model_path() != Some(artifact.local_path.as_str()) || !runtime.is_active() {
        runtime.activate(&artifact.local_path, 4096, threads)?;
    }
    runtime.complete_json_cancellable(system_prompt, prompt, max_tokens, schema, cancelled)
}

fn interactive_inference_threads() -> u32 {
    (physical_core_count() / 2).clamp(1, 4)
}

fn background_inference_threads() -> u32 {
    interactive_inference_threads().min(2)
}

fn physical_core_count() -> u32 {
    *PHYSICAL_CORE_COUNT.get_or_init(detect_physical_core_count)
}

#[cfg(windows)]
fn detect_physical_core_count() -> u32 {
    let fallback = || {
        std::thread::available_parallelism()
            .map(|value| value.get() as u32)
            .unwrap_or(2)
    };
    let mut returned_length = 0_u32;
    let _ = unsafe {
        GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut returned_length)
    };
    if returned_length == 0 {
        return fallback();
    }
    let word_size = std::mem::size_of::<usize>();
    let mut storage = vec![0_usize; (returned_length as usize).div_ceil(word_size)];
    if unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(storage.as_mut_ptr().cast()),
            &mut returned_length,
        )
    }
    .is_err()
    {
        return fallback();
    }
    let mut offset = 0_usize;
    let mut cores = 0_u32;
    while offset < returned_length as usize {
        let information = unsafe {
            &*(storage.as_ptr().cast::<u8>().add(offset)
                as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX)
        };
        if information.Size == 0 {
            break;
        }
        if information.Relationship == RelationProcessorCore {
            cores += 1;
        }
        offset += information.Size as usize;
    }
    if cores == 0 { fallback() } else { cores }
}

#[cfg(not(windows))]
fn detect_physical_core_count() -> u32 {
    std::thread::available_parallelism()
        .map(|value| value.get() as u32)
        .unwrap_or(2)
}

fn claim_support_is_verified(value: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphabetic())
        .find(|token| !token.is_empty())
        .is_some_and(|token| token.eq_ignore_ascii_case("SUPPORTED"))
}

#[derive(Debug, Deserialize)]
pub struct OperationRequest {
    operation_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct AskSessionQueryRequest {
    cursor: Option<String>,
    #[serde(default = "default_ask_session_page_size")]
    page_size: u32,
}

fn default_ask_session_page_size() -> u32 {
    30
}

#[derive(Debug, Deserialize)]
pub struct AskMessageQueryRequest {
    session_id: Uuid,
    cursor: Option<String>,
    #[serde(default = "default_ask_message_page_size")]
    page_size: u32,
}

fn default_ask_message_page_size() -> u32 {
    100
}

#[derive(Debug, Deserialize)]
pub struct AskSessionRenameRequest {
    session_id: Uuid,
    title: String,
}

#[derive(Debug, Deserialize)]
pub struct AskSessionIdRequest {
    session_id: Uuid,
}

#[tauri::command(async)]
pub fn ask_session_query(
    request: AskSessionQueryRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<AskSessionPage, AppError> {
    catalog
        .get()?
        .list_ask_sessions(request.cursor.as_deref(), request.page_size)
}

#[tauri::command(async)]
pub fn ask_message_query(
    request: AskMessageQueryRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<AskMessagePage, AppError> {
    catalog.get()?.list_ask_messages(
        &request.session_id,
        request.cursor.as_deref(),
        request.page_size,
    )
}

#[tauri::command(async)]
pub fn ask_session_rename(
    request: AskSessionRenameRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    catalog
        .get()?
        .rename_ask_session(&request.session_id, &request.title)
}

#[tauri::command(async)]
pub fn ask_session_delete(
    request: AskSessionIdRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<(), AppError> {
    catalog.get()?.delete_ask_session(&request.session_id)
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
    worker: State<'_, WorkerServiceState>,
) -> Result<FilePreview, AppError> {
    let _foreground_guard = ForegroundActivityGuard::begin(&worker.foreground_activity);
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
    let scans = scans.into_iter().collect::<Vec<_>>();
    let requested_count = scans.len();
    let state = app.state::<ScanCoordinatorState>();
    if let Ok(mut queue) = state.queue.lock() {
        for scan in scans {
            if !queue.iter().any(|queued| queued.1 == scan.1) {
                queue.push_back(scan);
            }
        }
        crate::runtime_log::event(
            "info",
            "scanner",
            "scan.queue_updated",
            None,
            &json!({
                "requested_count": requested_count,
                "queued_count": queue.len(),
                "worker_already_running": state.running.load(Ordering::Acquire),
            }),
        );
    } else {
        crate::runtime_log::event(
            "error",
            "scanner",
            "scan.queue_failed",
            None,
            &json!({ "error_code": "SCAN_QUEUE_UNAVAILABLE", "retryable": true }),
        );
        let _ = app.emit(
            "catalog:watch_degraded",
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
    let started = Instant::now();
    crate::runtime_log::event(
        "info",
        "scanner",
        "scan.started",
        Some(&job_id.to_string()),
        &json!({ "job_id": job_id, "root_id": root_id }),
    );
    match catalog.execute_scan(root_id, job_id) {
        Ok(job) => {
            crate::runtime_log::event(
                if matches!(job.status, fanfan_core::JobStatus::Failed) {
                    "error"
                } else {
                    "info"
                },
                "scanner",
                "scan.completed",
                Some(&job_id.to_string()),
                &json!({
                    "job_id": job.job_id,
                    "root_id": root_id,
                    "status": job.status,
                    "stage": job.stage,
                    "progress": job.progress,
                    "processed_items": job.processed_items,
                    "total_items": job.total_items,
                    "error_code": job.error.as_ref().map(|error| error.code.as_str()),
                    "retryable": job.error.as_ref().map(|error| error.retryable),
                    "error_details": job.error.as_ref().and_then(|error| error.details.as_deref()),
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            let _ = app.emit("job:progress", &job);
            let _ = app.emit("catalog:changed", root_id.to_string());
            if job.processed_items > 0
                && matches!(
                    job.status,
                    fanfan_core::JobStatus::Succeeded
                        | fanfan_core::JobStatus::Partial
                        | fanfan_core::JobStatus::Failed
                )
            {
                spawn_parse_pending(app.clone(), Arc::clone(catalog));
            }
        }
        Err(error) => {
            crate::runtime_log::event(
                "error",
                "scanner",
                "scan.failed",
                Some(&job_id.to_string()),
                &json!({
                    "job_id": job_id,
                    "root_id": root_id,
                    "error_code": error.code,
                    "retryable": error.retryable,
                    "elapsed_ms": started.elapsed().as_millis() as u64,
                }),
            );
            let _ = app.emit("job:progress", &error);
            spawn_parse_pending(app.clone(), Arc::clone(catalog));
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
        let cycle_id = Uuid::now_v7().to_string();
        let cycle_started = Instant::now();
        let mut completed_files = 0_u64;
        let mut failed_files = 0_u64;
        let mut parsed_nodes = 0_u64;
        let mut extracted_images = 0_u64;
        crate::runtime_log::event(
            "info",
            "parser",
            "parse.cycle_started",
            Some(&cycle_id),
            &json!({}),
        );
        loop {
            if worker.foreground_activity.load(Ordering::Acquire) > 0 {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
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
                crate::runtime_log::event(
                    "warning",
                    "parser",
                    "parse.paused_for_resources",
                    Some(&cycle_id),
                    &json!({}),
                );
                break;
            }
            let pending = match catalog.list_pending_parse_files(batch_size) {
                Ok(files) => files,
                Err(error) => {
                    crate::runtime_log::event(
                        "error",
                        "parser",
                        "parse.pending_query_failed",
                        Some(&cycle_id),
                        &json!({ "error_code": error.code, "retryable": error.retryable }),
                    );
                    let _ = app.emit("index:failed", error);
                    break;
                }
            };
            if pending.is_empty() {
                break;
            }
            let batch_count = pending.len();
            for file in pending {
                while worker.foreground_activity.load(Ordering::Acquire) > 0 {
                    thread::sleep(Duration::from_millis(250));
                }
                let Some(revision_id) = file.current_revision_id else {
                    continue;
                };
                let parse_threads = background_inference_threads();
                let source_format = file.extension.to_ascii_lowercase();
                let uses_ocr = matches!(
                    source_format.as_str(),
                    "pdf" | "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp" | "webp"
                );
                let ocr_runtime = if uses_ocr {
                    match active_ocr_runtime(&app, parse_threads) {
                        Ok(runtime) => runtime,
                        Err(error) => {
                            crate::runtime_log::event(
                                "warning",
                                "ocr",
                                "ocr.runtime_resolution_failed",
                                Some(&cycle_id),
                                &json!({
                                    "error_code": error.code,
                                    "retryable": error.retryable,
                                    "format": source_format,
                                }),
                            );
                            None
                        }
                    }
                } else {
                    None
                };
                let runtime_manager = app.state::<RuntimeManagerState>();
                let mut runtime_request = RuntimeTaskRequest::interactive(
                    if ocr_runtime.is_some() {
                        RuntimeTaskKind::Ocr
                    } else {
                        RuntimeTaskKind::Parse
                    },
                    if ocr_runtime.is_some() {
                        RuntimeBackendKind::PaddleOcr
                    } else {
                        RuntimeBackendKind::Parser
                    },
                );
                runtime_request.cpu_threads = parse_threads;
                runtime_request.memory_bytes = if ocr_runtime.is_some() {
                    512 * 1024 * 1024
                } else {
                    128 * 1024 * 1024
                };
                runtime_request.timeout = Duration::from_secs(2);
                runtime_request.idempotency_key = Some(format!("parse:{revision_id}"));
                let runtime_lease = match runtime_manager.0.acquire(runtime_request) {
                    Ok(lease) => lease,
                    Err(error) => {
                        crate::runtime_log::event(
                            "info",
                            "runtime",
                            "runtime.background_deferred",
                            Some(&cycle_id),
                            &json!({"task_kind": "parse", "error_code": error.code}),
                        );
                        break;
                    }
                };
                if let Err(error) = catalog.mark_file_parsing(&file.file_id, &revision_id) {
                    runtime_lease.fail(error.code.clone());
                    let _ = app.emit("index:failed", error);
                    continue;
                }
                let request = ParseRequest {
                    job_id: Uuid::now_v7(),
                    file_id: file.file_id,
                    revision_id,
                    source_path: strip_long_path_prefix(&file.canonical_path),
                    format: file.extension.clone(),
                    ocr_policy: "auto".to_owned(),
                    language_hints: vec!["zh".to_owned()],
                    max_pages: None,
                    asset_cache_dir: image_asset_cache_dir(&app, &revision_id),
                    ocr_runtime,
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
                        ocr_attempts: vec![],
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
                let mut runtime_error_code = result.error.as_ref().map(|error| error.code.clone());
                match catalog.commit_parse_result(&file.file_id, &result) {
                    Ok(()) => {
                        if !result.ocr_attempts.is_empty() {
                            crate::runtime_log::event(
                                if result
                                    .ocr_attempts
                                    .iter()
                                    .any(|attempt| attempt.status == "failed")
                                {
                                    "warning"
                                } else {
                                    "info"
                                },
                                "ocr",
                                "ocr.attempt_chain_completed",
                                Some(&cycle_id),
                                &json!({
                                    "file_id": file.file_id,
                                    "revision_id": revision_id,
                                    "attempts": result.ocr_attempts.iter().map(|attempt| json!({
                                        "engine": &attempt.engine,
                                        "model_version": attempt.model_version.as_deref(),
                                        "status": &attempt.status,
                                        "page_no": attempt.page_no,
                                        "confidence": attempt.confidence,
                                        "fallback_reason": attempt.fallback_reason.as_deref(),
                                        "elapsed_ms": attempt.elapsed_ms,
                                        "error_code": attempt.error.as_ref().map(|error| error.code.as_str()),
                                    })).collect::<Vec<_>>(),
                                }),
                            );
                        }
                        if matches!(result.status, ParseOutcome::Failed) {
                            failed_files = failed_files.saturating_add(1);
                            crate::runtime_log::event(
                                "error",
                                "parser",
                                "parse.document_failed",
                                Some(&cycle_id),
                                &json!({
                                    "file_id": file.file_id,
                                    "revision_id": revision_id,
                                    "format": file.extension,
                                    "error_code": result.error.as_ref().map(|error| error.code.as_str()),
                                    "retryable": result.error.as_ref().map(|error| error.retryable),
                                    "elapsed_ms": result.metrics.elapsed_ms,
                                }),
                            );
                        } else {
                            completed_files = completed_files.saturating_add(1);
                        }
                        parsed_nodes = parsed_nodes.saturating_add(result.nodes.len() as u64);
                        extracted_images =
                            extracted_images.saturating_add(result.image_assets.len() as u64);
                        let _ = app.emit("index:changed", file.file_id.to_string());
                    }
                    Err(error) => {
                        runtime_error_code = Some(error.code.clone());
                        failed_files = failed_files.saturating_add(1);
                        crate::runtime_log::event(
                            "error",
                            "parser",
                            "parse.commit_failed",
                            Some(&cycle_id),
                            &json!({
                                "file_id": file.file_id,
                                "revision_id": revision_id,
                                "format": file.extension,
                                "error_code": error.code,
                                "retryable": error.retryable,
                            }),
                        );
                        let _ = app.emit("index:failed", error);
                    }
                }
                if let Some(error_code) = runtime_error_code {
                    runtime_lease.fail(error_code);
                } else {
                    runtime_lease.complete();
                }
            }
            crate::runtime_log::event(
                "info",
                "parser",
                "parse.batch_completed",
                Some(&cycle_id),
                &json!({
                    "batch_size": batch_count,
                    "completed_files_total": completed_files,
                    "failed_files_total": failed_files,
                    "parsed_nodes_total": parsed_nodes,
                    "extracted_images_total": extracted_images,
                }),
            );
            thread::yield_now();
        }
        crate::runtime_log::event(
            if failed_files > 0 { "warning" } else { "info" },
            "parser",
            "parse.cycle_completed",
            Some(&cycle_id),
            &json!({
                "completed_files": completed_files,
                "failed_files": failed_files,
                "parsed_nodes": parsed_nodes,
                "extracted_images": extracted_images,
                "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
            }),
        );
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

        if !background_storage_budget_allows(&app) {
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
                crate::runtime_log::event(
                    "error",
                    "vision",
                    "vision.model_lookup_failed",
                    None,
                    &json!({ "error_code": error.code, "retryable": error.retryable }),
                );
                let _ = app.emit("vision:failed", error);
                return;
            }
        };
        let projector = match models.vision_projector_path(&artifact) {
            Ok(path) => path,
            Err(error) => {
                crate::runtime_log::event(
                    "error",
                    "vision",
                    "vision.projector_lookup_failed",
                    None,
                    &json!({ "error_code": error.code, "retryable": error.retryable }),
                );
                let _ = app.emit("vision:failed", error);
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
        let cycle_id = Uuid::now_v7().to_string();
        let cycle_started = Instant::now();
        crate::runtime_log::event(
            "info",
            "vision",
            "vision.cycle_started",
            Some(&cycle_id),
            &json!({ "model_artifact_id": model_artifact_id }),
        );
        let projector_path = projector.to_string_lossy().into_owned();
        let threads = background_inference_threads();
        let mut committed = 0_u64;
        let mut failed = 0_u64;
        loop {
            if worker.foreground_activity.load(Ordering::Acquire) > 0 {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            let degradation = catalog
                .maintenance_snapshot()
                .map(|snapshot| snapshot.degradation_level)
                .unwrap_or_else(|_| "balanced".to_owned());
            if degradation == "core" {
                break;
            }
            let runtime_manager = app.state::<RuntimeManagerState>();
            let mut runtime_request = RuntimeTaskRequest::interactive(
                RuntimeTaskKind::ImageUnderstanding,
                RuntimeBackendKind::LlamaCpp,
            );
            runtime_request.cpu_threads = threads;
            runtime_request.timeout = Duration::from_secs(2);
            runtime_request.model_id = Some(model_artifact_id.clone());
            let runtime_lease = match runtime_manager.0.acquire(runtime_request) {
                Ok(lease) => lease,
                Err(error) => {
                    crate::runtime_log::event(
                        "info",
                        "runtime",
                        "runtime.background_deferred",
                        Some(&cycle_id),
                        &json!({"task_kind": "image_understanding", "error_code": error.code}),
                    );
                    break;
                }
            };
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
                    crate::runtime_log::event(
                        "error",
                        "vision",
                        "vision.pending_claim_failed",
                        Some(&cycle_id),
                        &json!({ "error_code": error.code, "retryable": error.retryable }),
                    );
                    let _ = app.emit("vision:failed", error);
                    break;
                }
            };
            let _ = app.emit(
                "vision:progress",
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
                    "你是翻翻的本地图片与图表理解器。只描述图中可验证内容，并严格输出指定JSON。",
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
                    runtime_lease.complete();
                    committed = committed.saturating_add(1);
                    let _ = app.emit(
                        "vision:completed",
                        json!({"asset_id": pending.asset_id, "revision_id": pending.revision_id}),
                    );
                    let _ = app.emit("index:changed", pending.file_id.to_string());
                    if committed.is_multiple_of(8) {
                        crate::runtime_log::event(
                            "info",
                            "vision",
                            "vision.batch_completed",
                            Some(&cycle_id),
                            &json!({ "completed_assets": committed, "failed_assets": failed }),
                        );
                        spawn_embed_pending(app.clone(), Arc::clone(&catalog));
                    }
                }
                Err(error) => {
                    runtime_lease.fail(error.code.clone());
                    failed = failed.saturating_add(1);
                    crate::runtime_log::event(
                        "error",
                        "vision",
                        "vision.asset_failed",
                        Some(&cycle_id),
                        &json!({
                            "asset_id": pending.asset_id,
                            "revision_id": pending.revision_id,
                            "attempt": pending.attempt_count,
                            "error_code": error.code,
                            "retryable": error.retryable,
                        }),
                    );
                    let _ = catalog.fail_image_understanding(&pending.asset_id, &error);
                    let _ = app.emit(
                        "vision:failed",
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
        crate::runtime_log::event(
            if failed > 0 { "warning" } else { "info" },
            "vision",
            "vision.cycle_completed",
            Some(&cycle_id),
            &json!({
                "completed_assets": committed,
                "failed_assets": failed,
                "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
            }),
        );
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
    if !background_storage_budget_allows(app) {
        return;
    }
    let models_state = app.state::<ModelServiceState>();
    let models = match models_state.get() {
        Ok(models) => models,
        Err(error) => {
            let _ = app.emit("embedding:failed", error);
            return;
        }
    };
    let pending = match models.pending_embedding_activation() {
        Ok(Some(pending)) if pending.status == "indexing" => Some(pending),
        Ok(_) => None,
        Err(error) => {
            let _ = app.emit("embedding:failed", error);
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
    let cycle_id = Uuid::now_v7().to_string();
    let cycle_started = Instant::now();
    crate::runtime_log::event(
        "info",
        "embedding",
        "embedding.cycle_started",
        Some(&cycle_id),
        &json!({
            "model_artifact_id": model_artifact_id,
            "is_index_migration": pending.is_some(),
        }),
    );
    let expected_dimension = pending
        .as_ref()
        .map(|pending| pending.dimension)
        .or(artifact.embedding_dimension);
    let mut committed_total = 0_u64;
    let mut completed_dimension = expected_dimension;
    let result = (|| -> Result<bool, AppError> {
        loop {
            if worker.foreground_activity.load(Ordering::Acquire) > 0 {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
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
            let chunks = catalog.list_pending_embedding_chunks(&model_artifact_id, 32)?;
            if chunks.is_empty() {
                break;
            }
            let runtime_manager = app.state::<RuntimeManagerState>();
            let mut runtime_request = RuntimeTaskRequest::interactive(
                RuntimeTaskKind::IncrementalIndex,
                RuntimeBackendKind::OnnxRuntime,
            );
            runtime_request.cpu_threads = background_inference_threads();
            runtime_request.timeout = Duration::from_secs(2);
            runtime_request.model_id = Some(model_artifact_id.clone());
            let runtime_lease = match runtime_manager.0.acquire(runtime_request) {
                Ok(lease) => lease,
                Err(error) => {
                    crate::runtime_log::event(
                        "info",
                        "runtime",
                        "runtime.background_deferred",
                        Some(&cycle_id),
                        &json!({"task_kind": "incremental_index", "error_code": error.code}),
                    );
                    return Ok(false);
                }
            };
            let response = match worker.client.encode_embeddings(&EmbeddingRequest {
                model_path: artifact.local_path.clone(),
                tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                texts: chunks.iter().map(|chunk| chunk.text.clone()).collect(),
                max_length: artifact.max_length.unwrap_or(512),
                threads: 4,
            }) {
                Ok(response) => response,
                Err(error) => {
                    runtime_lease.fail(error.code.clone());
                    return Err(error);
                }
            };
            if response.dimension == 0
                || expected_dimension.is_some_and(|dimension| dimension != response.dimension)
                || response.vectors.len() != chunks.len()
                || response
                    .vectors
                    .iter()
                    .any(|vector| vector.len() != response.dimension as usize)
            {
                let error = AppError::new(
                    "EMBEDDING_OUTPUT_INVALID",
                    "向量数量或维度与模型自检结果不一致",
                    false,
                );
                runtime_lease.fail(error.code.clone());
                return Err(error);
            }
            let inputs = chunks
                .iter()
                .zip(response.vectors)
                .map(|(chunk, vector)| ChunkEmbeddingInput {
                    chunk_id: chunk.chunk_id,
                    vector,
                })
                .collect::<Vec<_>>();
            let committed = match catalog.commit_chunk_embeddings(
                &model_artifact_id,
                response.dimension,
                &inputs,
            ) {
                Ok(committed) => committed,
                Err(error) => {
                    runtime_lease.fail(error.code.clone());
                    return Err(error);
                }
            };
            runtime_lease.complete();
            if committed == 0 {
                break;
            }
            committed_total = committed_total.saturating_add(committed);
            completed_dimension = Some(response.dimension);
            let _ = app.emit("embedding:changed", committed);
            if committed_total == committed || committed_total.is_multiple_of(1_024) {
                crate::runtime_log::event(
                    "info",
                    "embedding",
                    "embedding.progress_checkpoint",
                    Some(&cycle_id),
                    &json!({
                        "committed_chunks": committed_total,
                        "dimension": response.dimension,
                    }),
                );
            }
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
        // 除新增分块外，active 代际覆盖不足（如上次构建被中断后残留的过期索引）也必须重建，
        // 否则过期索引会一直保持 active，语义检索/RAG 持续命中陈旧子集。
        let needs_rebuild = searchable_chunks > 0
            && (committed_total > 0
                || existing_generation.as_ref().is_none_or(|generation| {
                    generation.dimension != dimension
                        || generation.item_count < searchable_chunks
                }));
        if needs_rebuild {
            crate::runtime_log::event(
                "info",
                "embedding",
                "vector_index.build_started",
                Some(&cycle_id),
                &json!({
                    "model_artifact_id": model_artifact_id,
                    "dimension": dimension,
                    "searchable_chunks": searchable_chunks,
                }),
            );
            let _ = app.emit("embedding:index_phase", "building");
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
            let _ = app.emit("embedding:index_phase", "active");
            let _ = app.emit("embedding:index_changed", &generation);
            crate::runtime_log::event(
                "info",
                "embedding",
                "vector_index.activated",
                Some(&cycle_id),
                &json!({
                    "generation_id": generation.generation_id,
                    "dimension": generation.dimension,
                    "item_count": generation.item_count,
                    "coverage": generation.coverage,
                }),
            );
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
                        let generation = app.state::<GenerationServiceState>();
                        let runtime_state = inference_runtime_state(&generation).ok();
                        if let Ok(state) =
                            model_state_from_manager(&models, Some(catalog), runtime_state)
                        {
                            let _ = app.emit("model:state", state);
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
            crate::runtime_log::event(
                "info",
                "embedding",
                "embedding.cycle_completed",
                Some(&cycle_id),
                &json!({
                    "committed_chunks": committed_total,
                    "dimension": completed_dimension,
                    "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
                }),
            );
        }
        Ok(false) => {
            crate::runtime_log::event(
                "warning",
                "embedding",
                "embedding.cycle_deferred",
                Some(&cycle_id),
                &json!({
                    "committed_chunks": committed_total,
                    "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
                }),
            );
        }
        Err(error) => {
            crate::runtime_log::event(
                "error",
                "embedding",
                "embedding.cycle_failed",
                Some(&cycle_id),
                &json!({
                    "committed_chunks": committed_total,
                    "error_code": error.code,
                    "retryable": error.retryable,
                    "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
                }),
            );
            let _ = app.emit("embedding:index_phase", "fallback");
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
    crate::runtime_log::event(
        "error",
        "embedding",
        "embedding.activation_failed",
        pending
            .and_then(|item| item.download_job_id)
            .as_ref()
            .map(|job_id| job_id.to_string())
            .as_deref(),
        &json!({
            "artifact_id": pending.map(|item| item.artifact_id),
            "download_job_id": pending.and_then(|item| item.download_job_id),
            "error_code": error.code,
            "retryable": error.retryable,
        }),
    );
    if let Some(pending) = pending {
        let _ = models.fail_embedding_activation(&pending.artifact_id, error);
        if let Some(job_id) = pending.download_job_id
            && let Ok(mut job) = models.download_job(&job_id)
        {
            job.status = "completed".into();
            job.phase = "completed".into();
            job.bytes_per_second = 0;
            job.eta_seconds = Some(0);
            job.error = None;
            job.activation_status = Some("failed".into());
            job.activation_error = Some(error.clone());
            if let Ok(job) = models.update_download_job(&job) {
                emit_download_state(app, &job);
            }
        }
    }
    let _ = app.emit("embedding:failed", error.clone());
    let generation = app.state::<GenerationServiceState>();
    let runtime_state = inference_runtime_state(&generation).ok();
    if let Ok(state) = model_state_from_manager(models, Some(catalog), runtime_state) {
        let _ = app.emit("model:state", state);
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
        job.activation_status = Some("active".into());
        job.activation_error = None;
        for file in &mut job.files {
            file.status = "completed".into();
            file.downloaded_bytes = file.total_bytes;
        }
        if let Ok(job) = models.update_download_job(&job) {
            emit_download_state(app, &job);
            let _ = app.emit("model:download_completed", &job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fanfan_core::{
        AnswerClaim, AnswerSourceFile, EvidenceRef, GroundingStatus, SourceLocator, SupportStatus,
    };

    #[test]
    fn claim_support_requires_an_exact_positive_verdict() {
        assert!(claim_support_is_verified("SUPPORTED"));
        assert!(claim_support_is_verified("SUPPORTED\n"));
        assert!(!claim_support_is_verified("UNSUPPORTED"));
        assert!(!claim_support_is_verified("NOT SUPPORTED"));
        assert!(!claim_support_is_verified("The claim is SUPPORTED"));
    }

    #[cfg(windows)]
    #[test]
    fn environment_detection_reads_real_memory_and_disk() {
        let check = detect_environment(Path::new("."), None, None);

        assert!(check.memory_total_gb.is_some_and(|value| value > 0));
        assert!(check.disk_available_gb.is_some());
        assert!(matches!(check.status, "ready" | "degraded"));
        assert!(check.recommended_edition.is_some());
    }

    #[test]
    fn gpu_details_parses_llama_device_lines() {
        // CUDA 设备：剥离 backend 前缀与显存后缀，MiB 折算为 GB
        let cuda = vec!["CUDA0: NVIDIA GeForce RTX 3060 Laptop GPU: 6144 MiB".to_owned()];
        let (name, memory) = gpu_details_from_devices(Some(&cuda)).expect("cuda device found");
        assert_eq!(name.as_deref(), Some("NVIDIA GeForce RTX 3060 Laptop GPU"));
        assert_eq!(memory, Some(6));

        // Vulkan：GB 后缀与多位小数
        let vulkan =
            vec!["Vulkan0: Intel(R) Arc(TM) A770: 16.0 GB".to_owned()];
        let (name, memory) = gpu_details_from_devices(Some(&vulkan)).expect("vulkan device found");
        assert_eq!(name.as_deref(), Some("Intel(R) Arc(TM) A770"));
        assert_eq!(memory, Some(16));

        // CPU 行不是 GPU 设备：应整体跳过
        let cpu_only = vec!["CPU: 32768 MiB".to_owned()];
        assert!(gpu_details_from_devices(Some(&cpu_only)).is_none());

        // 无显存字段（如早期 llama.cpp 格式）：名称保留、显存为空
        let no_vram = vec!["CUDA0: NVIDIA GeForce RTX 4090".to_owned()];
        let (name, memory) = gpu_details_from_devices(Some(&no_vram)).expect("device found");
        assert_eq!(name.as_deref(), Some("NVIDIA GeForce RTX 4090"));
        assert_eq!(memory, None);

        // 空设备列表
        assert!(gpu_details_from_devices(Some(&[])).is_none());
        assert!(gpu_details_from_devices(None).is_none());
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
            runtime_backend: Some("cpu".into()),
            runtime_devices: Vec::new(),
            gpu_runtime_available: false,
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
    fn diagnostic_identifiers_reject_injection_and_oversized_values() {
        assert!(validate_diagnostic_identifier("component", "frontend.bridge", 80).is_ok());
        assert_eq!(
            validate_diagnostic_identifier("event_name", "action\nforged", 120)
                .expect_err("newlines must not enter the JSONL envelope")
                .code,
            "DIAGNOSTIC_IDENTIFIER_INVALID"
        );
        assert_eq!(
            validate_diagnostic_identifier("component", &"a".repeat(81), 80)
                .expect_err("oversized component must fail")
                .code,
            "DIAGNOSTIC_IDENTIFIER_INVALID"
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
                context_before: None,
                context_after: None,
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

    #[test]
    fn rerank_truncates_to_top_evidence_before_generation() {
        let file_id = |n: u32| Uuid::now_v7();
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
                context_before: None,
                context_after: None,
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
            claims: vec![
                claim(file_id(1), "第一条"),
                claim(file_id(2), "第二条"),
                claim(file_id(3), "第三条"),
                claim(file_id(4), "第四条"),
                claim(file_id(5), "第五条"),
            ],
            source_files: vec![],
            used_file_ids: vec![],
            elapsed_ms: 1,
            answer_mode: "extractive".into(),
            retrieval_channels: vec!["fts".into()],
            index_coverage: 1.0,
            degradation_reason: None,
        };

        apply_rerank_scores(&mut result, &[0.1, 0.5, 0.9, 0.3, 0.7]).expect("apply rerank scores");
        // 重排后按分数降序：0.9/0.7/0.5/0.3/0.1
        assert_eq!(result.claims[0].text, "第三条");
        assert_eq!(result.claims[1].text, "第五条");
        // 只保留相关性最高的前 3 条给生成模型
        result.claims.truncate(RERANK_TOP_EVIDENCE);
        assert_eq!(result.claims.len(), RERANK_TOP_EVIDENCE);
        assert_eq!(result.claims[0].text, "第三条");
        assert_eq!(result.claims[1].text, "第五条");
        assert_eq!(result.claims[2].text, "第二条");
        assert_eq!(result.claims[0].citations[0].retrieval_score, 0.9);
        assert_eq!(result.claims[2].citations[0].retrieval_score, 0.5);
    }

    #[test]
    fn answer_export_contains_verified_evidence_without_source_absolute_path() {
        let file_id = Uuid::now_v7();
        let answer = AnswerResult {
            session_id: Uuid::now_v7(),
            message_id: Uuid::now_v7(),
            answer: "项目采用混合召回。[S1]".into(),
            grounding_status: GroundingStatus::Grounded,
            insufficient_evidence: false,
            claims: vec![AnswerClaim {
                claim_id: Uuid::now_v7(),
                text: "项目采用混合召回。[S1]".into(),
                support_status: SupportStatus::Supported,
                citations: vec![EvidenceRef {
                    evidence_id: Uuid::now_v7(),
                    file_id,
                    revision_id: Uuid::now_v7(),
                    node_id: Uuid::now_v7(),
                    chunk_id: Uuid::now_v7(),
                    image_asset_id: None,
                    quote: "采用混合召回".into(),
                    context_before: None,
                    context_after: None,
                    locator: SourceLocator {
                        paragraph_no: Some(3),
                        ..Default::default()
                    },
                    retrieval_score: 1.0,
                }],
            }],
            source_files: vec![AnswerSourceFile {
                file_id,
                display_name: "项目说明.md".into(),
                canonical_path: "E:\\敏感目录\\项目说明.md".into(),
            }],
            used_file_ids: vec![file_id],
            elapsed_ms: 10,
            answer_mode: "generated".into(),
            retrieval_channels: vec!["fts".into()],
            index_coverage: 1.0,
            degradation_reason: None,
        };

        let markdown = render_answer_export(&answer, "md").expect("render export");
        assert!(markdown.contains("项目说明.md"));
        assert!(markdown.contains("采用混合召回"));
        assert!(!markdown.contains("敏感目录"));
    }
}
