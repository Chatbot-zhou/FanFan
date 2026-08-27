//! 高层只读 Knowledge Tool 契约（Phase 0+1）。
//!
//! 封装现有能力为少量高层只读 Tool，供受约束 Planner 选择；底层
//! Embedding/FTS/RRF/MMR/Chunk 一律不暴露给决策层，只由各 Tool 内部调用。
//!
//! 本模块只做契约、注册表、`QueryPlan → Tool` 映射、输入输出强类型与输出
//! 校验，均无 IO、无模型调用，纯函数（与 `query_planner` 同构）。真正的
//! 执行器在桌面应用层（时序编排与模型/存储调用不在核心职责内）。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ask::query_plan::{QueryIntent, QueryOperation, QueryPlan};
use crate::contracts::{DocumentType, EvidenceRef};

/// 高层只读 Tool 枚举。
///
/// 这些是高层的语义化操作，映射到现有 intent/operation 分发的实现，但把
/// 「选哪个工具 / 调什么底层」从立项逻辑中提出来，供 Phase 2 的 Planner
/// 显式选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeTool {
    /// 知识库概览：「我的知识库有什么？」
    LibraryOverview,
    /// 找文件：「我的简历在哪 / 哪个文件是…」
    SearchFiles,
    /// 读文档画像：标题/摘要/关键词/实体/类型/章节
    GetDocumentProfile,
    /// 文档大纲：只取结构（章节标题），不做逐节模型摘要
    GetOutline,
    /// 直接读文档（短文件）：原文片段直读
    ReadDocument,
    /// 在单个/已锁定文档内问答（scoped RAG）
    SearchInDocument,
    /// 全库/跨文档问答（library / multi-document RAG）
    RagSearch,
    /// 从文档抽取清单型信息（项目、技能等）
    ExtractFromDocument,
    /// 比较两份文档
    CompareDocuments,
}

impl KnowledgeTool {
    pub fn as_str(self) -> &'static str {
        match self {
            KnowledgeTool::LibraryOverview => "library_overview",
            KnowledgeTool::SearchFiles => "search_files",
            KnowledgeTool::GetDocumentProfile => "get_document_profile",
            KnowledgeTool::GetOutline => "get_outline",
            KnowledgeTool::ReadDocument => "read_document",
            KnowledgeTool::SearchInDocument => "search_in_document",
            KnowledgeTool::RagSearch => "rag_search",
            KnowledgeTool::ExtractFromDocument => "extract_from_document",
            KnowledgeTool::CompareDocuments => "compare_documents",
        }
    }

    /// 供 Planner 使用的简短自然语言描述（为什么选它 / 什么时候用）。
    pub fn describe(self) -> &'static str {
        match self {
            KnowledgeTool::LibraryOverview => {
                "查看知识库里有哪些资料——用户问「知识库有什么/目前有哪些资料」时优先，不检索正文"
            }
            KnowledgeTool::SearchFiles => {
                "按文件名/类型/标题找文件——用户想定位某个文件在哪里，或提供了一个文件名/短语"
            }
            KnowledgeTool::GetDocumentProfile => {
                "读取一份文档的画像（标题、摘要、关键词、实体、类型、章节）——先了解文档讲了什么的低成本途径"
            }
            KnowledgeTool::GetOutline => {
                "读取文档的结构大纲（章节标题串）——需要看文档组织/有哪些章节时用"
            }
            KnowledgeTool::ReadDocument => {
                "直接读取一份短文档的原文——文档很短、直接给原文即可，或回答依赖整段原文细节"
            }
            KnowledgeTool::SearchInDocument => {
                "在已锁定的单份文档内检索并问答——content_query 命中该文档内部细节时用（scoped RAG）"
            }
            KnowledgeTool::RagSearch => {
                "在全库/跨多文档检索并问答——不限定单文件、或需要跨文档综合时用"
            }
            KnowledgeTool::ExtractFromDocument => {
                "从文档抽取清单型实体（有哪些项目/技能/条款）——「有哪些/列出/提取」时用"
            }
            KnowledgeTool::CompareDocuments => {
                "比较两份文档的内容差异与共同点——「比较…和…」时用"
            }
        }
    }

    /// 宽容解析（剥分隔符/空白后小写匹配），失败返回 None。
    pub fn parse_lenient(input: &str) -> Option<KnowledgeTool> {
        let normalized: String = input
            .chars()
            .filter(|ch| !ch.is_whitespace() && *ch != '-' && *ch != '_')
            .collect::<String>()
            .to_ascii_lowercase();
        for tool in ALL_TOOLS {
            let key: String = tool
                .as_str()
                .chars()
                .filter(|ch| *ch != '_')
                .collect::<String>()
                .to_ascii_lowercase();
            if key == normalized {
                return Some(*tool);
            }
        }
        None
    }
}

