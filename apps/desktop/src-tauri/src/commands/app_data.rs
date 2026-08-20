use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use chrono::Utc;
use fanfan_core::ask::builtin_knowledge::lookup_builtin_knowledge;
use fanfan_core::ask::query_normalize::normalize_query_variants;
use fanfan_core::ask::query_plan::{QueryIntent, QueryOperation, ResolutionStatus};
use fanfan_core::ask::source_router::{SourceRouting, personal_reference_hit};
use fanfan_core::profile_builder::{TYPE_PROTOTYPE_TEXTS, TypePrototype, classify_document_type};
use fanfan_core::{
    AddRootRequest, AiRuntimeSnapshot, AnswerClaim, AnswerMode, AnswerResult, AnswerSourceFile,
    AnswerabilityInput, AnswerabilityStatus, AppError, AskEvaluationRunReport,
    AskEvaluationRunRequest, AskMessage, AskMessagePage, AskRequest, AskSessionContext,
    AskSessionPage, AskTrace, AskTraceExport, AskTraceExportRequest, AskTraceStage, AskTraceTiming,
    COMPARE_FALLBACK_ITEMS, COMPARE_MATERIAL_CHARS, COMPARE_MATERIAL_ITEMS, CandidateRoot,
    CatalogService, ChunkEmbeddingInput, ClarificationOption, ClarificationPayload,
    CollectionModelReview, CollectionRecord, CollectionRule, CollectionSuggestion,
    CollectionSuggestionPage, CollectionSuggestionQuery, CollectionSuggestionRefreshResult,
    CollectionSuggestionUpdateRequest, CompareResults, CreateCollectionRequest,
    DOCUMENT_RECALL_TOP_N, DOCUMENT_RECALL_VECTOR_CANDIDATES, DegradationLevel, DocumentCandidate,
    DocumentOverview, DocumentProfile, DocumentProfileInspect, DocumentProfileRebuildRequest,
    DocumentType, DownloadFile, DownloadedModelMetadata, EXTRACT_MATCH_MIN_LEN, EXTRACT_MAX_ITEMS,
    EmbeddingRequest, EvidenceRef, ExclusionRule, ExclusionRuleInput, ExportResult, FilePage,
    FilePreview, FileQuery, FileRecord, GateEvidence, GenerationActivation, GroundingStatus,
    ImageOcrResult, ImageOcrRoutingRequest, ImageUnderstandingResult, ImportCandidate, InboxItem,
    InboxPage, InboxQuery, InboxUpdateRequest, IncrementalWatchManager, IndexActivityStats,
    JobRecord, LOCAL_STRICT_SYSTEM_PROMPT, LocalGenerationRuntime, LogPage, LogQuery,
    MAX_CANDIDATE_SCOPE, MAX_SECTION_CHARS, MAX_SECTIONS, MaintenanceSnapshot, MemoryClearRequest,
    MemoryHint, MemoryInspectorView, MemoryKind, MemoryRelationStatusRequest, MemorySource,
    MemoryStatus, MemoryTargetRegistry, MemoryTargetType, MemoryWriteInput, MemoryWriterContext,
    ModelArtifact, ModelCatalogEntry, ModelDownloadFileProgress, ModelDownloadJob,
    ModelDownloadRemoval, ModelEdition, ModelFormat, ModelImportSelection, ModelManager,
    ModelPreset, ModelRole, ModelSource, ModelStoreStatus, NoEvidenceReason, NodeTracePage,
    NodeTraceQuery, NodeTraceRecord, OcrRuntimeConfig, OperationTraceInput, ParseMetrics,
    ParseOutcome, ParseRequest, ParseResult, PendingEmbeddingActivation, ProfileRefreshResult,
    QueryPlan, RagReadiness, RelationGroupPage, RelationGroupQuery, RelationPage, RelationQuery,
    RelationRefreshResult, RerankRequest, ResolverInput, RootRecord, RuntimeBackendKind,
    RuntimeCapability, RuntimeInstanceState, RuntimeManager, RuntimeResourceBudget,
    RuntimeTaskKind, RuntimeTaskRequest, ScopeFilter, SearchMode, SearchRequest, SearchSession,
    SectionChunk, SectionSummary, SemanticQuery, SourceIntent, SpeechRecognitionRequest,
    SpeechRecognitionResult, StructureEntry, SupportStatus, TraceFeatureType, TraceNodeInput,
    TraceNodeMeta, TriageStatus, WorkerClient, WorkerRole, answer_shape_directive,
    build_document_sections, chat_prompt, claim_subject_mismatch, compare_prompt, compare_schema,
    digests_json, document_overview_prompt, document_summary_prompt, evaluate_answerability,
    existence_requires_project_context, extract_item_is_entity_like, extract_prompt,
    extract_schema, find_external_knowledge_marker, local_no_evidence_answer,
    longest_common_substr_len, match_alias_hints, match_relation_hints, memory_writer_prompt,
    memory_writer_schema, merge_tail_sections, overview_schema, parse_compare_results,
    parse_extract_results, parse_overview, parse_query_plan, parse_rewritten_queries,
    parse_section_summaries, parse_source_routing, parse_writer_output,
    preselect_document_profiles, prewrite_validate, query_parser_prompt, query_parser_schema,
    query_rewrite_prompt, rank_document_candidates, resolve_ambiguous, resolve_documents,
    resolve_proposal_targets, section_summary_schema, source_router_prompt, source_routing_schema,
    strip_long_path_prefix,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

use crate::commands::memory_view::MemorySettingsServiceState;

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
    pub image_ocr_running: AtomicBool,
    pub vision_running: AtomicBool,
    pub foreground_activity: AtomicU32,
    pub search_embedding_cache: Mutex<SearchEmbeddingCache>,
}

/// 搜索查询 embedding 编码缓存：同一关键词在 TTL 内重复搜索时复用查询向量，
/// 避免每次搜索都跨进程调用 onnx worker 重新编码（冷启动/会话重建可达秒级）。
/// 只缓存查询向量（每个查询一条），不缓存任何资料内容，无隐私风险。
pub struct SearchEmbeddingCache {
    entries: HashMap<SearchEmbeddingKey, SearchEmbeddingValue>,
}

/// 缓存键：模型 + 规范化后的查询文本。
#[derive(Clone, PartialEq, Eq, Hash)]
struct SearchEmbeddingKey {
    model_artifact_id: String,
    query: String,
}

struct SearchEmbeddingValue {
    vector: Vec<f32>,
    expires_at: Instant,
}

/// 查询向量缓存有效期：与 embedding worker 会话缓存保留期（300s）对齐，
/// 过短导致冷启动反复出现，过长则占用少量内存。
const SEARCH_EMBEDDING_CACHE_TTL: Duration = Duration::from_secs(300);
/// 查询向量缓存条数上限，超限先清过期项、再兜底清空（搜索查询量远达不到）。
const SEARCH_EMBEDDING_CACHE_MAX: usize = 64;

impl SearchEmbeddingCache {
    pub fn new() -> Self {
        SearchEmbeddingCache {
            entries: HashMap::new(),
        }
    }

    /// 命中且未过期返回查询向量；过期项在此惰性清理。
    fn get(&mut self, key: &SearchEmbeddingKey) -> Option<Vec<f32>> {
        let now = Instant::now();
        let expired = self
            .entries
            .iter()
            .filter_map(|(k, v)| (k == key && v.expires_at <= now).then_some(k.clone()))
            .collect::<Vec<_>>();
        for stale in expired {
            self.entries.remove(&stale);
        }
        self.entries.get(key).map(|value| value.vector.clone())
    }

    /// 写入查询向量缓存；超容量时清理过期项，仍超则清空兜底。
    fn put(&mut self, key: SearchEmbeddingKey, vector: Vec<f32>) {
        if self.entries.len() >= SEARCH_EMBEDDING_CACHE_MAX {
            let now = Instant::now();
            self.entries.retain(|_, value| value.expires_at > now);
            if self.entries.len() >= SEARCH_EMBEDDING_CACHE_MAX {
                self.entries.clear();
            }
        }
        self.entries.insert(
            key,
            SearchEmbeddingValue {
                vector,
                expires_at: Instant::now() + SEARCH_EMBEDDING_CACHE_TTL,
            },
        );
    }
}

#[derive(Clone)]
pub struct SpeechWorkerState(pub WorkerClient);

/// ONNX（embedding/rerank）与 OCR 独立 sidecar 的注册表。
/// parse 角色由 WorkerServiceState.client 承载；speech 由 SpeechWorkerState 承载。
pub struct SidecarClients {
    pub onnx: WorkerClient,
    pub ocr: WorkerClient,
}

pub struct SidecarRegistryState(pub Arc<SidecarClients>);

