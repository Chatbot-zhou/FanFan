use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

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
    /// 每条实际创建的建议 → 其种子文件（种子扩展聚类中组的核心文件）。
    pub seed_file_id_by_suggestion: HashMap<Uuid, Uuid>,
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
                RelationType::ContainsOrSummarizes | RelationType::Related => has_summary = true,
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
                    let (summary_id, source_id) = (edge.left_file_id, edge.right_file_id);
                    member_roles
                        .entry(summary_id)
                        .or_insert(RelationGroupRole::Summary);
                    member_roles
                        .entry(source_id)
                        .or_insert(RelationGroupRole::Source);
                }
            }
        }
        let confidence = component_edges
            .iter()
            .map(|edge| edge.confidence)
            .sum::<f64>()
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

/// 种子扩展聚类的输入档案：文档级向量 + 所属候选桶。
#[derive(Debug, Clone)]
pub(crate) struct SeedProfile {
    pub file_id: Uuid,
    pub revision_id: Uuid,
    pub title: String,
    pub vector: Vec<f32>,
    pub bucket: String,
}

/// 种子组内成员：与种子的 cosine 相似度（已过阈值）。
#[derive(Debug, Clone)]
pub(crate) struct SemanticSeedMember {
    pub file_id: Uuid,
    pub revision_id: Uuid,
    pub title: String,
    pub similarity: f32,
}

/// 一个种子扩展组：种子 + 与其最相近的未消费成员。
#[derive(Debug, Clone)]
pub(crate) struct SemanticSeedGroup {
    pub seed_file_id: Uuid,
    /// 组置信度 = 成员边相似度均值
    pub confidence: f64,
    pub members: Vec<SemanticSeedMember>,
}

/// 归一化向量点积：向量已 L2 归一化时即 cosine 相似度。
fn normalized_dot(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() {
        return 0.0;
    }
    left.iter().zip(right).map(|(l, r)| l * r).sum()
}

