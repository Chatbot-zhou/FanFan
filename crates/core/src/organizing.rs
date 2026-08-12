use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppError, FileRecord, ParseStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxEventType {
    Discovered,
    Modified,
    Renamed,
    Missing,
    Restored,
    OcrRequired,
    ParseFailed,
    RelationSuggested,
    CollectionSuggested,
}

impl InboxEventType {
    pub(crate) fn as_storage(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::Modified => "modified",
            Self::Renamed => "renamed",
            Self::Missing => "missing",
            Self::Restored => "restored",
            Self::OcrRequired => "ocr_required",
            Self::ParseFailed => "parse_failed",
            Self::RelationSuggested => "relation_suggested",
            Self::CollectionSuggested => "collection_suggested",
        }
    }

    pub(crate) fn from_storage(value: &str) -> Self {
        match value {
            "modified" => Self::Modified,
            "renamed" => Self::Renamed,
            "missing" => Self::Missing,
            "restored" => Self::Restored,
            "ocr_required" => Self::OcrRequired,
            "parse_failed" => Self::ParseFailed,
            "relation_suggested" => Self::RelationSuggested,
            "collection_suggested" => Self::CollectionSuggested,
            _ => Self::Discovered,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageStatus {
    New,
    Reviewed,
    Ignored,
    Error,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    Normal,
    PendingRetry,
    Retrying,
    Resolved,
    Abandoned,
}

impl ResolutionStatus {
    pub(crate) fn from_storage(value: &str) -> Self {
        match value {
            "pending_retry" => Self::PendingRetry,
            "retrying" => Self::Retrying,
            "resolved" => Self::Resolved,
            "abandoned" => Self::Abandoned,
            _ => Self::Normal,
        }
    }
}

impl TriageStatus {
    pub(crate) fn as_storage(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reviewed => "reviewed",
            Self::Ignored => "ignored",
            Self::Error => "error",
            Self::All => "all",
        }
    }

    pub(crate) fn from_storage(value: &str) -> Self {
        match value {
            "reviewed" => Self::Reviewed,
            "ignored" => Self::Ignored,
            "error" => Self::Error,
            "all" => Self::All,
            _ => Self::New,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxQuery {
    pub status: TriageStatus,
    #[serde(default)]
    pub event_types: Vec<InboxEventType>,
    #[serde(default)]
    pub root_ids: Vec<Uuid>,
    pub date_from: Option<DateTime<Utc>>,
    pub date_to: Option<DateTime<Utc>>,
    pub cursor: Option<String>,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
}

fn default_page_size() -> u32 {
    50
}

impl InboxQuery {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=200).contains(&self.page_size) {
            return Err(AppError::new(
                "INBOX_QUERY_INVALID",
                "收件箱每页数量必须在1到200之间",
                false,
            ));
        }
        if self
            .date_from
            .zip(self.date_to)
            .is_some_and(|(from, to)| from > to)
        {
            return Err(AppError::new(
                "INBOX_QUERY_INVALID",
                "收件箱开始时间不能晚于结束时间",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub inbox_id: Uuid,
    pub file_id: Uuid,
    pub display_name: String,
    #[serde(
        rename(serialize = "display_path"),
        alias = "display_path",
        serialize_with = "crate::serialize_display_path"
    )]
    pub canonical_path: String,
    pub event_type: InboxEventType,
    pub observed_at: DateTime<Utc>,
    #[serde(
        rename(serialize = "previous_display_path"),
        alias = "previous_display_path",
        serialize_with = "crate::serialize_optional_display_path"
    )]
    pub previous_path: Option<String>,
    pub triage_status: TriageStatus,
    pub resolution_status: ResolutionStatus,
    pub attempt_count: u32,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub retry_action: Option<String>,
    pub suggested_collection_ids: Vec<Uuid>,
    pub duplicate_group_id: Option<Uuid>,
    pub summary: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxPage {
    pub items: Vec<InboxItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxUpdateRequest {
    pub inbox_id: Uuid,
    pub triage_status: TriageStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionKind {
    Manual,
    Rule,
    Ai,
}

impl CollectionKind {
    pub(crate) fn as_storage(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Rule => "rule",
            Self::Ai => "ai",
        }
    }

    pub(crate) fn from_storage(value: &str) -> Self {
        match value {
            "manual" => Self::Manual,
            "ai" => Self::Ai,
            _ => Self::Rule,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleOperator {
    All,
    Any,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionRule {
    pub operator: RuleOperator,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub filename_keywords: Vec<String>,
    #[serde(default)]
    pub path_keywords: Vec<String>,
    #[serde(default)]
    pub text_keywords: Vec<String>,
    #[serde(default)]
    pub parse_statuses: Vec<ParseStatus>,
    pub modified_within_days: Option<u32>,
    pub min_size_bytes: Option<u64>,
    pub max_size_bytes: Option<u64>,
    #[serde(default)]
    pub exclude_extensions: Vec<String>,
    #[serde(default)]
    pub exclude_filename_keywords: Vec<String>,
    #[serde(default)]
    pub exclude_path_keywords: Vec<String>,
    #[serde(default)]
    pub exclude_text_keywords: Vec<String>,
}

impl CollectionRule {
    pub fn validate(&self) -> Result<(), AppError> {
        let has_condition = !self.extensions.is_empty()
            || !self.filename_keywords.is_empty()
            || !self.path_keywords.is_empty()
            || !self.text_keywords.is_empty()
            || !self.parse_statuses.is_empty()
            || self.modified_within_days.is_some()
            || self.min_size_bytes.is_some()
            || self.max_size_bytes.is_some()
            || !self.exclude_extensions.is_empty()
            || !self.exclude_filename_keywords.is_empty()
            || !self.exclude_path_keywords.is_empty()
            || !self.exclude_text_keywords.is_empty();
        if !has_condition {
            return Err(AppError::new(
                "COLLECTION_RULE_INVALID",
                "规则集合至少需要一个条件",
                false,
            ));
        }
        if self
            .modified_within_days
            .is_some_and(|days| days == 0 || days > 3650)
        {
            return Err(AppError::new(
                "COLLECTION_RULE_INVALID",
                "最近修改天数必须在1到3650之间",
                false,
            ));
        }
        if self
            .min_size_bytes
            .zip(self.max_size_bytes)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err(AppError::new(
                "COLLECTION_RULE_INVALID",
                "文件大小下限不能大于上限",
                false,
            ));
        }
        if self
            .min_size_bytes
            .into_iter()
            .chain(self.max_size_bytes)
            .any(|value| value > i64::MAX as u64)
        {
            return Err(AppError::new(
                "COLLECTION_RULE_INVALID",
                "文件大小条件超过本地索引支持范围",
                false,
            ));
        }
        Ok(())
    }

    pub fn matches_metadata(&self, file: &FileRecord, now: DateTime<Utc>) -> bool {
        let mut checks = Vec::new();
        if !self.extensions.is_empty() {
            checks.push(self.extensions.iter().any(|value| {
                value
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(&file.extension)
            }));
        }
        if !self.filename_keywords.is_empty() {
            let name = file.display_name.to_lowercase();
            checks.push(
                self.filename_keywords
                    .iter()
                    .any(|value| name.contains(&value.to_lowercase())),
            );
        }
        if !self.path_keywords.is_empty() {
            let path = file.canonical_path.to_lowercase();
            checks.push(
                self.path_keywords
                    .iter()
                    .any(|value| path.contains(&value.to_lowercase())),
            );
        }
        if !self.parse_statuses.is_empty() {
            checks.push(self.parse_statuses.contains(&file.parse_status));
        }
        if let Some(days) = self.modified_within_days {
            checks.push(file.fs_modified_at >= now - Duration::days(i64::from(days)));
        }
        if self.min_size_bytes.is_some() || self.max_size_bytes.is_some() {
            checks.push(
                self.min_size_bytes
                    .is_none_or(|minimum| file.size_bytes >= minimum)
                    && self
                        .max_size_bytes
                        .is_none_or(|maximum| file.size_bytes <= maximum),
            );
        }
        let included = match self.operator {
            RuleOperator::All => checks.into_iter().all(|value| value),
            RuleOperator::Any => checks.into_iter().any(|value| value),
        };
        included && !self.excludes_metadata(file)
    }

    pub(crate) fn excludes_metadata(&self, file: &FileRecord) -> bool {
        self.exclude_extensions.iter().any(|value| {
            value
                .trim_start_matches('.')
                .eq_ignore_ascii_case(&file.extension)
        }) || self.exclude_filename_keywords.iter().any(|value| {
            file.display_name
                .to_lowercase()
                .contains(&value.to_lowercase())
        }) || self.exclude_path_keywords.iter().any(|value| {
            file.canonical_path
                .to_lowercase()
                .contains(&value.to_lowercase())
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCollectionRequest {
    pub name: String,
    pub description: Option<String>,
    pub icon: String,
    pub color: String,
    pub kind: CollectionKind,
    pub rule: Option<CollectionRule>,
}

impl CreateCollectionRequest {
    pub fn validate(&self) -> Result<(), AppError> {
        let name_length = self.name.trim().chars().count();
        if !(1..=40).contains(&name_length) {
            return Err(AppError::new(
                "COLLECTION_REQUEST_INVALID",
                "集合名称长度必须在1到40个字符之间",
                false,
            ));
        }
        match (self.kind, &self.rule) {
            (CollectionKind::Rule, Some(rule)) => rule.validate(),
            (CollectionKind::Rule, None) => Err(AppError::new(
                "COLLECTION_RULE_INVALID",
                "规则集合必须提供规则",
                false,
            )),
            (CollectionKind::Manual | CollectionKind::Ai, Some(_)) => Err(AppError::new(
                "COLLECTION_RULE_INVALID",
                "手动集合不能包含自动规则",
                false,
            )),
            (CollectionKind::Manual | CollectionKind::Ai, None) => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionRecord {
    pub collection_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub icon: String,
    pub color: String,
    pub kind: CollectionKind,
    pub rule: Option<CollectionRule>,
    pub file_count: u64,
    pub built_in: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSuggestedMember {
    pub file: FileRecord,
    pub revision_id: Uuid,
    pub confidence: f64,
    pub rationale: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSuggestion {
    pub suggestion_id: Uuid,
    pub suggested_name: String,
    pub description: String,
    pub confidence: f64,
    pub status: String,
    pub model_version: String,
    pub algorithm_version: String,
    pub members: Vec<CollectionSuggestedMember>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectionSuggestionQuery {
    pub cursor: Option<String>,
    pub page_size: u32,
    pub status: Option<String>,
}

impl CollectionSuggestionQuery {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=100).contains(&self.page_size) {
            return Err(AppError::new(
                "COLLECTION_SUGGESTION_QUERY_INVALID",
                "建议每页数量必须在1到100之间",
                false,
            ));
        }
        self.offset().map(|_| ())
    }

    pub fn offset(&self) -> Result<u64, AppError> {
        self.cursor.as_deref().unwrap_or("0").parse().map_err(|_| {
            AppError::new(
                "COLLECTION_SUGGESTION_QUERY_INVALID",
                "建议分页游标无效",
                false,
            )
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSuggestionPage {
    pub items: Vec<CollectionSuggestion>,
    pub next_cursor: Option<String>,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSuggestionRefreshResult {
    pub profiled_files: u64,
    pub candidate_edges: u64,
    pub created_suggestions: u64,
    pub suggestion_ids: Vec<Uuid>,
    pub algorithm_version: String,
    pub model_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSuggestionUpdateRequest {
    pub suggested_name: String,
    pub description: String,
    pub member_file_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionModelReviewMember {
    pub file_id: Uuid,
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionModelReview {
    pub suggested_name: String,
    pub description: String,
    pub members: Vec<CollectionModelReviewMember>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationType {
    ExactDuplicate,
    VersionCandidate,
    SemanticRelated,
    ContainsOrSummarizes,
    /// Legacy rows written before semantic relation types were split.
    Related,
}

impl RelationType {
    pub(crate) fn as_storage(self) -> &'static str {
        match self {
            Self::ExactDuplicate => "exact_duplicate",
            Self::VersionCandidate => "version_candidate",
            Self::SemanticRelated => "semantic_related",
            Self::ContainsOrSummarizes => "contains_or_summarizes",
            Self::Related => "related",
        }
    }

    pub(crate) fn from_storage(value: &str) -> Self {
        match value {
            "version_candidate" => Self::VersionCandidate,
            "semantic_related" => Self::SemanticRelated,
            "contains_or_summarizes" => Self::ContainsOrSummarizes,
            "related" => Self::Related,
            _ => Self::ExactDuplicate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRelation {
    pub relation_id: Uuid,
    pub relation_type: RelationType,
    pub left_file: FileRecord,
    pub right_file: FileRecord,
    pub confidence: f64,
    pub reasons: Vec<String>,
    pub review_status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationQuery {
    pub cursor: Option<String>,
    pub page_size: u32,
    #[serde(default)]
    pub relation_type: Option<RelationType>,
    #[serde(default)]
    pub review_status: Option<String>,
}

impl RelationQuery {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=500).contains(&self.page_size) {
            return Err(AppError::new(
                "RELATION_QUERY_INVALID",
                "关系查询数量必须在1到500之间",
                false,
            ));
        }
        if self
            .review_status
            .as_deref()
            .is_some_and(|status| !matches!(status, "suggested" | "accepted" | "rejected"))
        {
            return Err(AppError::new(
                "RELATION_QUERY_INVALID",
                "关系复核状态筛选无效",
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
            .map_err(|_| AppError::new("RELATION_QUERY_INVALID", "关系分页游标无效", false))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationPage {
    pub items: Vec<FileRelation>,
    pub next_cursor: Option<String>,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationRefreshResult {
    pub hashed_files: u64,
    pub exact_duplicate_pairs: u64,
    pub version_candidate_pairs: u64,
    #[serde(default)]
    pub semantic_related_pairs: u64,
    #[serde(default)]
    pub contains_or_summarizes_pairs: u64,
}

pub(crate) fn normalized_version_key(name: &str) -> String {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let lowered = stem.to_lowercase();
    let suffixes = ["副本", "copy", "final", "最终版", "最新版", "修订版"];
    let mut cleaned = lowered;
    for suffix in suffixes {
        cleaned = cleaned.replace(suffix, "");
    }
    cleaned
        .trim_end_matches(|character: char| {
            character.is_ascii_digit()
                || matches!(character, '-' | '_' | ' ' | '(' | ')' | '[' | ']' | 'v')
        })
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Availability, ParseStatus};

    fn file(name: &str) -> FileRecord {
        FileRecord {
            file_id: Uuid::now_v7(),
            volume_id: "vol-test".into(),
            canonical_path: format!("C:\\资料\\{name}"),
            display_name: name.into(),
            extension: name.rsplit_once('.').map_or("", |(_, ext)| ext).into(),
            mime_type: "application/octet-stream".into(),
            size_bytes: 100,
            fs_created_at: None,
            fs_modified_at: Utc::now(),
            windows_file_id: None,
            content_sha256: None,
            availability: Availability::Present,
            current_revision_id: Some(Uuid::now_v7()),
            parse_status: ParseStatus::Parsed,
            first_seen_at: Utc::now(),
            last_seen_at: Utc::now(),
        }
    }

    #[test]
    fn rule_collection_matches_metadata_without_a_model() {
        let rule = CollectionRule {
            operator: RuleOperator::All,
            extensions: vec!["pdf".into()],
            filename_keywords: vec!["合同".into()],
            path_keywords: vec![],
            text_keywords: vec![],
            parse_statuses: vec![ParseStatus::Parsed],
            modified_within_days: Some(7),
            min_size_bytes: None,
            max_size_bytes: None,
            exclude_extensions: vec![],
            exclude_filename_keywords: vec![],
            exclude_path_keywords: vec![],
            exclude_text_keywords: vec![],
        };

        assert!(rule.matches_metadata(&file("项目合同.pdf"), Utc::now()));
        assert!(!rule.matches_metadata(&file("项目合同.docx"), Utc::now()));
    }

    #[test]
    fn rule_collection_applies_size_and_hard_exclusions() {
        let mut rule = CollectionRule {
            operator: RuleOperator::Any,
            extensions: vec!["pdf".into()],
            filename_keywords: vec!["合同".into()],
            path_keywords: vec![],
            text_keywords: vec![],
            parse_statuses: vec![],
            modified_within_days: None,
            min_size_bytes: Some(50),
            max_size_bytes: Some(200),
            exclude_extensions: vec![],
            exclude_filename_keywords: vec!["草稿".into()],
            exclude_path_keywords: vec![],
            exclude_text_keywords: vec![],
        };

        assert!(rule.matches_metadata(&file("项目合同.pdf"), Utc::now()));
        assert!(!rule.matches_metadata(&file("项目合同草稿.pdf"), Utc::now()));
        rule.min_size_bytes = Some(200);
        rule.max_size_bytes = Some(100);
        assert!(rule.validate().is_err());
    }

    #[test]
    fn version_key_removes_common_copy_suffixes() {
        assert_eq!(
            normalized_version_key("归航计划-最终版-v2.docx"),
            "归航计划"
        );
        assert_eq!(normalized_version_key("归航计划 copy 3.docx"), "归航计划");
    }
}
