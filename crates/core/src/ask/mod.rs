//! 本地问答的新架构模块（P0 骨架 + P1 意图管线）。
//!
//! 链路从「Query → 相似 Chunk」升级为：
//!
//! ```text
//! Query
//!  → Source Router（LOCAL / GENERAL / AMBIGUOUS）
//!  → Query Parser（QueryPlan：目标对象 + 内容查询分离）
//!  → Context Resolver（AMBIGUOUS 结合会话上下文恢复）
//!  → Document Resolver（目标对象 → file_id 白名单）
//!    └→ Memory Resolver（alias / relation 命中优先覆盖 scope）
//!    └→ 多候选且 Memory 未消歧 → NEED_CLARIFICATION（用户选择后锁定重跑）
//!  → 意图分发：
//!    · chunk RAG 主链 → Scoped Retrieval（FTS + Embedding + RRF + MMR + Rerank）
//!      → Generation → Citation Validation
//!    · DOCUMENT_SUMMARY（章节分组分层摘要）
//!    · COMPARE_DOCUMENTS（对齐 + diff 证据）
//!    · EXTRACT（结构化抽取）
//!    · DOCUMENT_FIND（定位文件）
//!    · MULTI_DOCUMENT_QA（document recall 先筛后检）
//! ```
//!
//! 各子模块只依赖本模块内的类型与 `crate::knowledge` 的共享 prompt 工具，
//! 编排逻辑在桌面应用层（app_data.rs）调用，不在此堆 SQL 与模型调用。

pub mod answer_gate;
pub mod builtin_knowledge;
pub mod compare;
pub mod context_resolver;
pub mod document_resolver;
pub mod document_retrieval;
pub mod document_summary;
pub mod extract;
pub mod memory_resolver;
pub mod memory_writer;
pub mod no_evidence;
#[cfg(test)]
pub mod phase_4_2_cases;
pub mod query_normalize;
pub mod query_parser;
pub mod query_plan;
#[cfg(test)]
pub mod scenarios;
pub mod source_router;

pub use answer_gate::{
    AnswerShape, AnswerabilityInput, AnswerabilityStatus, AnswerabilityVerdict, EvidenceRole,
    GateEvidence, LOCAL_STRICT_SYSTEM_PROMPT, answer_shape_directive, claim_subject_mismatch,
    classify_answer_shape, classify_evidence_role, evaluate_answerability,
    existence_requires_project_context, extract_query_entities, find_external_knowledge_marker,
    local_no_evidence_answer,
};
pub use compare::{
    COMPARE_FALLBACK_ITEMS, COMPARE_MATERIAL_CHARS, COMPARE_MATERIAL_ITEMS, CompareDifference,
    ComparePoint, CompareResults, compare_prompt, compare_schema, parse_compare_results,
};
pub use context_resolver::{ContextResolution, ContextSignal, resolve_ambiguous};
pub use document_resolver::{
    HIGH_CONFIDENCE_THRESHOLD, HIGH_MARGIN, MAX_CANDIDATE_SCOPE, MEDIUM_CONFIDENCE_THRESHOLD,
    ResolverInput, SIGNAL_WEIGHTS, resolve_documents,
};
pub use document_retrieval::{
    DOCUMENT_RECALL_METADATA_ONLY_WEIGHT, DOCUMENT_RECALL_METADATA_WEIGHT,
    DOCUMENT_RECALL_MIN_SCORE, DOCUMENT_RECALL_MIN_SIGNAL_LEN, DOCUMENT_RECALL_TOP_N,
    DOCUMENT_RECALL_VECTOR_CANDIDATES, DOCUMENT_RECALL_VECTOR_WEIGHT, DocumentCandidateMatch,
    cosine_similarity, preselect_document_profiles, rank_document_candidates,
    score_document_metadata,
};
pub use document_summary::{
    DocumentOverview, DocumentSection, MAX_SECTION_CHARS, MAX_SECTIONS, SectionChunk,
    SectionSummary, StructureEntry, build_document_sections, digests_json,
    document_overview_prompt, document_summary_prompt, merge_tail_sections, overview_schema,
    parse_overview, parse_section_summaries, section_batch_json, section_summary_schema,
};
pub use extract::{
    EXTRACT_MATCH_MIN_LEN, EXTRACT_MATERIAL_CHARS, EXTRACT_MAX_ITEMS, ExtractItem, ExtractResults,
    extract_prompt, extract_schema, longest_common_substr_len, parse_extract_results,
};
pub use memory_resolver::{MemoryHint, match_alias_hints, match_relation_hints};
pub use memory_writer::{
    MemoryProposal, MemoryTargetRegistry, MemoryWriterContext, MemoryWriterOutput,
    memory_writer_prompt, memory_writer_schema, parse_writer_output, prewrite_validate,
    resolve_proposal_targets,
};
pub use no_evidence::NoEvidenceReason;
pub use query_normalize::normalize_query_variants;
pub use query_parser::{parse_query_plan, query_parser_prompt, query_parser_schema};
pub use query_plan::{
    DocumentCandidate, DocumentResolution, EvidenceStatus, QueryFilters, QueryIntent,
    QueryOperation, QueryPlan, QueryTarget, QuestionShape, ResolutionStatus, SourceIntent,
};
pub use source_router::{
    SourceRouting, parse_source_routing, source_router_prompt, source_routing_schema,
};