/// 种子扩展贪心聚类：每个种子取相似度最高的未消费邻居成组，组间互斥。
///
/// - 按 candidate bucket 分桶（不同桶永不成组），桶内取前 bucket_cap 个参与
///   （按调用方传入顺序，与存储侧「按 updated_at 排序 LIMIT」的窗口一致）；
/// - 已消费文件既不作种子也不作成员；
/// - 种子按簇核心度排序：top-1 邻居相似度降序 → 邻居平均相似度降序 → file_id 升序，
///   先成组的往往是最核心的簇，弱归属让给后面的组；
/// - 成员取「相似度降序、≥ threshold、未消费、≤ top_k」；
/// - 组置信度 = 成员边相似度均值（与连通分量聚类的口径一致）。
///
/// 纯函数、确定性：相同输入必得相同分组，便于幂等键与测试。
pub(crate) fn seed_expand_semantic_groups(
    profiles: &[SeedProfile],
    consumed: &HashSet<Uuid>,
    threshold: f32,
    top_k: usize,
    bucket_cap: usize,
) -> Vec<SemanticSeedGroup> {
    // 按桶分组，桶内保留传入顺序；不同桶向量不可比，永不成组
    let mut buckets = BTreeMap::<String, Vec<&SeedProfile>>::new();
    for profile in profiles {
        if consumed.contains(&profile.file_id) {
            continue;
        }
        buckets
            .entry(profile.bucket.clone())
            .or_default()
            .push(profile);
    }
    let mut groups = Vec::new();
    let mut consumed = consumed.clone();
    for profiles in buckets.values_mut() {
        profiles.truncate(bucket_cap);
        if profiles.len() < 2 {
            continue;
        }
        // 桶内两两相似度预计算，统计每文件的 top-1 与邻居均值（不设阈值，
        // 只描述「与同类接近程度」；邻居是否入选由贪心时再判定）
        let mut seed_order = Vec::with_capacity(profiles.len());
        for (index, profile) in profiles.iter().enumerate() {
            let mut top1 = f32::MIN;
            let mut sum = 0.0_f32;
            let mut count = 0_usize;
            for (other_index, other) in profiles.iter().enumerate() {
                if index == other_index {
                    continue;
                }
                let similarity = normalized_dot(&profile.vector, &other.vector);
                top1 = top1.max(similarity);
                sum += similarity;
                count += 1;
            }
            let avg = if count == 0 { 0.0 } else { sum / count as f32 };
            seed_order.push((top1, avg, profile.file_id, index));
        }
        // 簇核心度排序：top-1 相似度降序 → 邻居均值降序 → file_id 升序（确定性）
        seed_order.sort_by(|left, right| {
            right
                .0
                .partial_cmp(&left.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    right
                        .1
                        .partial_cmp(&left.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| left.2.cmp(&right.2))
        });
        for &(_, _, _, seed_index) in &seed_order {
            let seed = profiles[seed_index];
            if consumed.contains(&seed.file_id) {
                continue;
            }
            // 邻居：相似度降序、≥ threshold、未消费、≤ top_k
            let mut neighbors = Vec::new();
            for (other_index, other) in profiles.iter().enumerate() {
                if other_index == seed_index || consumed.contains(&other.file_id) {
                    continue;
                }
                let similarity = normalized_dot(&seed.vector, &other.vector);
                if similarity >= threshold {
                    neighbors.push((other_index, similarity));
                }
            }
            neighbors.sort_by(|left, right| {
                right
                    .1
                    .partial_cmp(&left.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| profiles[left.0].file_id.cmp(&profiles[right.0].file_id))
            });
            neighbors.truncate(top_k);
            if neighbors.is_empty() {
                continue;
            }
            let mut members = Vec::with_capacity(neighbors.len());
            let mut sum = 0.0_f64;
            for (other_index, similarity) in neighbors {
                let other = profiles[other_index];
                members.push(SemanticSeedMember {
                    file_id: other.file_id,
                    revision_id: other.revision_id,
                    title: other.title.clone(),
                    similarity,
                });
                consumed.insert(other.file_id);
                sum += f64::from(similarity);
            }
            consumed.insert(seed.file_id);
            groups.push(SemanticSeedGroup {
                seed_file_id: seed.file_id,
                confidence: sum / members.len() as f64,
                members,
            });
        }
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
        assert!(
            group
                .members
                .iter()
                .any(|m| m.file_id == a && m.role == RelationGroupRole::Member)
        );
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
        assert_eq!(
            types,
            vec![RelationGroupType::Duplicate, RelationGroupType::TopicGroup]
        );
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

    // ---- seed_expand_semantic_groups ----

    fn seed_profile(id: u64, x: f32, y: f32, bucket: &str) -> SeedProfile {
        SeedProfile {
            file_id: Uuid::from_u128(u128::from(id)),
            revision_id: Uuid::from_u128((u128::from(id) << 64) | 1),
            title: format!("文件{id}"),
            vector: vec![x, y],
            bucket: bucket.into(),
        }
    }

    #[test]
    fn seed_ranking_orders_by_top1_then_avg_then_id() {
        // 排序规则：top-1 相似度降序 → 邻居均值降序 → file_id 升序
        // - a 与 s1 的 top-1 相同（0.99），但 a 是桶中心（邻居均值 0.5675 > 0.5225）→ a 先组
        // - s2 的 top-1（0.954）高于 b 的（0.80），尽管 b 的邻居均值（0.1438）高于 s2（0.1238）
        //   → s2 组先于 b（b 的候选邻居全被先组消费，最终无组）
        // - x 与 s2 的 top-1 相同（0.954），x 的邻居均值更高 → x 先组
        let profiles = vec![
            seed_profile(1, 1.0, 0.0, "b"),    // s1
            seed_profile(2, 0.99, 0.141, "b"), // a（0.99 与 s1，桶中心）
            seed_profile(3, 0.8, -0.6, "b"),   // b（0.80 与 s1，与 a 仅 0.7074）
            seed_profile(4, 0.0, 1.0, "b"),    // s2
            seed_profile(5, 0.3, 0.954, "b"),  // x（0.954 与 s2，与 s1 仅 0.3）
        ];
        let groups = seed_expand_semantic_groups(&profiles, &HashSet::new(), 0.78, 12, 96);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].seed_file_id, Uuid::from_u128(2)); // a 先组
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[0].members[0].file_id, Uuid::from_u128(1));
        assert!((groups[0].members[0].similarity - 0.99).abs() < 1e-5);
        assert_eq!(groups[1].seed_file_id, Uuid::from_u128(5)); // x 组
        assert_eq!(groups[1].members.len(), 1);
        assert_eq!(groups[1].members[0].file_id, Uuid::from_u128(4));
    }

    #[test]
    fn seed_expansion_truncates_members_to_top_k() {
        // 种子 + 20 个相似度 0.98..0.79 的邻居排成连续弧：任意两两都 ≥0.78。
        // 最核心的组（由弧中间的邻居成种子）也最多收 12 个成员 → 任何组成员数 ≤ top_k，
        // 且 21 个文件全部被组覆盖
        let mut profiles = vec![seed_profile(1, 1.0, 0.0, "b")];
        for index in 0..20 {
            let cosine = 0.98 - index as f32 * 0.01;
            let y = (1.0 - cosine * cosine).sqrt();
            profiles.push(seed_profile(2 + index as u64, cosine, y, "b"));
        }
        let groups = seed_expand_semantic_groups(&profiles, &HashSet::new(), 0.78, 12, 96);
        assert!(groups.iter().all(|group| group.members.len() <= 12));
        let covered = groups
            .iter()
            .flat_map(|group| {
                std::iter::once(group.seed_file_id)
                    .chain(group.members.iter().map(|member| member.file_id))
            })
            .collect::<HashSet<_>>();
        assert_eq!(covered.len(), 21);
    }

    #[test]
    fn seed_expansion_respects_threshold() {
        // 0.77 的邻居低于阈值 0.78，被排除；0.80 的入选
        // （0.77 的邻居放种子另一侧，与 0.80 的成员互不相近）
        let y = (1.0 - 0.77_f32 * 0.77).sqrt();
        let profiles = vec![
            seed_profile(1, 1.0, 0.0, "b"),
            seed_profile(2, 0.8, 0.6, "b"),
            seed_profile(3, 0.77, -y, "b"),
        ];
        let groups = seed_expand_semantic_groups(&profiles, &HashSet::new(), 0.78, 12, 96);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[0].members[0].file_id, Uuid::from_u128(2));
    }

    #[test]
    fn seed_expansion_skips_consumed_files() {
        // consumed 中的文件既不作种子也不作成员
        let profiles = vec![
            seed_profile(1, 1.0, 0.0, "b"),
            seed_profile(2, 0.8, 0.6, "b"),
            seed_profile(3, 0.0, 1.0, "b"),
            seed_profile(4, 0.435, 0.90, "b"),
        ];
        let consumed = HashSet::from([Uuid::from_u128(3), Uuid::from_u128(4)]);
        let groups = seed_expand_semantic_groups(&profiles, &consumed, 0.78, 12, 96);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].seed_file_id, Uuid::from_u128(1));
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[0].members[0].file_id, Uuid::from_u128(2));
    }

    #[test]
    fn seed_expansion_groups_are_mutually_exclusive() {
        // s2（90°）的 top-1（x: 0.940）高于 s1（0°）的 top-1（y: 0.906），
        // s2 先成组独占 x，之后 s1 组只能拿 y —— 每个文件至多出现在一组；
        // x 与 y 相距 45°（0.707 < 0.78），不会互相争抢
        let profiles = vec![
            seed_profile(1, 1.0, 0.0, "b"),     // s1
            seed_profile(2, 0.0, 1.0, "b"),     // s2
            seed_profile(3, 0.105, 0.995, "b"), // x（0.995 与 s2，0.105 与 s1，与 y 0.515）
            seed_profile(4, 0.906, 0.423, "b"), // y（0.906 与 s1，0.423 与 s2，与 x 0.515）
        ];
        let groups = seed_expand_semantic_groups(&profiles, &HashSet::new(), 0.78, 12, 96);
        assert_eq!(groups.len(), 2);
        let mut seen = HashSet::new();
        for group in &groups {
            assert!(seen.insert(group.seed_file_id));
            for member in &group.members {
                assert!(seen.insert(member.file_id));
            }
        }
        assert_eq!(seen.len(), 4); // 4 个文件全部被组覆盖且无重复（insert 重复会 panic）
    }

    #[test]
    fn seed_expansion_isolated_profiles_form_no_group() {
        // 两两夹角 120°，cosine = -0.5，全部低于阈值
        let profiles = vec![
            seed_profile(1, 1.0, 0.0, "b"),
            seed_profile(2, -0.5, 0.866, "b"),
            seed_profile(3, -0.5, -0.866, "b"),
        ];
        let groups = seed_expand_semantic_groups(&profiles, &HashSet::new(), 0.78, 12, 96);
        assert!(groups.is_empty());
    }

    #[test]
    fn seed_expansion_confidence_is_mean_of_member_similarities() {
        // 种子两侧各一个 0.866 的成员（夹角 60°，互不相近）：
        // 组置信度 = (0.866 + 0.866) / 2；b2 桶同向量文件跨桶不成组
        let profiles = vec![
            seed_profile(1, 1.0, 0.0, "b1"),
            seed_profile(2, 0.866, 0.5, "b1"),
            seed_profile(3, 0.866, -0.5, "b1"),
            // 与 b1 桶的种子同向量但不同桶：跨桶永不成组
            seed_profile(4, 1.0, 0.0, "b2"),
        ];
        let groups = seed_expand_semantic_groups(&profiles, &HashSet::new(), 0.78, 12, 96);
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.seed_file_id, Uuid::from_u128(1));
        assert_eq!(group.members.len(), 2);
        assert!((group.confidence - 0.866).abs() < 1e-5);
    }
}
