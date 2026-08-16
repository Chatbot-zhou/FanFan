//! QueryPlan：本地问答的强类型查询计划。
//!
//! 重构后的问答链路不再让检索词携带目标文件信息（「我的简历 LangGraph」），
//! 而是先结构化理解「用户指什么」与「用户想对它做什么」：
//!
//! 1. Source Router 判定信息来源（LOCAL / GENERAL / AMBIGUOUS）；
//! 2. Query Parser 把 LOCAL 请求拆成 `QueryPlan`——目标对象（target）
//!    与目标对象内部查询的内容（content_query）完全分离；
//! 3. Document Resolver 把 target 解析成 file_id 白名单；
//! 4. Retrieval 只拿 content_query 在 file_id 白名单内检索。
//!
//! 类型全部为 typed enum/struct（约束：不使用散乱 JSON/String 传递语义）。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::contracts::DocumentType;

/// 信息来源（Source Router 输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceIntent {
    /// 回答依赖用户本地文件、资料、文档、项目、记录、合同、简历或之前查看过的内容
    Local,
    /// 普通聊天或通用知识，不需要本地资料
    General,
    /// 存在「这个、那个、里面、第二个、之前那个、它」等指代，仅凭当前一句无法确定，
    /// 必须结合会话上下文（Session Working Context）再判断
    Ambiguous,
}

impl SourceIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceIntent::Local => "local",
            SourceIntent::General => "general",
            SourceIntent::Ambiguous => "ambiguous",
        }
    }

    /// 宽容解析（大小写/空白噪声），失败返回 None。
    pub fn parse_lenient(input: &str) -> Option<SourceIntent> {
        let normalized = input.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "local" => Some(SourceIntent::Local),
            "general" => Some(SourceIntent::General),
            "ambiguous" => Some(SourceIntent::Ambiguous),
            _ => None,
        }
    }
}

/// 用户真正想对目标对象做的事（Query Parser 输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryIntent {
    /// 找某个文件/文档在哪里
    DocumentFind,
    /// 在某个文档/文件内部问答
    DocumentQa,
    /// 概括某份文档的内容（整文摘要，不是 top-3 chunk QA）
    DocumentSummary,
    /// 全库资料问答（不限单文件）
    LibraryQa,
    /// 跨多个文档的问答
    MultiDocumentQa,
    /// 比较两份文档
    CompareDocuments,
    /// 纯聊天（GENERAL 分支，不进检索）
    GeneralChat,
}

impl QueryIntent {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryIntent::DocumentFind => "document_find",
            QueryIntent::DocumentQa => "document_qa",
            QueryIntent::DocumentSummary => "document_summary",
            QueryIntent::LibraryQa => "library_qa",
            QueryIntent::MultiDocumentQa => "multi_document_qa",
            QueryIntent::CompareDocuments => "compare_documents",
            QueryIntent::GeneralChat => "general_chat",
        }
    }

    /// 宽容解析：剥分隔符后小写匹配变体名，兼容 "DOCUMENT_QA" / "DocumentQa" 等。
    pub fn parse_lenient(input: &str) -> Option<QueryIntent> {
        const VARIANTS: [(&str, QueryIntent); 7] = [
            ("documentfind", QueryIntent::DocumentFind),
            ("documentqa", QueryIntent::DocumentQa),
            ("documentsummary", QueryIntent::DocumentSummary),
            ("libraryqa", QueryIntent::LibraryQa),
            ("multidocumentqa", QueryIntent::MultiDocumentQa),
            ("comparedocuments", QueryIntent::CompareDocuments),
            ("generalchat", QueryIntent::GeneralChat),
        ];
        let normalized: String = input
            .chars()
            .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '_')
            .collect::<String>()
            .to_ascii_lowercase();
        VARIANTS
            .iter()
            .find(|(key, _)| *key == normalized)
            .map(|(_, variant)| *variant)
    }
}

/// 对目标对象执行的操作（Query Parser 输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryOperation {
    /// 找到对象本身
    Find,
    /// 对象内部问答
    Qa,
    /// 概括对象内容
    Summary,
    /// 提取对象内部信息（如「有哪些项目」「有没有 LangGraph」）
    Extract,
    /// 比较对象
    Compare,
}

impl QueryOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryOperation::Find => "find",
            QueryOperation::Qa => "qa",
            QueryOperation::Summary => "summary",
            QueryOperation::Extract => "extract",
            QueryOperation::Compare => "compare",
        }
    }

    pub fn parse_lenient(input: &str) -> Option<QueryOperation> {
        let normalized = input.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "find" => Some(QueryOperation::Find),
            "qa" => Some(QueryOperation::Qa),
            "summary" => Some(QueryOperation::Summary),
            "extract" => Some(QueryOperation::Extract),
            "compare" => Some(QueryOperation::Compare),
            _ => None,
        }
    }
}

