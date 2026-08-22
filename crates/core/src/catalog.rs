use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::{
    AnswerResult, AppError, AppLogRecord, AskRequest, CatalogStore, ChunkEmbeddingInput,
    CollectionRecord, CreateCollectionRequest, DegradationLevel, DegradationState,
    EvaluationCaseRecord, EvaluationResultRecord, EvaluationRunRecord, FilePreview, FileRecord,
    FileRelation, FileSystemEvent, ImageOcrResult, ImageUnderstandingResult, InboxPage, InboxQuery,
    InboxUpdateRequest, IndexActivityStats, IndexRebuildResult, JobRecord, JobStatus,
    LogEventInput, LogPage, LogQuery, MaintenanceSnapshot, NodeTracePage, NodeTraceQuery,
    NodeTraceRecord, OperationTraceInput, OperationTracePage, OperationTraceQuery, ParseResult,
    PendingEmbeddingChunk, PendingImageOcr, PendingImageUnderstanding, ProcessingCoverageSnapshot,
    RelationGroupPage, RelationGroupQuery, RelationPage, RelationQuery, RelationRefreshResult,
    RootRegistration, ScanControl, ScanPolicy, SearchRequest, SearchSession, SemanticQuery,
    TraceNodeInput, file_identity_for_path, path_key, scan_root_with_control,
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
    /// 该资料位置已进入活动 USearch 索引的可授权文件数。
    #[serde(default)]
    pub indexed_file_count: u64,
    #[serde(default)]
    pub indexable_file_count: u64,
    #[serde(default)]
    pub parsed_file_count: u64,
    #[serde(default)]
    pub embedded_file_count: u64,
    #[serde(default)]
    pub active_index_file_count: u64,
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
    Documents,
    Wechat,
    Qq,
}

