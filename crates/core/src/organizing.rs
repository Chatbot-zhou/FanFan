use std::collections::{HashMap, HashSet, VecDeque};

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
    /// 本次聚类发现的同主题/同用途分组数（已排除被已确认/已拒绝建议消费过的文件）。
    pub topic_groups: u64,
    /// 聚类发现但本批未展示的分组数（受每批建议数上限约束）。
    pub remaining_topic_groups: u64,
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

/// 生成模型对集合建议的命名润色：只改名称和说明，成员分组完全由 Embedding 聚类决定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionModelReview {
    pub suggested_name: String,
    pub description: String,
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
    #[serde(default)]
    pub groups_created: u64,
}

/// 文件关系组类型：把成对的边按连通分量聚成「族」后的组标签。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationGroupType {
    /// 字节级完全重复（组内全是 SHA-256 相同的副本）
    Duplicate,
    /// 版本族（同名/近名 + 内容相似，可能是副本改名、修订版、派生版）
    VersionFamily,
    /// 摘要/源稿组（一份是另一份的摘要、提纲或概括）
    SummaryGroup,
    /// 同主题或同用途（文档级语义相似）
    TopicGroup,
    /// 混合关系（组内边类型不止一种）
    Mixed,
}

impl RelationGroupType {
    pub(crate) fn as_storage(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::VersionFamily => "version_family",
            Self::SummaryGroup => "summary_group",
            Self::TopicGroup => "topic_group",
            Self::Mixed => "mixed",
        }
    }

    pub(crate) fn from_storage(value: &str) -> Self {
        match value {
            "version_family" => Self::VersionFamily,
            "summary_group" => Self::SummaryGroup,
            "topic_group" => Self::TopicGroup,
            "mixed" => Self::Mixed,
            _ => Self::Duplicate,
        }
    }
}

/// 组内成员角色：版本族里谁是主版本、谁是副本、谁是摘要/源稿。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationGroupRole {
    /// 版本族中修改时间最新的文件（候选主版本）
    Latest,
    /// 与组内最新版本内容相似度 ≥0.99 的近重复副本
    Copy,
    /// 摘要/提纲类文件
    Summary,
    /// 被摘要、被提纲化的源稿
    Source,
    /// 普通成员
    Member,
}

impl RelationGroupRole {
    pub(crate) fn as_storage(self) -> &'static str {
        match self {
            Self::Latest => "latest",
            Self::Copy => "copy",
            Self::Summary => "summary",
            Self::Source => "source",
            Self::Member => "member",
        }
    }

    pub(crate) fn from_storage(value: &str) -> Self {
        match value {
            "latest" => Self::Latest,
            "copy" => Self::Copy,
            "summary" => Self::Summary,
            "source" => Self::Source,
            _ => Self::Member,
        }
    }
}

/// 聚类算法的中间结果：一组文件 + 组类型 + 成员角色（不带文件详情）。
#[derive(Debug, Clone)]
pub struct RelationGroup {
    pub group_type: RelationGroupType,
    pub title: String,
    pub confidence: f64,
    pub members: Vec<RelationGroupMember>,
}

#[derive(Debug, Clone)]
pub struct RelationGroupMember {
    pub file_id: Uuid,
    pub role: RelationGroupRole,
}

