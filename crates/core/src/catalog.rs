use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    AnswerResult, AppError, AppLogRecord, AskRequest, CandidateStatus, CatalogStore,
    CheckpointType, ChunkEmbeddingInput, CollectionRecord, CreateCollectionRequest,
    DegradationLevel, DegradationState, ExplorationCandidate, ExtractionRunRequest,
    ExtractionRunResult, FilePreview, FileRecord, FileRelation, FileSystemEvent,
    ImageUnderstandingResult, InboxPage, InboxQuery, InboxUpdateRequest, IndexActivityStats,
    IndexRebuildResult, JobRecord, JobStatus, KnowledgeSpace, KnowledgeSpaceRequest, LogEventInput,
    LogPage, LogQuery, MaintenanceSnapshot, ParseResult, PendingEmbeddingChunk,
    PendingImageUnderstanding, PlanSkillRequest, RelationPage, RelationQuery,
    RelationRefreshResult, RootRegistration, ScanControl, ScanPolicy, SearchRequest, SearchSession,
    SemanticQuery, TaskExecutionResult, file_identity_for_path, path_key, plan_skill,
    scan_root_with_control,
};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use windows::{
    Win32::{
        Storage::FileSystem::GetDriveTypeW,
        System::Com::CoTaskMemFree,
        UI::Shell::{
            FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Pictures,
            KF_FLAG_DEFAULT, SHGetKnownFolderPath,
        },
    },
    core::{GUID, PCWSTR},
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootSource {
    KnownFolder,
    UserFolder,
    Volume,
    Candidate,
}

impl RootSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KnownFolder => "known_folder",
            Self::UserFolder => "user_folder",
            Self::Volume => "volume",
            Self::Candidate => "candidate",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "user_folder" => Self::UserFolder,
            "volume" => Self::Volume,
            "candidate" => Self::Candidate,
            _ => Self::KnownFolder,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootStatus {
    Discovering,
    Ready,
    Scanning,
    PartialDenied,
    PermissionDenied,
    Paused,
    Offline,
    Failed,
    Removing,
}

impl RootStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovering => "discovering",
            Self::Ready => "ready",
            Self::Scanning => "scanning",
            Self::PartialDenied => "partial_denied",
            Self::PermissionDenied => "permission_denied",
            Self::Paused => "paused",
            Self::Offline => "offline",
            Self::Failed => "failed",
            Self::Removing => "removing",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "discovering" => Self::Discovering,
            "scanning" => Self::Scanning,
            "partial_denied" => Self::PartialDenied,
            "permission_denied" => Self::PermissionDenied,
            "paused" => Self::Paused,
            "offline" => Self::Offline,
            "failed" => Self::Failed,
            "removing" => Self::Removing,
            _ => Self::Ready,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VolumeType {
    Fixed,
    Removable,
}

impl VolumeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Removable => "removable",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "removable" => Self::Removable,
            _ => Self::Fixed,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationSource {
    SystemDefault,
    UserSelected,
    CandidateConfirmed,
}

impl AuthorizationSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SystemDefault => "system_default",
            Self::UserSelected => "user_selected",
            Self::CandidateConfirmed => "candidate_confirmed",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "user_selected" => Self::UserSelected,
            "candidate_confirmed" => Self::CandidateConfirmed,
            _ => Self::SystemDefault,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    KnownFolder,
    Folder,
    VolumeRoot,
    AppCandidate,
}

impl RootKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KnownFolder => "known_folder",
            Self::Folder => "folder",
            Self::VolumeRoot => "volume_root",
            Self::AppCandidate => "app_candidate",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "folder" => Self::Folder,
            "volume_root" => Self::VolumeRoot,
            "app_candidate" => Self::AppCandidate,
            _ => Self::KnownFolder,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WatchMode {
    Realtime,
    Manual,
}