#[derive(Debug, Clone, Serialize)]
pub struct SpeechRecognitionSession {
    session_id: Uuid,
    status: &'static str,
    result: SpeechRecognitionResult,
    completed_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpeechRecognitionInput {
    samples: Vec<f32>,
    sample_rate: u32,
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

/// 带时限的生成运行时锁。VLM/LLM 推理持锁期间可达数十秒（模型加载 + 推理），
/// 若所有获取方都无限期等锁，app_status_get 等查询会排队几十秒、close 流程
/// 会永久卡死（实测 close_requested 后进程 10 分钟不退出）。获取方必须在
/// 时限内拿到锁，否则放弃本次操作留给后续重试。Poisoned 与 `if let Ok` 语义
/// 一致：只绕过错锁保护，锁本身仍可用。
pub(crate) fn try_lock_generation_until<T>(
    mutex: &Mutex<T>,
    timeout: Duration,
) -> Option<std::sync::MutexGuard<'_, T>> {
    let deadline = Instant::now() + timeout;
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                return Some(poisoned.into_inner());
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

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
pub(crate) fn gpu_details_from_devices(
    devices: Option<&[String]>,
) -> Option<(Option<String>, Option<u64>)> {
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
    let name = name
        .split_once(':')
        .map(|(_, rest)| rest.trim())
        .unwrap_or(name);
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
        value
            .get("gpu_name")
            .and_then(Value::as_str)
            .map(str::to_owned),
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
    // 锁带 500ms 时限：推理持锁期间状态轮询直接回退缓存，不排队（曾因此被拖 43s）。
    let runtime_capability = try_lock_generation_until(&generation.0, Duration::from_millis(500))
        .and_then(|runtime| runtime.current_capability().cloned());
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

/// 读取用户当前选定的官方模型预设 id（未选择时返回 `None`）。
#[tauri::command(async)]
pub fn model_preset_selected_get(
    catalog: State<'_, CatalogServiceState>,
) -> Result<Option<String>, AppError> {
    catalog.get()?.selected_preset_id()
}

#[derive(Debug, Deserialize)]
pub struct ModelPresetSelectRequest {
    preset_id: String,
}

/// 只读地评估「选中某档位后的就绪 / 缺失清单」：不持久化 preset_id、不切换运行时、
/// 不写库。供前端在选择档位前先弹「下载缺失模型」确认框，用户确认后才真正切换。
#[tauri::command(async)]
pub fn model_preset_plan(
    request: ModelPresetSelectRequest,
    models: State<'_, ModelServiceState>,
) -> Result<fanfan_core::PresetPlanReport, AppError> {
    models.get()?.plan_preset(&request.preset_id)
}

/// 选中官方档位：持久化 preset_id，并把各角色 active 对齐到「已就绪且 catalog_id
/// 与预设一致」的本地 artifact；缺的记入返回报告的 `missing`（由前端触发下载）。
#[tauri::command(async)]
pub fn model_preset_select(
    request: ModelPresetSelectRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
) -> Result<fanfan_core::PresetPlanReport, AppError> {
    let catalog = catalog.get()?;
    catalog.set_selected_preset_id(&request.preset_id)?;
    let report = models.get()?.apply_runtime_plan(&request.preset_id)?;
    let _ = app.emit(
        "model:preset-selected",
        &json!({ "preset_id": request.preset_id, "ready_count": report.ready.len(), "missing_count": report.missing.len() }),
    );
    Ok(report)
}

/// 基于本机硬件档案返回官方推荐档位（内存/显存决定，推荐不等于强制）。
#[tauri::command(async)]
pub fn model_preset_recommendation(
    environment: State<'_, EnvironmentServiceState>,
) -> Result<String, AppError> {
    let cached = environment
        .latest
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let check = match cached {
        Some(check) => check,
        None => detect_environment(
            &environment.data_directory,
            None,
            read_environment_cache(&environment.data_directory),
        ),
    };
    Ok(fanfan_core::recommended_preset_id(check.memory_total_gb, check.gpu_memory_gb).to_owned())
}

/// 清点当前索引陈旧状态：比较「当前激活 Embedding artifact」与「active 向量索引代
/// 实际采用的模型」是否一致，供前端提示重建索引。
#[derive(Debug, Serialize, Default)]
pub struct IndexStaleStatus {
    stale: bool,
    active_embedding_artifact_id: Option<String>,
    index_embedding_artifact_id: Option<String>,
}

#[tauri::command(async)]
pub fn index_stale_check(
    models: State<'_, ModelServiceState>,
    catalog: State<'_, CatalogServiceState>,
) -> Result<IndexStaleStatus, AppError> {
    let active_id = models
        .get()?
        .active_artifact(ModelRole::Embedding)?
        .map(|artifact| artifact.artifact_id.to_string());
    let index_id = catalog.get()?.active_index_model_artifact_id()?;
    let stale = match (&active_id, &index_id) {
        (Some(active), Some(index)) => active != index,
        (Some(_), None) => true,
        _ => false,
    };
    Ok(IndexStaleStatus {
        stale,
        active_embedding_artifact_id: active_id,
        index_embedding_artifact_id: index_id,
    })
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
    sidecars: State<'_, SidecarRegistryState>,
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
        Arc::clone(&sidecars.0),
        Arc::clone(&generation.0),
        &request.edition_id,
        &request.source,
    )
}

#[allow(clippy::too_many_arguments)]
fn begin_model_download(
    app: AppHandle,
    catalog: Arc<CatalogService>,
    manager: Arc<ModelManager>,
    downloads: ModelDownloadCoordinatorState,
    sidecars: Arc<SidecarClients>,
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
        sidecars,
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
    let sidecars = Arc::clone(&app.state::<SidecarRegistryState>().0);
    let generation = Arc::clone(&app.state::<GenerationServiceState>().0);
    begin_model_download(
        app.clone(),
        catalog,
        manager,
        downloads,
        sidecars,
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
    environment: State<'_, EnvironmentServiceState>,
    models: State<'_, ModelServiceState>,
) -> Result<ModelStoreStatus, AppError> {
    let mut status = models.get()?.store_status()?;
    let config =
        crate::read_model_store_location_config(&environment.config_directory).unwrap_or_default();
    status.pending_target_directory = config
        .pending
        .as_ref()
        .map(|pending| pending.target_directory.clone());
    status.restart_required = config.pending.is_some();
    status.last_error = config.last_error;
    status.previous_model_store = config.previous_model_store;
    if config.pending.is_some() {
        status.migration_state = "migrating".into();
    }
    Ok(status)
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
    sidecars: State<'_, SidecarRegistryState>,
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
        Arc::clone(&sidecars.0),
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
    sidecars: State<'_, SidecarRegistryState>,
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
        Arc::clone(&sidecars.0),
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
    sidecars: State<'_, SidecarRegistryState>,
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
        Arc::clone(&sidecars.0),
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
    sidecars: Arc<SidecarClients>,
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
        sidecars,
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
    sidecars: Arc<SidecarClients>,
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
            &sidecars,
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
    sidecars: &Arc<SidecarClients>,
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
            // 回填 catalog_id，让 apply_runtime_plan 能匹配到已安装的 artifact，
            // 避免切换 Preset 时已下载模型被误判为 missing 而重复下载。
            let catalog_id = fanfan_core::built_in_model_catalog()
                .iter()
                .find(|entry| entry.install_edition_id.as_deref() == Some(job.edition_id.as_str()))
                .map(|entry| entry.catalog_id.clone());
            if let Some(cat_id) = catalog_id {
                let _ = models.bind_artifact_catalog_id(&installed_artifact.artifact_id, &cat_id);
            }
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
            let speech_worker = app.state::<SpeechWorkerState>().0.clone();
            let embedding_indexing = self_test_and_activate_downloaded_roles(
                app,
                catalog,
                models,
                &sidecars.onnx,
                &sidecars.ocr,
                &speech_worker,
                generation,
                &installed,
                job.job_id,
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
            job.status = "completed".into();
            job.phase = "completed".into();
            persist_download_job(app, models, &mut job)?;
            let _ = app.emit("model:download_completed", &job);
            // 任务完成并自检通过后从下载列表移除，避免列表堆积。
            let _ = models.remove_download_job(&job.job_id);
            // embedding 模型下载完成后立即转入后台进程建立语义索引：
            // 索引构建由独立后台线程（spawn_embed_pending）执行，进度与结果
            // 通过 embedding:index_phase / embedding:failed 事件单独上报，
            // 不再作为下载任务阶段展示在下载界面。
            if embedding_indexing {
                spawn_embed_pending(app.clone(), Arc::clone(catalog));
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
        let embedding_test = sidecars.onnx.encode_embeddings(&EmbeddingRequest {
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
        // embedding 模型下载完成后立即转入后台进程执行：下载任务至此完成并从
        // 列表移除；语义索引（含首次激活切换与增量嵌入）由后台线程
        // spawn_embed_pending 自动构建，进度与结果经 embedding:index_phase /
        // embedding:failed 事件单独上报，不在下载界面展示索引阶段。
        spawn_embed_pending(app.clone(), Arc::clone(catalog));
        let _ = app.emit("model:download_completed", &job);
        // 任务完成并自检通过后从下载列表移除，避免列表堆积。
        let _ = models.remove_download_job(&job.job_id);
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

/// 自检输出的可见文本（Phase 4.3）：剥离 thinking 模型的 `<think>…</think>`
/// 思维链段。思维链未闭合（token 上限截断）时整段丢弃——此时若闭合段外
/// 已有可见回复仍判定通过；无 `<think>` 标记的普通模型原样返回。
fn self_test_visible_text(raw: &str) -> String {
    if !raw.contains("<think>") {
        return raw.trim().to_owned();
    }
    let mut visible = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find("<think>") {
        visible.push_str(&rest[..start]);
        let after_start = &rest[start + "<think>".len()..];
        match after_start.find("</think>") {
            Some(end) => {
                rest = &after_start[end + "</think>".len()..];
            }
            None => {
                // 未闭合的思维链（截断）：丢弃到结尾
                rest = "";
            }
        }
    }
    visible.push_str(rest);
    visible.trim().to_owned()
}

/// 模型激活后的 GPU 状态日志（Phase 4.3 第四部分）：device / backend /
/// gpu_layers / 模型文件一次打全，落 runtime 日志供「GPU 到底有没有用上」
/// 的启动期排查（与 llama.cpp 的 --list-devices 探测结果一致）。
fn log_model_activation_gpu_status(model_path: &str, activation: &GenerationActivation) {
    crate::runtime_log::event(
        "info",
        "model.runtime",
        "activation.gpu_status",
        None,
        &serde_json::json!({
            "model_file": model_path.rsplit(['/', '\\']).next().unwrap_or(model_path),
            "device": activation.device,
            "backend": activation.backend,
            "gpu_layers": activation.gpu_layers,
            "multimodal": activation.multimodal,
            "context_size": activation.context_size,
        }),
    );
}

#[allow(clippy::too_many_arguments)]
fn self_test_and_activate_downloaded_roles(
    app: &AppHandle,
    catalog: &Arc<CatalogService>,
    models: &ModelManager,
    onnx: &WorkerClient,
    ocr: &WorkerClient,
    speech: &WorkerClient,
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
                let activation = runtime.activate(
                    &artifact.local_path,
                    4096,
                    interactive_inference_threads(),
                )?;
                // Phase 4.3（Qwen3.5 自检失败修复）：thinking 模型的 chat
                // template 可能强制先输出  thinking 思维链，也可能按
                // 可见回复过短或纯思维链（如「好的」）不应被误判回滚，
                // 判定标准统一改为下方注释描述的非空检查。
                let generated = runtime.complete(
                    "你是本地模型健康检查器，直接输出答案，不要展开推理。",
                    "请用一句完整中文句子回复：翻翻本地模型可以正常工作。",
                    256,
                )?;
                let visible = self_test_visible_text(&generated);
                // 自检只验证本地推理能产出文本：可见文本或原始输出任一非空
                // 即通过；两者皆空才判定失败，避免 2B Q4 thinking 模型给出
                // 短确认（如「好的」）或纯思维链时被误判回滚。
                if visible.trim().is_empty() && generated.trim().is_empty() {
                    crate::runtime_log::event(
                        "error",
                        "model.download",
                        "self_test.generation_empty",
                        None,
                        &serde_json::json!({
                            "model_file": artifact
                                .local_path
                                .rsplit(['/', '\\'])
                                .next()
                                .unwrap_or(&artifact.local_path),
                            "generated_length": generated.chars().count(),
                            "generated_snippet": generated.chars().take(120).collect::<String>(),
                        }),
                    );
                    runtime.stop();
                    return Err(AppError::new(
                        "MODEL_SELF_TEST_FAILED",
                        "生成模型没有通过最小本地推理自检，已回滚",
                        true,
                    ));
                }
                log_model_activation_gpu_status(&artifact.local_path, &activation);
                models.activate_artifact(&artifact.artifact_id, None)?;
            }
            (ModelRole::Embedding, ModelFormat::Onnx) => {
                let tokenizer = PathBuf::from(&artifact.local_path)
                    .parent()
                    .map(|parent| parent.join("tokenizer.json"))
                    .ok_or_else(|| {
                        AppError::new("EMBEDDING_TOKENIZER_UNAVAILABLE", "语义模型目录无效", false)
                    })?;
                let response = onnx.encode_embeddings(&EmbeddingRequest {
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
                let response = onnx.rerank(&RerankRequest {
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
                let (det_file, cls_file, dict_file) = ocr_companion_file_names(artifact);
                ocr.self_test_ocr(
                    artifact.local_path.clone(),
                    model_companion_path(artifact, &det_file)?,
                    model_companion_path(artifact, &cls_file)?,
                    model_companion_path(artifact, &dict_file)?,
                    1,
                    ocr_version_for(artifact),
                )?;
                models.activate_artifact(&artifact.artifact_id, None)?;
            }
            (ModelRole::Asr, ModelFormat::Onnx) => {
                speech.self_test_asr(
                    artifact.local_path.clone(),
                    model_companion_path(artifact, "tokens.txt")?,
                    1,
                    asr_arch_for(artifact),
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
    // 纯读状态：VLM/LLM 推理持锁期间可达数十秒，状态轮询绝不能排队等锁
    //（app_status_get/model_state_get 曾因此被拖 43s）。拿不到锁就按
    // 「探测未完成 / CPU 生效中」回退，model:state 事件会驱动前端刷新。
    let mut runtime = match try_lock_generation_until(&generation.0, Duration::from_millis(500)) {
        Some(runtime) => runtime,
        None => {
            let hardware = current_hardware_profile(&[]);
            let budget = current_inference_budget(hardware.memory_total_bytes);
            return Ok(InferenceRuntimeState {
                backend: "cpu".into(),
                device_names: Vec::new(),
                gpu_available: false,
                gpu_offload_layers: None,
                gpu_offload_mode: "disabled".into(),
                thread_budget: interactive_inference_threads(),
                batch_thread_budget: background_inference_threads(),
                active: false,
                pressure_reason: None,
                hardware,
                runtime_package: RuntimeBackendPackage {
                    backend: "cpu".into(),
                    device_count: 0,
                    gpu_capable: false,
                    cpu_fallback_available: false,
                    validated: true,
                },
                budget,
            });
        }
    };
    let active = runtime.is_active();
    // 探测由启动阶段的后台线程负责；探测完成前如实返回当前 runtime 状态
    //（CPU 生效中），绝不在此同步启动 llama-server --list-devices——冷 GPU
    // 可达数十秒，曾导致标题栏/模型页整窗卡死。后台探测完成会发 model:state
    // 事件驱动前端刷新，前后端状态始终一致。
    let capability = runtime
        .current_capability()
        .cloned()
        .unwrap_or_else(|| RuntimeCapability {
            executable_available: true,
            backend: "cpu".into(),
            devices: Vec::new(),
            gpu_available: false,
            checked_at: chrono::Utc::now(),
            error_code: None,
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
        asr: models.active_artifact(ModelRole::Asr)?.is_some(),
    };
    let any_active = capabilities.generation
        || capabilities.embedding
        || capabilities.vision
        || capabilities.reranker
        || capabilities.ocr
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
    if active_scan.is_none()
        && let Ok(mut cache) = HOME_SUMMARY_CACHE
            .get_or_init(|| Mutex::new((Instant::now(), String::new(), Value::Null)))
            .lock()
    {
        *cache = (Instant::now(), request.local_date.clone(), summary.clone());
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

/// rerank 精排修剪阈值（sigmoid 中点）：成员与种子同主题的 logit ≥0 才保留。
/// 种子永不修剪；低分成员剔除后成员 <2 的整条建议作废。
const COLLECTION_RERANK_PRUNE_THRESHOLD: f32 = 0.5;

#[tauri::command(async)]
pub fn collection_suggestion_refresh(
    request: CollectionSuggestionRefreshRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    generation: State<'_, GenerationServiceState>,
    worker: State<'_, WorkerServiceState>,
    runtime_manager: State<'_, RuntimeManagerState>,
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
    // 操作级追踪：SMART_COLLECTION 链路入口。
    let operation_trace = ActiveOperationTrace::begin(
        &catalog,
        &correlation_id,
        None,
        TraceFeatureType::SmartCollection,
        &json!({ "max_files": request.max_files }),
        None,
    );
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
        let new_suggestions = candidates
            .items
            .into_iter()
            .filter(|item| new_ids.contains(&item.suggestion_id))
            .collect::<Vec<_>>();
        // 成员档案摘要（260 字内容片段）：rerank 的 query/documents 与命名 prompt 共用
        let summaries = catalog.collection_suggestion_member_summaries(&result.suggestion_ids)?;
        // 1. rerank 精排修剪：query=种子摘要，documents=成员摘要。
        //    低分成员剔除（种子永不修剪）；剔除后成员 <2 的整条作废。
        //    未配置 reranker / 降级 / 任何失败一律 fail-open：不修剪。
        let mut discarded = HashSet::<Uuid>::new();
        if catalog.maintenance_snapshot()?.degradation_level == "full"
            && let Some(reranker) = models.active_artifact(ModelRole::Reranker)?
            && reranker.format == ModelFormat::Onnx
        {
            let tokenizer_path = PathBuf::from(&reranker.local_path)
                .parent()
                .map(|parent| parent.join("tokenizer.json"))
                .filter(|path| path.is_file());
            if let Some(tokenizer_path) = tokenizer_path {
                for suggestion in &new_suggestions {
                    let Some(seed_file_id) = result
                        .seed_file_id_by_suggestion
                        .get(&suggestion.suggestion_id)
                        .copied()
                    else {
                        continue;
                    };
                    let Some(seed_summary) = summaries.get(&seed_file_id) else {
                        continue;
                    };
                    let member_pairs = suggestion
                        .members
                        .iter()
                        .filter(|member| member.file.file_id != seed_file_id)
                        .map(|member| {
                            (
                                member.file.file_id,
                                summaries
                                    .get(&member.file.file_id)
                                    .cloned()
                                    .unwrap_or_default(),
                            )
                        })
                        .collect::<Vec<_>>();
                    if member_pairs.is_empty() {
                        continue;
                    }
                    let _ = app.emit(
                        "collection:suggestion_phase",
                        json!({"suggestion_id": suggestion.suggestion_id, "phase": "reranking"}),
                    );
                    let rerank_started = Instant::now();
                    let mut rerank_runtime_request = RuntimeTaskRequest::interactive(
                        RuntimeTaskKind::Rerank,
                        RuntimeBackendKind::OnnxRuntime,
                    );
                    rerank_runtime_request.cpu_threads = 2;
                    rerank_runtime_request.timeout = Duration::from_secs(10);
                    rerank_runtime_request.model_id = Some(reranker.artifact_id.to_string());
                    let Ok(rerank_runtime_lease) =
                        runtime_manager.0.acquire(rerank_runtime_request)
                    else {
                        continue;
                    };
                    let rerank_result = worker.client.rerank(&RerankRequest {
                        model_path: reranker.local_path.clone(),
                        tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                        query: seed_summary.clone(),
                        documents: member_pairs
                            .iter()
                            .map(|(_, summary)| summary.clone())
                            .collect::<Vec<_>>(),
                        max_length: reranker.max_length.unwrap_or(512),
                        threads: 2,
                    });
                    rerank_runtime_lease.complete();
                    let Ok(response) = rerank_result else {
                        continue;
                    };
                    if response.scores.len() != member_pairs.len()
                        || !response.scores.iter().all(|score| score.is_finite())
                    {
                        continue; // 条数/数值校验失败 fail-open
                    }
                    let removed = member_pairs
                        .iter()
                        .zip(&response.scores)
                        .filter(|(_, score)| **score < COLLECTION_RERANK_PRUNE_THRESHOLD)
                        .map(|((file_id, _), _)| *file_id)
                        .collect::<Vec<_>>();
                    let discarded_this = if removed.is_empty() {
                        false
                    } else {
                        match catalog.prune_collection_suggestion_members(
                            &suggestion.suggestion_id,
                            &removed,
                        ) {
                            Ok(true) => false,
                            Ok(false) => true,
                            Err(_) => false, // fail-open：保留原样
                        }
                    };
                    if discarded_this {
                        discarded.insert(suggestion.suggestion_id);
                    }
                    trace_node(
                        &catalog,
                        "collection",
                        "reranking",
                        &correlation_id,
                        None,
                        Some(&suggestion.suggestion_id.to_string()),
                        &json!({
                            "seed_file_id": seed_file_id,
                            "query": seed_summary,
                            "document_count": member_pairs.len(),
                            "documents": member_pairs
                                .iter()
                                .map(|(file_id, summary)| {
                                    json!({"file_id": file_id, "summary": compact_for_prompt(summary, 600)})
                                })
                                .collect::<Vec<_>>(),
                        }),
                        &json!({
                            "scores": response.scores,
                            "removed_count": removed.len(),
                            "discarded": discarded_this,
                        }),
                        "ok",
                        Some(rerank_started.elapsed().as_millis() as u64),
                    );
                }
            }
        }
        // 2. 生成模型命名润色（作废的建议跳过）：只改名称和说明，成员分组不变。
        if let Some(generation_artifact) = generation_artifact {
            for suggestion in new_suggestions
                .into_iter()
                .filter(|item| !discarded.contains(&item.suggestion_id))
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
                                "summary": summaries.get(&member.file.file_id).cloned().unwrap_or_default(),
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
                    &format!(
                        "请给这个同主题资料分组起名并写说明，可参考每个成员的内容片段：\n{candidate_json}"
                    ),
                    320,
                    &cancelled,
                );
                // 命名只是润色：任何失败都保留规则名，不丢弃建议、不阻塞主流程。
                let (decision, parsed) =
                    match raw_review.as_deref().map(parse_collection_model_review) {
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
        // 3. 收敛结果：rerank 作废的建议从返回值剔除，与库内状态一致。
        result.suggestion_ids.retain(|id| !discarded.contains(id));
        result
            .seed_file_id_by_suggestion
            .retain(|id, _| !discarded.contains(id));
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
    operation_trace.complete(&catalog, "ok");
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
    // 操作级追踪：FILE_RELATION 链路入口。
    let operation_trace = ActiveOperationTrace::begin(
        &catalog,
        &correlation_id,
        None,
        TraceFeatureType::FileRelation,
        &json!({ "max_files": request.max_files }),
        None,
    );
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
    let semantic_found =
        if let Some(embedding) = models.get()?.active_artifact(ModelRole::Embedding)? {
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
        semantic_found
            .as_ref()
            .map(|(_, _, artifact_id)| artifact_id.as_str()),
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
    operation_trace.complete(&catalog, "ok");
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
        let arch = asr_arch_for(&artifact);
        match speech_worker.recognize_speech(&SpeechRecognitionRequest {
            model_path: artifact.local_path,
            tokens_path,
            vad_model_path,
            samples: request.samples,
            sample_rate: request.sample_rate,
            threads,
            arch,
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

/// 依据 artifact 的角色型号判定 ASR 运行架构（`sense_voice` / `paraformer`）。
/// 复用于语音自检与识别请求，避免各处硬编码；缺省按 paraformer 兼容旧模型。
fn asr_arch_for(artifact: &ModelArtifact) -> String {
    let id = artifact
        .catalog_id
        .as_deref()
        .or(Some(artifact.model_id.as_str()))
        .unwrap_or("");
    if id.contains("sense") {
        "sense_voice".to_owned()
    } else {
        "paraformer".to_owned()
    }
}

/// 依据 artifact 的角色型号判定 OCR 版本形态（`PPOCRV6` / `PPOCRV5`）。
/// 复用于 OCR 自检与解析运行时；缺省按 PPOCRV5 兼容旧模型。
fn ocr_version_for(artifact: &ModelArtifact) -> String {
    let id = artifact
        .catalog_id
        .as_deref()
        .or(Some(artifact.model_id.as_str()))
        .unwrap_or("");
    if id.contains("v6") || id.contains("ppocr-v6") {
        "PPOCRV6".to_owned()
    } else {
        "PPOCRV5".to_owned()
    }
}

/// 依据 artifact 的 OCR 版本与尺寸，返回配套文件名 (det, cls, dict)。
/// v6 的检测文件名按 small/medium 后缀区分，分类器与词典命名也随 v6 变更，
/// 不能沿用 v5 的旧命名（否则激活自检与运行时都会因找不到配套文件失败）。
fn ocr_companion_file_names(artifact: &ModelArtifact) -> (String, String, String) {
    let id = artifact
        .catalog_id
        .as_deref()
        .or(Some(artifact.model_id.as_str()))
        .unwrap_or("");
    if ocr_version_for(artifact) == "PPOCRV6" {
        let size = if id.contains("small") {
            "small"
        } else {
            "medium"
        };
        (
            format!("PP-OCRv6_det_{size}.onnx"),
            "ch_ppocr_mobile_v2.0_cls_mobile.onnx".to_owned(),
            "ppocrv6_dict.txt".to_owned(),
        )
    } else {
        (
            "ch_PP-OCRv5_mobile_det.onnx".to_owned(),
            "ch_ppocr_mobile_v2.0_cls_infer.onnx".to_owned(),
            "ppocrv5_dict.txt".to_owned(),
        )
    }
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
    let (det_file, cls_file, dict_file) = ocr_companion_file_names(&artifact);
    Ok(Some(OcrRuntimeConfig {
        model_path: artifact.local_path.clone(),
        det_model_path: model_companion_path(&artifact, &det_file)?,
        cls_model_path: model_companion_path(&artifact, &cls_file)?,
        dictionary_path: model_companion_path(&artifact, &dict_file)?,
        threads: threads.clamp(1, 2),
        ocr_version: ocr_version_for(&artifact),
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
/// 用户原始问题无关 → 按 LOCAL 检索失败返回固定文案「当前资料中没有找到
/// 足够依据」，不再转闲聊（路由语义已明确是资料检索，闲聊会答非所问）。
/// 杜绝「检索答错把资料库内容当答案」。阈值可依据 trace 中 reranking
/// 节点记录的每候选 score 实测调优。
const RERANK_NO_EVIDENCE_THRESHOLD: f32 = 0.1;

fn cached_index_activity_stats(catalog: &CatalogService) -> Result<IndexActivityStats, AppError> {
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
    // 合并并发轮询：前端多个组件会同时请求状态快照，400ms 内复用计算结果，
    // 避免在后台任务繁忙时反复触发 DB 统计与运行时检查造成竞争（曾出现
    // 同一毫秒 4 次并发调用、单次耗时 1.3~3.5s 的卡顿）。
    const SNAPSHOT_TTL: Duration = Duration::from_millis(400);
    static CACHE: OnceLock<Mutex<(Instant, Option<AppStatusSnapshot>)>> = OnceLock::new();
    let cache = CACHE
        .get_or_init(|| Mutex::new((Instant::now() - SNAPSHOT_TTL - Duration::from_secs(1), None)));
    let mut guard = cache
        .lock()
        .map_err(|_| AppError::new("STATUS_CACHE_LOCK", "状态快照缓存锁定失败", false))?;
    if guard.1.is_some() && guard.0.elapsed() < SNAPSHOT_TTL {
        return Ok(guard.1.clone().expect("缓存命中必然存在快照"));
    }
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
    let snapshot = AppStatusSnapshot {
        local_only: true,
        source_files_readonly: true,
        roots,
        scan_progress,
        maintenance,
        inference_runtime,
        ai_runtime: runtime_manager.0.snapshot()?,
        recovery_actions,
        checked_at,
    };
    guard.0 = Instant::now();
    guard.1 = Some(snapshot.clone());
    Ok(snapshot)
}

/// sidecar 角色 → 运行时后端展示名（设置页「运行时状态」实例列表）。
fn backend_for_role(role: WorkerRole) -> RuntimeBackendKind {
    match role {
        WorkerRole::Parse => RuntimeBackendKind::Parser,
        WorkerRole::Onnx => RuntimeBackendKind::OnnxRuntime,
        WorkerRole::Ocr => RuntimeBackendKind::PaddleOcr,
        WorkerRole::Speech => RuntimeBackendKind::SherpaOnnx,
    }
}

/// 把 4 个 sidecar 进程的 watchdog 观测（真实工作集内存/存活）同步进
/// RuntimeManager 实例列表：进程存活则覆盖注册/更新观测，失联则移除。
/// 固定 role UUID 保证进程重启后仍是同一实例（loaded_at 保持首次加载）。
fn sync_sidecar_instances(
    manager: &RuntimeManager,
    sidecars: &[(WorkerRole, &WorkerClient)],
) -> Result<(), AppError> {
    let now = Utc::now();
    for (role, client) in sidecars {
        let instance_id = role.instance_uuid();
        match client.supervisor_snapshot() {
            Some(snapshot) if snapshot.alive => {
                manager.sync_instance(RuntimeInstanceState {
                    instance_id,
                    backend: backend_for_role(*role),
                    model_id: None,
                    device: "cpu".to_string(),
                    status: "running".to_string(),
                    memory_bytes: snapshot.working_set_bytes,
                    gpu_memory_bytes: 0,
                    idle_timeout_seconds: snapshot.idle_timeout_seconds,
                    loaded_at: now,
                    last_used_at: now,
                })?;
            }
            _ => {
                let _ = manager.unregister_instance(&instance_id)?;
            }
        }
    }
    Ok(())
}

#[tauri::command(async)]
pub fn runtime_state_get(
    runtime_manager: State<'_, RuntimeManagerState>,
    worker: State<'_, WorkerServiceState>,
    sidecars: State<'_, SidecarRegistryState>,
    speech: State<'_, SpeechWorkerState>,
) -> Result<AiRuntimeSnapshot, AppError> {
    sync_sidecar_instances(
        &runtime_manager.0,
        &[
            (WorkerRole::Parse, &worker.client),
            (WorkerRole::Onnx, &sidecars.0.onnx),
            (WorkerRole::Ocr, &sidecars.0.ocr),
            (WorkerRole::Speech, &speech.0),
        ],
    )?;
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

#[derive(Debug, Deserialize)]
pub struct MigrationCleanupRequest {
    confirmation: String,
}

/// 清理迁移完成前的旧应用数据目录（迁移已完成且切换生效后）。
/// 前置校验：pending 为空、当前数据目录健康（marker 有效）；随后 rename 原子
/// 隔离再递归删除；途中崩溃由下次启动的 apply_pending_storage_cleanup 兜底。
#[tauri::command]
pub async fn storage_migration_cleanup(
    request: MigrationCleanupRequest,
    environment: State<'_, EnvironmentServiceState>,
) -> Result<crate::MigrationCleanupResult, AppError> {
    if request.confirmation != "CLEANUP_MIGRATED_STORAGE" {
        return Err(AppError::new(
            "STORAGE_MIGRATION_CLEANUP_CONFIRMATION_REQUIRED",
            "清理旧数据需要再次明确确认",
            false,
        ));
    }
    let config_directory = environment.config_directory.clone();
    let data_directory = environment.data_directory.clone();
    tauri::async_runtime::spawn_blocking(move || {
        crate::cleanup_storage_migration(&config_directory, &data_directory)
    })
    .await
    .map_err(|error| AppError::new("STORAGE_MIGRATION_CLEANUP_FAILED", error.to_string(), true))?
}

/// 清理迁移完成前的旧模型仓库（迁移已完成且切换生效后）。
/// 前置校验：pending 为空、当前仓库健康（marker 有效 + registry 可打开）；
/// 随后 rename 原子隔离再递归删除；途中崩溃由下次启动的
/// apply_pending_model_store_cleanup 兜底。
#[tauri::command]
pub async fn model_store_migration_cleanup(
    request: MigrationCleanupRequest,
    environment: State<'_, EnvironmentServiceState>,
    models: State<'_, ModelServiceState>,
) -> Result<crate::MigrationCleanupResult, AppError> {
    if request.confirmation != "CLEANUP_MIGRATED_MODEL_STORE" {
        return Err(AppError::new(
            "MODEL_STORE_CLEANUP_CONFIRMATION_REQUIRED",
            "清理旧模型仓库需要再次明确确认",
            false,
        ));
    }
    let config_directory = environment.config_directory.clone();
    let active_store = models.get()?.model_root().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        crate::cleanup_model_store_migration(&config_directory, &active_store)
    })
    .await
    .map_err(|error| AppError::new("MODEL_STORE_CLEANUP_FAILED", error.to_string(), true))?
}

#[derive(Debug, Deserialize)]
pub struct ModelStoreMigrationScheduleRequest {
    selected_directory: String,
    confirmation: String,
}

#[tauri::command]
pub async fn model_store_migration_schedule(
    request: ModelStoreMigrationScheduleRequest,
    environment: State<'_, EnvironmentServiceState>,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
) -> Result<ModelStoreStatus, AppError> {
    if request.confirmation != "MIGRATE_MODEL_STORE" {
        return Err(AppError::new(
            "MODEL_STORE_MIGRATION_CONFIRMATION_REQUIRED",
            "迁移模型仓库需要再次明确确认",
            false,
        ));
    }
    let config_directory = environment.config_directory.clone();
    let selected_directory = PathBuf::from(request.selected_directory);
    let current_store = models.get()?.model_root().to_path_buf();
    let base_status = models.get()?.store_status()?;
    let roots = catalog
        .get()?
        .list_roots()?
        .into_iter()
        .filter(|root| root.enabled)
        .map(|root| PathBuf::from(root.canonical_path))
        .collect::<Vec<_>>();
    tauri::async_runtime::spawn_blocking(move || {
        crate::schedule_model_store_migration(
            &config_directory,
            &current_store,
            &selected_directory,
            &roots,
            base_status,
        )
    })
    .await
    .map_err(|error| {
        AppError::new(
            "MODEL_STORE_MIGRATION_SCHEDULE_FAILED",
            error.to_string(),
            true,
        )
    })?
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

pub(crate) fn directory_size(path: &Path) -> Result<u64, AppError> {
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

pub(crate) fn clear_directory_contents(path: &Path) -> Result<u64, AppError> {
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

/// Ask Trace Viewer 阶段展示顺序（固定；未出现的阶段跳过，未知节点追加末尾）。
const ASK_TRACE_STAGE_ORDER: &[&str] = &[
    "source_routing",
    "query_parsing",
    "context_resolution",
    "memory_resolution",
    "document_resolution",
    "scope_planning",
    "clarification_selection",
    "query_rewrite",
    "document_recall",
    "retrieval",
    "reranking",
    "generation",
    "verification",
    "extract",
    "document_find",
    "document_compare",
    "document_summary",
    "repair",
    "operation_execution",
    "memory_candidate_write",
    "completed",
];

/// 拉取一次 Ask 的全部节点追踪并组装 Trace Viewer 结构
///（阶段分组、逐阶段耗时、诊断摘要一行）。任何阶段缺失都不算错——问答
/// 可能在澄清/拒绝/降级等中途收尾，只展示实际经过的节点。
fn build_ask_trace(catalog: &CatalogService, operation_id: &str) -> Result<AskTrace, AppError> {
    let records = catalog.query_node_traces_by_correlation("ask", operation_id)?;
    if records.is_empty() {
        return Err(AppError::new(
            "ASK_TRACE_NOT_FOUND",
            "找不到该次问答的调试追踪（可能已被清理，或该记录不是 Ask 流程）",
            false,
        ));
    }
    let stages = group_ask_trace_stages(&records);
    let timing = aggregate_ask_timing(&stages);
    let diagnostic_summary = build_diagnostic_summary(&stages);
    let question = ["source_routing", "query_parsing"]
        .iter()
        .find_map(|name| stages.iter().find(|stage| stage.node == *name))
        .and_then(|stage| stage.records.first())
        .and_then(|record| record.input_json.get("question"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let answer_mode = stages
        .iter()
        .find(|stage| stage.node == "completed")
        .and_then(|stage| stage.records.last())
        .and_then(|record| record.output_json.get("answer_mode"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(AskTrace {
        operation_id: operation_id.to_owned(),
        question,
        answer_mode,
        stages,
        timing,
        diagnostic_summary,
    })
}

/// 按固定顺序把一次 Ask 的节点追踪分组为阶段；未知节点（未来新增）追加末尾。
fn group_ask_trace_stages(records: &[NodeTraceRecord]) -> Vec<AskTraceStage> {
    let mut stages = ASK_TRACE_STAGE_ORDER
        .iter()
        .filter_map(|node| {
            let items = records
                .iter()
                .filter(|record| record.node == *node)
                .cloned()
                .collect::<Vec<_>>();
            (!items.is_empty()).then(|| AskTraceStage {
                node: (*node).to_owned(),
                records: items,
            })
        })
        .collect::<Vec<_>>();
    let known = ASK_TRACE_STAGE_ORDER
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut extra = records
        .iter()
        .map(|record| record.node.as_str())
        .filter(|node| !known.contains(*node))
        .collect::<Vec<_>>();
    extra.sort_unstable();
    extra.dedup();
    for node in extra {
        let items = records
            .iter()
            .filter(|record| record.node == node)
            .cloned()
            .collect::<Vec<_>>();
        stages.push(AskTraceStage {
            node: node.to_owned(),
            records: items,
        });
    }
    stages
}

/// 逐阶段耗时聚合：同节点多条记录（如每条 claim 一次 verification）取和；
/// 阶段未出现保持 null；总耗时取 completed 节点。
fn aggregate_ask_timing(stages: &[AskTraceStage]) -> AskTraceTiming {
    let node_sum = |name: &str| -> Option<u64> {
        let sum = stages
            .iter()
            .filter(|stage| stage.node == name)
            .flat_map(|stage| stage.records.iter())
            .filter_map(|record| record.elapsed_ms)
            .sum::<u64>();
        (sum > 0).then_some(sum)
    };
    let mut timing = AskTraceTiming {
        source_router_ms: node_sum("source_routing"),
        query_parser_ms: node_sum("query_parsing"),
        context_ms: node_sum("context_resolution"),
        memory_ms: node_sum("memory_resolution"),
        document_resolver_ms: node_sum("document_resolution"),
        document_recall_ms: node_sum("document_recall"),
        rerank_ms: node_sum("reranking"),
        generation_ms: node_sum("generation"),
        verification_ms: node_sum("verification"),
        ..Default::default()
    };
    // embedding 计时来自 retrieval 节点输出（app 层 encode 计时）；
    // fts 在 core 的 answer_extractively 内部合并执行、无法再拆，保持 null
    //（检索总耗时见 retrieval 节点输出 retrieval_elapsed_ms）。
    if let Some(record) = stages
        .iter()
        .find(|stage| stage.node == "retrieval")
        .and_then(|stage| stage.records.first())
    {
        timing.embedding_ms = record
            .output_json
            .get("embedding_ms")
            .and_then(Value::as_u64);
    }
    timing.total_ms = stages
        .iter()
        .find(|stage| stage.node == "completed")
        .and_then(|stage| stage.records.last())
        .and_then(|record| record.elapsed_ms);
    timing
}

/// 每轮一问的诊断摘要一行（仅 Developer 模式显示）。
/// 示例：LOCAL DOCUMENT_QA target=resume memory=miss doc=Resolved(0.91)
///       scope_files=1 retrieval_candidates=12 rerank_top1=0.83 claims=4
///       4/4 supported total=3421ms
fn build_diagnostic_summary(stages: &[AskTraceStage]) -> String {
    let output_of = |node: &str| -> Option<&Value> {
        stages
            .iter()
            .find(|stage| stage.node == node)
            .and_then(|stage| stage.records.last())
            .map(|record| &record.output_json)
    };
    let mut parts: Vec<String> = Vec::new();
    // 来源 + 意图（LOCAL DOCUMENT_QA）
    let source = output_of("source_routing")
        .and_then(|output| output.get("source"))
        .and_then(Value::as_str);
    let intent = output_of("query_parsing")
        .and_then(|output| output.get("plan"))
        .and_then(|plan| plan.get("intent"))
        .and_then(Value::as_str)
        .or_else(|| {
            stages
                .iter()
                .find(|stage| stage.node == "completed")
                .and_then(|stage| stage.records.last())
                .and_then(|record| record.output_json.get("answer_mode"))
                .and_then(Value::as_str)
        });
    match (source, intent) {
        (Some(source), Some(intent)) => parts.push(format!("{source} {intent}")),
        (Some(source), None) => parts.push(source.to_owned()),
        (None, Some(intent)) => parts.push(intent.to_owned()),
        (None, None) => {}
    }
    // 目标对象
    if let Some(target) = output_of("query_parsing")
        .and_then(|output| output.get("plan"))
        .and_then(|plan| plan.get("target"))
    {
        let reference = target
            .get("reference")
            .and_then(Value::as_str)
            .or_else(|| target.get("document_name").and_then(Value::as_str));
        if let Some(reference) = reference {
            parts.push(format!("target={}", compact_for_prompt(reference, 24)));
        }
    }
    // Memory 命中
    if let Some(ok) = output_of("memory_resolution")
        .and_then(|output| output.get("memory_resolution_ok"))
        .and_then(Value::as_bool)
    {
        parts.push(if ok {
            "memory=hit".to_owned()
        } else {
            "memory=miss".to_owned()
        });
    }
    // 文档解析结果（status + top 候选分数）
    if let Some(resolution) = output_of("document_resolution") {
        let status = resolution
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let top_score = resolution
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            .and_then(|candidate| candidate.get("score"))
            .and_then(Value::as_f64);
        match top_score {
            Some(score) => parts.push(format!("doc={status}({score:.2})")),
            None => parts.push(format!("doc={status}")),
        }
    }
    // 最终 scope 文件数
    if let Some(files) = output_of("scope_planning")
        .and_then(|output| output.get("scope_file_ids"))
        .and_then(Value::as_array)
    {
        parts.push(format!("scope_files={}", files.len()));
    }
    // 文档级召回候选数
    if let Some(count) = output_of("document_recall")
        .and_then(|output| output.get("candidate_count"))
        .and_then(Value::as_u64)
    {
        parts.push(format!("recall_candidates={count}"));
    }
    // 初始检索候选（trace 内最多展示 10 条）
    if let Some(candidates) = output_of("retrieval")
        .and_then(|output| output.get("candidates"))
        .and_then(Value::as_array)
    {
        parts.push(format!("retrieval_candidates={}", candidates.len()));
    }
    // 重排结果
    if let Some(rerank) = output_of("reranking") {
        if rerank.get("applied").and_then(Value::as_bool) == Some(true)
            && let Some(top) = rerank
                .get("scores")
                .and_then(Value::as_array)
                .and_then(|scores| scores.first())
                .and_then(Value::as_f64)
        {
            parts.push(format!("rerank_top1={top:.2}"));
        } else if let Some(fallback) = rerank.get("fallback").and_then(Value::as_str) {
            parts.push(format!("rerank={fallback}"));
        }
    }
    // 生成 claim 数 + 核验通过率
    if let Some(claim_count) = stages
        .iter()
        .find(|stage| stage.node == "completed")
        .and_then(|stage| stage.records.last())
        .and_then(|record| record.output_json.get("claim_count"))
        .and_then(Value::as_u64)
    {
        parts.push(format!("claims={claim_count}"));
    }
    if let Some(records) = stages
        .iter()
        .find(|stage| stage.node == "verification")
        .map(|stage| &stage.records)
    {
        let supported = records
            .iter()
            .filter(|record| {
                record.output_json.get("supported").and_then(Value::as_bool) == Some(true)
            })
            .count();
        parts.push(format!("{supported}/{} supported", records.len()));
    }
    // 总耗时
    if let Some(total) = stages
        .iter()
        .find(|stage| stage.node == "completed")
        .and_then(|stage| stage.records.last())
        .and_then(|record| record.elapsed_ms)
    {
        parts.push(format!("total={total}ms"));
    }
    parts.join(" ")
}

/// Debug Trace 导出脱敏：全路径 → [路径]文件名、长文本截断（默认）、
/// 模型完整 prompt 默认隐藏（generation 节点 input）。
fn sanitize_ask_trace_for_export(stages: &mut [AskTraceStage], include_detailed_text: bool) {
    for stage in stages {
        for record in &mut stage.records {
            let mut input = record.input_json.clone();
            let mut output = record.output_json.clone();
            sanitize_trace_value(&mut input, include_detailed_text);
            sanitize_trace_value(&mut output, include_detailed_text);
            if !include_detailed_text
                && stage.node == "generation"
                && let Some(Value::String(prompt)) = input.get("prompt")
            {
                let length = prompt.chars().count();
                input["prompt"] = json!(format!("[已隐藏] 共 {length} 字符"));
            }
            record.input_json = input;
            record.output_json = output;
        }
    }
}

/// 递归脱敏单个 trace 值：Windows 绝对路径 / UNC → 只留文件名；长文本截断。
/// 截断仅默认模式生效（include_detailed_text = true 保留详细文本）。
fn sanitize_trace_value(value: &mut Value, include_detailed_text: bool) {
    const EXPORT_TEXT_LIMIT: usize = 2_000;
    const EXPORT_KEEP_CHARS: usize = 500;
    match value {
        Value::String(text) => {
            *text = sanitize_trace_paths(text);
            if !include_detailed_text && text.chars().count() > EXPORT_TEXT_LIMIT {
                let mut kept = text.chars().take(EXPORT_KEEP_CHARS).collect::<String>();
                kept.push_str("…[已截断]");
                *text = kept;
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| sanitize_trace_value(item, include_detailed_text)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| sanitize_trace_value(item, include_detailed_text)),
        _ => {}
    }
}

/// 把文本中的 Windows 绝对路径 / UNC 路径替换为「[路径]文件名」。
/// 启发式扫描：`X:\`（盘符前不带字母数字，避免误伤 JSON 键）或 `\\` 起始，
/// 到引号 / 空白 / 常见分隔符为止；只保留最后一段作为文件名。
fn sanitize_trace_paths(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        // 扫描下一个路径起点
        let mut path_start = None;
        let mut cursor = index;
        while cursor < text.len() {
            let byte = bytes[cursor];
            let is_drive = byte.is_ascii_alphabetic()
                && bytes.get(cursor + 1) == Some(&b':')
                && bytes.get(cursor + 2) == Some(&b'\\')
                && (cursor == 0 || !bytes[cursor - 1].is_ascii_alphanumeric());
            let is_unc = byte == b'\\' && bytes.get(cursor + 1) == Some(&b'\\');
            if is_drive || is_unc {
                path_start = Some(cursor);
                break;
            }
            cursor += 1;
        }
        let Some(start) = path_start else {
            output.push_str(&text[index..]);
            break;
        };
        // 路径终点：引号 / 空白 / 常见分隔符
        let mut end = text.len();
        for (offset, ch) in text[start..].char_indices() {
            if matches!(
                ch,
                '"' | '\'' | ' ' | '\t' | '\n' | '\r' | ',' | '}' | ']' | ')'
            ) {
                end = start + offset;
                break;
            }
        }
        let path = &text[start..end];
        let name = path
            .rsplit('\\')
            .next()
            .unwrap_or(path)
            .trim_end_matches(['/', ':']);
        output.push_str("[路径]");
        output.push_str(name);
        index = end;
    }
    output
}

/// Ask Debug Trace Viewer 数据（Phase 3）：一次 Ask 的全部节点追踪 →
/// 12+ 阶段分组 + 逐阶段耗时 + 诊断摘要一行。仅用于 Developer / 诊断模式。
#[tauri::command(async)]
pub fn ask_trace_get(
    operation_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<AskTrace, AppError> {
    let catalog = catalog.get()?;
    build_ask_trace(&catalog, &operation_id)
}

/// Debug Trace 导出（单次问答 → JSON 文件）。
/// 隐私默认：文件路径脱敏、文本 chunk 截断、不含模型完整 prompt；
/// include_detailed_text 为开发者选项（默认关闭），仅跳过文本截断并保留
/// prompt，路径脱敏始终生效。
#[tauri::command(async)]
pub fn ask_trace_export(
    request: AskTraceExportRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<ExportResult, AppError> {
    if !request.confirmed {
        return Err(AppError::new(
            "EXPORT_CONFIRMATION_REQUIRED",
            "导出调试追踪前需要明确确认",
            false,
        ));
    }
    let target = PathBuf::from(request.target_path.trim());
    if !target.is_absolute() || target.extension().and_then(|value| value.to_str()) != Some("json")
    {
        return Err(AppError::new(
            "EXPORT_TARGET_INVALID",
            "调试追踪必须导出到绝对路径的JSON新文件",
            false,
        ));
    }
    let catalog = catalog.get()?;
    let trace = build_ask_trace(&catalog, &request.operation_id)?;
    let mut stages = trace.stages.clone();
    sanitize_ask_trace_for_export(&mut stages, request.include_detailed_text);
    let payload = AskTraceExport {
        schema_version: 1,
        generated_at: Utc::now().to_rfc3339(),
        operation_id: trace.operation_id.clone(),
        question: trace.question.clone(),
        answer_mode: trace.answer_mode.clone(),
        stages,
        timing: trace.timing.clone(),
        diagnostic_summary: trace.diagnostic_summary.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&payload)
        .map_err(|error| AppError::new("TRACE_SERIALIZE_FAILED", error.to_string(), false))?;
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
        row_count: trace
            .stages
            .iter()
            .map(|stage| stage.records.len() as u64)
            .sum(),
        size_bytes: bytes.len() as u64,
        sha256,
    })
}

/// Ask Evaluation Runner（Phase 3）：JSONL/JSON 测试集批量运行问答管线。
///
/// 隔离性设计：
/// - 每例独立 operation_id（node_traces 关联键，可对应用例打开 Trace Viewer）
///   与独立 session_id（本例专用会话，跑完即删——不污染 Ask History /
///   Session Context；测试「有/无 Memory 与有/无 Session Context」四态时，
///   Memory 由 Clear Memory 命令控制，Context 天然为空）。
/// - 不启动 Memory Candidate Writer、不出现 clarification_selection
///   （禁止自动写 Memory）。
/// - 结果写 output_path（create_new），每例 verdict 与 error_category
///   可在 JSON 中人工修改分类后复用。
#[tauri::command(async)]
pub async fn ask_evaluation_run(
    request: AskEvaluationRunRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    sidecars: State<'_, SidecarRegistryState>,
    generation: State<'_, GenerationServiceState>,
    runtime_manager: State<'_, RuntimeManagerState>,
) -> Result<AskEvaluationRunReport, AppError> {
    if !request.confirmed {
        return Err(AppError::new(
            "EVALUATION_CONFIRMATION_REQUIRED",
            "运行评估测试集前需要明确确认",
            false,
        ));
    }
    let target = PathBuf::from(request.target_path.trim());
    if !target.is_file() {
        return Err(AppError::new(
            "EVALUATION_SET_NOT_FOUND",
            "评估测试集文件不存在",
            false,
        ));
    }
    let output = PathBuf::from(request.output_path.trim());
    if !output.is_absolute() || output.extension().and_then(|value| value.to_str()) != Some("json")
    {
        return Err(AppError::new(
            "EVALUATION_OUTPUT_INVALID",
            "评估结果必须写到绝对路径的JSON新文件",
            false,
        ));
    }
    if output.exists() {
        return Err(AppError::new(
            "EVALUATION_OUTPUT_EXISTS",
            "评估结果文件已存在，请更换输出路径",
            false,
        ));
    }
    let content = fs::read_to_string(&target)
        .map_err(|error| AppError::new("EVALUATION_SET_READ_FAILED", error.to_string(), true))?;
    let cases = fanfan_core::evaluation::parse_evaluation_cases(&content)?;
    if cases.is_empty() {
        return Err(AppError::new(
            "EVALUATION_SET_EMPTY",
            "评估测试集为空，没有可运行的用例",
            false,
        ));
    }
    let catalog = catalog.get()?;
    let models = models.get()?;
    let worker = sidecars.0.onnx.isolated();
    let generation = Arc::clone(&generation.0);
    let runtime_manager = runtime_manager.0.clone();
    let cancelled = Arc::new(AtomicBool::new(false));
    tauri::async_runtime::spawn_blocking(move || {
        // 串行批量运行：本地模型推理不适合并发，逐例独立跑
        let mut results = Vec::with_capacity(cases.len());
        for case in &cases {
            results.push(run_single_evaluation_case(
                case,
                &catalog,
                &models,
                &worker,
                &generation,
                &runtime_manager,
                &cancelled,
            ));
        }
        for result in &mut results {
            let verdict = fanfan_core::evaluation::verdict_for(result);
            result.pass_fail = verdict.pass_fail;
            result.failed_fields = verdict.failed_fields;
        }
        let metrics = fanfan_core::evaluation::compute_metrics(&results);
        let passed = results.iter().filter(|result| result.pass_fail).count();
        let report = AskEvaluationRunReport {
            schema_version: 1,
            generated_at: Utc::now().to_rfc3339(),
            run_id: Uuid::now_v7().to_string(),
            total: results.len(),
            passed,
            failed: results.len() - passed,
            metrics,
            results,
        };
        let bytes = serde_json::to_vec_pretty(&report).map_err(|error| {
            AppError::new("EVALUATION_SERIALIZE_FAILED", error.to_string(), false)
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)
            .map_err(|error| AppError::new("EVALUATION_CREATE_FAILED", error.to_string(), true))?;
        file.write_all(&bytes)
            .and_then(|_| file.flush())
            .map_err(|error| AppError::new("EVALUATION_WRITE_FAILED", error.to_string(), true))?;
        crate::runtime_log::event(
            "info",
            "rag",
            "ask.evaluation_completed",
            Some(&report.run_id),
            &json!({
                "total": report.total,
                "passed": report.passed,
                "failed": report.failed,
                "output_path": output.to_string_lossy(),
            }),
        );
        Ok(report)
    })
    .await
    .map_err(|error| AppError::new("EVALUATION_RUNTIME_FAILED", error.to_string(), true))?
}

/// 单例运行：独立操作/会话，跑完即删会话，收集 trace 与 answer 侧字段。
#[allow(clippy::too_many_arguments)]
fn run_single_evaluation_case(
    case: &fanfan_core::evaluation::EvaluationCase,
    catalog: &CatalogService,
    models: &ModelManager,
    worker: &WorkerClient,
    generation: &Mutex<LocalGenerationRuntime>,
    runtime_manager: &RuntimeManager,
    cancelled: &AtomicBool,
) -> fanfan_core::evaluation::EvaluationRunResult {
    let operation_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let request = AskRequest {
        question: case.question.clone(),
        session_id: Some(session_id),
        scope: ScopeFilter {
            root_ids: Vec::new(),
            collection_ids: Vec::new(),
            file_ids: Vec::new(),
            extensions: Vec::new(),
            modified_from: None,
            modified_to: None,
            availability: fanfan_core::Availability::Present,
        },
        answer_style: fanfan_core::AnswerStyle::Concise,
        retrieval_limit: 12,
        max_source_files: 4,
        strict_evidence: true,
        clarification_selection: None,
    };
    let started = Instant::now();
    let (answer, run_error) = run_evaluation_ask(
        &request,
        catalog,
        models,
        worker,
        generation,
        runtime_manager,
        operation_id,
        cancelled,
    );
    let latency_ms = started.elapsed().as_millis() as u64;
    // 清理本例专用会话（失败路径也可能已写入失败记录；不存在则忽略）
    let _ = catalog.delete_ask_session(&session_id);
    let records = catalog
        .query_node_traces_by_correlation("ask", &operation_id.to_string())
        .unwrap_or_default();
    let output_of = |node: &str| {
        records
            .iter()
            .find(|record| record.node == node)
            .map(|record| &record.output_json)
    };
    let failed_nodes = records
        .iter()
        .filter(|record| record.status != "ok")
        .map(|record| record.node.as_str())
        .collect::<Vec<_>>();

    let actual_source = output_of("source_routing")
        .and_then(|output| output.get("source"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let actual_intent = output_of("query_parsing")
        .and_then(|output| output.get("plan"))
        .and_then(|plan| plan.get("intent"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let actual_document_type = output_of("query_parsing")
        .and_then(|output| output.get("plan"))
        .and_then(|plan| plan.get("target"))
        .and_then(|target| target.get("document_type"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            output_of("context_resolution")
                .and_then(|output| output.get("resolved_document_type"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    let memory_used = output_of("memory_resolution")
        .and_then(|output| output.get("memory_resolution_ok"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // 检索候选（retrieval trace 写入时仍是 pre-rerank 顺序，file_id 齐全）
    let retrieval_top_files = output_of("retrieval")
        .and_then(|output| output.get("candidates"))
        .and_then(Value::as_array)
        .map(|candidates| {
            candidates
                .iter()
                .filter_map(|candidate| candidate.get("file_id"))
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // rerank 已应用 → 按分数重排（scores 与候选一一对应，同一顺序）；
    // 未应用 → 与检索顺序一致
    let rerank_applied = output_of("reranking")
        .and_then(|output| output.get("applied"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let rerank_top_files = if rerank_applied {
        let scores = output_of("reranking")
            .and_then(|output| output.get("scores"))
            .and_then(Value::as_array);
        let mut paired = retrieval_top_files
            .iter()
            .enumerate()
            .map(|(index, file_id)| {
                let score = scores
                    .and_then(|scores| scores.get(index))
                    .and_then(|score| score.get("score"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                (file_id.clone(), score)
            })
            .collect::<Vec<_>>();
        paired.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        paired.into_iter().map(|(file_id, _)| file_id).collect()
    } else {
        retrieval_top_files.clone()
    };

    // answer 侧字段 + 错误分类
    let (
        actual_file_ids,
        grounding_status,
        answer_mode,
        evidence_found,
        answer_grounded,
        claim_count,
        supported_claim_count,
        clarification_used,
        error_message,
        error_category,
    ) = match answer {
        Some(result) => {
            let grounding_status =
                serde_json::to_string(&result.grounding_status).unwrap_or_default();
            let grounding_status = grounding_status.trim_matches('"').to_owned();
            let supported_count = result
                .claims
                .iter()
                .filter(|claim| claim.support_status == SupportStatus::Supported)
                .count();
            let claims_have_unsupported = supported_count < result.claims.len();
            let category = fanfan_core::evaluation::classify_error(
                &failed_nodes,
                None,
                Some(result.answer_mode.as_str()),
                result.insufficient_evidence,
                case.expected_should_find_evidence,
                claims_have_unsupported,
            );
            (
                result
                    .used_file_ids
                    .iter()
                    .map(|file_id| file_id.to_string())
                    .collect::<Vec<_>>(),
                Some(grounding_status),
                Some(result.answer_mode.as_str().to_owned()),
                !result.claims.is_empty() || !result.used_file_ids.is_empty(),
                result.grounding_status == GroundingStatus::Grounded && !claims_have_unsupported,
                result.claims.len() as u64,
                supported_count as u64,
                result.clarification.is_some() || result.answer_mode == AnswerMode::Clarification,
                None,
                category,
            )
        }
        None => {
            let category = fanfan_core::evaluation::classify_error(
                &failed_nodes,
                run_error.as_ref().map(|error| error.code.as_str()),
                None,
                false,
                case.expected_should_find_evidence,
                false,
            );
            (
                Vec::new(),
                None,
                None,
                false,
                false,
                0,
                0,
                false,
                run_error.map(|error| error.message),
                category,
            )
        }
    };

    fanfan_core::evaluation::EvaluationRunResult {
        case_id: case.id.clone(),
        operation_id: operation_id.to_string(),
        question: case.question.clone(),
        expected_source: case.expected_source.clone(),
        expected_intent: case.expected_intent.clone(),
        expected_file_ids: case.expected_file_ids.clone(),
        expected_document_type: case.expected_document_type.clone(),
        expected_should_find_evidence: case.expected_should_find_evidence,
        actual_source,
        actual_intent,
        actual_file_ids,
        actual_document_type,
        memory_used,
        clarification_used,
        retrieval_top_files,
        rerank_top_files,
        grounding_status,
        answer_mode,
        evidence_found,
        answer_grounded,
        claim_count,
        supported_claim_count,
        latency_ms,
        error_category,
        error_message,
        pass_fail: false,
        failed_fields: Vec::new(),
    }
}

/// 批量运行的单例执行器：与 ask_start 同口径的运行时租约 + compute_answer，
/// 不启动 Memory Candidate Writer，不写前台活动守卫。
#[allow(clippy::too_many_arguments)]
fn run_evaluation_ask(
    request: &AskRequest,
    catalog: &CatalogService,
    models: &ModelManager,
    worker: &WorkerClient,
    generation: &Mutex<LocalGenerationRuntime>,
    runtime_manager: &RuntimeManager,
    operation_id: Uuid,
    cancelled: &AtomicBool,
) -> (Option<AnswerResult>, Option<AppError>) {
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
    runtime_request.idempotency_key = Some(format!("ask-eval:{operation_id}"));
    let runtime_lease = match runtime_manager.acquire(runtime_request) {
        Ok(lease) => lease,
        Err(error) => return (None, Some(error)),
    };
    let phase = |_name: &str, _progress: f64| {};
    let verified_claim = |_claim: &AnswerClaim| {};
    match compute_answer(
        request,
        catalog,
        models,
        worker,
        generation,
        runtime_manager,
        operation_id,
        // 评估 runner 是开发诊断工具：固定按「记忆开启」基线运行，
        // 不读取用户设置，保证评估口径与历史结果可比。
        true,
        cancelled,
        (&phase, &verified_claim),
    ) {
        Ok(answer) => {
            runtime_lease.complete();
            (Some(answer), None)
        }
        Err(error) => {
            if error.code != "OPERATION_CANCELLED" {
                let _ = catalog.record_ask_failure(request, &error);
            }
            runtime_lease.fail(&error.code);
            (None, Some(error))
        }
    }
}

/// Document Profile Inspector（Phase 3）：单个文件的画像详情 + 向量在场状态。
#[tauri::command(async)]
pub fn document_profile_inspect(
    file_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<DocumentProfileInspect, AppError> {
    let file_id = Uuid::parse_str(&file_id)
        .map_err(|_| AppError::new("FILE_ID_INVALID", "文件标识无效", false))?;
    let catalog = catalog.get()?;
    let profile = catalog.get_document_profile(file_id)?;
    let embedding_present = catalog.profile_vector(&file_id)?.is_some();
    let display_name = profile
        .as_ref()
        .map(|profile| profile.title.clone())
        .unwrap_or_else(|| file_id.to_string());
    Ok(DocumentProfileInspect {
        file_id: file_id.to_string(),
        display_name,
        profile,
        embedding_present,
    })
}

/// 画像重建（单文件或全部）。不强制重新生成 Chunk Embedding——只有当前
/// revision 的 chunk 已全量嵌入的文件会被重建，其余如实计入 skipped。
#[tauri::command(async)]
pub fn document_profile_rebuild(
    request: DocumentProfileRebuildRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
) -> Result<ProfileRefreshResult, AppError> {
    fanfan_core::validate_rebuild_confirmation(&request.confirmation)?;
    let catalog = catalog.get()?;
    let artifact = models
        .get()?
        .active_artifact(ModelRole::Embedding)?
        .ok_or_else(|| {
            AppError::new(
                "RAG_EMBEDDING_MODEL_REQUIRED",
                "重建画像需要先配置并通过自检的中文 Embedding 模型",
                false,
            )
        })?;
    let file_ids = request
        .file_ids
        .map(|ids| {
            ids.iter()
                .map(|id| {
                    Uuid::parse_str(id)
                        .map_err(|_| AppError::new("FILE_ID_INVALID", "文件标识无效", false))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?;
    catalog.rebuild_document_profiles(&artifact.artifact_id.to_string(), file_ids.as_deref())
}

/// Memory Inspector（Phase 3，最小实现）：三张记忆表 + 可选关键字过滤
///（alias / entity 名 / predicate / file_id / entity_id）。
#[tauri::command(async)]
pub fn memory_inspector_query(
    search: Option<String>,
    catalog: State<'_, CatalogServiceState>,
) -> Result<MemoryInspectorView, AppError> {
    let catalog = catalog.get()?;
    let aliases = catalog.list_memory_aliases(500)?;
    let relations = catalog.list_memory_relations(None, 2000)?;
    let entities = catalog.list_memory_entities(2000)?;
    let Some(keyword) = search
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
    else {
        return Ok(MemoryInspectorView {
            aliases,
            relations,
            entities,
        });
    };
    let matches_text = |text: &str| text.to_lowercase().contains(&keyword);
    let matches_uuid = |id: Uuid| id.to_string().contains(&keyword);
    Ok(MemoryInspectorView {
        aliases: aliases
            .into_iter()
            .filter(|alias| matches_text(&alias.alias) || matches_uuid(alias.target_id))
            .collect(),
        relations: relations
            .into_iter()
            .filter(|relation| {
                matches_text(&relation.predicate)
                    || matches_uuid(relation.subject_id)
                    || matches_uuid(relation.object_id)
            })
            .collect(),
        entities: entities
            .into_iter()
            .filter(|entity| {
                matches_text(&entity.canonical_name)
                    || matches_text(&entity.entity_type)
                    || matches_uuid(entity.entity_id)
            })
            .collect(),
    })
}

/// Memory 关系 confirm / reject（只改 status，不动数据）。
#[tauri::command(async)]
pub fn memory_relation_set_status(
    request: MemoryRelationStatusRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let relation_id = Uuid::parse_str(&request.relation_id)
        .map_err(|_| AppError::new("RELATION_ID_INVALID", "关系标识无效", false))?;
    let status = match request.status.as_str() {
        "confirmed" => MemoryStatus::Confirmed,
        "rejected" => MemoryStatus::Rejected,
        _ => {
            return Err(AppError::new(
                "MEMORY_STATUS_INVALID",
                "只允许 confirmed 或 rejected",
                false,
            ));
        }
    };
    catalog
        .get()?
        .update_memory_relation_status(relation_id, status)
}

#[tauri::command(async)]
pub fn memory_alias_delete(
    alias_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let alias_id = Uuid::parse_str(&alias_id)
        .map_err(|_| AppError::new("ALIAS_ID_INVALID", "别名标识无效", false))?;
    catalog.get()?.delete_memory_alias(alias_id)
}

#[tauri::command(async)]
pub fn memory_entity_delete(
    entity_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let entity_id = Uuid::parse_str(&entity_id)
        .map_err(|_| AppError::new("ENTITY_ID_INVALID", "实体标识无效", false))?;
    catalog.get()?.delete_memory_entity(entity_id)
}

#[tauri::command(async)]
pub fn memory_relation_delete(
    relation_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let relation_id = Uuid::parse_str(&relation_id)
        .map_err(|_| AppError::new("RELATION_ID_INVALID", "关系标识无效", false))?;
    catalog.get()?.delete_memory_relation(relation_id)
}

/// 清空全部记忆（aliases / relations / entities 三表；二次确认短语校验）。
/// 不动文件 / 索引 / Embedding / Ask 历史。
#[tauri::command(async)]
pub fn memory_clear(
    request: MemoryClearRequest,
    catalog: State<'_, CatalogServiceState>,
) -> Result<u64, AppError> {
    if request.confirmation != "CLEAR_MEMORY" {
        return Err(AppError::new(
            "MEMORY_CLEAR_CONFIRMATION_REQUIRED",
            "清空记忆需要明确确认",
            false,
        ));
    }
    catalog.get()?.clear_memory()
}

/// 清空当前会话的 Ask Session Context（测试有/无 Memory × 有/无 Context 四态）。
#[tauri::command(async)]
pub fn ask_session_context_clear(
    session_id: String,
    catalog: State<'_, CatalogServiceState>,
) -> Result<bool, AppError> {
    let session_id = Uuid::parse_str(&session_id)
        .map_err(|_| AppError::new("SESSION_ID_INVALID", "会话标识无效", false))?;
    catalog
        .get()?
        .clear_ask_session_context(session_id)
        .map(|_| true)
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
    request: SearchRequest,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    worker: State<'_, WorkerServiceState>,
    sidecars: State<'_, SidecarRegistryState>,
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
    // 操作级追踪：SEARCH 链路入口（后续节点通过线程关联自动写入 operation_id）。
    let operation_trace = ActiveOperationTrace::begin(
        &catalog,
        &correlation_id,
        None,
        TraceFeatureType::Search,
        &json!({
            "query": request.query,
            "mode": format!("{:?}", request.mode),
            "page_size": request.page_size,
            "has_cursor": request.cursor.is_some(),
            "scope": json!({
                "roots": request.scope.root_ids.len(),
                "collections": request.scope.collection_ids.len(),
                "files": request.scope.file_ids.len(),
                "extensions": request.scope.extensions,
            }),
        }),
        None,
    );
    if matches!(request.mode, SearchMode::Hybrid)
        && let Some(artifact) = models.active_artifact(ModelRole::Embedding)?
    {
        let cache_key = SearchEmbeddingKey {
            model_artifact_id: artifact.artifact_id.to_string(),
            query: request.query.clone(),
        };
        // 命中缓存直接复用查询向量，跳过跨进程编码与资源租约；
        // 未命中才 acquire + 编码，成功后写入缓存供重复搜索复用。
        let query_vector = worker
            .search_embedding_cache
            .lock()
            .ok()
            .and_then(|mut cache| cache.get(&cache_key))
            .or_else(|| {
                let mut runtime_request = RuntimeTaskRequest::interactive(
                    RuntimeTaskKind::Search,
                    RuntimeBackendKind::OnnxRuntime,
                );
                runtime_request.cpu_threads = 2;
                // 语义通道只是增强，失败应立即降级回 filename+fulltext；acquire 等 5
                // 秒会让非语义搜索白白多等 5 秒（实测语义降级搜索 7s ≈ 5s acquire +
                // 检索）。压到 300ms：资源就绪就编码，否则立刻走无语义路径。
                runtime_request.timeout = Duration::from_millis(300);
                let lease = runtime_manager.0.acquire(runtime_request).ok()?;
                let tokenizer_path = PathBuf::from(&artifact.local_path)
                    .parent()
                    .map(|parent| parent.join("tokenizer.json"));
                let Some(tokenizer_path) = tokenizer_path else {
                    lease.complete();
                    return None;
                };
                let Ok(response) = sidecars.0.onnx.encode_embeddings(&EmbeddingRequest {
                    model_path: artifact.local_path,
                    tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
                    texts: vec![request.query.clone()],
                    max_length: 512,
                    threads: 2,
                }) else {
                    lease.complete();
                    return None;
                };
                let Some(vector) = response.vectors.first() else {
                    lease.complete();
                    return None;
                };
                lease.complete();
                let vector = vector.clone();
                if let Ok(mut cache) = worker.search_embedding_cache.lock() {
                    cache.put(cache_key.clone(), vector.clone());
                }
                Some(vector)
            });
        if let Some(vector) = query_vector {
            let result = catalog.search_with_semantic(
                &request,
                Some(SemanticQuery {
                    model_artifact_id: &artifact.artifact_id.to_string(),
                    vector: &vector,
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
            operation_trace.complete(&catalog, "ok");
            return Ok(result);
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
    operation_trace.complete(&catalog, "ok");
    Ok(result)
}

#[tauri::command(async)]
pub fn ask_start(
    mut request: AskRequest,
    app: AppHandle,
    catalog: State<'_, CatalogServiceState>,
    models: State<'_, ModelServiceState>,
    sidecars: State<'_, SidecarRegistryState>,
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
    let ask_worker = sidecars.0.onnx.isolated();
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
        // 操作级追踪：ASK 链路入口（节点 trace 的 correlation_id 使用协调器
        // operation_id，这里保持一致，完成态在下方各出口标记）。
        let operation_trace = ActiveOperationTrace::begin(
            &catalog,
            &operation_id.to_string(),
            request.session_id.map(|id| id.to_string()).as_deref(),
            TraceFeatureType::Ask,
            &json!({
                "question": request.question,
                "answer_style": request.answer_style,
                "strict_evidence": request.strict_evidence,
                "retrieval_limit": request.retrieval_limit,
                "max_source_files": request.max_source_files,
                "scope": json!({
                    "roots": request.scope.root_ids.len(),
                    "collections": request.scope.collection_ids.len(),
                    "files": request.scope.file_ids.len(),
                }),
            }),
            None,
        );
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
            memory_feature_enabled(&app),
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
            operation_trace.complete(&catalog, "cancelled");
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
                    operation_trace.complete(&catalog, "cancelled");
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
                // 异步 Memory Candidate Writer（Step 6）：问答结束后在后台尝试
                // 提取用户明确表达的关系/别名候选。全程失败静默，绝不影响已
                // 完成的回答；STRICT：只写 model_inference + candidate。
                // 「使用记忆」关闭时跳过（spec 三十八：不新增长期 Memory）。
                if memory_feature_enabled(&app) {
                    let catalog = Arc::clone(&catalog);
                    let models = Arc::clone(&models);
                    let generation = Arc::clone(&generation);
                    let question = request.question.clone();
                    let session_id = request.session_id;
                    let answer = answer.clone();
                    std::thread::spawn(move || {
                        run_memory_candidate_writer(
                            &catalog,
                            &models,
                            &generation,
                            &question,
                            session_id,
                            &answer,
                            operation_id,
                        );
                    });
                }
                operation_trace.complete(&catalog, "ok");
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
                operation_trace.complete(&catalog, "error");
                runtime_lease.fail(error.code.clone());
            }
        }
    });
    Ok(handle)
}

/// OperationTrace 的 RAII 守卫：入口创建 operation_traces 记录并设置
/// 线程关联，显式 `complete` 写完成态；Drop 时兜底清理线程关联。
/// 记录失败静默，绝不影响主链路（与既有 Trace 纪律一致）。
struct ActiveOperationTrace {
    operation_id: Option<String>,
}

impl ActiveOperationTrace {
    /// 新建一条 OperationTrace（status=running）并绑定当前线程。
    fn begin(
        catalog: &CatalogService,
        correlation_id: &str,
        session_id: Option<&str>,
        feature_type: TraceFeatureType,
        request: &Value,
        preset_id: Option<&str>,
    ) -> Self {
        let input = OperationTraceInput {
            correlation_id: correlation_id.to_string(),
            session_id: session_id.map(str::to_string),
            feature_type,
            request: request.clone(),
            preset_id: preset_id.map(str::to_string),
        };
        let operation_id = catalog.record_operation_trace(&input).ok();
        if let Some(operation_id) = &operation_id {
            fanfan_core::set_active_operation_trace(Some(operation_id.clone()));
        }
        ActiveOperationTrace { operation_id }
    }

    /// 标记完成态并解除线程关联（幂等；重复调用仅首次生效）。
    fn complete(mut self, catalog: &CatalogService, status: &str) {
        if let Some(operation_id) = self.operation_id.take() {
            let _ = catalog.complete_operation_trace(&operation_id, status);
        }
        fanfan_core::set_active_operation_trace(None);
    }
}

impl Drop for ActiveOperationTrace {
    fn drop(&mut self) {
        // 未显式 complete（错误/提前返回）时仅解除线程关联，避免污染后续请求。
        fanfan_core::set_active_operation_trace(None);
    }
}

/// 节点追踪：一条链路节点的输入输出快照，明文落库（失败静默，绝不影响主链路）。
/// 若当前线程绑定了 OperationTrace，则自动把 operation_id 写入节点记录。
#[allow(clippy::too_many_arguments)]
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
    let operation_id = fanfan_core::active_operation_trace();
    let input = truncate_for_trace(input_json);
    let output = truncate_for_trace(output_json);
    let trace = TraceNodeInput {
        flow: flow.to_owned(),
        node: node.to_owned(),
        correlation_id: correlation_id.to_owned(),
        session_id: session_id.map(str::to_owned),
        entity_id: entity_id.map(str::to_owned),
        input_json: input,
        output_json: output,
        status: status.to_owned(),
        elapsed_ms,
        meta: TraceNodeMeta {
            operation_id,
            ..TraceNodeMeta::default()
        },
    };
    let _ = catalog.record_node_trace(&trace);
}

/// operation_execution 精简节点：记录「本轮问答实际走了哪条管线」
/// （chat 侧的入口分支；检索侧在 finish_retrieval_with_plan 内记录）。
fn trace_operation_execution(
    catalog: &CatalogService,
    correlation_id: &str,
    session_id: Option<&str>,
    reason: &str,
    pipeline: &str,
) {
    trace_node(
        catalog,
        "ask",
        "operation_execution",
        correlation_id,
        session_id,
        None,
        &json!({ "reason": reason }),
        &json!({ "pipeline": pipeline }),
        "ok",
        None,
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
            Value::Object(map) => map.values_mut().for_each(|item| cap_strings(item, limit)),
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
/// 问答结束后的异步 Memory Candidate Writer（Step 5/6）。
///
/// 铁律：
/// - **禁止所有问答自动写 Memory**——只有用户明确表达/确认/起别名等事件，
///   由模型从本轮问答中判定（`should_write`），普通问答不产生任何记忆；
/// - STRICT：所有写入强制 `source = model_inference` + `status = candidate`，
///   **绝不自动确认**（确认只能来自用户显式操作，见澄清选择/确认链路）；
/// - 写入前对每个目标再次执行存储层合法性检查（文件在场 + 授权根）；
/// - 任何失败只记日志，绝不影响问答主链路、绝不上抛。
fn run_memory_candidate_writer(
    catalog: &Arc<CatalogService>,
    models: &Arc<ModelManager>,
    generation: &Arc<Mutex<LocalGenerationRuntime>>,
    question: &str,
    session_id: Option<Uuid>,
    answer: &AnswerResult,
    operation_id: Uuid,
) {
    let outcome = (|| -> Result<u64, AppError> {
        let Some(artifact) = models.active_artifact(ModelRole::Generation)? else {
            return Ok(0);
        };
        // 1. 注册表：本轮涉及的文件（id + 真名）+ 已知实体。
        //    无明确涉及文件时不写——目标无法确定性验证。
        let entities = catalog.list_memory_entities(2000)?;
        let used_ids: HashSet<Uuid> = answer
            .used_file_ids
            .iter()
            .chain(answer.source_files.iter().map(|source| &source.file_id))
            .copied()
            .collect();
        let profiles = catalog.list_document_profiles(None, 2000)?;
        let involved: Vec<(Uuid, String)> = profiles
            .into_iter()
            .map(|(profile, name)| (profile.file_id, name))
            .filter(|(file_id, _)| used_ids.contains(file_id))
            .collect();
        // 收藏集也是合法目标（别名可指向收藏集，见 resolve_name 实体→文件→收藏集）
        let collections = catalog
            .list_collections()?
            .into_iter()
            .map(|collection| (collection.collection_id, collection.name))
            .collect::<Vec<_>>();
        let registry = MemoryTargetRegistry {
            files: involved
                .iter()
                .map(|(file_id, name)| (*file_id, name.as_str()))
                .collect(),
            entities,
            collections: collections
                .iter()
                .map(|(collection_id, name)| (*collection_id, name.as_str()))
                .collect(),
        };
        if registry.files.is_empty() {
            return Ok(0);
        }
        // 2. Writer 判定（结构化 JSON；解析失败按「不写」处理）
        let history = session_id
            .map(|session_id| catalog.load_ask_history(&session_id, 20))
            .transpose()?
            .unwrap_or_default();
        let active_files = registry
            .files
            .iter()
            .map(|(_, name)| (*name).to_owned())
            .collect::<Vec<_>>();
        let known_entities = registry
            .entities
            .iter()
            .map(|entity| entity.canonical_name.clone())
            .collect::<Vec<_>>();
        let known_collections = collections
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        let context = MemoryWriterContext {
            question,
            answer: &answer.answer,
            history: &history,
            active_files: &active_files,
            known_entities: &known_entities,
            known_collections: &known_collections,
        };
        let (system, user) = memory_writer_prompt(&context);
        let raw = complete_json_with_model(
            generation,
            &artifact,
            &system,
            &user,
            200,
            &memory_writer_schema(),
            &AtomicBool::new(false),
        )?;
        let Some(output) = parse_writer_output(&raw) else {
            return Ok(0);
        };
        // 3. 确定性预写验证 → 名字解析 → 写入（先过合法性复核）
        let validated = prewrite_validate(output);
        let validated_count = validated.len();
        let writes = resolve_proposal_targets(validated, &registry);
        let mut written = 0_u64;
        let mut rejected = 0_u64;
        for write in &writes {
            let target_valid = memory_target_legality_valid(catalog, write)?;
            if !target_valid {
                rejected += 1;
                continue;
            }
            match write.kind {
                MemoryKind::Relation => {
                    catalog.upsert_memory_relation(write)?;
                }
                MemoryKind::Alias => {
                    catalog.upsert_memory_alias(write)?;
                }
            }
            written += 1;
        }
        trace_node(
            catalog,
            "ask",
            "memory_candidate_write",
            &operation_id.to_string(),
            session_id.map(|id| id.to_string()).as_deref(),
            None,
            &json!({
                "question": question,
                "involved_files": active_files,
            }),
            &json!({
                "writer_raw": raw,
                "validated_proposal_count": validated_count,
                "write_attempt_count": writes.len(),
                "written": written,
                "rejected_by_legality": rejected,
            }),
            "ok",
            None,
        );
        Ok(written)
    })();
    match outcome {
        Ok(written) if written > 0 => crate::runtime_log::event(
            "info",
            "rag",
            "memory.candidates_written",
            Some(&operation_id.to_string()),
            &json!({ "operation_id": operation_id, "written": written }),
        ),
        Ok(_) => {}
        Err(error) => crate::runtime_log::event(
            "warning",
            "rag",
            "memory.writer_failed",
            Some(&operation_id.to_string()),
            &json!({ "operation_id": operation_id, "error_code": error.code }),
        ),
    }
}

/// Memory 写入前合法性复核（确定性门）：文件目标必须在场且位于授权根，
/// 收藏集必须存在；实体恒有效。绝不放行悬空/越权目标。
fn memory_target_legality_valid(
    catalog: &CatalogService,
    write: &MemoryWriteInput,
) -> Result<bool, AppError> {
    let valid_target = |target_type: MemoryTargetType, target_id: Uuid| -> Result<bool, AppError> {
        match target_type {
            MemoryTargetType::File => catalog.memory_file_target_valid(target_id),
            MemoryTargetType::Collection => catalog.memory_collection_target_valid(target_id),
            MemoryTargetType::Entity => Ok(true),
        }
    };
    Ok(valid_target(write.subject_type, write.subject_id)?
        && valid_target(write.object_type, write.object_id)?)
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
    memory_enabled: bool,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
) -> Result<AnswerResult, AppError> {
    let (phase, verified_claim) = progress;
    if cancelled.load(Ordering::Acquire) {
        return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
    }
    phase("source_routing", 0.05);
    let generation_artifact = models.active_artifact(ModelRole::Generation)?;
    let maintenance = catalog.maintenance_snapshot()?;
    let generation_artifact = generation_artifact.ok_or_else(|| {
        AppError::new(
            "RAG_GENERATION_MODEL_REQUIRED",
            "问资料需要先配置并通过自检的本地生成模型",
            false,
        )
    })?;
    // 路由前加载一次会话历史（最多 20 条，覆盖路由/解析/闲聊/检索生成的 5+5 需要），
    // 各环节共用同一份；当前轮次问答在 record_ask_exchange 之后才落库，不含本轮。
    let history = request
        .session_id
        .map(|session_id| catalog.load_ask_history(&session_id, 20))
        .transpose()?
        .unwrap_or_default();
    // 会话工作上下文（Context/Document Resolver 的输入；读取失败按空处理，
    // Memory 层出错不得阻断问答）。
    let session_context: AskSessionContext = request
        .session_id
        .map(|session_id| catalog.get_ask_session_context(session_id))
        .transpose()?
        .flatten()
        .unwrap_or_default();
    let session_id = request.session_id.map(|id| id.to_string());
    let session_id_ref = session_id.as_deref();

    // Step 7：用户在 NEED_CLARIFICATION 中选定了目标文件 → 跳过路由，
    // 写 USER_SELECTION 记忆并锁定 scope 继续原问题。
    if let Some(selection) = request.clarification_selection {
        return run_clarified_answer(
            request,
            catalog,
            models,
            worker,
            runtime_manager,
            generation,
            &generation_artifact,
            maintenance,
            &history,
            &session_context,
            selection,
            operation_id,
            memory_enabled,
            cancelled,
            progress,
        );
    }

    // 1. Source Router（LOCAL / GENERAL / AMBIGUOUS）
    // AI 优先：来源判断完全交给 LLM 语义理解，不再用规则表抢先判定
    // （personal_reference_hit / existence_query_hit 的 Router 前置分支已移除）。
    // 生成模型已启用 enable_thinking=false，JSON 输出稳定；这里做一次重试兜底：
    // 首次解析失败（JSON 截断/噪声）时重试一次，仍失败才走诚实澄清兜底
    // （不猜意图、不进自由闲聊，避免幻觉）。
    let routing_started = Instant::now();
    let question = request.question.trim();
    let mut routing: Option<SourceRouting> = None;
    let mut routing_raw = String::new();
    for attempt in 0..2 {
        let (system, user) = source_router_prompt(question, &history);
        match complete_json_with_model(
            generation,
            &generation_artifact,
            &system,
            &user,
            128,
            &source_routing_schema(),
            cancelled,
        ) {
            Ok(raw) => {
                routing_raw = raw.clone();
                routing = parse_source_routing(&raw);
                if routing.is_some() {
                    break;
                }
            }
            Err(error) if attempt == 1 => return Err(error),
            Err(_) => {}
        }
    }
    trace_node(
        catalog,
        "ask",
        "source_routing",
        &operation_id.to_string(),
        session_id_ref,
        None,
        &json!({ "question": request.question }),
        &json!({
            "source": routing.map(|value| value.source.as_str()).unwrap_or("parse_failed"),
            "confidence": routing.map(|value| value.confidence).unwrap_or(0.0),
            "routing_ok": routing.is_some(),
            "routing_raw": routing_raw,
        }),
        "ok",
        Some(routing_started.elapsed().as_millis() as u64),
    );
    let Some(routing) = routing else {
        // 路由重试后仍无法解析 → 诚实澄清兜底：不猜意图（不做宽检索猜答案、
        // 不进自由闲聊产生幻觉），明确请用户澄清是查资料还是普通聊天。
        trace_operation_execution(
            catalog,
            &operation_id.to_string(),
            session_id_ref,
            "routing_parse_failed",
            "clarify",
        );
        return run_clarification_refusal(request, catalog, operation_id, phase);
    };
    if routing.source == SourceIntent::General {
        trace_operation_execution(
            catalog,
            &operation_id.to_string(),
            session_id_ref,
            "general",
            "chat",
        );
        return run_chat_answer(
            request,
            catalog,
            generation,
            &generation_artifact,
            &maintenance,
            &history,
            operation_id,
            cancelled,
            phase,
        );
    }

    // 2. AMBIGUOUS → Context Resolver（结合会话上下文恢复目标；恢复失败兜底闲聊）
    let mut context_scope: Vec<uuid::Uuid> = Vec::new();
    if routing.source == SourceIntent::Ambiguous {
        let context_started = Instant::now();
        let context_resolution = resolve_ambiguous(&session_context);
        trace_node(
            catalog,
            "ask",
            "context_resolution",
            &operation_id.to_string(),
            session_id_ref,
            None,
            &json!({ "question": request.question, "session_has_active_file": session_context.active_file_id.is_some() }),
            &json!({
                "status": context_resolution.status.as_str(),
                "source": context_resolution.source.as_str(),
                "intent": context_resolution.intent.as_str(),
                "resolved_file_ids": context_resolution.resolved_file_ids,
                "resolved_document_type": context_resolution.resolved_document_type.map(|value| value.as_str()),
                "signal": context_resolution.signal,
                "confidence": context_resolution.confidence,
                "fallback_reason": context_resolution.fallback_reason,
            }),
            "ok",
            Some(context_started.elapsed().as_millis() as u64),
        );
        if !context_resolution.is_resolved() {
            // P0 安全兜底：不猜文件、不假装 LOCAL，走闲聊（会如实说明没看懂指代）
            trace_operation_execution(
                catalog,
                &operation_id.to_string(),
                session_id_ref,
                "ambiguous_unresolved",
                "chat",
            );
            return run_chat_answer(
                request,
                catalog,
                generation,
                &generation_artifact,
                &maintenance,
                &history,
                operation_id,
                cancelled,
                phase,
            );
        }
        context_scope = context_resolution.resolved_file_ids.clone();
        // 只有文档类型可恢复时：把类型并入 target，交给 Document Resolver 按类型找
        if context_scope.is_empty()
            && let Some(document_type) = context_resolution.resolved_document_type
        {
            let Some(mut plan) = parse_ask_plan(
                request,
                catalog,
                generation,
                &generation_artifact,
                &history,
                operation_id,
                session_id_ref,
                cancelled,
                phase,
            )?
            else {
                // 解析失败：按类型宽检索（不锁文件，退化为全库）
                return run_retrieval_answer(
                    request,
                    request.question.trim(),
                    catalog,
                    models,
                    worker,
                    runtime_manager,
                    generation,
                    generation_artifact,
                    embedding_for_retrieval(models)?,
                    maintenance,
                    &history,
                    operation_id,
                    cancelled,
                    (phase, verified_claim),
                    None,
                    None,
                    false,
                    false,
                );
            };
            plan.source = SourceIntent::Local;
            plan.requires_document_resolution = true;
            plan.target.document_type = plan.target.document_type.or(Some(document_type));
            return finish_retrieval_with_plan(
                request,
                catalog,
                models,
                worker,
                runtime_manager,
                generation,
                generation_artifact,
                embedding_for_retrieval(models)?,
                maintenance,
                &history,
                &session_context,
                plan,
                context_scope,
                operation_id,
                memory_enabled,
                cancelled,
                (phase, verified_claim),
            );
        }
    }

    // 3. Query Parser（LOCAL 或已恢复的 AMBIGUOUS 都要先结构化）
    let plan = parse_ask_plan(
        request,
        catalog,
        generation,
        &generation_artifact,
        &history,
        operation_id,
        session_id_ref,
        cancelled,
        phase,
    )?;
    let Some(mut plan) = plan else {
        // 解析失败回退：原问题在（可能的）上下文 scope 内检索，不劣化现状
        let mut scoped = request.clone();
        if !context_scope.is_empty() {
            scoped.scope.file_ids = context_scope.clone();
        }
        return run_retrieval_answer(
            &scoped,
            request.question.trim(),
            catalog,
            models,
            worker,
            runtime_manager,
            generation,
            generation_artifact,
            embedding_for_retrieval(models)?,
            maintenance,
            &history,
            operation_id,
            cancelled,
            (phase, verified_claim),
            None,
            None,
            false,
            false,
        );
    };
    if routing.source == SourceIntent::Ambiguous && !context_scope.is_empty() {
        plan.source = SourceIntent::Local;
        // 会话上下文已锁定文件：目标对象即上下文恢复出的文件，跳过 Document Resolver
        plan.requires_document_resolution = false;
        plan.target.document_type = plan
            .target
            .document_type
            .or(session_context.active_document_type);
    }
    // 解析器兜底（CASE 1）：Router 判 local 但 Parser 输出 general_chat →
    // 尊重 Parser 的闲聊判定直接聊天（寒暄/身份问题已由 fast-path 拦截，
    // 这里兜 0.6B 双模型叠加错误；闲聊绝不进检索管线）。
    if plan.intent == QueryIntent::GeneralChat {
        trace_operation_execution(
            catalog,
            &operation_id.to_string(),
            session_id_ref,
            "parser_general_chat",
            "chat",
        );
        return run_chat_answer(
            request,
            catalog,
            generation,
            &generation_artifact,
            &maintenance,
            &history,
            operation_id,
            cancelled,
            phase,
        );
    }
    finish_retrieval_with_plan(
        request,
        catalog,
        models,
        worker,
        runtime_manager,
        generation,
        generation_artifact,
        embedding_for_retrieval(models)?,
        maintenance,
        &history,
        &session_context,
        plan,
        context_scope,
        operation_id,
        memory_enabled,
        cancelled,
        (phase, verified_claim),
    )
}

/// 澄清选择继续（Step 7）：用户在 NEED_CLARIFICATION 中选定目标文件后，
/// 带原问题重跑——先过合法性检查（与 Document Resolver 同口径），
/// 写 USER_SELECTION 别名记忆（用户明确选中 = 高信任信号），锁定 scope，
/// 再重新解析 content_query 完成检索。
#[allow(clippy::too_many_arguments)]
fn run_clarified_answer(
    request: &AskRequest,
    catalog: &CatalogService,
    models: &ModelManager,
    worker: &WorkerClient,
    runtime_manager: &RuntimeManager,
    generation: &Mutex<LocalGenerationRuntime>,
    generation_artifact: &ModelArtifact,
    maintenance: MaintenanceSnapshot,
    history: &[AskMessage],
    session_context: &AskSessionContext,
    selection: Uuid,
    operation_id: Uuid,
    memory_enabled: bool,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
) -> Result<AnswerResult, AppError> {
    let (phase, verified_claim) = progress;
    if !catalog.memory_file_target_valid(selection)? {
        return Err(AppError::new(
            "CLARIFICATION_SELECTION_INVALID",
            "所选文件已不可用（已删除/离线/越权），请重新提问",
            false,
        ));
    }
    // USER_SELECTION 记忆：待澄清引用（如「我的简历」）→ 别名指向所选文件。
    // 用户明确选中 = 用户选择来源（rank 4），但严格不自动确认其他推断；
    // 引用不适合做别名（过长/问句）则不写。「使用记忆」关闭时不写长期
    // Memory（spec 三十八），但下方 Session Context 锁定始终生效——关闭
    // 长期记忆不影响当前会话连续追问。
    let mut selection_memory_written = false;
    if memory_enabled
        && let Some(reference) = session_context
            .pending_clarification_reference
            .as_deref()
            .filter(|reference| is_alias_writable_reference(reference))
        && let Some(alias) = fanfan_core::normalize_alias(reference)
    {
        let _ = catalog.upsert_memory_alias(&MemoryWriteInput {
            kind: MemoryKind::Alias,
            subject_type: MemoryTargetType::File,
            subject_id: selection,
            predicate: "alias".to_owned(),
            object_type: MemoryTargetType::File,
            object_id: selection,
            alias: Some(alias),
            confidence: 0.95,
            source_type: MemorySource::UserSelection,
            source_id: Some(format!("clarification:{operation_id}")),
            status: MemoryStatus::Confirmed,
        });
        selection_memory_written = true;
    }
    // 会话上下文锁定所选文件，清除待澄清引用（选择即已消费）
    let mut updated_context = session_context.clone();
    updated_context.active_file_id = Some(selection);
    updated_context.active_file_ids = vec![selection];
    updated_context.pending_clarification_reference = None;
    updated_context.last_referenced_file_ids = vec![selection];
    updated_context.updated_at = Some(Utc::now());
    if let Some(session_id) = request.session_id {
        let _ = catalog.update_ask_session_context(session_id, &updated_context);
    }
    trace_node(
        catalog,
        "ask",
        "clarification_selection",
        &operation_id.to_string(),
        request.session_id.map(|id| id.to_string()).as_deref(),
        None,
        &json!({
            "question": request.question,
            "selection": selection,
            "pending_reference": session_context.pending_clarification_reference,
        }),
        &json!({
            "selection_valid": true,
            "memory_written": selection_memory_written,
            "locked_scope": vec![selection],
        }),
        "ok",
        None,
    );
    // 重新解析 content_query（目标已锁定，跳过 Document Resolver 与路由）
    let parsed = parse_ask_plan(
        request,
        catalog,
        generation,
        generation_artifact,
        history,
        operation_id,
        request.session_id.map(|id| id.to_string()).as_deref(),
        cancelled,
        phase,
    )?;
    let mut plan = parsed.unwrap_or_default();
    plan.source = SourceIntent::Local;
    plan.requires_document_resolution = false;
    finish_retrieval_with_plan(
        request,
        catalog,
        models,
        worker,
        runtime_manager,
        generation,
        generation_artifact.clone(),
        embedding_for_retrieval(models)?,
        maintenance,
        history,
        &updated_context,
        plan,
        vec![selection],
        operation_id,
        memory_enabled,
        cancelled,
        (phase, verified_claim),
    )
}

/// 待澄清引用是否适合写成别名（Step 7）：短名词短语、不是问句。
/// 长句/问句整段写别名只会污染记忆（如「我资料里有没有提到法律项目」）。
fn is_alias_writable_reference(reference: &str) -> bool {
    let trimmed = reference.trim();
    let length = trimmed.chars().count();
    (2..=20).contains(&length)
        && !trimmed.ends_with('？')
        && !trimmed.ends_with('?')
        && !trimmed.ends_with('吗')
        && !trimmed.ends_with('呢')
        && !trimmed.contains("什么")
        && !trimmed.contains("怎么")
        && !trimmed.contains("哪些")
        && !trimmed.contains("有没有")
}

/// Step 7 澄清触发条件：Memory 未能消歧 + 存在多个非常接近的候选 + 有可选候选。
/// 注意 MultipleCandidates 的 resolved_file_ids 是 top-2/3（非空），scope 非空
/// 不代表已经「锁定」，仍须回问用户——不能让分支条件与 resolver 输出相悖。
fn should_ask_clarification(
    memory_resolution_ok: bool,
    resolution_status: Option<ResolutionStatus>,
    has_candidates: bool,
) -> bool {
    !memory_resolution_ok
        && resolution_status == Some(ResolutionStatus::MultipleCandidates)
        && has_candidates
}

/// Embedding 只被检索分支需要：闲聊在缺 Embedding 时正常工作。
fn embedding_for_retrieval(models: &ModelManager) -> Result<Option<ModelArtifact>, AppError> {
    models.active_artifact(ModelRole::Embedding)
}

/// Query Parser 步骤：结构化 LOCAL（或已恢复）请求。返回 None 表示解析失败
/// （调用方按原问题回退检索，不中断问答）。
#[allow(clippy::too_many_arguments)]
fn parse_ask_plan(
    request: &AskRequest,
    catalog: &CatalogService,
    generation: &Mutex<LocalGenerationRuntime>,
    generation_artifact: &ModelArtifact,
    history: &[AskMessage],
    operation_id: Uuid,
    session_id_ref: Option<&str>,
    cancelled: &AtomicBool,
    phase: &dyn Fn(&str, f64),
) -> Result<Option<QueryPlan>, AppError> {
    phase("query_parsing", 0.12);
    // AI 优先：意图/操作/目标分离全部交给 LLM Parser 语义理解（含
    // DOCUMENT_FIND 的判定——Prompt 已教模型区分「找文件位置」与
    // 「查文件内容」）。此处做一次重试兜底：首次解析失败（JSON 截断/
    // 噪声/复读历史）时重试一次，仍失败返回 None（调用方按原问题宽检索，
    // 不猜意图）。
    let question = request.question.trim();
    let mut plan: Option<QueryPlan> = None;
    let mut raw = String::new();
    for attempt in 0..2 {
        let (system, user) = query_parser_prompt(question, history);
        match complete_json_with_model(
            generation,
            generation_artifact,
            &system,
            &user,
            320,
            &query_parser_schema(),
            cancelled,
        ) {
            Ok(text) => {
                raw = text.clone();
                plan = parse_query_plan(&text);
                if plan.is_some() {
                    break;
                }
            }
            Err(error) if attempt == 1 => return Err(error),
            Err(_) => {}
        }
    }
    trace_node(
        catalog,
        "ask",
        "query_parsing",
        &operation_id.to_string(),
        session_id_ref,
        None,
        &json!({ "question": request.question }),
        &json!({
            "parsed": plan.is_some(),
            "plan": plan,
            "parsing_raw": raw,
        }),
        "ok",
        None,
    );
    Ok(plan)
}

/// 读取「使用记忆」总开关（Phase 4.2 spec 三十八）：设置文件缺失 / 读取
/// 失败一律按默认开启处理，不因配置异常改变既有问答行为；关闭时
/// Memory Resolver / Memory Writer 均不参与新 Ask，但已存数据不删除。
fn memory_feature_enabled(app: &AppHandle) -> bool {
    let Some(state) = app.try_state::<MemorySettingsServiceState>() else {
        return true;
    };
    state
        .0
        .lock()
        .ok()
        .and_then(|service| service.get().ok())
        .map(|settings| settings.enabled)
        .unwrap_or(true)
}

/// Memory Resolver 数据装载（Step 6）：可信来源别名 + 已确认关系 → 命中提示。
/// 任何存储读取失败都静默降级为空（Memory 层出错不得阻断问答）。
/// `memory_enabled = false`（用户在设置中关闭「使用记忆」）时直接返回空，
/// 跳过 Memory Resolver。
fn load_memory_hints(
    catalog: &CatalogService,
    question: &str,
    memory_enabled: bool,
) -> (Vec<MemoryHint>, Vec<MemoryHint>) {
    if !memory_enabled {
        return (Vec::new(), Vec::new());
    }
    let aliases = catalog.list_memory_aliases(500).unwrap_or_default();
    let entities = catalog.list_memory_entities(2000).unwrap_or_default();
    let relations = catalog
        .list_memory_relations(Some(MemoryStatus::Confirmed), 2000)
        .unwrap_or_default();
    // 只有 confirmed 别名参与定位（Phase 4.2 起以 status 为准：用户在
    // 「待确认的记忆」里确认 → confirmed；拒绝 → rejected 绝不参与；
    // 推断类别名保持 candidate 等待用户确认）。与 relations 同口径。
    let trusted_aliases = aliases
        .into_iter()
        .filter(|alias| alias.status == MemoryStatus::Confirmed)
        .collect::<Vec<_>>();
    let alias_hints = match_alias_hints(question, &trusted_aliases);
    let relation_hints = match_relation_hints(question, &entities, &relations);
    (alias_hints, relation_hints)
}

/// 路由（LLM 重试后）仍无法解析来源时的诚实兜底：明确告知未理解，请用户
/// 澄清是查本地资料还是普通聊天。**不猜意图**——不做宽检索猜答案，也不进
/// 自由闲聊产生幻觉（RAG 定义错误 / 通用模板）。answer_mode = RagRefusal。
fn run_clarification_refusal(
    request: &AskRequest,
    catalog: &CatalogService,
    operation_id: Uuid,
    phase: &dyn Fn(&str, f64),
) -> Result<AnswerResult, AppError> {
    let started_at = Instant::now();
    let message = "我暂时没能判断你的问题是想查本地资料还是普通聊天。请换个说法，或明确说明（例如「查我的资料」「随便聊聊」）。"
        .to_owned();
    let result = AnswerResult {
        session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
        message_id: Uuid::now_v7(),
        answer: message.clone(),
        grounding_status: fanfan_core::GroundingStatus::Insufficient,
        insufficient_evidence: true,
        claims: Vec::new(),
        source_files: Vec::new(),
        used_file_ids: Vec::new(),
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        answer_mode: AnswerMode::RagRefusal,
        retrieval_channels: Vec::new(),
        index_coverage: 0.0,
        degradation_reason: None,
        no_evidence_reason: Some(NoEvidenceReason::TrueNoEvidence),
        clarification: None,
    };
    let session_id = request.session_id.map(|id| id.to_string());
    trace_node(
        catalog,
        "ask",
        "routing_clarify_refusal",
        &operation_id.to_string(),
        session_id.as_deref(),
        None,
        &json!({ "question": request.question }),
        &json!({ "answer_mode": result.answer_mode, "answer": result.answer }),
        "ok",
        Some(result.elapsed_ms),
    );
    catalog.record_ask_exchange(request, &result)?;
    phase("completed", 1.0);
    Ok(result)
}

/// DOCUMENT_FIND（spec 十一.1）：只运行 Document Resolver 的结果，不跑普通
/// chunk RAG。定位到文件 → 返回文件卡片式回答（answer_mode = find，无 claims
/// ——「找到文件」本身是目标，不需要证据句）；未定位到任何候选 → LOCAL +
/// NO_EVIDENCE 固定拒绝文案，绝不转闲聊。
#[allow(clippy::too_many_arguments)]
fn run_document_find_answer(
    request: &AskRequest,
    catalog: &CatalogService,
    resolved_scope: &[Uuid],
    resolution_candidates: &[DocumentCandidate],
    resolution_file_names: &HashMap<Uuid, String>,
    operation_id: Uuid,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
) -> Result<AnswerResult, AppError> {
    let (phase, _verified_claim) = progress;
    let _ = cancelled;
    let correlation_id = operation_id.to_string();
    let session_id = request.session_id.map(|id| id.to_string());
    let session_id_ref = session_id.as_deref();
    let started = Instant::now();
    let mut file_names = resolution_file_names.clone();
    if file_names.is_empty() {
        let profiles = catalog.list_document_profiles(None, 2000)?;
        for (profile, name) in profiles {
            file_names.entry(profile.file_id).or_insert(name);
        }
    }
    let file_id = resolved_scope.first().copied().or_else(|| {
        resolution_candidates
            .first()
            .map(|candidate| candidate.file_id)
    });
    let Some(file_id) = file_id else {
        // spec 十：FIND 查的是文件不是「信息」——NOT_FOUND 明确指出没有
        // 匹配该指代的文件，绝不转 Chat Prompt。
        let reference = compact_for_prompt(request.question.trim(), 40);
        let message = format!(
            "没有找到能够匹配「{reference}」的文件。可以换一种说法（文件名、类型或内容关键词），或确认资料已完成索引。"
        );
        return finish_summary_refusal(request, catalog, operation_id, &message, started, phase);
    };
    let name = file_names
        .get(&file_id)
        .cloned()
        .unwrap_or_else(|| "未命名文件".to_owned());
    let canonical_path = catalog
        .file_preview(&file_id, 1)
        .map(|preview| preview.file.canonical_path.clone())
        .unwrap_or_default();
    let result = AnswerResult {
        session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
        message_id: Uuid::now_v7(),
        answer: format!("找到了：{name}（{canonical_path}）"),
        grounding_status: GroundingStatus::Grounded,
        insufficient_evidence: false,
        claims: Vec::new(),
        source_files: vec![AnswerSourceFile {
            file_id,
            display_name: name.clone(),
            canonical_path: canonical_path.clone(),
        }],
        used_file_ids: vec![file_id],
        elapsed_ms: started.elapsed().as_millis() as u64,
        answer_mode: AnswerMode::Find,
        retrieval_channels: vec!["document_find".into()],
        index_coverage: 0.0,
        degradation_reason: None,
        no_evidence_reason: None,
        clarification: None,
    };
    trace_node(
        catalog,
        "ask",
        "document_find",
        &correlation_id,
        session_id_ref,
        None,
        &json!({ "reference": request.question }),
        &json!({
            "file_id": file_id.to_string(),
            "name": name,
            "canonical_path": canonical_path,
        }),
        "ok",
        Some(result.elapsed_ms),
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
            "answer_mode": result.answer_mode,
            "answer": result.answer,
            "claim_count": 0,
            "grounding_status": format!("{:?}", result.grounding_status),
            "insufficient_evidence": false,
            "degradation_reason": result.degradation_reason,
        }),
        "ok",
        Some(result.elapsed_ms),
    );
    catalog.record_ask_exchange(request, &result)?;
    phase("completed", 1.0);
    Ok(result)
}

/// QueryPlan → 检索的最后一步：Document Resolver 锁定文件白名单 → 写入
/// scope.file_ids → 用 content_query 检索。
#[allow(clippy::too_many_arguments)]
fn finish_retrieval_with_plan(
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
    session_context: &AskSessionContext,
    plan: QueryPlan,
    context_scope: Vec<uuid::Uuid>,
    operation_id: Uuid,
    memory_enabled: bool,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
) -> Result<AnswerResult, AppError> {
    // 4. Document Resolver（目标对象 → file_id 白名单）
    let mut resolved_scope = context_scope;
    let mut document_resolution: Option<Value> = None;
    // Step 7：解析状态与候选提升到外部，供 NEED_CLARIFICATION 分支使用
    let mut resolution_status: Option<ResolutionStatus> = None;
    let mut resolution_candidates: Vec<DocumentCandidate> = Vec::new();
    let mut resolution_file_names: HashMap<Uuid, String> = HashMap::new();
    if plan.requires_document_resolution && resolved_scope.is_empty() {
        let resolution_started = Instant::now();
        // 分类器未运行（document_type 全 NULL）时按类型过滤会整空（CASE
        // 5/8/9 实测 1197 个画像 document_type 全部为 NULL）：回退全量画像，
        // 由 Resolver 的类型等价（TYPE_KEYWORDS）与词元信号自行定位。
        let profiles_with_names =
            catalog.list_document_profiles(plan.target.document_type, 2000)?;
        let profiles_with_names =
            if profiles_with_names.is_empty() && plan.target.document_type.is_some() {
                catalog.list_document_profiles(None, 2000)?
            } else {
                profiles_with_names
            };
        let mut file_names = HashMap::with_capacity(profiles_with_names.len());
        let profiles = profiles_with_names
            .into_iter()
            .map(|(profile, name)| {
                file_names.insert(profile.file_id, name);
                profile
            })
            .collect::<Vec<DocumentProfile>>();
        let input = ResolverInput::new(&plan, session_context, profiles, file_names);
        let resolution = resolve_documents(&input);
        resolved_scope = resolution.resolved_file_ids.clone();
        resolution_status = Some(resolution.status);
        resolution_candidates = resolution.candidates.clone();
        resolution_file_names = input.file_names.clone();
        trace_node(
            catalog,
            "ask",
            "document_resolution",
            &operation_id.to_string(),
            request.session_id.map(|id| id.to_string()).as_deref(),
            None,
            &json!({
                "target": {
                    "reference": plan.target.reference,
                    "document_type": plan.target.document_type.map(|value| value.as_str()),
                    "document_name": plan.target.document_name,
                    "owner": plan.target.owner,
                    "entity_name": plan.target.entity_name,
                },
                "candidate_count": resolution.candidates.len(),
            }),
            &json!({
                "candidates": resolution.candidates,
                "resolved_file_ids": resolution.resolved_file_ids,
                "confidence": resolution.confidence,
                "status": resolution.status.as_str(),
                "fallback_reason": resolution.fallback_reason,
            }),
            "ok",
            Some(resolution_started.elapsed().as_millis() as u64),
        );
        document_resolution = Some(json!({
            "status": resolution.status.as_str(),
            "resolved_file_ids": resolution.resolved_file_ids,
            "candidate_count": resolution.candidates.len(),
        }));
    }

    // 4.5 Memory Resolver（别名/关系 → 定位提示）。
    // 设计约束（Step 4/6）：Memory 只帮助理解和定位，绝不作为事实证据；
    // 解析出的 file_id 必须逐个通过存储层合法性检查（存在 + present + 授权根，
    // 与 Document Resolver 同口径）；只有 confirmed 关系与「可信来源」别名参与
    // 定位；命中并验证通过后**以 Memory 定位优先**（用户明确起的别名/已确认关系
    // 是比 Resolver 启发式更强的定位信号，满足「禁止把简历当全库关键词搜索」）。
    let memory_started = Instant::now();
    let (alias_hints, relation_hints) =
        load_memory_hints(catalog, request.question.trim(), memory_enabled);
    let mut memory_scope: Vec<Uuid> = Vec::new();
    let mut memory_resolution_ok = false;
    {
        for hint in alias_hints.iter().chain(relation_hints.iter()) {
            match hint.target_type {
                MemoryTargetType::File => {
                    if catalog
                        .memory_file_target_valid(hint.target_id)
                        .unwrap_or(false)
                    {
                        memory_scope.push(hint.target_id);
                    }
                }
                MemoryTargetType::Collection => {
                    if catalog
                        .memory_collection_target_valid(hint.target_id)
                        .unwrap_or(false)
                        && let Ok(files) = catalog.collection_files(&hint.target_id)
                    {
                        // 收藏集成员逐个复验：绝不放行越权/离线文件
                        for file in files {
                            if catalog
                                .memory_file_target_valid(file.file_id)
                                .unwrap_or(false)
                            {
                                memory_scope.push(file.file_id);
                            }
                        }
                    }
                }
                MemoryTargetType::Entity => {}
            }
        }
        memory_scope.sort_unstable();
        memory_scope.dedup();
        if !memory_scope.is_empty() {
            resolved_scope = memory_scope.clone();
            memory_resolution_ok = true;
        }
    }
    // 被实际采纳进 scope 的别名 → 提升使用次数（repeated_usage 升级依据；
    // 失败静默，不阻断主链路）
    if memory_resolution_ok {
        for hint in &alias_hints {
            if resolved_scope.contains(&hint.target_id)
                && let Ok(aliases) = catalog.find_memory_aliases(&hint.matched_text)
            {
                for alias in aliases
                    .into_iter()
                    .filter(|alias| alias.target_id == hint.target_id)
                {
                    let _ = catalog.bump_memory_alias(alias.alias_id);
                }
            }
        }
    }
    trace_node(
        catalog,
        "ask",
        "memory_resolution",
        &operation_id.to_string(),
        request.session_id.map(|id| id.to_string()).as_deref(),
        None,
        &json!({
            "question": request.question,
            "requires_document_resolution": plan.requires_document_resolution,
        }),
        &json!({
            "memory_enabled": memory_enabled,
            "alias_hint_count": alias_hints.len(),
            "relation_hint_count": relation_hints.len(),
            "alias_hints": alias_hints,
            "relation_hints": relation_hints,
            "valid_memory_scope": memory_scope,
            "memory_resolution_ok": memory_resolution_ok,
            "resolved_scope_after_memory": resolved_scope,
        }),
        "ok",
        Some(memory_started.elapsed().as_millis() as u64),
    );

    // 4.5.4 Step 12：operation_execution —— 记录实际执行的管线分支
    // （前端流程展示与排障依据；任何分支都从本节点出发）。
    let execution_pipeline = if plan.intent == QueryIntent::CompareDocuments {
        "document_compare"
    } else if plan.intent == QueryIntent::DocumentFind {
        "document_find"
    } else if plan.requires_full_document {
        "document_summary"
    } else {
        "chunk_rag"
    };
    trace_node(
        catalog,
        "ask",
        "operation_execution",
        &operation_id.to_string(),
        request.session_id.map(|id| id.to_string()).as_deref(),
        None,
        &json!({
            "intent": plan.intent.as_str(),
            "operation": plan.operation.as_str(),
            "requires_document_resolution": plan.requires_document_resolution,
            "requires_full_document": plan.requires_full_document,
        }),
        &json!({
            "pipeline": execution_pipeline,
            "resolved_file_count": resolved_scope.len(),
            "candidate_count": resolution_candidates.len(),
        }),
        "ok",
        None,
    );

    // 4.5.5 Step 10：COMPARE_DOCUMENTS 走两侧对比管线（spec 十一.6）。
    // 放在 Clarification 之前：比较请求的目标是「两份文档」，primary 出现
    // 多候选时不问「哪一份」——两侧取最有把握的候选，比较意图优先。
    if plan.intent == QueryIntent::CompareDocuments {
        return run_compare_answer(
            request,
            catalog,
            models,
            worker,
            runtime_manager,
            generation,
            generation_artifact,
            embedding,
            maintenance,
            history,
            session_context,
            plan,
            resolved_scope,
            &resolution_candidates,
            &resolution_file_names,
            operation_id,
            cancelled,
            progress,
        );
    }

    // 4.6 Step 7：存在多个非常接近的候选且 Memory 未能消歧 → 返回强类型
    // NEED_CLARIFICATION，由用户明确选择目标文件后继续；不再静默保留
    // top-2/3 宽检索（用户明确指代时猜错比让用户选一次更伤）。
    // 注意：MultipleCandidates 的 resolved_file_ids 是 top-2/3（非空），
    // 触发条件只看「状态 + 有可选候选」，不能要求 scope 为空。
    if should_ask_clarification(
        memory_resolution_ok,
        resolution_status,
        !resolution_candidates.is_empty(),
    ) {
        let clarification_started = Instant::now();
        let reference = plan
            .target
            .reference
            .clone()
            .or_else(|| plan.target.document_name.clone())
            .unwrap_or_else(|| compact_for_prompt(request.question.trim(), 40));
        let options = resolution_candidates
            .iter()
            .take(MAX_CANDIDATE_SCOPE)
            .map(|candidate| ClarificationOption {
                file_id: candidate.file_id,
                display_name: resolution_file_names
                    .get(&candidate.file_id)
                    .cloned()
                    .unwrap_or_else(|| "未命名文件".to_owned()),
                document_type: None,
                score: candidate.score,
                signals: candidate.signals.clone(),
            })
            .collect::<Vec<_>>();
        // 保存待澄清引用：用户选择后据此写 USER_SELECTION 别名记忆
        if let Some(session_id) = request.session_id {
            let mut pending_context = session_context.clone();
            pending_context.pending_clarification_reference = Some(reference.clone());
            pending_context.updated_at = Some(Utc::now());
            let _ = catalog.update_ask_session_context(session_id, &pending_context);
        }
        let payload = ClarificationPayload {
            reference: reference.clone(),
            reason: "本地存在多份非常接近的候选文件，请选择你指的是哪一份。".to_owned(),
            options,
        };
        let result = AnswerResult {
            session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
            message_id: Uuid::now_v7(),
            answer: format!("请选择你指的是哪一份：\n{}", payload.reason),
            grounding_status: fanfan_core::GroundingStatus::Insufficient,
            insufficient_evidence: false,
            claims: Vec::new(),
            source_files: Vec::new(),
            used_file_ids: Vec::new(),
            elapsed_ms: clarification_started.elapsed().as_millis() as u64,
            answer_mode: AnswerMode::Clarification,
            retrieval_channels: Vec::new(),
            index_coverage: 0.0,
            degradation_reason: None,
            no_evidence_reason: None,
            clarification: Some(payload),
        };
        trace_node(
            catalog,
            "ask",
            "clarification",
            &operation_id.to_string(),
            request.session_id.map(|id| id.to_string()).as_deref(),
            None,
            &json!({
                "reference": reference,
                "candidate_count": resolution_candidates.len(),
            }),
            &json!({
                "answer_mode": result.answer_mode,
                "clarification_reason": result
                    .clarification
                    .as_ref()
                    .map(|payload| payload.reason.clone()),
                "options": result.clarification,
                "question": request.question,
            }),
            "ok",
            Some(clarification_started.elapsed().as_millis() as u64),
        );
        catalog.record_ask_exchange(request, &result)?;
        progress.0("clarification", 1.0);
        return Ok(result);
    }

    // 4.5.6 Step 12：DOCUMENT_FIND（spec 十一.1）只返回定位结果，不跑 chunk RAG。
    // 放在 Clarification 之后：多候选且未消歧时先让用户选定目标文件。
    if plan.intent == QueryIntent::DocumentFind {
        return run_document_find_answer(
            request,
            catalog,
            &resolved_scope,
            &resolution_candidates,
            &resolution_file_names,
            operation_id,
            cancelled,
            progress,
        );
    }

    // 5. 保存会话工作上下文（定位结果供下一轮 AMBIGUOUS 恢复；失败不阻断回答）
    let mut updated_context = session_context.clone();
    if !resolved_scope.is_empty() {
        updated_context.active_file_id = Some(resolved_scope[0]);
        updated_context.active_file_ids = resolved_scope.clone();
        updated_context.last_referenced_file_ids = resolved_scope.clone();
    }
    if let Some(document_type) = plan.target.document_type {
        updated_context.active_document_type = Some(document_type);
    }
    updated_context.last_intent = Some(plan.intent.as_str().to_owned());
    updated_context.updated_at = Some(Utc::now());
    if let Some(session_id) = request.session_id {
        let _ = catalog.update_ask_session_context(session_id, &updated_context);
    }

    // 6. 组装检索请求：scope.file_ids 白名单 + content_query 检索词
    let mut scoped = request.clone();
    if !resolved_scope.is_empty() {
        scoped.scope.file_ids = resolved_scope.clone();
    }
    if let Some(content_query) = plan.content_query.clone() {
        scoped.question = content_query;
    }
    trace_node(
        catalog,
        "ask",
        "scope_planning",
        &operation_id.to_string(),
        request.session_id.map(|id| id.to_string()).as_deref(),
        None,
        &json!({
            "intent": plan.intent.as_str(),
            "operation": plan.operation.as_str(),
            "requires_document_resolution": plan.requires_document_resolution,
            "requires_full_document": plan.requires_full_document,
            "content_query": plan.content_query,
        }),
        &json!({
            "scope_file_ids": scoped.scope.file_ids,
            "retrieval_question": scoped.question,
            "document_resolution": document_resolution,
        }),
        "ok",
        None,
    );
    // Step 8：DOCUMENT_SUMMARY 走整文分层摘要管线（spec 十一.3：禁止只拿
    // top-3 chunk 生成）。目标文件已由 Document/Memory Resolver 与澄清锁定，
    // 不再经过普通 chunk 检索。
    if plan.requires_full_document {
        return run_document_summary_answer(
            request,
            catalog,
            models,
            generation,
            generation_artifact,
            operation_id,
            cancelled,
            progress,
            resolved_scope,
            &resolution_candidates,
            plan.target.document_type,
        );
    }
    // NO_EVIDENCE 六分类预置（spec 十二）：Document Resolver 未解析出目标
    //（Unresolved）且 Memory 未消歧 → 后续拒绝路径记 TARGET_NOT_RESOLVED；
    // 已被 Memory/上下文消歧时根因不在定位层。
    let resolution_reason = match (resolution_status, memory_resolution_ok) {
        (Some(ResolutionStatus::Unresolved), false) => Some(NoEvidenceReason::TargetNotResolved),
        _ => None,
    };
    run_retrieval_answer(
        &scoped,
        request.question.trim(),
        catalog,
        models,
        worker,
        runtime_manager,
        generation,
        generation_artifact,
        embedding,
        maintenance,
        history,
        operation_id,
        cancelled,
        progress,
        // Step 11：完整 QueryPlan 随主检索管线走（Gate 从 plan 读
        // operation / question_shape / requires_project_context）
        Some(&plan),
        resolution_reason,
        // 改写跳过仅用于 DOCUMENT_SUMMARY（已分流）；此处恒为 false
        false,
        // 文档级召回只服务全库资料请求（scope 由 recall 填充）；单文档
        // QA/SUMMARY 的目标已由 Resolver 锁定，不在此列。
        plan.intent == QueryIntent::LibraryQa || plan.intent == QueryIntent::MultiDocumentQa,
    )
}

/// 文档级召回（Step 9，spec 十一.5 / 十二）：全库画像 metadata 信号粗筛 →
/// 粗筛集批量取向量精排 → 融合排序取前 `DOCUMENT_RECALL_TOP_N`。
///
/// 只在 scope 为空的全库资料请求时调用；任何失败（画像读取 / 向量缺失 /
/// 数据源未就绪）都在内部吞掉并如实 trace，返回空集让调用方回落到 wider
/// chunk retrieval——召回是增益层，绝不中断问答。trace 节点 `document_recall`
/// 记录问题、候选分数与信号（前端可展示「按哪些依据找到这些文档」）。
fn run_document_recall(
    catalog: &CatalogService,
    question: &str,
    question_vector: Option<&[f32]>,
    correlation_id: &str,
    session_id_ref: Option<&str>,
) -> Vec<Uuid> {
    let started = Instant::now();
    let mut trace_status = "ok";
    let mut reason = "recalled";
    let mut recalled: Vec<Uuid> = Vec::new();
    let mut recall_signals: Vec<serde_json::Value> = Vec::new();
    let outcome: Result<(), AppError> = (|| {
        let profiles = catalog.list_document_profiles(None, 10_000)?;
        if profiles.is_empty() {
            trace_status = "empty";
            reason = "no_profiles";
            return Ok(());
        }
        // 第 1 级：metadata 粗筛（分数降序、截断到向量候选上限），
        // 只对这批取向量——避免对全库逐文件查一次库。
        let preselected = preselect_document_profiles(question, &profiles);
        if preselected.is_empty() {
            trace_status = "empty";
            reason = "no_metadata_match";
            return Ok(());
        }
        let vector_ids: Vec<Uuid> = preselected.iter().map(|(_, file_id, _)| *file_id).collect();
        let vectors = catalog.profile_vectors(&vector_ids)?;
        // 第 2 级：向量精排 + 融合。
        let ranked = rank_document_candidates(question, question_vector, &profiles, &vectors);
        recalled = ranked.iter().map(|candidate| candidate.file_id).collect();
        recall_signals = ranked
            .iter()
            .map(|candidate| {
                json!({
                    "file_id": candidate.file_id.to_string(),
                    "score": candidate.score,
                    "signals": candidate.signals,
                })
            })
            .collect();
        if recalled.is_empty() {
            trace_status = "empty";
            reason = "below_min_score";
        }
        Ok(())
    })();
    if let Err(error) = outcome {
        trace_node(
            catalog,
            "ask",
            "document_recall",
            correlation_id,
            session_id_ref,
            None,
            &json!({ "question": question, "has_vector": question_vector.is_some() }),
            &json!({
                "status": "failed",
                "error_code": error.code,
                "candidate_count": 0,
            }),
            "error",
            Some(started.elapsed().as_millis() as u64),
        );
        return Vec::new();
    }
    trace_node(
        catalog,
        "ask",
        "document_recall",
        correlation_id,
        session_id_ref,
        None,
        &json!({
            "question": question,
            "has_vector": question_vector.is_some(),
            "preselect_cap": DOCUMENT_RECALL_VECTOR_CANDIDATES,
        }),
        &json!({
            "status": trace_status,
            "reason": reason,
            "candidate_count": recalled.len(),
            "candidates": recall_signals,
            "top_n_cap": DOCUMENT_RECALL_TOP_N,
        }),
        trace_status,
        Some(started.elapsed().as_millis() as u64),
    );
    recalled
}

/// 单次摘要批次的最大正文字符数（生成模型上下文 4096 token，中文 1 字符
/// ≈ 1~2 token，3500 字符给 prompt 留出输出与结构余量）。
const SUMMARY_BATCH_CHARS: usize = 3_500;
/// 单节进入 prompt 的正文上限（超长节截断，不截证据引用）。
const SUMMARY_SECTION_CAP_CHARS: usize = 1_200;
/// 摘要失败时的确定性回退：展示节内前 220 字，绝不因模型失败而整体失败
/// （开发约束 7：所有模型失败不得导致应用崩溃）。
const SUMMARY_FALLBACK_CHARS: usize = 220;

/// DOCUMENT_SUMMARY：整文分层摘要（spec 十一.3 / 二十 CASE 7）。
///
/// 管线：读取整份文档结构（document_nodes 分页）+ 当前修订全部 chunk →
/// 章节分组（heading 边界 + 超长拆分）→ 分批逐节摘要（每批一个
/// complete_json_with_model 调用，JSON Schema 约束）→ 总览聚合 →
/// 逐节 claims（引用真实 chunk 原文，通过 validate_answer_evidence 全量校验）。
///
/// 失败语义：模型任一批次失败都回退到确定性节内摘录（`SUMMARY_FALLBACK_CHARS`
/// 前 220 字），只有目标文件完全无法解析/无正文时才按 LOCAL + NO_EVIDENCE
/// 返回固定拒绝文案；绝不转闲聊。
#[allow(clippy::too_many_arguments)]
fn run_document_summary_answer(
    request: &AskRequest,
    catalog: &CatalogService,
    _models: &ModelManager,
    generation: &Mutex<LocalGenerationRuntime>,
    generation_artifact: ModelArtifact,
    operation_id: Uuid,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
    resolved_scope: Vec<Uuid>,
    resolution_candidates: &[DocumentCandidate],
    document_type_hint: Option<DocumentType>,
) -> Result<AnswerResult, AppError> {
    let (phase, verified_claim) = progress;
    let correlation_id = operation_id.to_string();
    let session_id = request.session_id.map(|id| id.to_string());
    let session_id_ref = session_id.as_deref();
    let started_at = Instant::now();

    // 目标文件：多候选时按 Resolver 综合得分取最高者（摘要按单文档执行）
    let mut target_file = resolved_scope.first().copied();
    if resolved_scope.len() > 1 {
        target_file = resolution_candidates
            .iter()
            .find(|candidate| resolved_scope.contains(&candidate.file_id))
            .map(|candidate| candidate.file_id)
            .or(target_file);
    }
    let Some(target_file) = target_file else {
        // LOCAL + NO_EVIDENCE：目标未锁定 → 固定文案拒绝（spec 十三），不转闲聊
        return finish_summary_refusal(
            request,
            catalog,
            operation_id,
            "无法确定要概括哪份资料。你可以说得更具体一些，例如文件的名称或类型。",
            started_at,
            phase,
        );
    };
    let file = catalog.file_preview(&target_file, 1)?.file;
    let file_name = file.display_name.clone();
    let file_path = file.canonical_path.clone();

    phase("understanding", 0.08);
    // 1. 整份文档结构：document_nodes 分页读取（200/批，上限 4000 节点防失控）
    let nodes_total = catalog.file_document_node_count(&target_file)?;
    let mut nodes = Vec::new();
    let mut offset = 0usize;
    let mut summary_truncated = false;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
        }
        let preview = catalog.file_preview_page(&target_file, offset, 200, None)?;
        if preview.revision_id.is_none() {
            break;
        }
        let batch_len = preview.nodes.len();
        nodes.extend(preview.nodes);
        offset = match preview.next_offset {
            Some(next) => next as usize,
            None => break,
        };
        if batch_len == 0 || nodes.len() > 4_000 {
            // 截断是明确行为而非隐藏降级：trace 里如实记录总数/用量/截断标记
            summary_truncated = nodes.len() > 4_000;
            break;
        }
    }
    let nodes_used = nodes.len();
    let node_heading_paths = nodes
        .into_iter()
        .map(|node| (node.node_id, node.heading_path))
        .collect::<HashMap<_, _>>();

    // 2. 当前修订全部 chunk（摘要证据：真实 chunk 原文）
    let section_chunks = catalog
        .file_chunks(&target_file)?
        .into_iter()
        .map(|chunk| SectionChunk {
            chunk_id: chunk.chunk_id,
            node_id: chunk.node_id,
            revision_id: chunk.revision_id,
            ordinal: chunk.ordinal,
            text: chunk.text,
            locator: chunk.locator,
        })
        .collect::<Vec<_>>();
    if section_chunks.is_empty() {
        return finish_summary_refusal(
            request,
            catalog,
            operation_id,
            "这份资料还没有可概括的正文内容（可能仍在解析，或为纯图片/扫描件）。",
            started_at,
            phase,
        );
    }

    // 3. 章节分组（heading 边界 + 超长拆分 + 尾部合并）
    let mut sections =
        build_document_sections(&section_chunks, &node_heading_paths, MAX_SECTION_CHARS);
    merge_tail_sections(&mut sections, MAX_SECTIONS);
    let type_hint = document_type_hint.map(|value| value.as_str());

    // 4. 分批逐节摘要；模型失败/解析失败 → 确定性节内摘录回退
    let mut digests = Vec::<SectionSummary>::with_capacity(sections.len());
    let mut batch_fallbacks = 0_usize;
    let mut index = 0usize;
    while index < sections.len() {
        if cancelled.load(Ordering::Acquire) {
            return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
        }
        let mut batch = Vec::new();
        let mut batch_chars = 0usize;
        while index < sections.len() && (batch.is_empty() || batch_chars < SUMMARY_BATCH_CHARS) {
            let section = &sections[index];
            let compacted = compact_for_prompt(&section.text(), SUMMARY_SECTION_CAP_CHARS);
            if !batch.is_empty()
                && batch_chars.saturating_add(compacted.len()) > SUMMARY_BATCH_CHARS
            {
                break;
            }
            batch_chars = batch_chars.saturating_add(compacted.len());
            batch.push((index, section, compacted));
            index += 1;
        }
        phase("generating", 0.62);
        let batch_started = Instant::now();
        let payload = batch
            .iter()
            .map(|(_, section, compacted)| {
                json!({
                    "title": section.title,
                    "content": compacted,
                })
            })
            .collect::<Value>();
        let (system, user) = document_summary_prompt(&file_name, type_hint, &payload.to_string());
        let parsed = match complete_json_with_model(
            generation,
            &generation_artifact,
            &system,
            &user,
            640,
            &section_summary_schema(),
            cancelled,
        ) {
            Ok(raw) => parse_section_summaries(&raw),
            Err(_) => Vec::new(),
        };
        let mut by_title = HashMap::<String, SectionSummary>::new();
        for digest in parsed {
            by_title.insert(digest.title.trim().to_ascii_lowercase(), digest);
        }
        let mut batch_digests = Vec::with_capacity(batch.len());
        let mut batch_failed = false;
        for (_, section, _) in &batch {
            let key = section.title.trim().to_ascii_lowercase();
            let base_key = key
                .trim_end_matches(|ch: char| ch.is_ascii_digit())
                .trim_end_matches("（续")
                .trim();
            let digest = by_title
                .remove(&key)
                .or_else(|| by_title.remove(base_key))
                .unwrap_or_else(|| {
                    batch_failed = true;
                    SectionSummary {
                        title: section.title.clone(),
                        summary: compact_for_prompt(&section.text(), SUMMARY_FALLBACK_CHARS),
                        key_points: Vec::new(),
                    }
                });
            batch_digests.push(digest);
        }
        if batch_failed {
            batch_fallbacks += 1;
        }
        trace_node(
            catalog,
            "ask",
            "document_summary",
            &correlation_id,
            session_id_ref,
            Some(&(batch.len()).to_string()),
            &json!({
                "file_id": target_file.to_string(),
                "file_name": file_name,
                "section_count": batch.len(),
                "section_titles": batch.iter().map(|(_, section, _)| &section.title).collect::<Vec<_>>(),
            }),
            &json!({
                "fallback_sections": batch_fallbacks,
                "digests": batch_digests,
                "summary_truncated": summary_truncated,
                "nodes_total": nodes_total,
                "nodes_used": nodes_used,
            }),
            "ok",
            Some(batch_started.elapsed().as_millis() as u64),
        );
        digests.extend(batch_digests);
    }

    // 5. 总览聚合（最后一层）：各节摘要 → 文档总览
    let overview_started = Instant::now();
    let overview = {
        let payload = digests_json(&digests);
        let (system, user) = document_overview_prompt(&file_name, type_hint, &payload.to_string());
        match complete_json_with_model(
            generation,
            &generation_artifact,
            &system,
            &user,
            512,
            &overview_schema(),
            cancelled,
        )
        .ok()
        .and_then(|raw| parse_overview(&raw))
        {
            Some(overview) => overview,
            None => {
                batch_fallbacks += 1;
                DocumentOverview {
                    overview: String::new(),
                    overall_summary: digests
                        .iter()
                        .map(|digest| digest.summary.as_str())
                        .collect::<Vec<_>>()
                        .join("；"),
                    structure: digests
                        .iter()
                        .map(|digest| StructureEntry {
                            title: digest.title.clone(),
                            key_points: digest.key_points.clone(),
                        })
                        .collect(),
                }
            }
        }
    };
    trace_node(
        catalog,
        "ask",
        "document_summary",
        &correlation_id,
        session_id_ref,
        Some("overview"),
        &json!({ "file_id": target_file.to_string(), "digest_count": digests.len() }),
        &json!({ "overview": overview, "fallback": batch_fallbacks }),
        "ok",
        Some(overview_started.elapsed().as_millis() as u64),
    );

    // 6. 组装 AnswerResult：逐节 claim + 真实 chunk 引用（quote = 原文全文）
    let mut claims = Vec::with_capacity(digests.len());
    for (section, digest) in sections.iter().zip(digests.iter()) {
        let mut text = format!("【{}】{}", digest.title, digest.summary);
        if !digest.key_points.is_empty() {
            text.push_str(&format!("\n要点：{}", digest.key_points.join("；")));
        }
        let citations = section
            .chunks
            .iter()
            .take(3)
            .map(|chunk| EvidenceRef {
                evidence_id: Uuid::now_v7(),
                file_id: target_file,
                revision_id: chunk.revision_id,
                node_id: chunk.node_id,
                chunk_id: chunk.chunk_id,
                image_asset_id: None,
                quote: chunk.text.clone(),
                context_before: None,
                context_after: None,
                locator: chunk.locator.clone(),
                retrieval_score: 0.0,
            })
            .collect::<Vec<_>>();
        claims.push(AnswerClaim {
            claim_id: Uuid::now_v7(),
            text,
            support_status: SupportStatus::Supported,
            citations,
        });
    }

    let mut answer_parts = Vec::new();
    if !overview.overview.is_empty() {
        answer_parts.push(overview.overview.clone());
    }
    if !overview.overall_summary.is_empty() {
        answer_parts.push(overview.overall_summary.clone());
    }
    answer_parts.push("## 文档结构".to_owned());
    for digest in &digests {
        answer_parts.push(format!("### {}\n{}", digest.title, digest.summary));
        for point in &digest.key_points {
            answer_parts.push(format!("- {point}"));
        }
    }
    let result = AnswerResult {
        session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
        message_id: Uuid::now_v7(),
        answer: answer_parts.join("\n\n"),
        grounding_status: if claims.is_empty() {
            GroundingStatus::Insufficient
        } else {
            GroundingStatus::Grounded
        },
        insufficient_evidence: claims.is_empty(),
        claims,
        source_files: vec![AnswerSourceFile {
            file_id: target_file,
            display_name: file_name.clone(),
            canonical_path: file_path,
        }],
        used_file_ids: vec![target_file],
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        answer_mode: AnswerMode::Summary,
        retrieval_channels: vec!["document_structure".into()],
        index_coverage: 0.0,
        degradation_reason: (batch_fallbacks > 0).then(|| {
            format!("有{batch_fallbacks}个摘要批次回退为确定性节内摘录（模型输出未通过解析）")
        }),
        no_evidence_reason: None,
        clarification: None,
    };
    for claim in &result.claims {
        verified_claim(claim);
    }
    catalog.validate_answer_evidence(&result)?;
    trace_node(
        catalog,
        "ask",
        "completed",
        &correlation_id,
        session_id_ref,
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
            // spec 十八：摘要的证据绑定数（每个 summary bullet 绑定的
            // EvidenceRef 总量，验证 Summary 不绕过 Citation）
            "summary_evidence_count": result
                .claims
                .iter()
                .map(|claim| claim.citations.len())
                .sum::<usize>(),
        }),
        "ok",
        Some(result.elapsed_ms),
    );
    catalog.record_ask_exchange(request, &result)?;
    phase("completed", 1.0);
    Ok(result)
}

/// 摘要目标的确定性拒绝（LOCAL + NO_EVIDENCE / 无正文）：固定文案返回，
/// 落库 + trace + 结束相位，不转闲聊。
fn finish_summary_refusal(
    request: &AskRequest,
    catalog: &CatalogService,
    operation_id: Uuid,
    message: &str,
    started_at: Instant,
    phase: &dyn Fn(&str, f64),
) -> Result<AnswerResult, AppError> {
    let result = AnswerResult {
        session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
        message_id: Uuid::now_v7(),
        answer: message.to_owned(),
        grounding_status: GroundingStatus::Insufficient,
        insufficient_evidence: true,
        claims: Vec::new(),
        source_files: Vec::new(),
        used_file_ids: Vec::new(),
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        answer_mode: AnswerMode::RagRefusal,
        retrieval_channels: Vec::new(),
        index_coverage: 0.0,
        degradation_reason: None,
        no_evidence_reason: None,
        clarification: None,
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
            "claim_count": 0,
            "insufficient_evidence": true,
            "degradation_reason": "summary_target_unresolved_or_empty",
        }),
        "ok",
        Some(result.elapsed_ms),
    );
    catalog.record_ask_exchange(request, &result)?;
    phase("completed", 1.0);
    Ok(result)
}

/// COMPARE_DOCUMENTS：两篇文档对比（spec 十一.6 / 二十 CASE 8）。
///
/// 管线：两侧目标确定（primary 走 Resolver 结果；secondary_target 独立
/// 解析，失败时用 primary 候选集的次高文件——「比较两个版本」时 Resolver
/// 的多候选常恰为两侧）→ 两侧分别 chunk 取证（scope 单文件）→ 比较生成
/// （JSON Schema 约束输出）→ 逐比较点 claims（每点引用两侧真实 chunk
/// 原文，通过 validate_answer_evidence 精确引用校验）。
///
/// 失败语义：比较生成失败/解析失败 → 确定性回退（两侧检索到的原文材料
/// 并排呈现，无模型文本）；两侧目标不足或重合 → 退化为普通检索回答
/// （多文档 QA 语义），trace 如实记录；绝不转闲聊（spec 十三）。
#[allow(clippy::too_many_arguments)]
fn run_compare_answer(
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
    session_context: &AskSessionContext,
    plan: QueryPlan,
    resolved_scope: Vec<Uuid>,
    resolution_candidates: &[DocumentCandidate],
    resolution_file_names: &HashMap<Uuid, String>,
    operation_id: Uuid,
    cancelled: &AtomicBool,
    progress: AskProgressCallbacks<'_>,
) -> Result<AnswerResult, AppError> {
    let (phase, verified_claim) = progress;
    let correlation_id = operation_id.to_string();
    let session_id = request.session_id.map(|id| id.to_string());
    let session_id_ref = session_id.as_deref();
    let started_at = Instant::now();
    let compare_started = Instant::now();

    // 1. 确定两侧文件。
    let mut file_names = resolution_file_names.clone();
    if file_names.is_empty() {
        let profiles_with_names = catalog.list_document_profiles(None, 2000)?;
        for (profile, name) in profiles_with_names {
            file_names.entry(profile.file_id).or_insert(name);
        }
    }
    let mut side_candidates = resolved_scope.clone();
    side_candidates.extend(
        resolution_candidates
            .iter()
            .map(|candidate| candidate.file_id),
    );
    side_candidates.dedup();
    // 多候选（>2 侧）时 COMPARE 不清澄直接取 top-2：trace 如实标记自动选择（Phase 2 欠项）
    let compare_auto_selected = side_candidates.len() > 2;
    let primary_file = side_candidates.first().copied();
    let mut secondary_file = side_candidates.get(1).copied();
    // secondary_target 独立解析（若 parser 拆出了第二个目标）
    if let Some(secondary) = plan.secondary_target.as_ref() {
        let mut secondary_plan = plan.clone();
        secondary_plan.target = secondary.clone();
        let profiles = catalog.list_document_profiles(None, 2000)?;
        let input = ResolverInput::new(
            &secondary_plan,
            session_context,
            profiles
                .iter()
                .map(|(profile, _)| profile.clone())
                .collect(),
            file_names.clone(),
        );
        let resolution = resolve_documents(&input);
        secondary_file = resolution
            .resolved_file_ids
            .first()
            .copied()
            .or_else(|| {
                resolution
                    .candidates
                    .first()
                    .map(|candidate| candidate.file_id)
            })
            .or(secondary_file);
    }
    if primary_file == secondary_file {
        secondary_file = None;
    }
    let (Some(side_a), Some(side_b)) = (primary_file, secondary_file) else {
        // 不假装比较（trace 如实记录 fallback）。
        let mut scoped = request.clone();
        if !side_candidates.is_empty() {
            scoped.scope.file_ids = side_candidates.clone();
        }
        trace_node(
            catalog,
            "ask",
            "document_compare",
            &correlation_id,
            session_id_ref,
            None,
            &json!({
                "intent": "compare_documents",
                "primary_file": side_candidates.first().map(|id| id.to_string()),
                "secondary_target": plan.secondary_target.as_ref().map(|target| target.reference.clone()),
            }),
            &json!({ "status": "fallback_insufficient_targets", "fallback": "multi_document_qa" }),
            "ok",
            Some(compare_started.elapsed().as_millis() as u64),
        );
        return run_retrieval_answer(
            &scoped,
            request.question.trim(),
            catalog,
            models,
            worker,
            runtime_manager,
            generation,
            generation_artifact,
            embedding,
            maintenance,
            history,
            operation_id,
            cancelled,
            progress,
            Some(&plan),
            None,
            false,
            false,
        );
    };

    // 2. 编码对比问题向量（两侧检索共用同一向量）。
    let embedding = embedding.ok_or_else(|| {
        AppError::new(
            "RAG_EMBEDDING_MODEL_REQUIRED",
            "问资料需要先配置并通过自检的中文 Embedding 模型",
            false,
        )
    })?;
    let artifact_id = embedding.artifact_id.to_string();
    let question_text = plan
        .content_query
        .clone()
        .unwrap_or_else(|| request.question.trim().to_owned());
    let tokenizer_path = PathBuf::from(&embedding.local_path)
        .parent()
        .map(|parent| parent.join("tokenizer.json"))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            AppError::new(
                "EMBEDDING_TOKENIZER_MISSING",
                "Embedding tokenizer 不存在，完整 RAG 已停止",
                true,
            )
        })?;
    let mut embedding_runtime_request = RuntimeTaskRequest::interactive(
        RuntimeTaskKind::Embedding,
        RuntimeBackendKind::OnnxRuntime,
    );
    embedding_runtime_request.cpu_threads = 2;
    embedding_runtime_request.timeout = Duration::from_secs(10);
    embedding_runtime_request.model_id = Some(artifact_id.clone());
    let embedding_runtime_lease = runtime_manager.acquire(embedding_runtime_request)?;
    let response = worker.encode_embeddings(&EmbeddingRequest {
        model_path: embedding.local_path.clone(),
        tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
        texts: vec![format!(
            "{}{}",
            embedding.query_prefix.as_deref().unwrap_or(""),
            question_text
        )],
        max_length: embedding.max_length.unwrap_or(512),
        threads: 2,
    })?;
    embedding_runtime_lease.complete();
    if response.vectors.is_empty() {
        return Err(AppError::new(
            "EMBEDDING_EMPTY",
            "Embedding 运行时没有返回查询向量",
            true,
        ));
    }

    // 3. 两侧分别取证：scope = 单侧文件，各跑一次 extractive 检索。
    phase("hybrid_retrieval", 0.3);
    let mut side_materials: Vec<(Uuid, String, Vec<String>, Vec<EvidenceRef>)> = Vec::new();
    for file_id in [side_a, side_b] {
        let mut sub_request = request.clone();
        sub_request.question = question_text.clone();
        sub_request.scope.file_ids = vec![file_id];
        sub_request.retrieval_limit = sub_request
            .retrieval_limit
            .min(COMPARE_MATERIAL_ITEMS as u32);
        sub_request.max_source_files = sub_request.max_source_files.min(1);
        let result = catalog.answer_extractively(
            &sub_request,
            Some(SemanticQuery {
                model_artifact_id: &artifact_id,
                vector: &response.vectors[0],
            }),
        )?;
        let name = file_names
            .get(&file_id)
            .cloned()
            .unwrap_or_else(|| "未命名文件".to_owned());
        let mut quotes = Vec::<String>::new();
        let mut evidence = Vec::<EvidenceRef>::new();
        for claim in result.claims.into_iter().take(COMPARE_MATERIAL_ITEMS) {
            for citation in claim.citations {
                if quotes.len() >= COMPARE_MATERIAL_ITEMS {
                    break;
                }
                quotes.push(compact_for_prompt(&citation.quote, COMPARE_MATERIAL_CHARS));
                evidence.push(citation);
            }
        }
        side_materials.push((file_id, name, quotes, evidence));
    }
    let (_, a_name, a_quotes, a_evidence) = &side_materials[0];
    let (_, b_name, b_quotes, b_evidence) = &side_materials[1];
    if a_quotes.is_empty() || b_quotes.is_empty() {
        // 任一侧无取证材料 → LOCAL + NO_EVIDENCE 语义：固定文案拒绝，
        // 不转闲聊（spec 十三）。
        let message = format!(
            "没能在两侧都找到可比较的内容（{} 命中 {} 条，{} 命中 {} 条）。可以换一种说法，或确认资料已完成索引。",
            a_name,
            a_quotes.len(),
            b_name,
            b_quotes.len()
        );
        return finish_summary_refusal(request, catalog, operation_id, &message, started_at, phase);
    }

    // 4. 比较生成（JSON Schema 约束）；失败 → 确定性回退。
    phase("generating", 0.6);
    let compare_generate_started = Instant::now();
    let mut fallback = false;
    let results = {
        let (system, user) = compare_prompt(a_name, a_quotes, b_name, b_quotes, &question_text);
        match complete_json_with_model(
            generation,
            &generation_artifact,
            &system,
            &user,
            800,
            &compare_schema(),
            cancelled,
        )
        .ok()
        .and_then(|raw| parse_compare_results(&raw))
        {
            Some(results) => results,
            None => {
                fallback = true;
                CompareResults {
                    similarities: Vec::new(),
                    differences: Vec::new(),
                    conclusion: String::new(),
                }
            }
        }
    };

    // 5. 组装 claims 与答案。claims 的引用证据永远来自两侧检索到的真实
    // chunk 原文（quote = 原文全文，通过 validate_answer_evidence 精确校验）；
    // 模型输出的左右摘引（left/right_evidence）只作展示。
    let mut claims = Vec::<AnswerClaim>::new();
    let mut answer_parts = Vec::<String>::new();
    if !results.conclusion.is_empty() {
        answer_parts.push(format!("**结论**：{}", results.conclusion));
    }
    for point in &results.similarities {
        let mut citations = Vec::with_capacity(2);
        if let Some(evidence) = a_evidence.first() {
            citations.push(evidence.clone());
        }
        if let Some(evidence) = b_evidence.first() {
            citations.push(evidence.clone());
        }
        claims.push(AnswerClaim {
            claim_id: Uuid::now_v7(),
            text: format!("相同点：{}", point.point),
            support_status: if citations.len() == 2 {
                SupportStatus::Supported
            } else {
                SupportStatus::Partial
            },
            citations,
        });
    }
    for difference in &results.differences {
        let mut citations = Vec::with_capacity(2);
        if let Some(evidence) = a_evidence.first() {
            citations.push(evidence.clone());
        }
        if let Some(evidence) = b_evidence.first() {
            citations.push(evidence.clone());
        }
        let mut text = format!("差异：{}", difference.point);
        if !difference.left_evidence.is_empty() {
            text.push_str(&format!("\n左（{}）：{}", a_name, difference.left_evidence));
        }
        if !difference.right_evidence.is_empty() {
            text.push_str(&format!(
                "\n右（{}）：{}",
                b_name, difference.right_evidence
            ));
        }
        claims.push(AnswerClaim {
            claim_id: Uuid::now_v7(),
            text,
            support_status: if citations.len() == 2 {
                SupportStatus::Supported
            } else {
                SupportStatus::Partial
            },
            citations,
        });
    }
    // 总览 claim：结论引用两侧首条证据。
    if !results.conclusion.is_empty() {
        let mut citations = Vec::with_capacity(2);
        if let Some(evidence) = a_evidence.first() {
            citations.push(evidence.clone());
        }
        if let Some(evidence) = b_evidence.first() {
            citations.push(evidence.clone());
        }
        claims.push(AnswerClaim {
            claim_id: Uuid::now_v7(),
            text: format!("结论：{}", results.conclusion),
            support_status: if citations.len() == 2 {
                SupportStatus::Supported
            } else {
                SupportStatus::Partial
            },
            citations,
        });
    }

    let used_file_ids = vec![side_a, side_b];
    let mut source_files = Vec::new();
    for file_id in used_file_ids.clone() {
        let name = file_names
            .get(&file_id)
            .cloned()
            .unwrap_or_else(|| "未命名文件".to_owned());
        let canonical_path = catalog
            .file_preview(&file_id, 1)
            .map(|preview| preview.file.canonical_path.clone())
            .unwrap_or_default();
        source_files.push(AnswerSourceFile {
            file_id,
            display_name: name,
            canonical_path,
        });
    }

    // 确定性回退：比较生成失败 → 两侧检索到的原文材料并排呈现。
    if fallback {
        claims.clear();
        answer_parts.clear();
        answer_parts.push("未能生成对比结论，以下为两侧检索到的原文依据：".to_owned());
        for (index, (_, name, quotes, evidence)) in side_materials.iter().enumerate() {
            answer_parts.push(format!("### 侧 {}：{}", index + 1, name));
            for (quote_index, quote) in quotes.iter().take(COMPARE_FALLBACK_ITEMS).enumerate() {
                answer_parts.push(format!("{}. {}", quote_index + 1, quote));
            }
            for evidence in evidence.iter().take(COMPARE_FALLBACK_ITEMS) {
                claims.push(AnswerClaim {
                    claim_id: Uuid::now_v7(),
                    text: format!("【{}】{}", name, compact_for_prompt(&evidence.quote, 260)),
                    support_status: SupportStatus::Supported,
                    citations: vec![evidence.clone()],
                });
            }
        }
    }
    if !results.similarities.is_empty() {
        answer_parts.push("## 相同点".to_owned());
        for point in &results.similarities {
            answer_parts.push(format!("- {}", point.point));
        }
    }
    if !results.differences.is_empty() {
        answer_parts.push("## 差异点".to_owned());
        for difference in &results.differences {
            answer_parts.push(format!("- {}", difference.point));
        }
    }

    let result = AnswerResult {
        session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
        message_id: Uuid::now_v7(),
        answer: answer_parts.join("\n\n"),
        grounding_status: if claims.is_empty() {
            GroundingStatus::Insufficient
        } else {
            GroundingStatus::Grounded
        },
        insufficient_evidence: claims.is_empty(),
        claims,
        source_files,
        used_file_ids,
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        answer_mode: AnswerMode::Compare,
        retrieval_channels: vec!["document_compare".into()],
        index_coverage: 0.0,
        degradation_reason: fallback
            .then(|| "对比生成未通过解析，回退为两侧原文材料并排呈现".to_owned()),
        no_evidence_reason: None,
        clarification: None,
    };
    for claim in &result.claims {
        verified_claim(claim);
    }
    catalog.validate_answer_evidence(&result)?;
    trace_node(
        catalog,
        "ask",
        "document_compare",
        &correlation_id,
        session_id_ref,
        None,
        &json!({
            "question": question_text,
            "side_a": { "file_id": side_a.to_string(), "name": a_name },
            "side_b": { "file_id": side_b.to_string(), "name": b_name },
            "quotes_side_a": a_quotes.len(),
            "quotes_side_b": b_quotes.len(),
            "secondary_target": plan.secondary_target.as_ref().map(|target| target.reference.clone()),
            "compare_auto_selected": compare_auto_selected,
        }),
        &json!({
            "similarities": results.similarities,
            "differences": results.differences,
            "conclusion": results.conclusion,
            "fallback": fallback,
            "claim_count": result.claims.len(),
        }),
        "ok",
        Some(compare_generate_started.elapsed().as_millis() as u64),
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

/// 闲聊分支：跳过检索/索引 gate，直接用生成模型对话（带会话历史）。
#[allow(clippy::too_many_arguments)]
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
    // Phase 4.3 第三部分：Chat 幻觉最终保护。personal query（我的/我之前/
    // 我的资料/毕业时候……）出现在 Chat 入口，说明路由/解析链路已把它误判
    // 为闲聊且 source_files 为空——这类问题的答案只能来自本地证据，自由
    // 生成必然幻觉（RAG 定义错误 / 通用简历模板）。此处是最后一道闸：
    // 固定 NO_EVIDENCE 拒绝文案（RagRefusal），绝不调模型。
    if let Some(marker) = personal_reference_hit(request.question.trim()) {
        let started_at = Instant::now();
        let answer = local_no_evidence_answer(request.question.trim(), &[], false);
        let result = AnswerResult {
            session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
            message_id: Uuid::now_v7(),
            answer: answer.clone(),
            grounding_status: fanfan_core::GroundingStatus::Insufficient,
            insufficient_evidence: true,
            claims: Vec::new(),
            source_files: Vec::new(),
            used_file_ids: Vec::new(),
            elapsed_ms: started_at.elapsed().as_millis() as u64,
            answer_mode: AnswerMode::RagRefusal,
            retrieval_channels: Vec::new(),
            index_coverage: 0.0,
            degradation_reason: None,
            no_evidence_reason: Some(NoEvidenceReason::TrueNoEvidence),
            clarification: None,
        };
        let session_id = request.session_id.map(|id| id.to_string());
        trace_node(
            catalog,
            "ask",
            "chat_guard_blocked",
            &operation_id.to_string(),
            session_id.as_deref(),
            None,
            &json!({ "question": request.question }),
            &json!({
                "guard": "personal_reference_no_evidence",
                "marker": marker,
                "answer_mode": result.answer_mode,
                "answer": result.answer,
            }),
            "ok",
            Some(result.elapsed_ms),
        );
        catalog.record_ask_exchange(request, &result)?;
        phase("completed", 1.0);
        return Ok(result);
    }
    // Phase 4.3 第二部分：builtin_knowledge 优先于 LLM 自由生成（仅
    // GENERAL 链路；personal query 已被上方 guard 拦截，词条与个人资料
    // 完全隔离）。本地小模型对 LangGraph/RAG/Transformer 等稳定技术概念
    // 极易幻觉（实测 LangGraph→GNN、RAG→递归架构），内置词条直接命中
    // 返回，不消耗一次生成调用。
    if let Some(hit) = lookup_builtin_knowledge(request.question.trim()) {
        let started_at = Instant::now();
        let result = AnswerResult {
            session_id: request.session_id.unwrap_or_else(Uuid::now_v7),
            message_id: Uuid::now_v7(),
            answer: hit.answer.clone(),
            grounding_status: fanfan_core::GroundingStatus::Insufficient,
            insufficient_evidence: false,
            claims: Vec::new(),
            source_files: Vec::new(),
            used_file_ids: Vec::new(),
            elapsed_ms: started_at.elapsed().as_millis() as u64,
            answer_mode: AnswerMode::Chat,
            retrieval_channels: Vec::new(),
            index_coverage: 0.0,
            degradation_reason: None,
            no_evidence_reason: None,
            clarification: None,
        };
        let session_id = request.session_id.map(|id| id.to_string());
        trace_node(
            catalog,
            "ask",
            "builtin_knowledge_hit",
            &operation_id.to_string(),
            session_id.as_deref(),
            None,
            &json!({ "question": request.question }),
            &json!({
                "key": hit.key,
                "category": hit.category,
                "answer_mode": result.answer_mode,
                "answer": result.answer,
            }),
            "ok",
            Some(result.elapsed_ms),
        );
        catalog.record_ask_exchange(request, &result)?;
        phase("completed", 1.0);
        return Ok(result);
    }
    phase("chat_generating", 0.7);
    let (system, user) = chat_prompt(request, history);
    let started_at = Instant::now();
    let answer = complete_with_model(
        generation,
        generation_artifact,
        &system,
        &user,
        512,
        cancelled,
    )?;
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
        answer_mode: AnswerMode::Chat,
        retrieval_channels: Vec::new(),
        index_coverage: 0.0,
        degradation_reason: None,
        no_evidence_reason: None,
        clarification: None,
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
    // 用户原始问题（request.question 可能已被替换为 content_query 检索词；
    // Answerability Gate 与回答语义判断必须基于原始问题）。
    original_question: &str,
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
    // 完整 QueryPlan（LLM Parser 输出）：Answerability Gate 从 plan 读取
    // operation / question_shape / requires_project_context，不再用关键词
    // 猜测回答形态与证据要求。解析失败回退时为 None。
    plan: Option<&QueryPlan>,
    // NO_EVIDENCE 根因预置（spec 十二）：Document Resolver 未解析出目标时
    // 由调用方传入 TARGET_NOT_RESOLVED，最终拒绝路径据此分类。
    resolution_reason: Option<NoEvidenceReason>,
    skip_query_rewrite: bool,
    document_recall: bool,
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
    // 改写：只在 content_query 上做（目标对象已在 Query Parser 阶段与检索词
    // 分离，改写不再接触 target）；空泛/一题多问交给模型决定，行式输出每行
    // 一个问题；已明确单一的问题模型会原样复读。解析失败/为空 → 回退当前
    // 检索词，不中断检索。DOCUMENT_SUMMARY（requires_full_document）跳过
    // 改写——整文摘要不需要检索词改写。
    let rewritten_queries = if skip_query_rewrite {
        Vec::new()
    } else {
        let (system, user) = query_rewrite_prompt(request.question.trim(), history);
        let rewritten = complete_with_model(
            generation,
            &generation_artifact,
            &system,
            &user,
            160,
            cancelled,
        )?;
        let parsed = parse_rewritten_queries(&rewritten);
        // 防复读校验：0.6B 改写时会把历史里最后一条助手回复整句复读成改写
        // 结果（历史标记若未生效）。与历史任一助手消息相同的输出视为改写
        // 失败 → 回退当前检索词，避免拿聊天回复去检索。
        let echoes_history = parsed.iter().any(|query| {
            history
                .iter()
                .filter(|message| message.role != "user")
                .any(|message| {
                    message
                        .content
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        == *query
                })
        });
        if echoes_history { Vec::new() } else { parsed }
    };
    let mut retrieval_questions = if rewritten_queries.is_empty() {
        vec![request.question.trim().to_owned()]
    } else {
        rewritten_queries
    };
    // CASE 9 轻量规范化双路径：拼音混写（开fa→开发）、全半角、大小写等
    // 变体与原词一起参与检索（原词优先召回，变体补召回；拼音展开只作用于
    // 「CJK+拼音音节」相邻片段，绝不改写 RAG/LangGraph 等专有名词）。仅单
    // 检索词时启用，改写已拆分多问题时不再叠加（避免查询膨胀）。
    if retrieval_questions.len() == 1 {
        for variant in normalize_query_variants(&retrieval_questions[0])
            .into_iter()
            .skip(1)
        {
            retrieval_questions.push(variant);
        }
    }
    let history_count = history.len();
    let rewritten_marked = retrieval_questions.len() > 1
        || retrieval_questions
            .first()
            .is_some_and(|question| question != request.question.trim());
    // 改写只在 content_query 上做；跳过（DOCUMENT_SUMMARY）或改写失败
    // （空结果/复读历史）时回退当前检索词，节点如实记录原因。
    let rewrite_outcome = if skip_query_rewrite {
        "skipped"
    } else if rewritten_marked {
        "applied"
    } else {
        "fallback_original"
    };
    trace_node(
        catalog,
        "ask",
        "query_rewrite",
        &correlation_id,
        session_id_ref,
        None,
        &json!({ "question": request.question, "history_count": history_count }),
        &json!({
            "rewritten_queries": &retrieval_questions,
            "rewritten": rewritten_marked,
            "skip_query_rewrite": skip_query_rewrite,
            "outcome": rewrite_outcome,
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
    let embedding_started = Instant::now();
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
    let embedding_ms = embedding_started.elapsed().as_millis() as u64;
    if response.vectors.len() != retrieval_questions.len() {
        return Err(AppError::new(
            "EMBEDDING_EMPTY",
            "Embedding 运行时没有返回全部查询向量",
            true,
        ));
    }
    let artifact_id = embedding.artifact_id.to_string();
    // 文档级召回（Step 9，spec 十一.5 / 十二）：scope 为空的全库资料请求
    // （LibraryQa / MultiDocumentQa）先定位相关文档候选集，再把 chunk 检索
    // 约束在候选文档内部——不要让所有 chunk 永远直接参与竞争。召回失败/
    // 无结果 → 保留 wider chunk retrieval 兜底，绝不中断（增益层，任何
    // 失败都在 run_document_recall 内部吞掉并如实 trace）。
    let mut scoped = request.clone();
    // 文档级召回为空（spec 十二 DOCUMENT_RECALL_EMPTY 分类依据）：recall 没
    // 定位到任何相关文档 → 后续 NO_EVIDENCE 的根因记在召回层。
    let mut document_recall_empty = false;
    if document_recall && scoped.scope.file_ids.is_empty() {
        let recalled = run_document_recall(
            catalog,
            &retrieval_questions[0],
            Some(&response.vectors[0]),
            &correlation_id,
            session_id_ref,
        );
        if !recalled.is_empty() {
            scoped.scope.file_ids = recalled;
        } else {
            document_recall_empty = true;
        }
    }
    let mut sub_results = Vec::with_capacity(retrieval_questions.len());
    for (question, vector) in retrieval_questions.iter().zip(response.vectors.iter()) {
        let mut sub_request = scoped.clone();
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
    // 检索计时：FTS + 语义 + RRF 在 answer_extractively 内部合并执行，
    // 该总耗时已覆盖 fts/semantic/rrf/mmr（core 内不可再拆，如实记录）。
    let retrieval_elapsed_ms = sub_results
        .iter()
        .map(|result| result.elapsed_ms)
        .sum::<u64>();
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
            "embedding_ms": embedding_ms,
            "retrieval_elapsed_ms": retrieval_elapsed_ms,
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
        // NO_EVIDENCE 六分类（spec 十二）：子查询根因已在 storage 层写入；
        // 无子查询根因时按阶段优先级取 TARGET_NOT_RESOLVED（调用方预置）>
        // DOCUMENT_RECALL_EMPTY > TRUE_NO_EVIDENCE。
        if extractive.no_evidence_reason.is_none() {
            extractive.no_evidence_reason = Some(
                resolution_reason
                    .or_else(|| {
                        document_recall_empty.then_some(NoEvidenceReason::DocumentRecallEmpty)
                    })
                    .unwrap_or(NoEvidenceReason::TrueNoEvidence),
            );
        }
        extractive.answer_mode = AnswerMode::RagRefusal;
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
                "no_evidence_reason": extractive
                    .no_evidence_reason
                    .map(|reason| reason.as_str()),
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
                // 问题无关，按 LOCAL 检索失败处理（返回固定文案，不转闲聊，
                // 不拿弱相关片段当答案）。阈值据 trace 实测调优。
                let top_score = extractive
                    .claims
                    .first()
                    .and_then(|claim| claim.citations.first())
                    .map(|citation| citation.retrieval_score)
                    .unwrap_or(0.0);
                if top_score < RERANK_NO_EVIDENCE_THRESHOLD {
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
                            "fallback": "rerank_no_evidence",
                            "top_score": top_score,
                            "threshold": RERANK_NO_EVIDENCE_THRESHOLD,
                        }),
                        "ok",
                        None,
                    );
                    // LOCAL 检索失败：清空证据与来源，按固定文案返回
                    //（与检索阶段无证据的 rag_refusal 一致，前端据此
                    // 引导用户补充资料或换种说法）。
                    extractive.claims.clear();
                    extractive.source_files.clear();
                    extractive.used_file_ids.clear();
                    extractive.insufficient_evidence = true;
                    extractive.grounding_status = fanfan_core::GroundingStatus::Insufficient;
                    // 与检索阶段无证据的固定文案一致（assemble_extractive_answer）
                    extractive.answer =
                        "当前资料中未找到足够依据。你可以换一种说法、扩大检索范围，或等待相关资料完成索引。"
                            .to_owned();
                    extractive.answer_mode = AnswerMode::RagRefusal;
                    extractive.degradation_reason = Some(
                        "候选证据与问题相关性过低（rerank top-1 低于阈值），已拒绝生成".to_owned(),
                    );
                    // NO_EVIDENCE 六分类：rerank 拒答根因（spec 十二 RERANK_REJECTED）
                    extractive.no_evidence_reason = Some(NoEvidenceReason::RerankRejected);
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
                            "no_evidence_reason": "RERANK_REJECTED",
                        }),
                        "ok",
                        Some(extractive.elapsed_ms),
                    );
                    catalog.record_ask_exchange(request, &extractive)?;
                    phase("completed", 1.0);
                    return Ok(extractive);
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
    // 6.5 Answerability Gate（Phase 4.2 spec 二/三/五）：最终 Generation 前
    // 判断「这些 Evidence 是否足够直接回答当前问题」。Embedding/Rerank 分数
    // 只是召回信号——实体明显不一致（CASE A：RAG 问题 + Agent 证据）或
    // 存在性断言缺少项目语境证据（概念证据不能证明「做过项目」）时，
    // 禁止进入普通生成，按 LOCAL 无证据统一文案拒答。
    phase("answerability_gate", 0.52);
    let gate_evidence: Vec<GateEvidence> = extractive
        .claims
        .iter()
        .map(|claim| GateEvidence {
            text: claim.text.clone(),
            heading: claim
                .citations
                .first()
                .and_then(|citation| citation.locator.heading_path.last().cloned()),
        })
        .collect::<Vec<_>>();
    let gate_plan = plan.cloned().unwrap_or_else(|| QueryPlan {
        operation: QueryOperation::Qa,
        ..QueryPlan::default()
    });
    let gate_input = AnswerabilityInput {
        question: original_question,
        content_query: Some(request.question.trim()),
        plan: &gate_plan,
        evidence: &gate_evidence,
    };
    let verdict = evaluate_answerability(&gate_input);
    trace_node(
        catalog,
        "ask",
        "answerability_gate",
        &correlation_id,
        session_id_ref,
        None,
        &json!({
            "question": original_question,
            "content_query": request.question,
            "evidence_count": gate_evidence.len(),
        }),
        &json!({
            "answerability_status": verdict.status.as_str(),
            "answerability_reason": verdict.reason,
            "answerability_confidence": verdict.confidence,
            "answer_shape": verdict.answer_shape.as_str(),
            "query_entities": verdict.query_entities,
            "evidence_entities": verdict.evidence_entities,
            "missing_entities": verdict.missing_entities,
            "evidence_roles": verdict
                .evidence_roles
                .iter()
                .map(|role| role.as_str())
                .collect::<Vec<_>>(),
        }),
        if verdict.status == AnswerabilityStatus::NotAnswerable {
            "rejected"
        } else {
            "ok"
        },
        None,
    );
    if verdict.status == AnswerabilityStatus::NotAnswerable {
        // LOCAL 拒答：统一无证据文案（spec 十六），绝不转闲聊、绝不追加通用知识
        let requires_project = existence_requires_project_context(original_question, &gate_plan);
        extractive.claims.clear();
        extractive.source_files.clear();
        extractive.used_file_ids.clear();
        extractive.insufficient_evidence = true;
        extractive.grounding_status = fanfan_core::GroundingStatus::Insufficient;
        extractive.answer = local_no_evidence_answer(
            original_question,
            &verdict.missing_entities,
            requires_project,
        );
        extractive.answer_mode = AnswerMode::RagRefusal;
        extractive.degradation_reason =
            Some(format!("Answerability Gate 拒绝：{}", verdict.reason));
        extractive.no_evidence_reason = Some(NoEvidenceReason::AnswerabilityRejected);
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
                "no_evidence_reason": "ANSWERABILITY_REJECTED",
                "answerability_status": verdict.status.as_str(),
                "answerability_reason": verdict.reason,
                "answer_shape": verdict.answer_shape.as_str(),
            }),
            "ok",
            Some(extractive.elapsed_ms),
        );
        catalog.record_ask_exchange(request, &extractive)?;
        phase("completed", 1.0);
        return Ok(extractive);
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
    let mut prompt = fanfan_core::generation_prompt(request, &extractive, history);
    if !image_analysis_context.is_empty() {
        prompt.push_str(
            "\n\n以下是针对当前问题重新查看候选原图得到的辅助观察。它不能替代[S数字]原始引用；只有同时受到原始引用支持的事实才能写入答案：\n",
        );
        prompt.push_str(&image_analysis_context.join("\n"));
    }
    // Answer Semantics（spec 四）：按 answer_shape 约束回答结构（如
    // BOOLEAN_EXISTENCE 第一句必须给出存在性结论）；PARTIAL 时限定
    // 只回答证据明确支持的部分（spec 二）。
    let mut shape_directive = answer_shape_directive(verdict.answer_shape);
    if verdict.status == AnswerabilityStatus::Partial {
        shape_directive.push_str(&format!(
            "\n【证据覆盖提示】\n当前证据只覆盖了问题的一部分（未覆盖：{}）。只回答证据明确支持的部分，并说明资料只支持哪部分；未覆盖的部分不要猜测，也不要用通用知识补齐。",
            verdict.missing_entities.join("、")
        ));
    }
    prompt.push_str("\n\n");
    prompt.push_str(&shape_directive);
    let answer_schema = fanfan_core::grounded_answer_json_schema();
    let mut generated = runtime
        .complete_json_cancellable(
            LOCAL_STRICT_SYSTEM_PROMPT,
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
    // spec 十八：本轮是否拦截过 LOCAL 外部知识泄漏（completed trace 汇总）
    let mut local_external_knowledge_blocked = false;
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
        // Unsupported Claim Gate（spec 十五）+ LOCAL 外部知识拦截（spec 六）：
        // 1) 主体一致性（确定性）：claim 的关键实体必须出现在它自己引用的
        //    证据里——Evidence 讲 Agent、Claim 说 RAG → UNSUPPORTED；
        // 2) 非逐字引用的 claim 出现「通常来说/一般来说/建议联系管理员」等
        //    标记 → 判为混入外部通用知识，拦截（CASE Transformer）。
        // 逐字引用（deterministic 支持）跳过两项检查——标记若来自证据原文
        //    则不算外部知识。
        let quotes: Vec<&str> = claim
            .citations
            .iter()
            .map(|citation| citation.quote.as_str())
            .collect();
        let mut unsupported_claim_reason: Option<String> = None;
        if !deterministically_supported {
            if let Some(entity) = claim_subject_mismatch(&claim.text, &quotes) {
                unsupported_claim_reason = Some(format!("subject_entity_mismatch:{entity}"));
            } else if let Some(marker) = find_external_knowledge_marker(&claim.text) {
                unsupported_claim_reason = Some(format!("external_knowledge_blocked:{marker}"));
                local_external_knowledge_blocked = true;
            }
        }
        let mut llm_verdict = None;
        let supported = if unsupported_claim_reason.is_some() {
            false
        } else if deterministically_supported {
            true
        } else {
            let verification = complete_with_model(
                generation,
                &generation_artifact,
                "你是严格的中文证据核验员。判断「事实句」是否完全由「原文证据」支持：\
判断依据是证据是否足以支持事实句中的主体、关系和结论——事实句的每个要点都要能在证据中找到对应文字；\
主体不同（证据讲的是另一个概念，如证据讲 Agent 而事实句说 RAG）也算 UNSUPPORTED。\
只输出一个词：SUPPORTED 或 UNSUPPORTED，不要输出解释、标点或多余文字。",
                &format!(
                    "【示例一】\n事实句：公司的报销流程是先填单再审批\n原文证据：\n[E1] 报销需先填写报销单，经部门主管审批后方可发放\n输出：SUPPORTED\n\n【示例二】\n事实句：公司的报销上限是五千元\n原文证据：\n[E1] 员工报销需提供正规发票\n输出：UNSUPPORTED\n\n【示例三】\n事实句：RAG 是一种根据目标判断下一步并选择工具的能力\n原文证据：\n[E1] Agent 根据目标判断下一步、选择工具并读取工具结果\n输出：UNSUPPORTED\n\n【正式任务】\n事实句：{}\n\n原文证据：\n{}",
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
                "unsupported_claim_reason": unsupported_claim_reason,
                "local_external_knowledge_blocked": unsupported_claim_reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("external_knowledge_blocked")),
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
        grounded.answer_mode = AnswerMode::Generated;
        grounded.degradation_reason = Some(format!(
            "有{rejected_claims}个候选事实句未通过原文支持性校验，已自动隐藏"
        ));
    }
    grounded.index_coverage = index_coverage;
    grounded.retrieval_channels = extractive.retrieval_channels;
    // Step 11：EXTRACT operation（spec 十六 CASE 1）→ 把已验证的 grounded
    // 回答重组为「条目 + 每项证据」结构化列表。重组失败/空条目 → 原样保留
    // 已验证回答（绝不 crash、不劣化）。重组后的 claims 逐条 verified_claim
    // 并重新过 validate_answer_evidence（引用证据都是已验证的真实 chunk）。
    if gate_plan.operation == QueryOperation::Extract
        && !grounded.claims.is_empty()
        && restructure_as_extract(
            catalog,
            generation,
            &generation_artifact,
            request,
            // LLM Parser 语义判断：EXTRACT 清单条目是否必须是实体/名称形式
            // （如项目名称），替代原 is_project_list_question 关键词表。
            gate_plan.requires_entity_items,
            &mut grounded,
            &correlation_id,
            session_id_ref,
            cancelled,
        )?
    {
        for claim in &grounded.claims {
            verified_claim(claim);
        }
        catalog.validate_answer_evidence(&grounded)?;
    }
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
            "answerability_status": verdict.status.as_str(),
            "answerability_reason": verdict.reason,
            "answer_shape": verdict.answer_shape.as_str(),
            "local_external_knowledge_blocked": local_external_knowledge_blocked,
        }),
        "ok",
        Some(grounded.elapsed_ms),
    );
    catalog.validate_answer_evidence(&grounded)?;
    catalog.record_ask_exchange(request, &grounded)?;
    phase("completed", 1.0);
    Ok(grounded)
}

/// Step 11：EXTRACT operation（spec 十六 CASE 1「我的简历里面有哪些项目」）。
///
/// 把已验证的 grounded 回答重组为「条目 + 每项证据」结构化列表：
/// 模型只产出条目清单（`extract_schema` JSON 约束），每条目的引用证据由
/// 确定性最长公共子串对齐到已验证的真实 chunk 原文（≥ EXTRACT_MATCH_MIN_LEN
/// 字符才算命中）；未命中任何证据的条目回退到重排分最高的证据。抽取生成
/// 失败或空条目 → 返回 false，调用方原样保留已验证回答（绝不 crash、不
/// 劣化既有结果）。模型输出的 evidence 摘引只作展示，不作为引用证据。
///
/// 返回 true 表示已完成重组（调用方负责逐条 verified_claim 与
/// validate_answer_evidence）。
#[allow(clippy::too_many_arguments)]
fn restructure_as_extract(
    catalog: &CatalogService,
    generation: &Mutex<LocalGenerationRuntime>,
    generation_artifact: &ModelArtifact,
    request: &AskRequest,
    // LLM Parser 语义判断：清单条目必须是实体/名称形式（如项目名称）时，
    // prompt 追加实体规范，并对模型输出做 spec 十二实体形态校验。
    requires_entity_items: bool,
    grounded: &mut AnswerResult,
    correlation_id: &str,
    session_id_ref: Option<&str>,
    cancelled: &AtomicBool,
) -> Result<bool, AppError> {
    let started = Instant::now();
    let materials = grounded
        .claims
        .iter()
        .enumerate()
        .map(|(index, claim)| {
            let quotes = claim
                .citations
                .iter()
                .map(|citation| compact_for_prompt(&citation.quote, 220))
                .collect::<Vec<_>>()
                .join("；");
            format!(
                "[M{}] {}（证据：{}）",
                index + 1,
                compact_for_prompt(&claim.text, 200),
                quotes
            )
        })
        .collect::<Vec<_>>();
    let (system, user) = extract_prompt(request.question.trim(), requires_entity_items, &materials);
    let raw = complete_json_with_model(
        generation,
        generation_artifact,
        &system,
        &user,
        400,
        &extract_schema(),
        cancelled,
    )?;
    let Some(results) = parse_extract_results(&raw) else {
        trace_node(
            catalog,
            "ask",
            "extract",
            correlation_id,
            session_id_ref,
            None,
            &json!({ "question": request.question, "material_count": materials.len() }),
            &json!({ "status": "empty_or_unparseable", "fallback": "keep_grounded_answer" }),
            "ok",
            Some(started.elapsed().as_millis() as u64),
        );
        return Ok(false);
    };
    // 每条目 → 确定性证据对齐（最长公共子串；不信任模型自报编号）。
    let mut new_claims = Vec::with_capacity(results.items.len());
    let mut matched_count = 0_usize;
    // spec 十二类型验证：LLM 判定为实体清单（如项目列表）时，条目必须是
    // 「实体/标题式文本」；完整描述句（「大模型不仅负责生成文本，还会…」）
    // 即使与证据有公共子串也当不了 project_name，直接丢弃（宁缺毋滥）。
    let mut rejected_entity_form = 0_usize;
    for item in results.items.iter().take(EXTRACT_MAX_ITEMS) {
        if requires_entity_items && !extract_item_is_entity_like(item.item.trim()) {
            rejected_entity_form += 1;
            continue;
        }
        let mut best: Option<(usize, EvidenceRef)> = None;
        for claim in &grounded.claims {
            for citation in &claim.citations {
                let overlap = longest_common_substr_len(&item.item, &citation.quote);
                if overlap >= EXTRACT_MATCH_MIN_LEN
                    && best.as_ref().is_none_or(|(current, _)| overlap > *current)
                {
                    best = Some((overlap, citation.clone()));
                }
            }
        }
        let citations = match best {
            Some((_, citation)) => {
                matched_count = matched_count.saturating_add(1);
                vec![citation]
            }
            // 回退：重排分最高的证据（保持每项都有真实出处）
            None => grounded
                .claims
                .iter()
                .flat_map(|claim| claim.citations.iter())
                .max_by(|left, right| left.retrieval_score.total_cmp(&right.retrieval_score))
                .cloned()
                .into_iter()
                .collect(),
        };
        let mut text = item.item.clone();
        if !item.evidence.is_empty() {
            text.push_str(&format!("\n证据：{}", item.evidence));
        }
        new_claims.push(AnswerClaim {
            claim_id: Uuid::now_v7(),
            text,
            support_status: if citations.is_empty() {
                SupportStatus::Partial
            } else {
                SupportStatus::Supported
            },
            citations,
        });
    }
    if new_claims.is_empty() {
        trace_node(
            catalog,
            "ask",
            "extract",
            correlation_id,
            session_id_ref,
            None,
            &json!({ "question": request.question, "material_count": materials.len() }),
            &json!({ "status": "no_items", "fallback": "keep_grounded_answer" }),
            "ok",
            Some(started.elapsed().as_millis() as u64),
        );
        return Ok(false);
    }
    grounded.claims = new_claims;
    grounded.answer = grounded
        .claims
        .iter()
        .enumerate()
        .map(|(index, claim)| format!("{}. {}", index + 1, claim.text))
        .collect::<Vec<_>>()
        .join("\n");
    grounded.answer_mode = AnswerMode::Extract;
    grounded
        .retrieval_channels
        .push("structured_extract".into());
    trace_node(
        catalog,
        "ask",
        "extract",
        correlation_id,
        session_id_ref,
        None,
        &json!({ "question": request.question, "material_count": materials.len() }),
        &json!({
            "status": "ok",
            "item_count": grounded.claims.len(),
            "matched_evidence": matched_count,
            "fallback_evidence_used": grounded.claims.len().saturating_sub(matched_count),
            "rejected_entity_form": rejected_entity_form,
        }),
        "ok",
        Some(started.elapsed().as_millis() as u64),
    );
    Ok(true)
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
        // NO_EVIDENCE 根因：保留第一个子查询的根因（最早发生、最贴近目标）
        if merged.no_evidence_reason.is_none() {
            merged.no_evidence_reason = other.no_evidence_reason;
        }
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
        spawn_image_ocr_pending(app.clone(), Arc::clone(&catalog));
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

pub(crate) fn spawn_image_ocr_pending(app: AppHandle, catalog: Arc<CatalogService>) {
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
        let worker = app.state::<WorkerServiceState>();
        if worker.image_ocr_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let _running_reset = RunningReset(&worker.image_ocr_running);
        match catalog.backfill_ready_image_search_nodes(500) {
            Ok(file_ids) if !file_ids.is_empty() => {
                for file_id in &file_ids {
                    let _ = catalog.promote_ocr_pending_file_when_assets_ready(file_id);
                    let _ = app.emit("index:changed", file_id.to_string());
                }
                crate::runtime_log::event(
                    "info",
                    "index",
                    "image_search.backfill_completed",
                    None,
                    &json!({ "file_count": file_ids.len() }),
                );
                spawn_embed_pending(app.clone(), Arc::clone(&catalog));
            }
            Ok(_) => {}
            Err(error) => crate::runtime_log::event(
                "warning",
                "index",
                "image_search.backfill_failed",
                None,
                &json!({ "error_code": error.code, "retryable": error.retryable }),
            ),
        }
        let threads = background_inference_threads();
        let models = app.state::<ModelServiceState>();
        let models = match models.get() {
            Ok(models) => models,
            Err(_) => return,
        };
        let artifact = match models.active_artifact(ModelRole::Ocr) {
            Ok(Some(artifact)) => artifact,
            Ok(None) => return,
            Err(error) => {
                crate::runtime_log::event(
                    "error",
                    "ocr",
                    "image_ocr.model_lookup_failed",
                    None,
                    &json!({ "error_code": error.code, "retryable": error.retryable }),
                );
                return;
            }
        };
        let runtime = match active_ocr_runtime(&app, threads) {
            Ok(Some(runtime)) => runtime,
            Ok(None) => return,
            Err(error) => {
                crate::runtime_log::event(
                    "error",
                    "ocr",
                    "image_ocr.runtime_resolution_failed",
                    None,
                    &json!({ "error_code": error.code, "retryable": error.retryable }),
                );
                return;
            }
        };
        let artifact_id = artifact.artifact_id.to_string();
        let cycle_id = Uuid::now_v7().to_string();
        let cycle_started = Instant::now();
        let mut ready = 0_u64;
        let mut routed_to_vision = 0_u64;
        let mut failed = 0_u64;
        crate::runtime_log::event(
            "info",
            "ocr",
            "image_ocr.cycle_started",
            Some(&cycle_id),
            &json!({ "model_artifact_id": artifact_id }),
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
            if degradation == "core" {
                break;
            }
            let runtime_manager = app.state::<RuntimeManagerState>();
            let mut runtime_request = RuntimeTaskRequest::interactive(
                RuntimeTaskKind::Ocr,
                RuntimeBackendKind::PaddleOcr,
            );
            runtime_request.cpu_threads = threads;
            runtime_request.memory_bytes = 512 * 1024 * 1024;
            runtime_request.timeout = Duration::from_secs(2);
            runtime_request.model_id = Some(artifact_id.clone());
            let runtime_lease = match runtime_manager.0.acquire(runtime_request) {
                Ok(lease) => lease,
                Err(error) => {
                    crate::runtime_log::event(
                        "info",
                        "runtime",
                        "runtime.background_deferred",
                        Some(&cycle_id),
                        &json!({ "task_kind": "image_ocr", "error_code": error.code }),
                    );
                    break;
                }
            };
            let pending = match catalog.claim_pending_image_ocr(&artifact_id) {
                Ok(Some(pending)) => pending,
                Ok(None) => {
                    runtime_lease.complete();
                    break;
                }
                Err(error) => {
                    runtime_lease.fail(error.code.clone());
                    crate::runtime_log::event(
                        "error",
                        "ocr",
                        "image_ocr.pending_claim_failed",
                        Some(&cycle_id),
                        &json!({ "error_code": error.code, "retryable": error.retryable }),
                    );
                    break;
                }
            };
            let _ = app.emit(
                "ocr:progress",
                json!({
                    "asset_id": pending.asset_id,
                    "revision_id": pending.revision_id,
                    "stage": "image_ocr",
                    "attempt": pending.attempt_count,
                }),
            );
            let page_no = pending.locator.page_no.unwrap_or(1);
            let sidecars = app.state::<SidecarRegistryState>();
            let operation = sidecars.0.ocr.route_image_ocr(&ImageOcrRoutingRequest {
                model_path: runtime.model_path.clone(),
                det_model_path: runtime.det_model_path.clone(),
                cls_model_path: runtime.cls_model_path.clone(),
                dictionary_path: runtime.dictionary_path.clone(),
                image_path: pending.cache_path.clone(),
                page_no,
                threads: runtime.threads,
                ocr_version: runtime.ocr_version.clone(),
                confidence_threshold: 0.45,
                asset_kind: pending.asset_kind.clone(),
            });
            match operation.and_then(|routed| {
                let result = ImageOcrResult {
                    asset_id: pending.asset_id,
                    revision_id: pending.revision_id,
                    model_artifact_id: artifact_id.clone(),
                    ocr_text: routed.ocr_text,
                    confidence: routed.confidence,
                    engine: routed.engine,
                    model_version: routed.model_version,
                    vision_required: routed.vision_required,
                    route_reason: routed.route_reason,
                    attempts: routed.attempts,
                    idempotency_key: pending.idempotency_key.clone(),
                };
                catalog.commit_image_ocr(&result)?;
                Ok(result)
            }) {
                Ok(result) => {
                    runtime_lease.complete();
                    if result.vision_required {
                        routed_to_vision = routed_to_vision.saturating_add(1);
                    } else {
                        ready = ready.saturating_add(1);
                        if catalog
                            .promote_ocr_pending_file_when_assets_ready(&pending.file_id)
                            .unwrap_or(false)
                        {
                            let _ = app.emit("catalog:changed", pending.file_id.to_string());
                        }
                    }
                    let _ = app.emit(
                        "ocr:completed",
                        json!({
                            "asset_id": pending.asset_id,
                            "revision_id": pending.revision_id,
                            "vision_required": result.vision_required,
                            "route_reason": result.route_reason,
                        }),
                    );
                    let _ = app.emit("index:changed", pending.file_id.to_string());
                }
                Err(error) => {
                    runtime_lease.fail(error.code.clone());
                    failed = failed.saturating_add(1);
                    let _ = catalog.fail_image_ocr(&pending.asset_id, &error);
                    crate::runtime_log::event(
                        "error",
                        "ocr",
                        "image_ocr.asset_failed",
                        Some(&cycle_id),
                        &json!({
                            "asset_id": pending.asset_id,
                            "revision_id": pending.revision_id,
                            "attempt": pending.attempt_count,
                            "error_code": error.code,
                            "retryable": error.retryable,
                        }),
                    );
                }
            }
            thread::yield_now();
        }
        crate::runtime_log::event(
            if failed > 0 { "warning" } else { "info" },
            "ocr",
            "image_ocr.cycle_completed",
            Some(&cycle_id),
            &json!({
                "ready_assets": ready,
                "routed_to_vision": routed_to_vision,
                "failed_assets": failed,
                "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
            }),
        );
        drop(_running_reset);
        if routed_to_vision > 0 || failed > 0 {
            spawn_image_understanding_pending(app.clone(), Arc::clone(&catalog));
        }
        if ready > 0 {
            spawn_embed_pending(app, catalog);
        }
    });
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
            // 推理锁带时限：VLM 推理可能被其他推理长时间占用，拿不到锁就跳过
            // 本张图（下个 cycle 重试），绝不无限期阻塞等锁的查询与 close。
            let runtime_guard =
                match try_lock_generation_until(&generation.0, Duration::from_millis(2_000)) {
                    Some(guard) => guard,
                    None => {
                        crate::runtime_log::event(
                            "info",
                            "vision",
                            "vision.runtime_busy",
                            Some(&cycle_id),
                            &json!({ "reason": "generation_runtime_locked" }),
                        );
                        runtime_lease.complete();
                        break;
                    }
                };
            let operation = (|| {
                let mut runtime = runtime_guard;
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
                    match catalog.promote_ocr_pending_file_when_assets_ready(&pending.file_id) {
                        Ok(true) => {
                            crate::runtime_log::event(
                                "info",
                                "vision",
                                "vision.file_promoted",
                                Some(&cycle_id),
                                &json!({ "file_id": pending.file_id }),
                            );
                            let _ = app.emit("catalog:changed", pending.file_id.to_string());
                        }
                        Ok(false) => {}
                        Err(error) => crate::runtime_log::event(
                            "warning",
                            "vision",
                            "vision.file_promote_failed",
                            Some(&cycle_id),
                            &json!({
                                "file_id": pending.file_id,
                                "error_code": error.code,
                                "retryable": error.retryable,
                            }),
                        ),
                    }
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
        if let Some(mut runtime) =
            try_lock_generation_until(&generation.0, Duration::from_millis(2_000))
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
        // 嵌入收敛后：后台构建文档画像（画像要求当前 revision 全量嵌入完成，
        // 所以放在嵌入循环结束后）。与嵌入共用线程，避免新增调度点；前台
        // 活动期间让出，等待下一次 spawn_embed_pending 再继续。
        loop {
            if worker.foreground_activity.load(Ordering::Acquire) > 0 {
                break;
            }
            if !run_profile_build_cycle(&app, &catalog, &worker) {
                break;
            }
            thread::yield_now();
        }
    });
}

/// 画像构建每批最大文件数（与集合建议一致的上限）。
const PROFILE_BUILD_BATCH: u32 = 200;

/// 分类扫描每批最大画像数（与画像构建同量级）。
const CLASSIFY_BATCH: u32 = 200;

/// 文档画像构建循环（Step 1 + Step 2）：每次调用处理一批（≤[`PROFILE_BUILD_BATCH`]）
/// 「已解析 + 全量嵌入」的文件，画像就绪后立即对仍未分类的画像做类型判定。
///
/// 返回 true 表示本批已满（画像或分类可能还有更多，继续下一批）；
/// 返回 false 表示无事可做或出错（本轮结束，下次 spawn 再试）。
///
/// 构建/分类失败只记日志：画像与类型只影响 Document Resolver 的定位精度，
/// 绝不影响浏览 / FTS / 语义检索 / 基础 RAG。
fn run_profile_build_cycle(
    app: &AppHandle,
    catalog: &CatalogService,
    worker: &WorkerServiceState,
) -> bool {
    let models_state = app.state::<ModelServiceState>();
    let models = match models_state.get() {
        Ok(models) => models,
        Err(error) => {
            crate::runtime_log::event(
                "warning",
                "profile",
                "profile.cycle_failed",
                None,
                &json!({"error_code": error.code}),
            );
            return false;
        }
    };
    let artifact = match models.active_artifact(ModelRole::Embedding) {
        Ok(Some(artifact)) => artifact,
        Ok(None) => return false, // 未启用 Embedding 模型，无法计算画像向量
        Err(error) => {
            crate::runtime_log::event(
                "warning",
                "profile",
                "profile.cycle_failed",
                None,
                &json!({"error_code": error.code}),
            );
            return false;
        }
    };
    let artifact_id = artifact.artifact_id.to_string();
    if worker.foreground_activity.load(Ordering::Acquire) > 0 {
        return false;
    }
    let cycle_id = Uuid::now_v7().to_string();
    let cycle_started = Instant::now();
    match catalog.refresh_document_profiles(&artifact_id, PROFILE_BUILD_BATCH) {
        Ok(result) => {
            if result.profiled_files > 0 || result.skipped_files > 0 {
                crate::runtime_log::event(
                    "info",
                    "profile",
                    "profile.cycle_completed",
                    Some(&cycle_id),
                    &json!({
                        "profiled_files": result.profiled_files,
                        "skipped_files": result.skipped_files,
                        "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
                    }),
                );
            }
            // 画像就绪后立即尝试分类（Step 2）：纯计算 + 少量回写，失败只记日志
            let attempted = run_classification_pass(app, catalog, &artifact);
            // 画像批次打满或分类还有待处理画像 → 继续下一轮
            result.profiled_files >= u64::from(PROFILE_BUILD_BATCH)
                || attempted >= u64::from(CLASSIFY_BATCH)
        }
        Err(error) => {
            crate::runtime_log::event(
                "warning",
                "profile",
                "profile.cycle_failed",
                Some(&cycle_id),
                &json!({
                    "error_code": error.code,
                    "elapsed_ms": cycle_started.elapsed().as_millis() as u64,
                }),
            );
            false
        }
    }
}

/// 分类原型向量缓存（进程级，按 Embedding 模型 artifact_id 缓存）。
/// 原型 = TYPE_PROTOTYPE_TEXTS 逐类型取句向量均值并 L2 归一化；
/// 模型不变则只计算一次，全进程复用（模型切换时自然重算）。
type PrototypeVector = (DocumentType, Vec<f32>);
type PrototypeVectorCache = HashMap<String, Vec<PrototypeVector>>;

static PROTOTYPE_VECTORS: OnceLock<Mutex<PrototypeVectorCache>> = OnceLock::new();

fn prototype_cache() -> MutexGuard<'static, PrototypeVectorCache> {
    PROTOTYPE_VECTORS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 取当前 Embedding 模型的分类原型向量：缓存命中直接返回，否则用 onnx
/// 对 TYPE_PROTOTYPE_TEXTS 编码一次、按类型聚合。失败返回 Err——分类是
/// best-effort，调用方记日志后跳过本轮即可。
fn prototype_vectors_for(
    worker: &WorkerClient,
    runtime_manager: &RuntimeManager,
    artifact: &ModelArtifact,
) -> Result<Vec<(DocumentType, Vec<f32>)>, AppError> {
    let key = artifact.artifact_id.to_string();
    if let Some(cached) = prototype_cache().get(&key) {
        return Ok(cached.clone());
    }
    let texts = TYPE_PROTOTYPE_TEXTS
        .iter()
        .flat_map(|(_, prototype_texts)| prototype_texts.iter().copied())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let tokenizer_path = PathBuf::from(&artifact.local_path)
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
            "Embedding tokenizer 不存在，文档类型分类已跳过",
            false,
        ));
    }
    let mut runtime_request = RuntimeTaskRequest::interactive(
        RuntimeTaskKind::Embedding,
        RuntimeBackendKind::OnnxRuntime,
    );
    runtime_request.cpu_threads = 2;
    runtime_request.timeout = Duration::from_secs(30);
    runtime_request.model_id = Some(key.clone());
    let runtime_lease = runtime_manager.acquire(runtime_request)?;
    let response = worker.encode_embeddings(&EmbeddingRequest {
        model_path: artifact.local_path.clone(),
        tokenizer_path: Some(tokenizer_path.to_string_lossy().into_owned()),
        texts,
        max_length: artifact.max_length.unwrap_or(512),
        threads: 2,
    })?;
    runtime_lease.complete();
    if response.dimension == 0
        || response.vectors.len()
            != TYPE_PROTOTYPE_TEXTS
                .iter()
                .map(|(_, texts)| texts.len())
                .sum::<usize>()
    {
        return Err(AppError::new(
            "EMBEDDING_EMPTY",
            "Embedding 运行时没有返回全量原型向量",
            false,
        ));
    }
    let mut prototypes = Vec::with_capacity(TYPE_PROTOTYPE_TEXTS.len());
    let mut offset = 0;
    for (document_type, prototype_texts) in TYPE_PROTOTYPE_TEXTS {
        let count = prototype_texts.len();
        let vectors = response.vectors[offset..offset + count].to_vec();
        offset += count;
        let mut mean = vec![0.0_f32; response.dimension as usize];
        for vector in &vectors {
            for (target, value) in mean.iter_mut().zip(vector) {
                *target += value;
            }
        }
        let norm = mean.iter().map(|value| value * value).sum::<f32>().sqrt();
        if !norm.is_finite() || norm <= f32::EPSILON {
            continue; // 该类型原型退化（无法归一化）：跳过，不参与比对
        }
        mean.iter_mut().for_each(|value| *value /= norm);
        prototypes.push((*document_type, mean));
    }
    prototype_cache().insert(key, prototypes.clone());
    Ok(prototypes)
}

/// 文档类型分类扫描（Step 2）：把「画像就绪但 document_type 仍为 NULL」的
/// 画像按 规则 + Embedding 原型 判定并回写。返回本轮尝试的画像数；调用方
/// 用它决定是否继续下一轮。
///
/// 无信号（分类函数返回 None）的画像保持 NULL：这是「其他/未定」的显式
/// 表示，绝不猜类型；下次有内容变化（新 revision 触发重建）时自然重判。
/// 分类失败只记日志：类型只影响 Document Resolver 的信号权重与类型化检索，
/// 绝不阻塞索引或问答主链。
fn run_classification_pass(
    app: &AppHandle,
    catalog: &CatalogService,
    artifact: &ModelArtifact,
) -> u64 {
    let pending = match catalog.list_profiles_needing_classification(CLASSIFY_BATCH) {
        Ok(pending) => pending,
        Err(error) => {
            crate::runtime_log::event(
                "warning",
                "profile",
                "profile.classify_failed",
                None,
                &json!({"error_code": error.code, "phase": "list"}),
            );
            return 0;
        }
    };
    if pending.is_empty() {
        return 0; // 无待分类画像：不动原型缓存，不打扰 Embedding 运行时
    }
    let runtime_manager = app.state::<RuntimeManagerState>();
    // 分类原型向量必须由 onnx 角色 worker 编码（parse 角色不加载 embedding
    // 运行时，直接调用会返回 OPERATION_UNSUPPORTED）；与嵌入循环同一取法。
    let onnx_worker = app.state::<SidecarRegistryState>().0.onnx.clone();
    let prototypes = match prototype_vectors_for(&onnx_worker, &runtime_manager.0, artifact) {
        Ok(prototypes) => prototypes,
        Err(error) => {
            crate::runtime_log::event(
                "warning",
                "profile",
                "profile.classify_failed",
                None,
                &json!({"error_code": error.code, "phase": "prototype"}),
            );
            return 0;
        }
    };
    if prototypes.is_empty() {
        return 0; // 原型全部退化：本轮无向量可比较，下轮重试
    }
    let started = Instant::now();
    let mut classified = 0_u64;
    let mut failed = 0_u64;
    for (profile, file_name) in pending {
        // 画像向量读取失败按「无向量」处理：退化为纯规则路径，不阻塞该画像
        let vector = catalog
            .profile_vector(&profile.file_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let prototype_refs = prototypes
            .iter()
            .map(|(document_type, vector)| TypePrototype {
                document_type: *document_type,
                vector,
            })
            .collect::<Vec<_>>();
        let decision = classify_document_type(
            &profile.title,
            &file_name,
            &profile.section_titles,
            &profile.summary,
            &vector,
            &prototype_refs,
        );
        let Some((document_type, confidence)) = decision else {
            continue; // 无信号：保持 NULL，等新内容触发重新判定
        };
        let mut updated = profile.clone();
        updated.document_type = Some(document_type);
        updated.type_confidence = Some(confidence);
        updated.updated_at = Utc::now();
        match catalog.update_document_profile_classifier(&updated) {
            Ok(true) => classified += 1,
            Ok(false) => failed += 1, // 画像被并发删除/重建：下轮由待分类列表补齐
            Err(error) => {
                failed += 1;
                crate::runtime_log::event(
                    "warning",
                    "profile",
                    "profile.classify_failed",
                    None,
                    &json!({"error_code": error.code, "phase": "write"}),
                );
            }
        }
    }
    let attempted = classified + failed;
    crate::runtime_log::event(
        "info",
        "profile",
        "profile.classify_completed",
        None,
        &json!({
            "attempted": attempted,
            "classified": classified,
            "elapsed_ms": started.elapsed().as_millis() as u64,
        }),
    );
    attempted
}

/// 自愈恢复：下载完成后索引构建中断的 Embedding 模型会长期卡在
/// `pending_self_test`，而 `active_artifact` 只认 `ready`，导致后台索引循环
/// 永远拿不到可用模型、静默跳过。这里在「无待处理激活」时，若存在本地文件
/// 完整（包校验通过）的 `pending_self_test` Embedding 模型，则重建激活任务，
/// 使 `run_embedding_cycle` 恢复向量化与索引构建，模型随后由索引完成路径置为
/// `ready`。纯聊天（未装 Embedding）场景不会命中，保持原有静默行为。
fn recover_stuck_embedding_activation(
    models: &ModelManager,
) -> Result<Option<ModelArtifact>, AppError> {
    let candidates = models.list_artifacts()?.into_iter().filter(|artifact| {
        artifact.role == ModelRole::Embedding
            && artifact.format == ModelFormat::Onnx
            && artifact.status == "pending_self_test"
            && Path::new(&artifact.local_path).is_file()
            && artifact
                .package_manifest
                .as_ref()
                .is_some_and(|manifest| manifest.integrity_status == "ready")
    });
    for artifact in candidates {
        // 自检阶段已记录向量维度；缺失时退回默认 512 维
        let dimension = artifact
            .embedding_dimension
            .filter(|dimension| *dimension > 0)
            .unwrap_or(512);
        if let Some(pending) =
            models.begin_embedding_activation_with_job(&artifact.artifact_id, dimension, None)?
        {
            crate::runtime_log::event(
                "warning",
                "embedding",
                "embedding.activation_recovered",
                None,
                &json!({
                    "artifact_id": artifact.artifact_id.to_string(),
                    "dimension": pending.dimension,
                    "reason": "stuck_pending_self_test",
                }),
            );
            return Ok(Some(models.artifact_by_id(&artifact.artifact_id)?));
        }
    }
    Ok(None)
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
    let mut pending = match models.pending_embedding_activation() {
        Ok(Some(pending)) if pending.status == "indexing" => Some(pending),
        Ok(_) => None,
        Err(error) => {
            let _ = app.emit("embedding:failed", error);
            return;
        }
    };
    // 自愈：无待处理激活时，尝试恢复「文件完整但卡在 pending_self_test」的
    // Embedding 模型，重新进入索引构建流程（下载后中断的激活得以继续）。
    if pending.is_none() {
        match recover_stuck_embedding_activation(&models) {
            Ok(Some(_)) => {
                if let Ok(recovered) = models.pending_embedding_activation() {
                    pending = recovered;
                }
            }
            Ok(None) => {}
            Err(error) => {
                let _ = app.emit("embedding:failed", error);
            }
        }
    }
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
    let mut execution_device: Option<String> = None;
    let mut execution_provider: Option<String> = None;
    let mut device_fallback_reason: Option<String> = None;
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
            let onnx_worker = app.state::<SidecarRegistryState>().0.onnx.clone();
            let response = match onnx_worker.encode_embeddings(&EmbeddingRequest {
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
            execution_device = response.device.clone();
            execution_provider = response.execution_provider.clone();
            device_fallback_reason = response.fallback_reason.clone();
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
        // 注意：不能把 item_count 与 searchable_chunks 直接比较——searchable 会随新增
        // 嵌入持续增长，直接比较会让每次启动（即使只嵌入几十个 chunk）都触发 19.9 万条
        // 全量重建，期间 CPU 打满、全部页面查询排队 10-38s。覆盖缺口 <5%（约 1 万
        // chunk）时继续用旧索引，搜索最多短暂 miss 新入库内容，积累到阈值后自动补齐。
        let stale_generation = existing_generation.as_ref().is_none_or(|generation| {
            generation.dimension != dimension
                || generation.item_count == 0
                || (generation.item_count as f64) < (searchable_chunks as f64 * 0.95)
        });
        let needs_rebuild = searchable_chunks > 0 && stale_generation;
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
            // 校验阈值必须与上面的 needs_rebuild（95%）同口径，不能用 0.999999：
            // 增量嵌入滞后时总有几个 chunk 尚无当前模型向量，全量重建后 coverage
            // 稳定差这几条而永远失败，active 永不切换 → 每个 cycle 都触发 19.9 万条
            // 全量重建，CPU 打满、UI 冻结，close 时 shutdown 路径被拖死。
            if generation.status != "active"
                || generation.dimension != dimension
                || generation.item_count == 0
                || generation.coverage < 0.95
            {
                return Err(AppError::new(
                    "VECTOR_INDEX_INCOMPLETE",
                    "新语义索引未覆盖当前有效分块（缺口大于 5%），未切换活动 Embedding",
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
                || generation.coverage < 0.95
            {
                return Err(AppError::new(
                    "VECTOR_INDEX_INCOMPLETE",
                    "新语义索引尚未通过覆盖率校验（缺口大于 5%），未切换活动 Embedding",
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
                    "device": execution_device,
                    "execution_provider": execution_provider,
                    "fallback_reason": device_fallback_reason,
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
            // embedding 激活完成后从下载列表移除。
            let _ = models.remove_download_job(&job.job_id);
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

    #[test]
    fn self_test_visible_text_strips_thinking_segments() {
        // Phase 4.3 CASE F 配套（Qwen3.5 自检失败根因）：剥离  thinking
        // 思维链后以可见文本参与自检判定，避免思维链截断误判回滚。
        // 闭合思维链：只留可见回复
        assert_eq!(
            self_test_visible_text(
                "<think>用户让我确认，我应该简短回复。</think>翻翻本地模型可以工作。"
            ),
            "翻翻本地模型可以工作。"
        );
        // 未闭合（token 截断）：思维链整段丢弃
        assert_eq!(
            self_test_visible_text("翻翻已就绪。<think>我再检查一下输出格"),
            "翻翻已就绪。"
        );
        // 纯思维链截断（无可见文本）得到空串；自检时原始输出非空仍判定通过
        assert_eq!(self_test_visible_text("<think>好的，我需要确认一下"), "");
        // 普通模型无 think 标记：原样返回
        assert_eq!(
            self_test_visible_text("  翻翻本地模型可以工作。  "),
            "翻翻本地模型可以工作。"
        );
        // 多段思维链全部剥离
        assert_eq!(
            self_test_visible_text("<think>a</think>中段<think>b</think>尾段"),
            "中段尾段"
        );
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
        let vulkan = vec!["Vulkan0: Intel(R) Arc(TM) A770: 16.0 GB".to_owned()];
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
            answer_mode: AnswerMode::Generated,
            retrieval_channels: vec!["fts".into()],
            index_coverage: 1.0,
            degradation_reason: None,
            no_evidence_reason: None,
            clarification: None,
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
        let file_id = |_n: u32| Uuid::now_v7();
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
            answer_mode: AnswerMode::Generated,
            retrieval_channels: vec!["fts".into()],
            index_coverage: 1.0,
            degradation_reason: None,
            no_evidence_reason: None,
            clarification: None,
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
            answer_mode: AnswerMode::Generated,
            retrieval_channels: vec!["fts".into()],
            index_coverage: 1.0,
            degradation_reason: None,
            no_evidence_reason: None,
            clarification: None,
        };

        let markdown = render_answer_export(&answer, "md").expect("render export");
        assert!(markdown.contains("项目说明.md"));
        assert!(markdown.contains("采用混合召回"));
        assert!(!markdown.contains("敏感目录"));
    }

    #[test]
    fn clarification_reference_alias_writability_gate() {
        // 场景 F：用户澄清选择后，「待澄清引用」只有适合做别名的短名词短语
        // 才写入 USER_SELECTION 记忆；问句/长句整段写入会污染记忆。
        assert!(is_alias_writable_reference("我的简历"));
        assert!(is_alias_writable_reference("第二份合同"));
        assert!(is_alias_writable_reference("LangGraph 项目"));
        // 问句 / 疑问词 / 过长引用：不写
        assert!(!is_alias_writable_reference("我的简历里有什么？"));
        assert!(!is_alias_writable_reference("有没有 LangGraph"));
        assert!(!is_alias_writable_reference("第二份合同里面有没有身份证号"));
        assert!(!is_alias_writable_reference("我"));
        assert!(!is_alias_writable_reference(&"很".repeat(21)));
    }

    #[test]
    fn should_ask_clarification_dispatch_gate() {
        // 修复验证（Step 14）：MultipleCandidates 的 resolved_file_ids 是
        // top-2/3（非空），旧条件 `resolved_scope.is_empty()` 与 resolver 输出
        // 相悖，导致 NEED_CLARIFICATION 分支永远不触发。触发条件只看
        // 「状态 + 有可选候选」，与 scope 是否非空无关。
        // 注意：fanfan_core 根不 glob 导出本类型（与 organizing::ResolutionStatus
        // 撞名），沿用文件顶部 ask::query_plan 的导入。

        // 核心场景：Memory 未消歧 + MultipleCandidates + 有候选 → 必须回问
        // （触发条件与 scope 是否非空无关——MultipleCandidates 的
        // resolved_file_ids 是 top-2/3，scope 非空不代表锁定）
        assert!(should_ask_clarification(
            false,
            Some(ResolutionStatus::MultipleCandidates),
            true
        ));
        // Memory 已消歧 → 不问（Memory 定位优先，锁定目标）
        assert!(!should_ask_clarification(
            true,
            Some(ResolutionStatus::MultipleCandidates),
            true
        ));
        // 唯一候选 / 未解析 → 不问
        // 唯一候选（Resolved）→ 不问
        assert!(!should_ask_clarification(
            false,
            Some(ResolutionStatus::Resolved),
            true
        ));
        assert!(!should_ask_clarification(
            false,
            Some(ResolutionStatus::Unresolved),
            true
        ));
        // 未运行 Resolver（scope 已被上下文/记忆锁定）→ 不问
        assert!(!should_ask_clarification(false, None, true));
        // 无候选（理论防御）→ 不问
        assert!(!should_ask_clarification(
            false,
            Some(ResolutionStatus::MultipleCandidates),
            false
        ));
    }
}