/// 注册表：全部高层 Tool（供 Planner 的候选清单 / 描述）。
pub const ALL_TOOLS: &[KnowledgeTool] = &[
    KnowledgeTool::LibraryOverview,
    KnowledgeTool::SearchFiles,
    KnowledgeTool::GetDocumentProfile,
    KnowledgeTool::GetOutline,
    KnowledgeTool::ReadDocument,
    KnowledgeTool::SearchInDocument,
    KnowledgeTool::RagSearch,
    KnowledgeTool::ExtractFromDocument,
    KnowledgeTool::CompareDocuments,
];

/// 单个 Tool 的注册条目（name + 描述，供 Planner prompt 与 trace 使用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
}

/// 当前注册表条目（有序，供 Planner 逐条挑选）。
pub fn registry() -> Vec<ToolSpec> {
    ALL_TOOLS
        .iter()
        .map(|tool| ToolSpec {
            name: tool.as_str().to_owned(),
            description: tool.describe().to_owned(),
        })
        .collect()
}

/// 把现有 `QueryPlan`（Source/Query Parser 产物）映射到单个高层 Tool。
///
/// 这是「Legacy 意图分发 → 高层 Tool 选择」的纯数据映射，返回 `None` 表示
/// 不属于本地资料工具（GENERAL 聊天），应交给非资料分支。映射原则与现有
/// 分发一致：**不针对具体文件/关键词/测试问题写特判**，只按意图/操作/内容
/// 有无做通用路由。
pub fn tool_for_plan(plan: &QueryPlan) -> Option<KnowledgeTool> {
    match plan.intent {
        QueryIntent::GeneralChat => None,
        QueryIntent::DocumentFind => Some(KnowledgeTool::SearchFiles),
        QueryIntent::DocumentSummary => {
            // 仅「结构枚举大纲」（要章节标题串）走 get_outline；整文内容摘要
            // （写了什么/总结一下，structure_enumeration=false）回落 legacy 摘要
            // 管线，避免大纲工具接管后丢失逐节内容概览。
            if plan.structure_enumeration {
                Some(KnowledgeTool::GetOutline)
            } else {
                None
            }
        }
        QueryIntent::CompareDocuments => Some(KnowledgeTool::CompareDocuments),
        QueryIntent::LibraryQa => Some(if plan.content_query.is_some() {
            // 全库带内容问 → 库级 RAG；纯概览（无内容）→ 库概览
            KnowledgeTool::RagSearch
        } else {
            KnowledgeTool::LibraryOverview
        }),
        QueryIntent::MultiDocumentQa => Some(KnowledgeTool::RagSearch),
        QueryIntent::DocumentQa => Some(match plan.operation {
            // 「有哪些项目/列出/提取」清单型抽取
            QueryOperation::Extract => KnowledgeTool::ExtractFromDocument,
            _ => KnowledgeTool::SearchInDocument,
        }),
    }
}