impl WatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Realtime => "realtime",
            Self::Manual => "manual",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "manual" => Self::Manual,
            _ => Self::Realtime,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootRecord {
    pub root_id: Uuid,
    pub path: String,
    pub canonical_path: String,
    pub path_key: String,
    pub root_file_id: Option<String>,
    pub volume_id: String,
    pub volume_type: VolumeType,
    pub authorization_source: AuthorizationSource,
    pub root_kind: RootKind,
    pub label: String,
    pub enabled: bool,
    pub status: RootStatus,
    pub watch_mode: WatchMode,
    pub coverage_parent_root_id: Option<Uuid>,
    pub file_count: u64,
    pub permission_error_count: u64,
    pub last_scan_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootDiscoveryFailure {
    pub label: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RootDiscoveryResult {
    pub roots: Vec<RootRecord>,
    pub failures: Vec<RootDiscoveryFailure>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRootType {
    Onedrive,
    Wechat,
    Qq,
}

impl CandidateRootType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Onedrive => "onedrive",
            Self::Wechat => "wechat",
            Self::Qq => "qq",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "wechat" => Self::Wechat,
            "qq" => Self::Qq,
            _ => Self::Onedrive,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Onedrive => "OneDrive",
            Self::Wechat => "微信接收文件",
            Self::Qq => "QQ接收文件",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateRootStatus {
    Suggested,
    Adding,
    Added,
    Ignored,
}

impl CandidateRootStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Suggested => "suggested",
            Self::Adding => "adding",
            Self::Added => "added",
            Self::Ignored => "ignored",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "adding" => Self::Adding,
            "added" => Self::Added,
            "ignored" => Self::Ignored,
            _ => Self::Suggested,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateRoot {
    pub candidate_id: Uuid,
    pub candidate_type: CandidateRootType,
    pub label: String,
    pub display_path: String,
    pub status: CandidateRootStatus,
}

#[derive(Debug, Clone)]
pub struct CandidateActionOutcome {
    pub candidate: CandidateRoot,
    pub root: Option<RootRecord>,
}

#[derive(Debug, Clone)]
pub struct CatalogService {
    store: CatalogStore,
    scan_policy: Arc<RwLock<ScanPolicy>>,
    scan_controls: Arc<Mutex<HashMap<Uuid, ScanControl>>>,
    scan_execution: Arc<Mutex<()>>,
}

#[derive(Debug, Clone)]
pub struct PreparedScan {
    pub job: JobRecord,
    pub should_start: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddRootRequest {
    pub path: String,
    pub label: Option<String>,
    pub watch_mode: WatchMode,
    pub authorization_source: AuthorizationSource,
    pub full_volume_confirmed: bool,
}

impl CatalogService {
    pub fn open(data_directory: PathBuf) -> Result<Self, AppError> {
        let store = CatalogStore::open(data_directory.join("remin.db"))?;
        let exclusion_rules = store.list_exclusion_rules()?;
        Ok(Self {
            store,
            scan_policy: Arc::new(RwLock::new(
                ScanPolicy::new([data_directory]).with_rules(exclusion_rules),
            )),
            scan_controls: Arc::new(Mutex::new(HashMap::new())),
            scan_execution: Arc::new(Mutex::new(())),
        })
    }

    pub fn discover_default_roots(&self) -> RootDiscoveryResult {
        let mut result = RootDiscoveryResult::default();
        for (label, resolved) in default_known_folders() {
            match resolved
                .and_then(|path| self.register_folder(label, path, RootSource::KnownFolder))
            {
                Ok(root) if root.enabled => result.roots.push(root),
                Ok(_) => {}
                Err(error) => result.failures.push(RootDiscoveryFailure {
                    label: label.to_owned(),
                    code: error.code,
                    message: error.message,
                }),
            }
        }
        result
    }

    pub fn discover_candidate_roots(&self) -> Result<Vec<CandidateRoot>, AppError> {
        let mut candidates = Vec::new();
        for variable in ["OneDrive", "OneDriveConsumer"] {
            if let Some(path) = std::env::var_os(variable).map(PathBuf::from)
                && path.is_dir()
            {
                candidates.push((CandidateRootType::Onedrive, path));
            }
        }
        if let Some(documents) = default_known_folders()
            .into_iter()
            .find(|(label, _)| *label == "文档")
            .and_then(|(_, path)| path.ok())
        {
            candidates.push((CandidateRootType::Wechat, documents.join("WeChat Files")));
            candidates.push((CandidateRootType::Qq, documents.join("Tencent Files")));
        }
        let mut seen = std::collections::HashSet::new();
        for (candidate_type, path) in candidates {
            if !path.is_dir() {
                continue;
            }
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            let key = path_key(&canonical);
            if seen.insert(key.clone()) {
                self.store.upsert_candidate_root(
                    candidate_type,
                    &canonical.to_string_lossy(),
                    &key,
                )?;
            }
        }
        self.store.list_candidate_roots()
    }

    pub fn candidate_root_action(
        &self,
        candidate_id: &Uuid,
        action: &str,
    ) -> Result<CandidateActionOutcome, AppError> {
        let candidate = self.store.candidate_root_by_id(candidate_id)?;
        match action {
            "ignore" => Ok(CandidateActionOutcome {
                candidate: self
                    .store
                    .update_candidate_root_status(candidate_id, CandidateRootStatus::Ignored)?,
                root: None,
            }),
            "add" => {
                let root = self.register_path(
                    &candidate.label,
                    PathBuf::from(&candidate.display_path),
                    RootSource::Candidate,
                    AuthorizationSource::CandidateConfirmed,
                    WatchMode::Realtime,
                    false,
                )?;
                Ok(CandidateActionOutcome {
                    candidate: self
                        .store
                        .update_candidate_root_status(candidate_id, CandidateRootStatus::Added)?,
                    root: Some(root),
                })
            }
            _ => Err(AppError::new(
                "CANDIDATE_ACTION_INVALID",
                "候选资料来源操作必须是add或ignore",
                false,
            )),
        }
    }

    pub fn register_folder(
        &self,
        label: &str,
        path: PathBuf,
        source: RootSource,
    ) -> Result<RootRecord, AppError> {
        let authorization_source = match source {
            RootSource::KnownFolder => AuthorizationSource::SystemDefault,
            RootSource::UserFolder | RootSource::Volume => AuthorizationSource::UserSelected,
            RootSource::Candidate => AuthorizationSource::CandidateConfirmed,
        };
        self.register_path(
            label,
            path,
            source,
            authorization_source,
            WatchMode::Realtime,
            source == RootSource::Volume,
        )
    }

    pub fn add_root(&self, request: AddRootRequest) -> Result<RootRecord, AppError> {
        let path = PathBuf::from(&request.path);
        let label = request
            .label
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| {
                path.file_name()
                    .map(|value| value.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| request.path.clone());
        let canonical = std::fs::canonicalize(&path).unwrap_or(path.clone());
        let volume_root = canonical.parent().is_none();
        if volume_root && !request.full_volume_confirmed {
            return Err(AppError::new(
                "ROOT_VOLUME_CONFIRMATION_REQUIRED",
                "添加整个磁盘前必须确认性能影响",
                false,
            ));
        }
        self.register_path(
            &label,
            path,
            if volume_root {
                RootSource::Volume
            } else {
                RootSource::UserFolder
            },
            request.authorization_source,
            request.watch_mode,
            request.full_volume_confirmed,
        )
    }

    fn register_path(
        &self,
        label: &str,
        path: PathBuf,
        source: RootSource,
        authorization_source: AuthorizationSource,
        watch_mode: WatchMode,
        full_volume_confirmed: bool,
    ) -> Result<RootRecord, AppError> {
        if !path.is_dir() {
            return Err(AppError::new(
                "KNOWN_FOLDER_UNAVAILABLE",
                format!("{label}目录不存在或当前不可访问"),
                true,
            ));
        }
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        if path_key(&canonical).starts_with("\\\\") {
            return Err(AppError::new(
                "ROOT_NETWORK_UNSUPPORTED",
                "V1不支持网络共享、NAS或UNC资料目录",
                false,
            ));
        }
        if self
            .scan_policy
            .read()
            .map_err(|_| AppError::new("SCAN_POLICY_UNAVAILABLE", "扫描排除策略状态不可用", true))?
            .protects(&canonical)
        {
            return Err(AppError::new(
                "ROOT_PROTECTED",
                "拾忆的内部数据目录不能作为资料来源",
                false,
            ));
        }
        let (volume_id, root_file_id) = file_identity_for_path(&canonical);
        let root_kind = match source {
            RootSource::KnownFolder => RootKind::KnownFolder,
            RootSource::UserFolder => RootKind::Folder,
            RootSource::Volume => RootKind::VolumeRoot,
            RootSource::Candidate => RootKind::AppCandidate,
        };
        if root_kind == RootKind::VolumeRoot && !full_volume_confirmed {
            return Err(AppError::new(
                "ROOT_VOLUME_CONFIRMATION_REQUIRED",
                "添加整个磁盘前必须确认性能影响",
                false,
            ));
        }
        let volume_type = volume_type_for_path(&canonical)?;
        self.store.upsert_root(&RootRegistration {
            label: label.to_owned(),
            canonical_path: canonical.to_string_lossy().into_owned(),
            path_key: path_key(&canonical),
            source,
            volume_id,
            root_file_id,
            authorization_source,
            root_kind,
            volume_type,
            watch_mode,
        })
    }

    pub fn list_roots(&self) -> Result<Vec<RootRecord>, AppError> {
        self.store.list_roots()
    }

    pub fn disable_root(&self, root_id: &Uuid) -> Result<(), AppError> {
        self.store.disable_root(root_id)
    }

    pub fn list_files(&self) -> Result<Vec<FileRecord>, AppError> {
        self.store.list_files()
    }

    pub fn query_files(&self, request: &crate::FileQuery) -> Result<crate::FilePage, AppError> {
        self.store.query_files(request)
    }

    pub fn authorized_files_by_ids(&self, file_ids: &[Uuid]) -> Result<Vec<FileRecord>, AppError> {
        self.store.authorized_files_by_ids(file_ids)
    }

    pub fn list_exclusion_rules(&self) -> Result<Vec<crate::ExclusionRule>, AppError> {
        self.store.list_exclusion_rules()
    }

    pub fn upsert_exclusion_rule(
        &self,
        input: &crate::ExclusionRuleInput,
    ) -> Result<crate::ExclusionRule, AppError> {
        let rule = self.store.upsert_exclusion_rule(input)?;
        self.reload_exclusion_rules()?;
        Ok(rule)
    }

    pub fn delete_exclusion_rule(&self, rule_id: &Uuid) -> Result<(), AppError> {
        self.store.delete_exclusion_rule(rule_id)?;
        self.reload_exclusion_rules()
    }

    fn reload_exclusion_rules(&self) -> Result<(), AppError> {
        let rules = self.store.list_exclusion_rules()?;
        let mut policy = self.scan_policy.write().map_err(|_| {
            AppError::new("SCAN_POLICY_UNAVAILABLE", "扫描排除策略状态不可用", true)
        })?;
        *policy = policy.clone().with_rules(rules);
        Ok(())
    }

    pub fn home_file_summary(&self, local_date: &str) -> Result<(u64, Vec<FileRecord>), AppError> {
        self.store.home_file_summary(local_date)
    }

    pub fn run_extraction(
        &self,
        request: &ExtractionRunRequest,
    ) -> Result<ExtractionRunResult, AppError> {
        self.store.run_extraction(request)
    }

    pub fn execute_task(
        &self,
        request: &PlanSkillRequest,
    ) -> Result<TaskExecutionResult, AppError> {
        let requested_plan = plan_skill(request)?;
        let (mut plan, initial_job) =
            if let Some(stored_plan) = self.store.task_plan_by_id(&requested_plan.task_id)? {
                let same_contract = stored_plan.skill_id == requested_plan.skill_id
                    && stored_plan.steps.len() == requested_plan.steps.len()
                    && stored_plan.steps.iter().zip(&requested_plan.steps).all(
                        |(stored, requested)| {
                            stored.ordinal == requested.ordinal
                                && stored.step_type == requested.step_type
                                && stored.inputs == requested.inputs
                                && stored.expected_outputs == requested.expected_outputs
                                && stored.checkpoint == requested.checkpoint
                        },
                    );
                if !same_contract {
                    return Err(AppError::new(
                        "TASK_RESUME_CONTRACT_MISMATCH",
                        "恢复请求与原任务计划不一致，请重新预览任务",
                        false,
                    ));
                }
                let job = self.store.resume_task(&stored_plan.task_id)?;
                (stored_plan, job)
            } else {
                let job = self.store.begin_task(&requested_plan)?;
                (requested_plan, job)
            };
        let execution = (|| {
            if request.skill_id == "duplicate_review" {
                self.store
                    .refresh_selected_file_relations(&request.file_ids)?;
            }
            let selected_files = self.store.authorized_files_by_ids(&request.file_ids)?;
            if selected_files.len() != request.file_ids.len() {
                return Err(AppError::new(
                    "TASK_FILE_UNAVAILABLE",
                    "任务中至少一份资料已离开授权范围，请刷新后重试",
                    false,
                ));
            }
            let mut checkpoints = self.store.task_checkpoints(&plan.task_id)?;
            let mut completed = checkpoints
                .iter()
                .filter(|checkpoint| checkpoint.status == crate::CheckpointStatus::Passed)
                .map(|checkpoint| checkpoint.unit_id)
                .collect::<HashSet<_>>();
            if !completed.contains(&plan.steps[0].step_id) {
                let checkpoint = self.store.pass_task_step(
                    &plan.task_id,
                    &plan.steps[0],
                    CheckpointType::Permission,
                    json!({"authorized_files": selected_files.len(), "source_files_readonly": true}),
                )?;
                completed.insert(checkpoint.unit_id);
                checkpoints.push(checkpoint);
            }
            if !completed.contains(&plan.steps[1].step_id) {
                let checkpoint = self.store.pass_task_step(
                    &plan.task_id,
                    &plan.steps[1],
                    CheckpointType::Invariant,
                    json!({"revision_count": selected_files.iter().filter(|file| file.current_revision_id.is_some()).count()}),
                )?;
                completed.insert(checkpoint.unit_id);
                checkpoints.push(checkpoint);
            }

            let preset_id = match request.skill_id.as_str() {
                "generate_catalog" | "export_index" => "file_catalog".to_owned(),
                "multi_document_summary" => "extractive_summary".to_owned(),
                "recommend_filename" => "filename_suggestions".to_owned(),
                "recommend_folders" => "folder_suggestions".to_owned(),
                "duplicate_review" => "duplicate_review".to_owned(),
                "version_compare" => "version_compare".to_owned(),
                "merge_tables" => "merge_tables".to_owned(),
                "rerun_ocr" => "ocr_report".to_owned(),
                _ => request
                    .parameters
                    .get("preset_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("file_catalog")
                    .to_owned(),
            };
            let result = self.store.run_extraction(&ExtractionRunRequest {
                file_ids: request.file_ids.clone(),
                preset_id,
            })?;
            let values = result.rows.iter().flat_map(|row| row.values.iter());
            let (non_empty_values, evidence_values) =
                values.fold((0_u64, 0_u64), |mut counts, value| {
                    if !value.normalized_value.is_null() {
                        counts.0 += 1;
                        if !value.evidence.is_empty() {
                            counts.1 += 1;
                        }
                    }
                    counts
                });
            if non_empty_values != evidence_values {
                return Err(AppError::new(
                    "TASK_EVIDENCE_VALIDATION_FAILED",
                    "抽取结果中存在没有来源的非空字段",
                    false,
                ));
            }
            let evidence_score = if non_empty_values == 0 {
                1.0
            } else {
                evidence_values as f32 / non_empty_values as f32
            };
            let strategies: &[&str] = match request.skill_id.as_str() {
                "multi_document_summary" => &[
                    "extractive_first",
                    "metadata_outline",
                    "conservative_fallback",
                ],
                "recommend_filename" => &[
                    "content_heading",
                    "existing_name_normalized",
                    "conservative_keep_current",
                ],
                "recommend_folders" => &[
                    "content_keywords",
                    "path_and_type",
                    "conservative_virtual_inbox",
                ],
                _ => &[],
            };
            let candidates = strategies
                .iter()
                .enumerate()
                .map(|(index, strategy)| ExplorationCandidate {
                    candidate_id: Uuid::now_v7(),
                    job_id: plan.task_id,
                    strategy: (*strategy).into(),
                    status: if index == 0 {
                        CandidateStatus::Selected
                    } else {
                        CandidateStatus::Valid
                    },
                    result_ref: (index == 0)
                        .then(|| format!("remin://extraction/{}", result.run_id)),
                    quality_score: Some(match index {
                        0 => 0.9,
                        1 => 0.72,
                        _ => 0.6,
                    }),
                    evidence_score: Some(evidence_score),
                    latency_ms: None,
                    resource_cost: Some(match index {
                        0 => 0.5,
                        1 => 0.2,
                        _ => 0.05,
                    }),
                    rejection_reasons: Vec::new(),
                })
                .collect::<Vec<_>>();
            if !candidates.is_empty() {
                self.store
                    .replace_task_exploration_candidates(&plan.task_id, &candidates)?;
            }
            if !completed.contains(&plan.steps[2].step_id) {
                let checkpoint = self.store.pass_task_step(
                    &plan.task_id,
                    &plan.steps[2],
                    CheckpointType::Evidence,
                      json!({"rows": result.rows.len(), "non_empty_values": non_empty_values, "values_with_evidence": evidence_values, "exploration_candidates": candidates.len()}),
                )?;
                completed.insert(checkpoint.unit_id);
                checkpoints.push(checkpoint);
            }
            if !completed.contains(&plan.steps[3].step_id) {
                let checkpoint = self.store.pass_task_step(
                    &plan.task_id,
                    &plan.steps[3],
                    CheckpointType::Schema,
                    json!({"run_id": result.run_id, "status": result.status, "exported": false}),
                )?;
                completed.insert(checkpoint.unit_id);
                checkpoints.push(checkpoint);
            }
            let job = self.store.finish_task(&plan.task_id)?;
            for step in &mut plan.steps {
                if completed.contains(&step.step_id) {
                    step.status = "succeeded".into();
                    step.attempt_count = 1;
                }
            }
            Ok(TaskExecutionResult {
                plan,
                job,
                result,
                checkpoints,
                candidates,
            })
        })();
        match execution {
            Ok(result) => Ok(result),
            Err(error) => {
                let _ = self.store.fail_task(&initial_job.job_id, &error);
                Err(error)
            }
        }
    }

    pub fn recover_interrupted_tasks(&self) -> Result<u64, AppError> {
        self.store.recover_interrupted_tasks()
    }

    pub fn latest_recoverable_task_plan(&self) -> Result<Option<crate::TaskPlan>, AppError> {
        self.store.latest_recoverable_task_plan()
    }

    pub fn resume_task_execution(&self, task_id: &Uuid) -> Result<TaskExecutionResult, AppError> {
        let plan = self
            .store
            .task_plan_by_id(task_id)?
            .ok_or_else(|| AppError::new("TASK_JOB_NOT_FOUND", "待恢复任务不存在", false))?;
        let file_ids = plan
            .steps
            .first()
            .and_then(|step| step.inputs.get("file_ids"))
            .and_then(|value| value.as_array())
            .ok_or_else(|| {
                AppError::new(
                    "TASK_PLAN_DESERIALIZE_FAILED",
                    "恢复计划缺少文件范围",
                    false,
                )
            })?
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .and_then(|value| Uuid::parse_str(value).ok())
                    .ok_or_else(|| {
                        AppError::new(
                            "TASK_PLAN_DESERIALIZE_FAILED",
                            "恢复计划包含无效文件标识",
                            false,
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let parameters = plan
            .steps
            .get(2)
            .map(|step| step.inputs.clone())
            .unwrap_or_else(|| json!({}));
        self.execute_task(&PlanSkillRequest {
            task_id: Some(*task_id),
            skill_id: plan.skill_id,
            file_ids,
            parameters,
            user_instruction: None,
        })
    }

    pub fn query_inbox(&self, request: &InboxQuery) -> Result<InboxPage, AppError> {
        self.store.query_inbox(request)
    }

    pub fn update_inbox_item(
        &self,
        request: &InboxUpdateRequest,
    ) -> Result<crate::InboxItem, AppError> {
        self.store.update_inbox_item(request)
    }

    pub fn list_knowledge_spaces(&self) -> Result<Vec<KnowledgeSpace>, AppError> {
        self.store.list_knowledge_spaces()
    }

    pub fn create_knowledge_space(
        &self,
        request: &KnowledgeSpaceRequest,
    ) -> Result<KnowledgeSpace, AppError> {
        self.store.create_knowledge_space(request)
    }

    pub fn update_knowledge_space(
        &self,
        space_id: &Uuid,
        request: &KnowledgeSpaceRequest,
    ) -> Result<KnowledgeSpace, AppError> {
        self.store.update_knowledge_space(space_id, request)
    }

    pub fn delete_knowledge_space(&self, space_id: &Uuid) -> Result<(), AppError> {
        self.store.delete_knowledge_space(space_id)
    }

    pub fn storage_quota_override(&self) -> Result<Option<u64>, AppError> {
        self.store.storage_quota_override()
    }

    pub fn set_storage_quota_override(&self, quota_bytes: u64) -> Result<u64, AppError> {
        self.store.set_storage_quota_override(quota_bytes)
    }

    pub fn list_collections(&self) -> Result<Vec<CollectionRecord>, AppError> {
        self.store.list_collections()
    }

    pub fn create_collection(
        &self,
        request: &CreateCollectionRequest,
    ) -> Result<CollectionRecord, AppError> {
        self.store.create_collection(request)
    }

    pub fn update_collection(
        &self,
        collection_id: &Uuid,
        request: &CreateCollectionRequest,
    ) -> Result<CollectionRecord, AppError> {
        self.store.update_collection(collection_id, request)
    }

    pub fn delete_collection(&self, collection_id: &Uuid) -> Result<(), AppError> {
        self.store.delete_collection(collection_id)
    }

    pub fn preview_collection_rule(
        &self,
        rule: &crate::CollectionRule,
        limit: u32,
    ) -> Result<Vec<FileRecord>, AppError> {
        self.store.preview_collection_rule(rule, limit)
    }

    pub fn add_file_to_collection(
        &self,
        collection_id: &Uuid,
        file_id: &Uuid,
    ) -> Result<(), AppError> {
        self.store.add_file_to_collection(collection_id, file_id)
    }

    pub fn remove_file_from_collection(
        &self,
        collection_id: &Uuid,
        file_id: &Uuid,
    ) -> Result<(), AppError> {
        self.store
            .remove_file_from_collection(collection_id, file_id)
    }

    pub fn collection_files(&self, collection_id: &Uuid) -> Result<Vec<FileRecord>, AppError> {
        self.store.collection_files(collection_id)
    }

    pub fn query_collection_files(
        &self,
        collection_id: &Uuid,
        request: &crate::FileQuery,
    ) -> Result<crate::FilePage, AppError> {
        self.store.query_collection_files(collection_id, request)
    }

    pub fn refresh_collection_suggestions(
        &self,
        model_artifact_id: &str,
        max_files: u32,
    ) -> Result<crate::CollectionSuggestionRefreshResult, AppError> {
        self.store
            .refresh_collection_suggestions(model_artifact_id, max_files)
    }

    pub fn query_collection_suggestions(
        &self,
        request: &crate::CollectionSuggestionQuery,
    ) -> Result<crate::CollectionSuggestionPage, AppError> {
        self.store.query_collection_suggestions(request)
    }

    pub fn update_collection_suggestion(
        &self,
        suggestion_id: &Uuid,
        request: &crate::CollectionSuggestionUpdateRequest,
    ) -> Result<crate::CollectionSuggestion, AppError> {
        self.store
            .update_collection_suggestion(suggestion_id, request)
    }

    pub fn apply_collection_model_review(
        &self,
        suggestion_id: &Uuid,
        review: &crate::CollectionModelReview,
        model_version: &str,
    ) -> Result<crate::CollectionSuggestion, AppError> {
        self.store
            .apply_collection_model_review(suggestion_id, review, model_version)
    }

    pub fn confirm_collection_suggestion(
        &self,
        suggestion_id: &Uuid,
    ) -> Result<CollectionRecord, AppError> {
        self.store.confirm_collection_suggestion(suggestion_id)
    }

    pub fn reject_collection_suggestion(&self, suggestion_id: &Uuid) -> Result<(), AppError> {
        self.store.reject_collection_suggestion(suggestion_id)
    }

    pub fn refresh_file_relations(
        &self,
        max_files: u32,
    ) -> Result<RelationRefreshResult, AppError> {
        self.store.refresh_file_relations(max_files)
    }

    pub fn refresh_selected_file_relations(
        &self,
        file_ids: &[Uuid],
    ) -> Result<RelationRefreshResult, AppError> {
        self.store.refresh_selected_file_relations(file_ids)
    }

    pub fn list_file_relations(&self, limit: u32) -> Result<Vec<FileRelation>, AppError> {
        self.store.list_file_relations(limit)
    }

    pub fn query_file_relations(&self, request: &RelationQuery) -> Result<RelationPage, AppError> {
        self.store.query_file_relations(request)
    }

    pub fn review_file_relation(&self, relation_id: &Uuid, action: &str) -> Result<(), AppError> {
        self.store.review_file_relation(relation_id, action)
    }

    pub fn list_pending_parse_files(&self, limit: usize) -> Result<Vec<FileRecord>, AppError> {
        self.store.list_pending_parse_files(limit)
    }

    pub fn retry_ocr(&self, file_id: &Uuid) -> Result<(), AppError> {
        self.store.retry_ocr(file_id)
    }

    pub fn mark_file_parsing(&self, file_id: &Uuid, revision_id: &Uuid) -> Result<(), AppError> {
        self.store.mark_file_parsing(file_id, revision_id)
    }

    pub fn recover_interrupted_parses(&self) -> Result<u64, AppError> {
        self.store.recover_interrupted_parses()
    }

    pub fn commit_parse_result(
        &self,
        file_id: &Uuid,
        result: &ParseResult,
    ) -> Result<(), AppError> {
        self.store.commit_parse_result(file_id, result)
    }

    pub fn recover_interrupted_image_understanding(&self) -> Result<u64, AppError> {
        self.store.recover_interrupted_image_understanding()
    }

    pub fn claim_pending_image_understanding(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<PendingImageUnderstanding>, AppError> {
        self.store
            .claim_pending_image_understanding(model_artifact_id)
    }

    pub fn commit_image_understanding(
        &self,
        result: &ImageUnderstandingResult,
    ) -> Result<(), AppError> {
        self.store.commit_image_understanding(result)
    }

    pub fn fail_image_understanding(
        &self,
        asset_id: &Uuid,
        error: &AppError,
    ) -> Result<(), AppError> {
        self.store.fail_image_understanding(asset_id, error)
    }

    pub fn retry_image_understanding(&self, asset_id: &Uuid) -> Result<(), AppError> {
        self.store.retry_image_understanding(asset_id)
    }

    pub fn image_understanding_stats(&self) -> Result<(u64, u64, u64), AppError> {
        self.store.image_understanding_stats()
    }

    pub fn search(&self, request: &SearchRequest) -> Result<SearchSession, AppError> {
        self.store.search(request)
    }

    pub fn search_with_semantic(
        &self,
        request: &SearchRequest,
        semantic_query: Option<SemanticQuery<'_>>,
    ) -> Result<SearchSession, AppError> {
        self.store.search_with_semantic(request, semantic_query)
    }

    pub fn semantic_index_coverage(
        &self,
        scope: &crate::ScopeFilter,
        model_artifact_id: &str,
    ) -> Result<(f64, f64), AppError> {
        self.store.semantic_index_coverage(scope, model_artifact_id)
    }

    pub fn answer_extractively(
        &self,
        request: &AskRequest,
        semantic_query: Option<SemanticQuery<'_>>,
    ) -> Result<AnswerResult, AppError> {
        self.store.answer_extractively(request, semantic_query)
    }

    pub fn load_ask_history(
        &self,
        session_id: &Uuid,
        limit: usize,
    ) -> Result<Vec<crate::AskMessage>, AppError> {
        self.store.load_ask_history(session_id, limit)
    }

    pub fn record_ask_exchange(
        &self,
        request: &AskRequest,
        result: &AnswerResult,
    ) -> Result<(), AppError> {
        self.store.record_ask_exchange(request, result)
    }

    pub fn validate_answer_evidence(&self, result: &AnswerResult) -> Result<(), AppError> {
        self.store.validate_answer_evidence(result)
    }

    pub fn maintenance_snapshot(&self) -> Result<MaintenanceSnapshot, AppError> {
        self.store.maintenance_snapshot()
    }

    pub fn maintenance_check(
        &self,
        level: &str,
    ) -> Result<crate::MaintenanceCheckResult, AppError> {
        self.store.maintenance_check(level)
    }

    pub fn reconcile_degradation_state(
        &self,
        desired_level: DegradationLevel,
        triggers: Vec<String>,
    ) -> Result<DegradationState, AppError> {
        self.store
            .reconcile_degradation_state(desired_level, triggers)
    }

    pub fn index_activity_stats(&self) -> Result<IndexActivityStats, AppError> {
        self.store.index_activity_stats()
    }

    pub fn list_logs(&self, limit: u32) -> Result<Vec<AppLogRecord>, AppError> {
        self.store.list_logs(limit)
    }

    pub fn query_logs(&self, request: &LogQuery) -> Result<LogPage, AppError> {
        self.store.query_logs(request)
    }

    pub fn clear_logs(&self) -> Result<u64, AppError> {
        self.store.clear_logs()
    }

    pub fn rebuild_index(&self, confirmation: &str) -> Result<IndexRebuildResult, AppError> {
        self.store.rebuild_index(confirmation)
    }

    pub fn list_pending_embedding_chunks(
        &self,
        model_artifact_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbeddingChunk>, AppError> {
        self.store
            .list_pending_embedding_chunks(model_artifact_id, limit)
    }

    pub fn commit_chunk_embeddings(
        &self,
        model_artifact_id: &str,
        dimension: u32,
        embeddings: &[ChunkEmbeddingInput],
    ) -> Result<u64, AppError> {
        self.store
            .commit_chunk_embeddings(model_artifact_id, dimension, embeddings)
    }

    pub fn rebuild_vector_generation(
        &self,
        model_artifact_id: &str,
        dimension: u32,
    ) -> Result<crate::IndexGeneration, AppError> {
        self.store
            .rebuild_vector_generation(model_artifact_id, dimension)
    }

    pub fn active_vector_generation(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<crate::IndexGeneration>, AppError> {
        self.store.active_vector_generation(model_artifact_id)
    }

    pub fn file_preview(&self, file_id: &Uuid, node_limit: usize) -> Result<FilePreview, AppError> {
        self.store.file_preview(file_id, node_limit)
    }

    pub fn file_preview_page(
        &self,
        file_id: &Uuid,
        offset: usize,
        node_limit: usize,
        anchor_node_id: Option<&Uuid>,
    ) -> Result<FilePreview, AppError> {
        self.store
            .file_preview_page(file_id, offset, node_limit, anchor_node_id)
    }

    pub fn authorized_file_path(&self, file_id: &Uuid) -> Result<PathBuf, AppError> {
        self.store.authorized_file_path(file_id)
    }

    pub fn authorized_image_asset_path(
        &self,
        asset_id: &Uuid,
    ) -> Result<(PathBuf, String, u64), AppError> {
        self.store.authorized_image_asset_path(asset_id)
    }

    pub fn latest_active_scan_job(&self) -> Result<Option<JobRecord>, AppError> {
        self.store.latest_active_scan_job()
    }

    pub fn prepare_scan(&self, root_id: &Uuid, reason: &str) -> Result<PreparedScan, AppError> {
        let (job, should_start) = self.store.prepare_scan_job(root_id, reason)?;
        if should_start || reason != "filesystem_event" {
            let fields = json!({ "reason": reason });
            let _ = self.store.append_log(&LogEventInput {
                level: "info",
                component: "catalog",
                event_name: if should_start {
                    "scan.prepared"
                } else {
                    "scan.reused"
                },
                job_id: Some(&job.job_id),
                root_id: Some(root_id),
                file_id: None,
                fields: &fields,
            });
        }
        Ok(PreparedScan { job, should_start })
    }

    pub fn recover_interrupted_scans(&self) -> Result<Vec<(Uuid, JobRecord)>, AppError> {
        let recovered = self.store.recover_interrupted_scan_jobs()?;
        for (root_id, job) in &recovered {
            let fields = json!({ "stage": job.stage });
            let _ = self.store.append_log(&LogEventInput {
                level: "warning",
                component: "catalog",
                event_name: "scan.recovered",
                job_id: Some(&job.job_id),
                root_id: Some(root_id),
                file_id: None,
                fields: &fields,
            });
        }
        Ok(recovered)
    }

    pub fn apply_incremental_events(
        &self,
        root_id: &Uuid,
        events: &[FileSystemEvent],
    ) -> Result<JobRecord, AppError> {
        self.store.record_file_events(root_id, events)?;
        let prepared = self.prepare_scan(root_id, "filesystem_event")?;
        if !prepared.should_start {
            return Err(AppError::new(
                "SCAN_QUEUE_BUSY",
                "现有扫描任务仍在运行，增量事件已保留并将在稍后重试",
                true,
            ));
        }
        let job = self.execute_scan(*root_id, prepared.job.job_id)?;
        match job.status {
            JobStatus::Succeeded => {
                self.store.mark_file_events_coalesced(root_id)?;
                Ok(job)
            }
            JobStatus::Failed => Err(job.error.unwrap_or_else(|| {
                AppError::new(
                    "INCREMENTAL_SCAN_FAILED",
                    "增量事件重扫失败，已保留待处理事件",
                    true,
                )
            })),
            _ => Err(AppError::new(
                "INCREMENTAL_SCAN_PARTIAL",
                "增量事件只完成了部分重扫，已保留待处理事件",
                true,
            )),
        }
    }

    pub fn pause_scan(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        let controls = self.scan_controls.lock().expect("scan controls poisoned");
        let control = controls.get(job_id).cloned().ok_or_else(|| {
            AppError::new(
                "JOB_CONTROL_UNAVAILABLE",
                "扫描任务当前没有可用的运行控制器",
                true,
            )
        })?;
        control.pause();
        drop(controls);
        match self.store.pause_scan(job_id) {
            Ok(job) => Ok(job),
            Err(error) => {
                control.resume();
                Err(error)
            }
        }
    }

    pub fn resume_scan(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        let controls = self.scan_controls.lock().expect("scan controls poisoned");
        let control = controls.get(job_id).cloned().ok_or_else(|| {
            AppError::new(
                "JOB_CONTROL_UNAVAILABLE",
                "扫描任务当前没有可用的运行控制器",
                true,
            )
        })?;
        control.resume();
        drop(controls);
        self.store.resume_scan(job_id)
    }

    pub fn cancel_scan(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        if let Some(control) = self
            .scan_controls
            .lock()
            .expect("scan controls poisoned")
            .get(job_id)
        {
            control.cancel();
        }
        self.store.cancel_scan(job_id)
    }

    pub fn execute_scan(&self, root_id: Uuid, job_id: Uuid) -> Result<JobRecord, AppError> {
        let _execution_guard = self.scan_execution.lock().map_err(|_| {
            AppError::new("SCAN_SCHEDULER_UNAVAILABLE", "扫描调度器状态已经损坏", true)
        })?;
        let root = self.store.root_by_id(&root_id)?;
        self.store.mark_scan_running(&root_id, &job_id)?;
        let control = ScanControl::default();
        self.scan_controls
            .lock()
            .expect("scan controls poisoned")
            .insert(job_id, control.clone());
        let fields = json!({ "root_kind": root.root_kind, "watch_mode": root.watch_mode });
        let _ = self.store.append_log(&LogEventInput {
            level: "info",
            component: "catalog",
            event_name: "scan.started",
            job_id: Some(&job_id),
            root_id: Some(&root_id),
            file_id: None,
            fields: &fields,
        });
        let root_path = PathBuf::from(root.canonical_path);
        let policy = self
            .scan_policy
            .read()
            .map_err(|_| AppError::new("SCAN_POLICY_UNAVAILABLE", "扫描排除策略状态不可用", true))?
            .clone();
        let result = match scan_root_with_control(&root_path, &policy, &control) {
            Ok(outcome) => self.store.commit_scan(&root_id, &job_id, &outcome),
            Err(error) if error.code == "SCAN_CANCELLED" => self.store.cancel_scan(&job_id),
            Err(error) => self.store.fail_scan(&root_id, &job_id, error),
        };
        self.scan_controls
            .lock()
            .expect("scan controls poisoned")
            .remove(&job_id);
        if let Ok(job) = &result {
            let fields = json!({ "status": job.status, "processed_items": job.processed_items });
            let _ = self.store.append_log(&LogEventInput {
                level: if job.status == crate::JobStatus::Failed {
                    "error"
                } else {
                    "info"
                },
                component: "catalog",
                event_name: "scan.finished",
                job_id: Some(&job_id),
                root_id: Some(&root_id),
                file_id: None,
                fields: &fields,
            });
        }
        result
    }
}

#[cfg(windows)]
fn volume_type_for_path(path: &std::path::Path) -> Result<VolumeType, AppError> {
    let root = path.ancestors().last().unwrap_or(path);
    let mut wide = root.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    match unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) } {
        2 => Ok(VolumeType::Removable),
        4 | 5 => Err(AppError::new(
            "ROOT_NETWORK_UNSUPPORTED",
            "V1不支持网络映射盘或光盘持续监听",
            false,
        )),
        _ => Ok(VolumeType::Fixed),
    }
}

#[cfg(not(windows))]
fn volume_type_for_path(_path: &std::path::Path) -> Result<VolumeType, AppError> {
    Ok(VolumeType::Fixed)
}

#[cfg(windows)]
fn default_known_folders() -> Vec<(&'static str, Result<PathBuf, AppError>)> {
    vec![
        ("桌面", known_folder_path(&FOLDERID_Desktop)),
        ("文档", known_folder_path(&FOLDERID_Documents)),
        ("下载", known_folder_path(&FOLDERID_Downloads)),
        ("图片", known_folder_path(&FOLDERID_Pictures)),
    ]
}

#[cfg(not(windows))]
fn default_known_folders() -> Vec<(&'static str, Result<PathBuf, AppError>)> {
    Vec::new()
}

#[cfg(windows)]
fn known_folder_path(folder_id: &GUID) -> Result<PathBuf, AppError> {
    let path = unsafe { SHGetKnownFolderPath(folder_id, KF_FLAG_DEFAULT, None) }
        .map_err(|error| AppError::new("KNOWN_FOLDER_RESOLVE_FAILED", error.to_string(), true))?;
    let value = unsafe { path.to_string() }
        .map(PathBuf::from)
        .map_err(|error| AppError::new("KNOWN_FOLDER_PATH_INVALID", error.to_string(), false));
    unsafe { CoTaskMemFree(Some(path.0.cast())) };
    value
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn catalog_scan_persists_results_without_changing_source_files() {
        let app_data = tempfile::tempdir().expect("app data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let document = source.path().join("过去的记录.txt");
        fs::write(&document, "拾起你被遗忘的记忆").expect("write source fixture");
        let source_before = fs::read(&document).expect("read source before scan");
        let service = CatalogService::open(app_data.path().to_path_buf()).expect("open catalog");
        let root = service
            .register_folder(
                "测试资料",
                source.path().to_path_buf(),
                RootSource::UserFolder,
            )
            .expect("register root");

        let prepared = service
            .prepare_scan(&root.root_id, "integration_test")
            .expect("prepare scan");
        assert!(prepared.should_start);
        let completed = service
            .execute_scan(root.root_id, prepared.job.job_id)
            .expect("execute scan");

        assert_eq!(completed.status, crate::JobStatus::Succeeded);
        assert_eq!(completed.processed_items, 1);
        assert!(completed.started_at.is_some());
        assert!(completed.finished_at.is_some());
        assert_eq!(
            fs::read(document).expect("read source after scan"),
            source_before
        );
        let stored_root = service
            .list_roots()
            .expect("list roots")
            .into_iter()
            .next()
            .expect("stored root");
        assert_eq!(stored_root.file_count, 1);
        assert_eq!(stored_root.status, RootStatus::Ready);
    }

    #[test]
    fn active_scan_is_reused_instead_of_duplicated() {
        let app_data = tempfile::tempdir().expect("app data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let service = CatalogService::open(app_data.path().to_path_buf()).expect("open catalog");
        let root = service
            .register_folder(
                "测试资料",
                source.path().to_path_buf(),
                RootSource::UserFolder,
            )
            .expect("register root");

        let first = service
            .prepare_scan(&root.root_id, "first")
            .expect("prepare first scan");
        let second = service
            .prepare_scan(&root.root_id, "second")
            .expect("reuse first scan");

        assert!(first.should_start);
        assert!(!second.should_start);
        assert_eq!(first.job.job_id, second.job.job_id);
    }

    #[cfg(windows)]
    #[test]
    fn rename_modify_and_delete_preserve_identity_and_create_revisions() {
        let app_data = tempfile::tempdir().expect("app data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let original = source.path().join("旧名称.txt");
        let renamed = source.path().join("新名称.txt");
        fs::write(&original, "第一版").expect("write first revision");
        let service = CatalogService::open(app_data.path().to_path_buf()).expect("open catalog");
        let root = service
            .register_folder(
                "测试资料",
                source.path().to_path_buf(),
                RootSource::UserFolder,
            )
            .expect("register root");

        let first = service
            .prepare_scan(&root.root_id, "first")
            .expect("prepare first scan");
        service
            .execute_scan(root.root_id, first.job.job_id)
            .expect("execute first scan");
        let first_file = service.list_files().expect("list files").remove(0);
        let first_revision = first_file.current_revision_id.expect("first revision");
        assert!(first_file.windows_file_id.is_some());

        fs::rename(&original, &renamed).expect("rename source");
        let second = service
            .prepare_scan(&root.root_id, "renamed")
            .expect("prepare rename scan");
        service
            .execute_scan(root.root_id, second.job.job_id)
            .expect("execute rename scan");
        let renamed_file = service.list_files().expect("list renamed files").remove(0);
        assert_eq!(renamed_file.file_id, first_file.file_id);
        assert_eq!(renamed_file.display_name, "新名称.txt");
        assert_eq!(renamed_file.current_revision_id, Some(first_revision));

        fs::write(&renamed, "第二版内容更长").expect("write second revision");
        let third = service
            .prepare_scan(&root.root_id, "modified")
            .expect("prepare modified scan");
        service
            .execute_scan(root.root_id, third.job.job_id)
            .expect("execute modified scan");
        let modified_file = service.list_files().expect("list modified files").remove(0);
        assert_eq!(modified_file.file_id, first_file.file_id);
        assert_ne!(modified_file.current_revision_id, Some(first_revision));

        fs::remove_file(&renamed).expect("remove source");
        let fourth = service
            .prepare_scan(&root.root_id, "deleted")
            .expect("prepare deletion scan");
        service
            .execute_scan(root.root_id, fourth.job.job_id)
            .expect("execute deletion scan");
        let missing_file = service.list_files().expect("list missing files").remove(0);
        assert_eq!(missing_file.file_id, first_file.file_id);
        assert_eq!(missing_file.availability, crate::Availability::Missing);
    }

    #[test]
    fn application_data_directory_cannot_be_registered() {
        let app_data = tempfile::tempdir().expect("app data tempdir");
        let service = CatalogService::open(app_data.path().to_path_buf()).expect("open catalog");

        let error = service
            .register_folder(
                "内部数据",
                app_data.path().to_path_buf(),
                RootSource::UserFolder,
            )
            .expect_err("internal directory must be rejected");

        assert_eq!(error.code, "ROOT_PROTECTED");
    }

    #[test]
    fn user_selected_root_keeps_authorization_and_watch_mode() {
        let app_data = tempfile::tempdir().expect("app data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let service = CatalogService::open(app_data.path().to_path_buf()).expect("open catalog");

        let root = service
            .add_root(AddRootRequest {
                path: source.path().to_string_lossy().into_owned(),
                label: Some("自定义资料".to_owned()),
                watch_mode: WatchMode::Manual,
                authorization_source: AuthorizationSource::UserSelected,
                full_volume_confirmed: false,
            })
            .expect("add custom root");

        assert_eq!(root.label, "自定义资料");
        assert_eq!(root.watch_mode, WatchMode::Manual);
        assert_eq!(root.authorization_source, AuthorizationSource::UserSelected);
        assert_eq!(root.root_kind, RootKind::Folder);
    }
}
