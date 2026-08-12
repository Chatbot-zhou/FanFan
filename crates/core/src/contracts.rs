use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

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