/// Tool 输入（强类型契约，禁止散乱 JSON/String 传递语义）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ToolInput {
    /// 派生该次调用的原始问题（用于校验与 trace）
    pub question: String,
    /// Document Resolver 锁定的 file_id 白名单（空 = 全库/未锁定）
    #[serde(default)]
    pub file_ids: Vec<Uuid>,
    /// 目标对象内部真正要检索/抽取的内容（None = 只读结构/概览）
    pub content_query: Option<String>,
    /// 目标对象类型（如 resume / contract），供按类型约束召回
    pub document_type: Option<DocumentType>,
    /// 读取上限（如 read_document 的字符上限）
    pub max_chars: Option<usize>,
}

/// Tool 执行结果状态（供 Recovery / 校验使用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// 成功且产出可用结果
    Success,
    /// 执行完成但证据不足（无可信引用/命中），供 Recovery 或拒答判断
    Insufficient,
    /// 执行失败（存储/模型错误）
    Failed,
}

/// Tool 输出（Evidence-bearing 强类型）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub tool: KnowledgeTool,
    pub status: ToolStatus,
    /// 可信证据引用（只会来自原始 chunk/image evidence，契约同受限 RAG）
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// 命中的候选文件 id（search_files / 文档召回产物）
    #[serde(default)]
    pub candidate_file_ids: Vec<Uuid>,
    /// 结构化结果的紧凑摘要（引用之外的必要文本，供生成/校验复用）
    pub summary: Option<String>,
    /// 过程说明（命中信号 / 空因等，进 trace 便于根因归因）
    #[serde(default)]
    pub notes: Vec<String>,
    pub elapsed_ms: u64,
}

impl ToolOutput {
    /// 快速构造「失败」输出（存储/模型错误等）。
    pub fn failed(tool: KnowledgeTool, reason: impl Into<String>) -> Self {
        Self {
            tool,
            status: ToolStatus::Failed,
            evidence: Vec::new(),
            candidate_file_ids: Vec::new(),
            summary: None,
            notes: vec![reason.into()],
            elapsed_ms: 0,
        }
    }
}

/// 输出校验结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolValidation {
    /// 校验通过：字段满足契约（evidence 引用完整、与问题匹配）
    Passed,
    /// 证据不足：请求了内容但无证据/无候选，可触发单次 Recovery 或拒答
    Insufficient(String),
    /// 校验失败：结果违反不变量（引用越权 / 字段缺失），应视为失败
    Incomplete(String),
}

/// 统一校验函数（Phase 3 将在结果注入生成前调用）。
///
/// 校验是通用不可变约束，不针对具体文件/测试写规则：
/// - 请求了内容检索（`input.content_query` 非空）但产出无证据 → 证据不足；
/// - 产出的任一证据引用没有落进 `file_ids` 白名单（或全库场景）→ 越权/不完整。
pub fn validate_output(input: &ToolInput, output: &ToolOutput) -> ToolValidation {
    let requested_content = input
        .content_query
        .as_ref()
        .is_some_and(|q| !q.trim().is_empty());
    if output.status == ToolStatus::Failed {
        return ToolValidation::Incomplete("tool_execution_failed".to_owned());
    }
    if requested_content && output.evidence.is_empty() {
        return ToolValidation::Insufficient("no_evidence_for_content_query".to_owned());
    }
    // 越权引用检查：非全库场景下，证据必须落在 file_id 白名单内
    if !input.file_ids.is_empty() {
        for ev in &output.evidence {
            if !input.file_ids.contains(&ev.file_id) {
                return ToolValidation::Incomplete("evidence_out_of_scope".to_owned());
            }
        }
    }
    ToolValidation::Passed
}

/// 便捷别名：状态 + 校验合并，供调用方统一分支。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolValidator;

impl ToolValidator {
    pub fn validate(input: &ToolInput, output: &ToolOutput) -> ToolValidation {
        validate_output(input, output)
    }
}
