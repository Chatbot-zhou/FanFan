use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

use crate::knowledge::DocumentProfile;
use crate::memory::{MemoryAlias, MemoryEntity, MemoryRelation};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub user_action: Option<String>,
    pub file_id: Option<Uuid>,
    pub details: Option<Box<Value>>,
}

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            user_action: None,
            file_id: None,
            details: None,
        }
    }

    pub fn local_config(message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: "LOCAL_CONFIG_ERROR".into(),
            message: message.into(),
            retryable,
            user_action: Some("请检查本地配置目录是否可写".into()),
            file_id: None,
            details: None,
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AppError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScopeFilter {
    pub root_ids: Vec<Uuid>,
    pub collection_ids: Vec<Uuid>,
    pub file_ids: Vec<Uuid>,
    pub extensions: Vec<String>,
    pub modified_from: Option<DateTime<Utc>>,
    pub modified_to: Option<DateTime<Utc>>,
    pub availability: Availability,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Present,
    Missing,
    Unreadable,
}

impl Availability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
            Self::Unreadable => "unreadable",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "missing" => Self::Missing,
            "unreadable" => Self::Unreadable,
            _ => Self::Present,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParseStatus {
    Pending,
    Parsing,
    OcrPending,
    Parsed,
    Unsupported,
    Encrypted,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileProcessingDisposition {
    ParseableContent,
    ImageOcr,
    ReadOnlyText,
    ArchiveManifest,
    MediaMetadata,
    SafeMetadata,
    EncryptedOrDamaged,
    CapabilityMissing,
    Unknown,
}

impl FileProcessingDisposition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParseableContent => "parseable_content",
            Self::ImageOcr => "image_ocr",
            Self::ReadOnlyText => "read_only_text",
            Self::ArchiveManifest => "archive_manifest",
            Self::MediaMetadata => "media_metadata",
            Self::SafeMetadata => "safe_metadata",
            Self::EncryptedOrDamaged => "encrypted_or_damaged",
            Self::CapabilityMissing => "capability_missing",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "parseable_content" => Self::ParseableContent,
            "image_ocr" => Self::ImageOcr,
            "read_only_text" => Self::ReadOnlyText,
            "archive_manifest" => Self::ArchiveManifest,
            "media_metadata" => Self::MediaMetadata,
            "safe_metadata" => Self::SafeMetadata,
            "encrypted_or_damaged" => Self::EncryptedOrDamaged,
            "capability_missing" => Self::CapabilityMissing,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessingAttempt {
    pub attempt_id: Uuid,
    pub file_id: Option<Uuid>,
    pub revision_id: Option<Uuid>,
    pub operation: String,
    pub engine: Option<String>,
    pub model_version: Option<String>,
    pub status: String,
    pub attempt_no: u32,
    pub elapsed_ms: u64,
    pub retryable: bool,
    pub error: Option<AppError>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanCheckpoint {
    pub job_id: Uuid,
    pub root_id: Uuid,
    pub batch_no: u32,
    pub enumerated_items: u64,
    pub committed_items: u64,
    pub isolated_failures: u64,
    pub retry_count: u32,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProcessingCoverageSnapshot {
    pub discovered_files: u64,
    pub parseable_files: u64,
    pub parsed_files: u64,
    pub failed_files: u64,
    pub explicitly_excluded_files: u64,
    pub fts_chunks: u64,
    pub embedding_chunks: u64,
    pub active_vector_keys: u64,
    pub pending_ocr_assets: u64,
    pub pending_vision_assets: u64,
    pub parse_coverage: f64,
    pub embedding_coverage: f64,
    pub vector_coverage: f64,
    pub measured_at: DateTime<Utc>,
}

impl ParseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Parsing => "parsing",
            Self::OcrPending => "ocr_pending",
            Self::Parsed => "parsed",
            Self::Unsupported => "unsupported",
            Self::Encrypted => "encrypted",
            Self::Failed => "failed",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "parsing" => Self::Parsing,
            "ocr_pending" => Self::OcrPending,
            "parsed" => Self::Parsed,
            "unsupported" => Self::Unsupported,
            "encrypted" => Self::Encrypted,
            "failed" => Self::Failed,
            _ => Self::Pending,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRecord {
    pub file_id: Uuid,
    pub volume_id: String,
    #[serde(
        rename(serialize = "display_path"),
        alias = "display_path",
        serialize_with = "serialize_display_path"
    )]
    pub canonical_path: String,
    pub display_name: String,
    pub extension: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub fs_created_at: Option<DateTime<Utc>>,
    pub fs_modified_at: DateTime<Utc>,
    pub windows_file_id: Option<String>,
    pub content_sha256: Option<String>,
    pub availability: Availability,
    pub current_revision_id: Option<Uuid>,
    pub parse_status: ParseStatus,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

pub fn privacy_safe_display_path(path: &str) -> String {
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

pub fn serialize_display_path<S>(path: &str, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&privacy_safe_display_path(path))
}

pub fn serialize_optional_display_path<S>(
    path: &Option<String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    path.as_deref()
        .map(privacy_safe_display_path)
        .serialize(serializer)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileQuery {
    pub cursor: Option<String>,
    pub page_size: u32,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub parse_statuses: Vec<String>,
    #[serde(default)]
    pub availability: Option<Availability>,
}

impl FileQuery {
    pub fn validated_page_size(&self) -> u32 {
        self.page_size.clamp(1, 200)
    }

    pub fn validate_filters(&self) -> Result<(), AppError> {
        if self
            .query
            .as_ref()
            .is_some_and(|query| query.chars().count() > 200)
            || self.extensions.len() > 32
            || self.parse_statuses.len() > 8
        {
            return Err(AppError::new(
                "FILE_FILTER_INVALID",
                "资料过滤条件过长或数量超出限制",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilePage {
    pub items: Vec<FileRecord>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileRootMembership {
    pub file_id: Uuid,
    pub root_id: Uuid,
    pub relative_path: String,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BoundingBox {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl BoundingBox {
    pub fn is_normalized(&self) -> bool {
        [self.x0, self.y0, self.x1, self.y1]
            .into_iter()
            .all(|value| (0.0..=1.0).contains(&value))
            && self.x0 <= self.x1
            && self.y0 <= self.y1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceLocator {
    pub kind: SourceKind,
    pub page_no: Option<u32>,
    pub slide_no: Option<u32>,
    pub sheet_name: Option<String>,
    pub cell_range: Option<String>,
    pub paragraph_no: Option<u32>,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub shape_no: Option<u32>,
    pub bbox: Option<BoundingBox>,
    pub heading_path: Vec<String>,
}

impl Default for SourceLocator {
    fn default() -> Self {
        Self {
            kind: SourceKind::Text,
            page_no: None,
            slide_no: None,
            sheet_name: None,
            cell_range: None,
            paragraph_no: None,
            line_start: None,
            line_end: None,
            shape_no: None,
            bbox: None,
            heading_path: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Pdf,
    Docx,
    Spreadsheet,
    Presentation,
    Text,
    Code,
    Archive,
    Image,
}

/// 文档类型画像（Document Profile 的 document_type 维度）。
/// 第一版类型全集；文件名只作弱信号，类型判断主要依赖正文语义
/// （title / summary / section titles / entities / embedding）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentType {
    Resume,
    Contract,
    Invoice,
    Paper,
    ProjectDocument,
    Meeting,
    LearningMaterial,
    Certificate,
    Report,
    Spreadsheet,
    Other,
}

impl DocumentType {
    /// 全部类型的稳定枚举值（prompt 词表与校验共用）。
    pub const ALL: [DocumentType; 11] = [
        DocumentType::Resume,
        DocumentType::Contract,
        DocumentType::Invoice,
        DocumentType::Paper,
        DocumentType::ProjectDocument,
        DocumentType::Meeting,
        DocumentType::LearningMaterial,
        DocumentType::Certificate,
        DocumentType::Report,
        DocumentType::Spreadsheet,
        DocumentType::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DocumentType::Resume => "resume",
            DocumentType::Contract => "contract",
            DocumentType::Invoice => "invoice",
            DocumentType::Paper => "paper",
            DocumentType::ProjectDocument => "project_document",
            DocumentType::Meeting => "meeting",
            DocumentType::LearningMaterial => "learning_material",
            DocumentType::Certificate => "certificate",
            DocumentType::Report => "report",
            DocumentType::Spreadsheet => "spreadsheet",
            DocumentType::Other => "other",
        }
    }

    /// 中文展示名：文档级召回的类型命中信号用（问题通常用中文指称类型，
    /// 如「我的简历」「那几份合同」，而 as_str 是英文变体名，无法子串匹配）。
    pub fn display_name(self) -> &'static str {
        match self {
            DocumentType::Resume => "简历",
            DocumentType::Contract => "合同",
            DocumentType::Invoice => "发票",
            DocumentType::Paper => "论文",
            DocumentType::ProjectDocument => "项目文档",
            DocumentType::Meeting => "会议纪要",
            DocumentType::LearningMaterial => "学习资料",
            DocumentType::Certificate => "证书",
            DocumentType::Report => "报告",
            DocumentType::Spreadsheet => "表格",
            DocumentType::Other => "其他",
        }
    }

    /// 宽容解析：去空白/连字符/下划线后按小写匹配变体名。
    /// 覆盖 LLM 输出的大小写变体（"RESUME"/"Resume"/"project-document" 等）。
    pub fn parse_lenient(input: &str) -> Option<DocumentType> {
        let normalized: String = input
            .chars()
            .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '_')
            .collect::<String>()
            .to_ascii_lowercase();
        DocumentType::ALL
            .iter()
            .copied()
            .find(|variant| variant.as_str().replace('_', "") == normalized)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvidenceRef {
    pub evidence_id: Uuid,
    pub file_id: Uuid,
    pub revision_id: Uuid,
    pub node_id: Uuid,
    pub chunk_id: Uuid,
    #[serde(default)]
    pub image_asset_id: Option<Uuid>,
    pub quote: String,
    /// 命中块在原文中紧邻的前一块文本（同节点 ordinal-1，已按 token 上限截断）。
    /// 只用于让生成模型理解线性上下文，不作为可引用证据。
    #[serde(default)]
    pub context_before: Option<String>,
    /// 命中块在原文中紧邻的后一块文本（同节点 ordinal+1，已按 token 上限截断）。
    /// 只用于让生成模型理解线性上下文，不作为可引用证据。
    #[serde(default)]
    pub context_after: Option<String>,
    pub locator: SourceLocator,
    pub retrieval_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobRecord {
    pub job_id: Uuid,
    pub job_type: String,
    pub status: JobStatus,
    pub stage: String,
    pub progress: f32,
    pub processed_items: u64,
    pub total_items: u64,
    pub error: Option<AppError>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Paused,
    AwaitingUser,
    Succeeded,
    Partial,
    Failed,
    Cancelled,
}

impl JobRecord {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(0.0..=1.0).contains(&self.progress) {
            return Err(AppError {
                code: "SCHEMA_INVALID_PROGRESS".into(),
                message: "任务进度必须位于 0 到 1 之间".into(),
                retryable: false,
                user_action: None,
                file_id: None,
                details: None,
            });
        }
        if self.processed_items > self.total_items && self.total_items > 0 {
            return Err(AppError {
                code: "SCHEMA_INVALID_COUNTS".into(),
                message: "已处理数量不能超过总数量".into(),
                retryable: false,
                user_action: None,
                file_id: None,
                details: None,
            });
        }
        Ok(())
    }

    pub fn can_transition_to(&self, next: JobStatus) -> bool {
        self.status == next
            || matches!(
                (self.status, next),
                (JobStatus::Queued, JobStatus::Running | JobStatus::Cancelled)
                    | (
                        JobStatus::Running,
                        JobStatus::Paused
                            | JobStatus::AwaitingUser
                            | JobStatus::Succeeded
                            | JobStatus::Partial
                            | JobStatus::Failed
                            | JobStatus::Cancelled
                    )
                    | (
                        JobStatus::Paused | JobStatus::AwaitingUser,
                        JobStatus::Running | JobStatus::Failed | JobStatus::Cancelled
                    )
            )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DegradationLevel {
    Full,
    Balanced,
    Core,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DegradationState {
    pub level: DegradationLevel,
    pub triggers: Vec<String>,
    pub disabled_features: Vec<String>,
    pub entered_at: Option<DateTime<Utc>>,
    pub recover_after: Option<DateTime<Utc>>,
    pub manual_override: bool,
}

impl DegradationState {
    pub fn full() -> Self {
        Self {
            level: DegradationLevel::Full,
            triggers: Vec::new(),
            disabled_features: Vec::new(),
            entered_at: None,
            recover_after: None,
            manual_override: false,
        }
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.level == DegradationLevel::Full
            && (!self.triggers.is_empty() || !self.disabled_features.is_empty())
        {
            return Err(AppError::new(
                "SCHEMA_INVALID_DEGRADATION_STATE",
                "full状态不能保留降级触发原因或禁用能力",
                false,
            ));
        }
        if self.level != DegradationLevel::Full
            && (self.triggers.is_empty() || self.entered_at.is_none())
        {
            return Err(AppError::new(
                "SCHEMA_INVALID_DEGRADATION_STATE",
                "降级状态必须记录触发原因和进入时间",
                false,
            ));
        }
        if let (Some(entered_at), Some(recover_after)) = (self.entered_at, self.recover_after)
            && recover_after < entered_at
        {
            return Err(AppError::new(
                "SCHEMA_INVALID_RECOVERY_TIME",
                "恢复检查时间不能早于降级进入时间",
                false,
            ));
        }
        Ok(())
    }

    pub fn can_transition_to(&self, next: DegradationLevel) -> bool {
        self.level == next
            || matches!(
                (self.level, next),
                (DegradationLevel::Full, DegradationLevel::Balanced)
                    | (DegradationLevel::Balanced, DegradationLevel::Full)
                    | (DegradationLevel::Balanced, DegradationLevel::Core)
                    | (DegradationLevel::Core, DegradationLevel::Balanced)
            )
    }
}

/// 一次链路节点的输入输出快照（用于复盘优化，明文存储）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeTraceRecord {
    pub trace_id: String,
    /// ask | search | relation | collection
    pub flow: String,
    pub node: String,
    /// 一次用户操作的关联键（Ask 用 operation_id，其余用现有 correlation_id）
    pub correlation_id: String,
    pub session_id: Option<String>,
    /// suggestion_id / claim 序号等，按节点类型而异
    pub entity_id: Option<String>,
    pub input_json: Value,
    pub output_json: Value,
    /// ok | error
    pub status: String,
    pub elapsed_ms: Option<u64>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeTraceQuery {
    pub flow: Option<String>,
    pub node: Option<String>,
    pub cursor: Option<String>,
    pub page_size: u32,
}

impl NodeTraceQuery {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=500).contains(&self.page_size) {
            return Err(AppError::new(
                "NODE_TRACE_LIMIT_INVALID",
                "追踪记录读取数量需要在1到500之间",
                false,
            ));
        }
        self.offset().map(|_| ())
    }

    pub fn offset(&self) -> Result<u64, AppError> {
        self.cursor
            .as_deref()
            .unwrap_or("0")
            .parse::<u64>()
            .map_err(|_| AppError::new("NODE_TRACE_CURSOR_INVALID", "追踪记录分页游标无效", false))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeTracePage {
    pub items: Vec<NodeTraceRecord>,
    pub next_cursor: Option<String>,
    pub total: u64,
}

/// 一次 Ask 的逐阶段耗时（阶段未出现则为 null）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AskTraceTiming {
    pub source_router_ms: Option<u64>,
    pub query_parser_ms: Option<u64>,
    pub context_ms: Option<u64>,
    pub memory_ms: Option<u64>,
    pub document_resolver_ms: Option<u64>,
    pub document_recall_ms: Option<u64>,
    pub embedding_ms: Option<u64>,
    pub fts_ms: Option<u64>,
    pub rerank_ms: Option<u64>,
    pub generation_ms: Option<u64>,
    pub verification_ms: Option<u64>,
    pub total_ms: Option<u64>,
}

/// Trace Viewer 的一次 Ask 追踪视图：按固定阶段顺序分组的节点记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskTrace {
    pub operation_id: String,
    pub question: Option<String>,
    pub answer_mode: Option<String>,
    pub stages: Vec<AskTraceStage>,
    pub timing: AskTraceTiming,
    pub diagnostic_summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskTraceStage {
    /// 阶段节点名（source_routing / query_parsing / …），展示顺序见 STAGE_ORDER。
    pub node: String,
    /// 该阶段全部记录（按时间正序）
    pub records: Vec<NodeTraceRecord>,
}

/// Debug Trace 导出请求（写文件前需 confirmed）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AskTraceExportRequest {
    pub operation_id: String,
    pub target_path: String,
    /// 开发选项：保留详细文本（chunk 全文 / 模型 prompt）；默认关闭。
    pub include_detailed_text: bool,
    pub confirmed: bool,
}

/// Debug Trace 导出 JSON 文件内容（已脱敏：路径去全路径、chunk 截断、
/// 默认不含模型完整 prompt）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskTraceExport {
    pub schema_version: u32,
    pub generated_at: String,
    pub operation_id: String,
    pub question: Option<String>,
    pub answer_mode: Option<String>,
    pub stages: Vec<AskTraceStage>,
    pub timing: AskTraceTiming,
    pub diagnostic_summary: String,
}

/// Ask Evaluation Runner 请求：读 JSONL/JSON 测试集，批量跑问答，写结果文件。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AskEvaluationRunRequest {
    /// 测试集路径（JSONL 或 JSON 数组）
    pub target_path: String,
    /// 结果文件路径（JSON，写文件前需 confirmed）
    pub output_path: String,
    pub confirmed: bool,
}

/// 批量运行报告：指标 + 每例结果（结果同时落盘 output_path）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskEvaluationRunReport {
    pub schema_version: u32,
    pub generated_at: String,
    pub run_id: String,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub metrics: crate::evaluation::EvaluationRunMetrics,
    pub results: Vec<crate::evaluation::EvaluationRunResult>,
}

/// Document Profile Inspector：一次查询返回画像详情 + 文档级向量在场状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentProfileInspect {
    pub file_id: String,
    pub display_name: String,
    /// 画像尚未构建（含当前 revision 未嵌入完成）时为 None
    pub profile: Option<DocumentProfile>,
    /// 文档级画像向量（分类器/原型比对用）是否已存在
    pub embedding_present: bool,
}

/// 画像重建请求：file_ids 为 None 表示全部文件；不重建 Chunk Embedding。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DocumentProfileRebuildRequest {
    pub file_ids: Option<Vec<String>>,
    pub confirmation: String,
}

/// Memory Inspector 视图（最小实现：三张表 + 可选关键字过滤）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MemoryInspectorView {
    pub aliases: Vec<MemoryAlias>,
    pub relations: Vec<MemoryRelation>,
    pub entities: Vec<MemoryEntity>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MemoryRelationStatusRequest {
    pub relation_id: String,
    /// confirmed | rejected
    pub status: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MemoryClearRequest {
    pub confirmation: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounding_box_must_be_normalized() {
        assert!(
            BoundingBox {
                x0: 0.1,
                y0: 0.2,
                x1: 0.9,
                y1: 1.0,
            }
            .is_normalized()
        );
        assert!(
            !BoundingBox {
                x0: -0.1,
                y0: 0.2,
                x1: 0.9,
                y1: 1.0,
            }
            .is_normalized()
        );
    }

    #[test]
    fn terminal_job_statuses_cannot_restart() {
        let job = JobRecord {
            job_id: Uuid::now_v7(),
            job_type: "scan".into(),
            status: JobStatus::Succeeded,
            stage: "completed".into(),
            progress: 1.0,
            processed_items: 1,
            total_items: 1,
            error: None,
            created_at: Utc::now(),
            started_at: Some(Utc::now()),
            finished_at: Some(Utc::now()),
        };

        assert!(!job.can_transition_to(JobStatus::Running));
        assert!(job.can_transition_to(JobStatus::Succeeded));
    }

    #[test]
    fn public_file_contract_exposes_only_a_sanitized_display_path() {
        let now = Utc::now();
        let record = FileRecord {
            file_id: Uuid::now_v7(),
            volume_id: "volume".into(),
            canonical_path: "C:\\Users\\Private\\Documents\\案件\\证据.pdf".into(),
            display_name: "证据.pdf".into(),
            extension: "pdf".into(),
            mime_type: "application/pdf".into(),
            size_bytes: 1,
            fs_created_at: None,
            fs_modified_at: now,
            windows_file_id: None,
            content_sha256: None,
            availability: Availability::Present,
            current_revision_id: Some(Uuid::now_v7()),
            parse_status: ParseStatus::Parsed,
            first_seen_at: now,
            last_seen_at: now,
        };
        let value = serde_json::to_value(record).expect("serialize public file record");
        assert!(value.get("canonical_path").is_none());
        assert_eq!(value["display_path"], "…\\Documents\\案件\\证据.pdf");
        assert!(!value.to_string().contains("Private"));
    }
}