/// 落库/查询用：组 + 组内边的证据信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationGroupRecord {
    pub group_id: Uuid,
    pub group_type: RelationGroupType,
    pub title: String,
    pub confidence: f64,
    pub member_count: u32,
    pub review_status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub members: Vec<RelationGroupMemberRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationGroupMemberRecord {
    pub file_id: Uuid,
    pub role: RelationGroupRole,
    pub file: FileRecord,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationGroupQuery {
    pub cursor: Option<String>,
    pub page_size: u32,
    #[serde(default)]
    pub group_type: Option<RelationGroupType>,
    #[serde(default)]
    pub review_status: Option<String>,
}

impl RelationGroupQuery {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=500).contains(&self.page_size) {
            return Err(AppError::new(
                "RELATION_GROUP_QUERY_INVALID",
                "关系组查询数量必须在1到500之间",
                false,
            ));
        }
        if self
            .review_status
            .as_deref()
            .is_some_and(|status| !matches!(status, "suggested" | "accepted" | "rejected"))
        {
            return Err(AppError::new(
                "RELATION_GROUP_QUERY_INVALID",
                "关系组复核状态筛选无效",
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
            .map_err(|_| AppError::new("RELATION_GROUP_QUERY_INVALID", "关系组分页游标无效", false))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationGroupPage {
    pub items: Vec<RelationGroupRecord>,
    pub next_cursor: Option<String>,
    pub total: u64,
}

/// 聚类输入：一条「边」的轻量表示（算法层不直接依赖数据库行）。
#[derive(Debug, Clone)]
pub(crate) struct RelationEdge {
    pub left_file_id: Uuid,
    pub right_file_id: Uuid,
    pub relation_type: RelationType,
    pub confidence: f64,
    /// 方向（contains_or_summarizes 才有意义：左 → 右 表示左是右的摘要）
    pub directed: bool,
}

/// 把边按连通分量聚成组。
///
/// - exact_duplicate / version_candidate 是强证据（字节或名称），传递性成立，
///   直接连通即可；链式蔓延风险由调用方对语义边做向量一致性校验兜底。
/// - 组类型按优先级取：重复 > 版本族 > 摘要组 > 同主题组 > 混合。
/// - 组置信度 = 组内边置信度的均值。
pub(crate) fn cluster_relation_edges(
    edges: &[RelationEdge],
    title_for: &dyn Fn(&[Uuid], RelationGroupType) -> String,
) -> Vec<RelationGroup> {
    if edges.is_empty() {
        return Vec::new();
    }
    let mut adjacency = HashMap::<Uuid, Vec<(Uuid, &RelationEdge)>>::new();
    for edge in edges {
        adjacency
            .entry(edge.left_file_id)
            .or_default()
            .push((edge.right_file_id, edge));
        adjacency
            .entry(edge.right_file_id)
            .or_default()
            .push((edge.left_file_id, edge));
    }
    let mut visited = HashSet::<Uuid>::new();
    let mut groups = Vec::new();
    let mut member_ids = Vec::new();
    let mut member_roles = HashMap::<Uuid, RelationGroupRole>::new();
    for &start in adjacency.keys() {
        if visited.contains(&start) {
            continue;
        }
        // BFS 收集连通分量
        member_ids.clear();
        member_roles.clear();
        let mut queue = VecDeque::new();
        queue.push_back(start);
        visited.insert(start);
        let mut component_edges = Vec::new();
        while let Some(file_id) = queue.pop_front() {
            member_ids.push(file_id);
            for (neighbor, edge) in &adjacency[&file_id] {
                if edge.left_file_id == file_id || edge.right_file_id == file_id {
                    component_edges.push(*edge);
                }
                if !visited.contains(neighbor) {
                    visited.insert(*neighbor);
                    queue.push_back(*neighbor);
                }
            }
        }
        if member_ids.len() < 2 {
            continue;
        }
        // 组类型：按优先级取
        let mut has_duplicate = false;
        let mut has_version = false;
        let mut has_summary = false;
        let mut has_semantic = false;
        for edge in &component_edges {
            match edge.relation_type {
                RelationType::ExactDuplicate => has_duplicate = true,
                RelationType::VersionCandidate => has_version = true,
                RelationType::ContainsOrSummarizes | RelationType::Related => {
                    has_summary = true
                }
                RelationType::SemanticRelated => has_semantic = true,
            }
        }
        let group_type = if has_duplicate && !has_version && !has_summary && !has_semantic {
            RelationGroupType::Duplicate
        } else if has_version {
            RelationGroupType::VersionFamily
        } else if has_summary && !has_semantic {
            RelationGroupType::SummaryGroup
        } else if has_semantic {
            if has_summary || has_duplicate {
                RelationGroupType::Mixed
            } else {
                RelationGroupType::TopicGroup
            }
        } else if has_duplicate {
            RelationGroupType::Duplicate
        } else {
            RelationGroupType::Mixed
        };
        // 摘要方向：contains 边指向的成员标 summary/source
        if has_summary {
            for edge in &component_edges {
                if matches!(
                    edge.relation_type,
                    RelationType::ContainsOrSummarizes | RelationType::Related
                ) {
                    let (summary_id, source_id) = if edge.directed {
                        (edge.left_file_id, edge.right_file_id)
                    } else {
                        (edge.left_file_id, edge.right_file_id)
                    };
                    member_roles
                        .entry(summary_id)
                        .or_insert(RelationGroupRole::Summary);
                    member_roles
                        .entry(source_id)
                        .or_insert(RelationGroupRole::Source);
                }
            }
        }
        let confidence = component_edges.iter().map(|edge| edge.confidence).sum::<f64>()
            / component_edges.len() as f64;
        let title = title_for(&member_ids, group_type);
        let members = member_ids
            .iter()
            .map(|file_id| RelationGroupMember {
                file_id: *file_id,
                role: member_roles
                    .get(file_id)
                    .cloned()
                    .unwrap_or(RelationGroupRole::Member),
            })
            .collect();
        groups.push(RelationGroup {
            group_type,
            title,
            confidence,
            members,
        });
    }
    groups
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

    fn edge(left: u64, right: u64, relation_type: RelationType, confidence: f64) -> RelationEdge {
        RelationEdge {
            left_file_id: Uuid::from_u128(u128::from(left)),
            right_file_id: Uuid::from_u128(u128::from(right)),
            relation_type,
            confidence,
            directed: false,
        }
    }

    fn ids(left: u64, right: u64) -> (Uuid, Uuid) {
        (
            Uuid::from_u128(u128::from(left)),
            Uuid::from_u128(u128::from(right)),
        )
    }

    fn title_hook(_members: &[Uuid], _group_type: RelationGroupType) -> String {
        "组".into()
    }

    #[test]
    fn duplicate_edges_form_single_group() {
        let (a, b) = ids(1, 2);
        let (c, _) = ids(3, 1);
        let groups = cluster_relation_edges(
            &[
                edge(1, 2, RelationType::ExactDuplicate, 1.0),
                edge(3, 1, RelationType::ExactDuplicate, 1.0),
            ],
            &title_hook,
        );
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.group_type, RelationGroupType::Duplicate);
        assert_eq!(group.members.len(), 3);
        assert!(group.members.iter().any(|m| m.file_id == a && m.role == RelationGroupRole::Member));
        assert!(group.members.iter().any(|m| m.file_id == b));
        assert!(group.members.iter().any(|m| m.file_id == c));
        assert_eq!(group.confidence, 1.0);
    }

    #[test]
    fn version_family_takes_priority_over_semantic() {
        let groups = cluster_relation_edges(
            &[
                edge(1, 2, RelationType::VersionCandidate, 0.78),
                edge(2, 3, RelationType::SemanticRelated, 0.82),
            ],
            &title_hook,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_type, RelationGroupType::VersionFamily);
        assert_eq!(groups[0].members.len(), 3);
    }

    #[test]
    fn disconnected_components_split_into_separate_groups() {
        let groups = cluster_relation_edges(
            &[
                edge(1, 2, RelationType::ExactDuplicate, 1.0),
                edge(3, 4, RelationType::SemanticRelated, 0.90),
            ],
            &title_hook,
        );
        assert_eq!(groups.len(), 2);
        let mut types = groups.iter().map(|g| g.group_type).collect::<Vec<_>>();
        types.sort_by_key(|t| t.as_storage());
        assert_eq!(types, vec![RelationGroupType::Duplicate, RelationGroupType::TopicGroup]);
    }

    #[test]
    fn pure_semantic_chain_forms_topic_group() {
        let groups = cluster_relation_edges(
            &[
                edge(1, 2, RelationType::SemanticRelated, 0.80),
                edge(2, 3, RelationType::SemanticRelated, 0.81),
            ],
            &title_hook,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_type, RelationGroupType::TopicGroup);
        assert_eq!(groups[0].members.len(), 3);
    }

    #[test]
    fn contains_edge_marks_summary_and_source_roles() {
        let groups = cluster_relation_edges(
            &[RelationEdge {
                left_file_id: Uuid::from_u128(1),
                right_file_id: Uuid::from_u128(2),
                relation_type: RelationType::ContainsOrSummarizes,
                confidence: 0.90,
                directed: true,
            }],
            &title_hook,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_type, RelationGroupType::SummaryGroup);
        let roles = groups[0]
            .members
            .iter()
            .map(|m| (m.file_id, m.role))
            .collect::<HashMap<_, _>>();
        assert_eq!(roles[&Uuid::from_u128(1)], RelationGroupRole::Summary);
        assert_eq!(roles[&Uuid::from_u128(2)], RelationGroupRole::Source);
    }

    #[test]
    fn single_vertex_component_is_skipped() {
        let groups = cluster_relation_edges(
            &[edge(1, 2, RelationType::SemanticRelated, 0.80)],
            &title_hook,
        );
        assert_eq!(groups.len(), 1);
        let groups = cluster_relation_edges(&[], &title_hook);
        assert!(groups.is_empty());
    }

    #[test]
    fn mixed_exact_and_semantic_is_mixed_type() {
        let groups = cluster_relation_edges(
            &[
                edge(1, 2, RelationType::ExactDuplicate, 1.0),
                edge(2, 3, RelationType::SemanticRelated, 0.85),
            ],
            &title_hook,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_type, RelationGroupType::Mixed);
    }
}