/// 用户所指的目标对象。与 content_query 严格分离：
/// target 只用于定位文件，绝不拼回检索 Query。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryTarget {
    /// 用户的原话指代（如「我的简历」「那份合同」「第二个」）
    pub reference: Option<String>,
    /// 期望的文档类型（resume / contract / invoice …）
    pub document_type: Option<DocumentType>,
    /// 用户给目标文档起的名字（弱信号，正文语义优先）
    pub document_name: Option<String>,
    /// 归属者（self = 用户自己）
    pub owner: Option<String>,
    pub entity_type: Option<String>,
    pub entity_name: Option<String>,
}

impl Default for QueryTarget {
    fn default() -> Self {
        Self {
            reference: None,
            document_type: None,
            document_name: None,
            owner: None,
            entity_type: None,
            entity_name: None,
        }
    }
}

/// 检索过滤条件（第一版只建模时间与文件类型，P0 阶段多为空）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryFilters {
    pub time: Option<String>,
    pub file_type: Option<String>,
    pub path: Option<String>,
}

impl Default for QueryFilters {
    fn default() -> Self {
        Self {
            time: None,
            file_type: None,
            path: None,
        }
    }
}

/// 结构化查询计划：Source Router 与 Query Parser 的完整产物。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QueryPlan {
    pub source: SourceIntent,
    pub intent: QueryIntent,
    pub operation: QueryOperation,
    pub target: QueryTarget,
    /// 比较类请求（COMPARE_DOCUMENTS）的第二个目标对象（「比较我两个简历
    /// 版本」→ target = 简历，secondary_target = 版本指代）。为 None 时
    /// 表示单目标请求。
    #[serde(default)]
    pub secondary_target: Option<QueryTarget>,
    /// 目标对象内部真正要检索的内容（如「项目经历」「LangGraph」）。
    /// 为 None 时表示不检索（DOCUMENT_FIND / DOCUMENT_SUMMARY）。
    pub content_query: Option<String>,
    pub filters: QueryFilters,
    /// 为 true 时必须先执行 Document Resolver 锁定 file_id 才能检索
    pub requires_document_resolution: bool,
    /// 为 true 时必须读取整份文档结构（DOCUMENT_SUMMARY），不能只拿 top-3 chunk
    pub requires_full_document: bool,
    pub confidence: f32,
}

impl Default for QueryPlan {
    fn default() -> Self {
        Self {
            source: SourceIntent::Ambiguous,
            intent: QueryIntent::DocumentQa,
            operation: QueryOperation::Qa,
            target: QueryTarget::default(),
            secondary_target: None,
            content_query: None,
            filters: QueryFilters::default(),
            requires_document_resolution: false,
            requires_full_document: false,
            confidence: 0.0,
        }
    }
}

/// 证据状态：检索结果与信息来源解耦后，LOCAL 请求无证据也必须保持 LOCAL。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    EvidenceFound,
    NoEvidence,
    NeedClarification,
}

/// Document Resolver 的解析结果状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionStatus {
    /// 高置信度锁定（候选唯一或显著胜出）→ 收缩 scope
    Resolved,
    /// 存在多个非常接近的候选 → 需要澄清，或保留 top-2/3 进 scope
    MultipleCandidates,
    /// 低置信度 / 无候选 → 不要错误锁定文件，退回较宽 scope
    Unresolved,
}

impl ResolutionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ResolutionStatus::Resolved => "resolved",
            ResolutionStatus::MultipleCandidates => "multiple_candidates",
            ResolutionStatus::Unresolved => "unresolved",
        }
    }
}

/// Document Resolver 的候选条目：文件 + 综合打分 + 命中的信号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentCandidate {
    pub file_id: Uuid,
    /// 综合得分（0..=1，越高越可能是用户指的文件）
    pub score: f32,
    /// 命中信号（如 "session_active" / "document_type" / "title_match" / "embedding"…）
    pub signals: Vec<String>,
}

impl DocumentCandidate {
    pub fn new(file_id: Uuid, score: f32, signals: Vec<String>) -> Self {
        Self {
            file_id,
            score,
            signals,
        }
    }
}

/// Document Resolver 的输出：候选列表 + 选定的文件白名单 + 状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentResolution {
    pub candidates: Vec<DocumentCandidate>,
    /// 最终进入 RetrievalScope 的 file_id 白名单
    pub resolved_file_ids: Vec<Uuid>,
    pub confidence: f32,
    pub status: ResolutionStatus,
    /// 未锁定文件时退回宽 scope 的说明（fallback reason，进 trace）
    pub fallback_reason: Option<String>,
}

impl DocumentResolution {
    pub fn unresolved(fallback_reason: impl Into<String>) -> Self {
        Self {
            candidates: Vec::new(),
            resolved_file_ids: Vec::new(),
            confidence: 0.0,
            status: ResolutionStatus::Unresolved,
            fallback_reason: Some(fallback_reason.into()),
        }
    }
}