impl CandidateRootType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Onedrive => "onedrive",
            Self::Documents => "documents",
            Self::Wechat => "wechat",
            Self::Qq => "qq",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "documents" => Self::Documents,
            "wechat" => Self::Wechat,
            "qq" => Self::Qq,
            _ => Self::Onedrive,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Onedrive => "OneDrive",
            Self::Documents => "文档",
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
        let store = CatalogStore::open(data_directory.join("fanfan.db"))?;
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
            candidates.push((CandidateRootType::Documents, documents.clone()));
            candidates.push((CandidateRootType::Wechat, documents.join("WeChat Files")));
            candidates.push((CandidateRootType::Qq, documents.join("Tencent Files")));
        }
        // 收集已授权资料位置的规范化路径键，避免对已授权目录或已被其覆盖的
        // 子目录重复推荐（如已授权“文档”，文档本身与文档下的微信/QQ 都不再推荐）。
        let authorized_keys: Vec<String> = self
            .store
            .list_roots()?
            .into_iter()
            .filter(|root| root.enabled)
            .map(|root| path_key(&PathBuf::from(&root.canonical_path)))
            .collect();
        let mut seen = std::collections::HashSet::new();
        for (candidate_type, path) in candidates {
            if !path.is_dir() {
                continue;
            }
            let canonical = std::fs::canonicalize(&path).unwrap_or(path);
            let key = path_key(&canonical);
            if seen.insert(key.clone()) && !Self::overlaps_authorized(&key, &authorized_keys) {
                self.store.upsert_candidate_root(
                    candidate_type,
                    &canonical.to_string_lossy(),
                    &key,
                )?;
            }
        }
        self.store.list_candidate_roots()
    }

    /// 判断候选路径键是否与某个已授权资料位置存在覆盖关系（相等、候选是已授权目录的
    /// 子目录、或已授权目录是候选的子目录）。被覆盖的候选不再推荐，避免重复加入。
    fn overlaps_authorized(candidate_key: &str, authorized_keys: &[String]) -> bool {
        authorized_keys.iter().any(|root_key| {
            candidate_key == root_key
                || candidate_key.starts_with(&format!("{root_key}\\"))
                || root_key.starts_with(&format!("{candidate_key}\\"))
        })
    }

    pub fn list_candidate_roots(&self) -> Result<Vec<CandidateRoot>, AppError> {
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
                "翻翻的内部数据目录不能作为资料来源",
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

    pub fn cleanup_disabled_root(&self, root_id: &Uuid) -> Result<u64, AppError> {
        self.store.cleanup_disabled_root(root_id)
    }

    pub fn list_files(&self) -> Result<Vec<FileRecord>, AppError> {
        self.store.list_files()
    }

    pub fn processing_coverage_snapshot(&self) -> Result<ProcessingCoverageSnapshot, AppError> {
        self.store.processing_coverage_snapshot()
    }

    pub fn evaluation_integrity_snapshot(
        &self,
    ) -> Result<crate::EvaluationIntegritySnapshot, AppError> {
        self.store.evaluation_integrity_snapshot()
    }

    pub fn query_files(&self, request: &crate::FileQuery) -> Result<crate::FilePage, AppError> {
        self.store.query_files(request)
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

    pub fn query_inbox(&self, request: &InboxQuery) -> Result<InboxPage, AppError> {
        self.store.query_inbox(request)
    }

    pub fn update_inbox_item(
        &self,
        request: &InboxUpdateRequest,
    ) -> Result<crate::InboxItem, AppError> {
        self.store.update_inbox_item(request)
    }

    pub fn retry_inbox_item(&self, inbox_id: &Uuid) -> Result<crate::InboxItem, AppError> {
        self.store.retry_inbox_item(inbox_id)
    }

    pub fn storage_quota_override(&self) -> Result<Option<u64>, AppError> {
        self.store.storage_quota_override()
    }

    pub fn set_storage_quota_override(&self, quota_bytes: u64) -> Result<u64, AppError> {
        self.store.set_storage_quota_override(quota_bytes)
    }

    /// 读取用户当前选定的官方模型预设 id（未选择时返回 `None`）。
    pub fn selected_preset_id(&self) -> Result<Option<String>, AppError> {
        self.store.selected_preset_id()
    }

    /// 持久化用户选定的官方模型预设 id。只保存 preset_id 而非展示名，
    /// 便于未来升级 Qwen/Embedding 而不破坏迁移。
    pub fn set_selected_preset_id(&self, preset_id: &str) -> Result<(), AppError> {
        self.store.set_selected_preset_id(preset_id)
    }

    /// 读取官方模型预设 schema 版本（未写入时返回 0）。
    pub fn model_preset_version(&self) -> Result<u32, AppError> {
        self.store.model_preset_version()
    }

    /// 持久化官方模型预设 schema 版本。
    pub fn set_model_preset_version(&self, version: u32) -> Result<(), AppError> {
        self.store.set_model_preset_version(version)
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

    /// DocumentProfile 生产链（Step 1）：为「已解析 + 全量嵌入完成」的文件
    /// 构建/重建文档画像。后台批次调用，单文件失败只跳过。
    pub fn refresh_document_profiles(
        &self,
        model_artifact_id: &str,
        max_files: u32,
    ) -> Result<crate::ProfileRefreshResult, AppError> {
        self.store
            .refresh_document_profiles(model_artifact_id, max_files)
    }

    /// 强制重建画像（单文件或全部，不重建 Chunk Embedding）。
    pub fn rebuild_document_profiles(
        &self,
        model_artifact_id: &str,
        file_ids: Option<&[Uuid]>,
    ) -> Result<crate::ProfileRefreshResult, AppError> {
        self.store
            .rebuild_document_profiles(model_artifact_id, file_ids)
    }

    pub fn get_document_profile(
        &self,
        file_id: Uuid,
    ) -> Result<Option<crate::DocumentProfile>, AppError> {
        self.store.get_document_profile(file_id)
    }

    /// 列出待分类画像（document_type IS NULL 且当前版本在场），供分类器扫描。
    pub fn list_profiles_needing_classification(
        &self,
        limit: u32,
    ) -> Result<Vec<(crate::DocumentProfile, String)>, AppError> {
        self.store.list_profiles_needing_classification(limit)
    }

    /// 读取画像已存的文档级向量（分类器与原型向量比对用）。
    pub fn profile_vector(&self, file_id: &Uuid) -> Result<Option<Vec<f32>>, AppError> {
        self.store.profile_vector(file_id)
    }

    /// 回写画像的分类器扩展列（document_type/type_confidence 等）。
    pub fn update_document_profile_classifier(
        &self,
        profile: &crate::DocumentProfile,
    ) -> Result<bool, AppError> {
        self.store.update_document_profile_classifier(profile)
    }

    // ------------------------------ Memory 数据层（Step 3） ------------------------------

    pub fn upsert_memory_entity(
        &self,
        entity_type: &str,
        canonical_name: &str,
        metadata_json: &serde_json::Value,
    ) -> Result<Uuid, AppError> {
        self.store
            .upsert_memory_entity(entity_type, canonical_name, metadata_json)
    }

    pub fn memory_entity_by_id(
        &self,
        entity_id: Uuid,
    ) -> Result<Option<crate::MemoryEntity>, AppError> {
        self.store.memory_entity_by_id(entity_id)
    }

    pub fn memory_entity_by_name(
        &self,
        entity_type: &str,
        canonical_name: &str,
    ) -> Result<Option<crate::MemoryEntity>, AppError> {
        self.store
            .memory_entity_by_name(entity_type, canonical_name)
    }

    pub fn list_memory_entities(&self, limit: u32) -> Result<Vec<crate::MemoryEntity>, AppError> {
        self.store.list_memory_entities(limit)
    }

    pub fn update_memory_entity(
        &self,
        entity_id: Uuid,
        canonical_name: &str,
        metadata_json: &serde_json::Value,
    ) -> Result<bool, AppError> {
        self.store
            .update_memory_entity(entity_id, canonical_name, metadata_json)
    }

    pub fn delete_memory_entity(&self, entity_id: Uuid) -> Result<bool, AppError> {
        self.store.delete_memory_entity(entity_id)
    }

    pub fn upsert_memory_relation(
        &self,
        input: &crate::MemoryWriteInput,
    ) -> Result<Uuid, AppError> {
        self.store.upsert_memory_relation(input)
    }

    pub fn memory_relation_by_id(
        &self,
        relation_id: Uuid,
    ) -> Result<Option<crate::MemoryRelation>, AppError> {
        self.store.memory_relation_by_id(relation_id)
    }

    pub fn update_memory_relation_status(
        &self,
        relation_id: Uuid,
        status: crate::MemoryStatus,
    ) -> Result<bool, AppError> {
        self.store
            .update_memory_relation_status(relation_id, status)
    }

    pub fn list_memory_relations_by_subject(
        &self,
        subject_type: crate::MemoryTargetType,
        subject_id: Uuid,
        status: Option<crate::MemoryStatus>,
        limit: u32,
    ) -> Result<Vec<crate::MemoryRelation>, AppError> {
        self.store
            .list_memory_relations_by_subject(subject_type, subject_id, status, limit)
    }

    pub fn list_memory_relations_by_object(
        &self,
        object_type: crate::MemoryTargetType,
        object_id: Uuid,
        status: Option<crate::MemoryStatus>,
        limit: u32,
    ) -> Result<Vec<crate::MemoryRelation>, AppError> {
        self.store
            .list_memory_relations_by_object(object_type, object_id, status, limit)
    }

    pub fn list_memory_relation_candidates(
        &self,
        limit: u32,
    ) -> Result<Vec<crate::MemoryRelation>, AppError> {
        self.store.list_memory_relation_candidates(limit)
    }

    /// 列出全部关系（可按状态过滤；Memory Resolver 定位用）。
    pub fn list_memory_relations(
        &self,
        status: Option<crate::MemoryStatus>,
        limit: u32,
    ) -> Result<Vec<crate::MemoryRelation>, AppError> {
        self.store
            .list_memory_relations("1 = 1", Vec::new(), status, limit)
    }

    pub fn delete_memory_relation(&self, relation_id: Uuid) -> Result<bool, AppError> {
        self.store.delete_memory_relation(relation_id)
    }

    pub fn upsert_memory_alias(&self, input: &crate::MemoryWriteInput) -> Result<Uuid, AppError> {
        self.store.upsert_memory_alias(input)
    }

    pub fn find_memory_aliases(&self, alias: &str) -> Result<Vec<crate::MemoryAlias>, AppError> {
        self.store.find_memory_aliases(alias)
    }

    pub fn memory_alias_by_id(
        &self,
        alias_id: Uuid,
    ) -> Result<Option<crate::MemoryAlias>, AppError> {
        self.store.memory_alias_by_id(alias_id)
    }

    /// 别名 confirm / reject（Phase 4.2「待确认的记忆」）。
    pub fn update_memory_alias_status(
        &self,
        alias_id: Uuid,
        status: crate::MemoryStatus,
    ) -> Result<bool, AppError> {
        self.store.update_memory_alias_status(alias_id, status)
    }

    pub fn bump_memory_alias(&self, alias_id: Uuid) -> Result<bool, AppError> {
        self.store.bump_memory_alias(alias_id)
    }

    pub fn list_memory_aliases(&self, limit: u32) -> Result<Vec<crate::MemoryAlias>, AppError> {
        self.store.list_memory_aliases(limit)
    }

    pub fn delete_memory_alias(&self, alias_id: Uuid) -> Result<bool, AppError> {
        self.store.delete_memory_alias(alias_id)
    }

    /// 清空全部记忆（aliases / relations / entities）。
    pub fn clear_memory(&self) -> Result<u64, AppError> {
        self.store.clear_memory()
    }

    pub fn invalidate_memory_for_file(&self, file_id: Uuid) -> Result<u64, AppError> {
        self.store.invalidate_memory_for_file(file_id)
    }

    /// Memory Resolver 合法性检查（Step 4）：文件必须存在、在场且位于授权根。
    pub fn memory_file_target_valid(&self, file_id: Uuid) -> Result<bool, AppError> {
        self.store.memory_file_target_valid(file_id)
    }

    /// Memory Resolver 合法性检查：收藏集必须真实存在。
    pub fn memory_collection_target_valid(&self, collection_id: Uuid) -> Result<bool, AppError> {
        self.store.memory_collection_target_valid(collection_id)
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

    pub fn apply_collection_model_naming(
        &self,
        suggestion_id: &Uuid,
        review: &crate::CollectionModelReview,
        model_version: &str,
    ) -> Result<crate::CollectionSuggestion, AppError> {
        self.store
            .apply_collection_model_naming(suggestion_id, review, model_version)
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

    pub fn collection_suggestion_member_summaries(
        &self,
        suggestion_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, AppError> {
        self.store
            .collection_suggestion_member_summaries(suggestion_ids)
    }

    pub fn prune_collection_suggestion_members(
        &self,
        suggestion_id: &Uuid,
        removed_file_ids: &[Uuid],
    ) -> Result<bool, AppError> {
        self.store
            .prune_collection_suggestion_members(suggestion_id, removed_file_ids)
    }

    pub fn refresh_file_relations(
        &self,
        max_files: u32,
    ) -> Result<RelationRefreshResult, AppError> {
        self.store.refresh_file_relations(max_files)
    }

    pub fn refresh_semantic_file_relations(
        &self,
        model_artifact_id: &str,
        max_files: u32,
    ) -> Result<(u64, u64), AppError> {
        self.store
            .refresh_semantic_file_relations(model_artifact_id, max_files)
    }

    pub fn refresh_relation_groups(
        &self,
        model_artifact_id: Option<&str>,
    ) -> Result<u64, AppError> {
        self.store.refresh_relation_groups(model_artifact_id)
    }

    pub fn query_relation_groups(
        &self,
        request: &RelationGroupQuery,
    ) -> Result<RelationGroupPage, AppError> {
        self.store.query_relation_groups(request)
    }

    pub fn review_relation_group(&self, group_id: &Uuid, action: &str) -> Result<(), AppError> {
        self.store.review_relation_group(group_id, action)
    }

    pub fn review_relation_groups(
        &self,
        group_ids: &[Uuid],
        action: &str,
    ) -> Result<u64, AppError> {
        self.store.review_relation_groups(group_ids, action)
    }

    pub fn list_file_relations(&self, limit: u32) -> Result<Vec<FileRelation>, AppError> {
        self.store.list_file_relations(limit)
    }

    pub fn count_exact_duplicate_relations(&self) -> Result<u64, AppError> {
        self.store.count_exact_duplicate_relations()
    }

    pub fn query_file_relations(&self, request: &RelationQuery) -> Result<RelationPage, AppError> {
        self.store.query_file_relations(request)
    }

    pub fn review_file_relation(&self, relation_id: &Uuid, action: &str) -> Result<(), AppError> {
        self.store.review_file_relation(relation_id, action)
    }

    pub fn review_file_relations(
        &self,
        relation_ids: &[Uuid],
        action: &str,
    ) -> Result<u64, AppError> {
        self.store.review_file_relations(relation_ids, action)
    }

    pub fn list_pending_parse_files(&self, limit: usize) -> Result<Vec<FileRecord>, AppError> {
        self.store.list_pending_parse_files(limit)
    }

    pub fn retry_ocr(&self, file_id: &Uuid) -> Result<(), AppError> {
        self.store.retry_ocr(file_id)
    }

    pub fn requeue_ocr_pending_for_available_runtime(&self, limit: usize) -> Result<u64, AppError> {
        self.store.requeue_ocr_pending_for_available_runtime(limit)
    }

    pub fn sanitize_existing_ocr_attempt_errors(&self) -> Result<u64, AppError> {
        self.store.sanitize_existing_ocr_attempt_errors()
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

    pub fn recover_interrupted_image_ocr(&self) -> Result<u64, AppError> {
        self.store.recover_interrupted_image_ocr()
    }

    pub fn backfill_ready_image_search_nodes(&self, limit: usize) -> Result<Vec<Uuid>, AppError> {
        self.store.backfill_ready_image_search_nodes(limit)
    }

    pub fn claim_pending_image_ocr(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<PendingImageOcr>, AppError> {
        self.store.claim_pending_image_ocr(model_artifact_id)
    }

    pub fn commit_image_ocr(&self, result: &ImageOcrResult) -> Result<(), AppError> {
        self.store.commit_image_ocr(result)
    }

    pub fn fail_image_ocr(&self, asset_id: &Uuid, error: &AppError) -> Result<(), AppError> {
        self.store.fail_image_ocr(asset_id, error)
    }

    pub fn claim_pending_image_understanding(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<PendingImageUnderstanding>, AppError> {
        self.store
            .claim_pending_image_understanding(model_artifact_id)
    }

    pub fn claim_pending_ocr_image_understanding(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<PendingImageUnderstanding>, AppError> {
        self.store
            .claim_pending_ocr_image_understanding(model_artifact_id)
    }

    pub fn promote_ocr_pending_file_when_assets_ready(
        &self,
        file_id: &Uuid,
    ) -> Result<bool, AppError> {
        self.store
            .promote_ocr_pending_file_when_assets_ready(file_id)
    }

    pub fn list_ocr_pending_files(&self) -> Result<Vec<Uuid>, AppError> {
        self.store.list_ocr_pending_files()
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

    pub fn list_ask_sessions(
        &self,
        cursor: Option<&str>,
        page_size: u32,
    ) -> Result<crate::AskSessionPage, AppError> {
        self.store.list_ask_sessions(cursor, page_size)
    }

    pub fn list_ask_messages(
        &self,
        session_id: &Uuid,
        cursor: Option<&str>,
        page_size: u32,
    ) -> Result<crate::AskMessagePage, AppError> {
        self.store.list_ask_messages(session_id, cursor, page_size)
    }

    pub fn rename_ask_session(&self, session_id: &Uuid, title: &str) -> Result<(), AppError> {
        self.store.rename_ask_session(session_id, title)
    }

    pub fn delete_ask_session(&self, session_id: &Uuid) -> Result<(), AppError> {
        self.store.delete_ask_session(session_id)
    }

    pub fn record_ask_failure(
        &self,
        request: &AskRequest,
        error: &AppError,
    ) -> Result<(), AppError> {
        self.store.record_ask_failure(request, error)
    }

    pub fn get_ask_session_context(
        &self,
        session_id: Uuid,
    ) -> Result<Option<crate::AskSessionContext>, AppError> {
        self.store.get_ask_session_context(session_id)
    }

    pub fn update_ask_session_context(
        &self,
        session_id: Uuid,
        context: &crate::AskSessionContext,
    ) -> Result<(), AppError> {
        self.store.update_ask_session_context(session_id, context)
    }

    pub fn clear_ask_session_context(&self, session_id: Uuid) -> Result<(), AppError> {
        self.store.clear_ask_session_context(session_id)
    }

    /// 列出文档画像（Document Resolver 候选数据源），返回 (画像, 文件名)。
    pub fn list_document_profiles(
        &self,
        document_type: Option<crate::DocumentType>,
        limit: u32,
    ) -> Result<Vec<(crate::DocumentProfile, String)>, AppError> {
        self.store.list_document_profiles(document_type, limit)
    }

    /// 当前修订的 document_nodes 总数（SUMMARY 截断追踪用；未解析 → 0）。
    pub fn file_document_node_count(&self, file_id: &Uuid) -> Result<u64, AppError> {
        self.store.file_document_node_count(file_id)
    }

    /// 批量读取画像向量（文档级召回精排用，key = file_id）。
    pub fn profile_vectors(
        &self,
        file_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<f32>>, AppError> {
        self.store.profile_vectors(file_ids)
    }

    pub fn answer_result(&self, message_id: &Uuid) -> Result<AnswerResult, AppError> {
        self.store.answer_result(message_id)
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

    /// 当前已激活的向量索引代所采用的 embedding 模型 artifact id（无则 `None`）。
    /// 供 `index_stale_check` 与索引重建提示使用。
    pub fn active_index_model_artifact_id(&self) -> Result<Option<String>, AppError> {
        self.store.active_index_model_artifact_id()
    }

    /// 语义索引重建进度：返回 (目标模型已嵌入分块数, 可搜索分块总数)。
    /// 供前端在「换模型→重建索引」期间轮询展示进度。
    pub fn embedding_rebuild_progress(
        &self,
        model_artifact_id: &str,
    ) -> Result<(u64, u64), AppError> {
        self.store.embedding_rebuild_progress(model_artifact_id)
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

    pub fn record_node_trace(&self, input: &TraceNodeInput) -> Result<(), AppError> {
        self.store.record_node_trace_input(input)
    }

    pub fn record_operation_trace(&self, input: &OperationTraceInput) -> Result<String, AppError> {
        self.store.record_operation_trace(input)
    }

    pub fn complete_operation_trace(
        &self,
        operation_id: &str,
        status: &str,
    ) -> Result<(), AppError> {
        self.store.complete_operation_trace(operation_id, status)
    }

    pub fn query_operation_traces(
        &self,
        request: &OperationTraceQuery,
    ) -> Result<OperationTracePage, AppError> {
        self.store.query_operation_traces(request)
    }

    pub fn record_evaluation_case(&self, case: &EvaluationCaseRecord) -> Result<(), AppError> {
        self.store.record_evaluation_case(case)
    }

    pub fn record_evaluation_cases(
        &self,
        cases: &[EvaluationCaseRecord],
    ) -> Result<usize, AppError> {
        self.store.record_evaluation_cases(cases)
    }

    pub fn record_evaluation_run(&self, run: &EvaluationRunRecord) -> Result<(), AppError> {
        self.store.record_evaluation_run(run)
    }

    pub fn complete_evaluation_run(
        &self,
        run_id: &str,
        metrics: &serde_json::Value,
    ) -> Result<(), AppError> {
        self.store.complete_evaluation_run(run_id, metrics)
    }

    pub fn record_evaluation_result(
        &self,
        result: &EvaluationResultRecord,
    ) -> Result<(), AppError> {
        self.store.record_evaluation_result(result)
    }

    pub fn record_evaluation_results(
        &self,
        results: &[EvaluationResultRecord],
    ) -> Result<usize, AppError> {
        self.store.record_evaluation_results(results)
    }

    pub fn query_evaluation_cases(
        &self,
        split: &str,
        feature_type: Option<&str>,
    ) -> Result<Vec<EvaluationCaseRecord>, AppError> {
        self.store.query_evaluation_cases(split, feature_type)
    }

    pub fn query_evaluation_runs(
        &self,
        optimization_round: Option<u32>,
    ) -> Result<Vec<EvaluationRunRecord>, AppError> {
        self.store.query_evaluation_runs(optimization_round)
    }

    pub fn query_evaluation_results(
        &self,
        run_id: &str,
    ) -> Result<Vec<EvaluationResultRecord>, AppError> {
        self.store.query_evaluation_results(run_id)
    }

    pub fn query_node_traces(&self, request: &NodeTraceQuery) -> Result<NodeTracePage, AppError> {
        self.store.query_node_traces(request)
    }

    pub fn query_node_traces_by_correlation(
        &self,
        flow: &str,
        correlation_id: &str,
    ) -> Result<Vec<NodeTraceRecord>, AppError> {
        self.store
            .query_node_traces_by_correlation(flow, correlation_id)
    }

    pub fn clear_node_traces(&self) -> Result<u64, AppError> {
        self.store.clear_node_traces()
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

    /// 是否存在任何已激活的向量索引代际（不限 Embedding 模型）。
    /// 用于 Embedding 换代但尚未为新模型建索引的回落提示；只读不触碰索引数据。
    pub fn any_active_vector_generation(&self) -> Result<bool, AppError> {
        self.store.any_active_vector_generation()
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

    /// DOCUMENT_SUMMARY 用：按文档顺序读取当前修订的全部 chunk。
    pub fn file_chunks(&self, file_id: &Uuid) -> Result<Vec<crate::ContentChunk>, AppError> {
        self.store.file_chunks(file_id)
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
            Ok(outcome) => match self.store.commit_scan(&root_id, &job_id, &outcome) {
                Ok(job) => Ok(job),
                Err(error) => self.store.fail_scan(&root_id, &job_id, error),
            },
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
