pub mod ask;
pub mod catalog;
pub mod contracts;
pub mod evaluation;
pub mod exclusions;
pub mod generation;
pub mod indexing;
pub mod knowledge;
pub mod maintenance;
pub mod memory;
pub mod model_catalog;
pub mod models;
pub mod organizing;
pub mod preset_resolver;
pub mod profile_builder;
pub mod router;
pub mod runtime;
pub mod scanner;
pub mod storage;
pub mod theme;
pub mod vector_index;
pub mod watcher;
pub mod welcome;
pub mod worker;

// 注意：ResolutionStatus 与 organizing::ResolutionStatus 撞名，不在此 glob 导出，
// 需要的调用方从 ask::query_plan 路径引入。
pub use ask::answer_gate::{
    AnswerShape, AnswerabilityInput, AnswerabilityStatus, AnswerabilityVerdict, EvidenceRole,
    GateEvidence, LOCAL_STRICT_SYSTEM_PROMPT, answer_shape_directive, claim_subject_mismatch,
    classify_answer_shape, classify_evidence_role, evaluate_answerability,
    existence_requires_project_context, extract_query_entities, find_external_knowledge_marker,
    local_no_evidence_answer,
};
pub use ask::compare::{
    COMPARE_FALLBACK_ITEMS, COMPARE_MATERIAL_CHARS, COMPARE_MATERIAL_ITEMS, CompareDifference,
    ComparePoint, CompareResults, compare_prompt, compare_schema, parse_compare_results,
};
pub use ask::context_resolver::{ContextResolution, ContextSignal, resolve_ambiguous};
pub use ask::document_resolver::{MAX_CANDIDATE_SCOPE, ResolverInput, resolve_documents};
pub use ask::document_retrieval::{
    DOCUMENT_RECALL_METADATA_ONLY_WEIGHT, DOCUMENT_RECALL_METADATA_WEIGHT,
    DOCUMENT_RECALL_MIN_SCORE, DOCUMENT_RECALL_MIN_SIGNAL_LEN, DOCUMENT_RECALL_TOP_N,
    DOCUMENT_RECALL_VECTOR_CANDIDATES, DOCUMENT_RECALL_VECTOR_WEIGHT, DocumentCandidateMatch,
    cosine_similarity, preselect_document_profiles, rank_document_candidates,
    score_document_metadata,
};
pub use ask::document_summary::{
    DocumentOverview, DocumentSection, MAX_SECTION_CHARS, MAX_SECTIONS, SectionChunk,
    SectionSummary, StructureEntry, build_document_sections, digests_json,
    document_overview_prompt, document_summary_prompt, merge_tail_sections, overview_schema,
    parse_overview, parse_section_summaries, section_batch_json, section_summary_schema,
};
pub use ask::extract::{
    EXTRACT_MATCH_MIN_LEN, EXTRACT_MATERIAL_CHARS, EXTRACT_MAX_ITEMS, ExtractItem, ExtractResults,
    extract_item_is_entity_like, extract_prompt, extract_schema, longest_common_substr_len,
    parse_extract_results,
};
pub use ask::memory_resolver::{MemoryHint, match_alias_hints, match_relation_hints};
pub use ask::memory_writer::{
    MemoryTargetRegistry, MemoryWriterContext, MemoryWriterOutput, memory_writer_prompt,
    memory_writer_schema, parse_writer_output, prewrite_validate, resolve_proposal_targets,
};
pub use ask::no_evidence::NoEvidenceReason;
pub use ask::query_parser::{parse_query_plan, query_parser_prompt, query_parser_schema};
pub use ask::query_plan::{
    DocumentCandidate, DocumentResolution, EvidenceStatus, QueryFilters, QueryOperation, QueryPlan,
    QueryTarget, SourceIntent,
};
pub use ask::source_router::{
    SourceRouting, parse_source_routing, source_router_prompt, source_routing_schema,
};
pub use catalog::*;
pub use contracts::*;
pub use evaluation::*;
pub use exclusions::*;
pub use generation::*;
pub use indexing::*;
pub use knowledge::*;
pub use maintenance::*;
pub use memory::*;
pub use model_catalog::*;
pub use models::*;
pub use organizing::*;
pub use preset_resolver::*;
pub use router::*;
pub use runtime::*;
pub use scanner::*;
pub use storage::*;
pub use theme::*;
pub use vector_index::*;
pub use watcher::*;
pub use welcome::WelcomeService;
pub use worker::*;

/// 当前线程正在执行的操作级追踪（OperationTrace）的 operation_id。
/// 由上层链路（app 层 ActiveOperationTrace）在入口设置、出口清除；
/// Core 层各节点（storage 等）写入 TraceNode 时自动读取以关联
/// operation_traces.operation_id。仅用于可观测性，读取失败视为无关联。
use std::cell::RefCell;
thread_local! {
    static ACTIVE_OPERATION_TRACE_ID: RefCell<Option<String>> =
        const { RefCell::new(None) };
}

/// 设置当前线程的 OperationTrace operation_id（传 None 解除关联）。
pub fn set_active_operation_trace(operation_id: Option<String>) {
    ACTIVE_OPERATION_TRACE_ID.with(|cell| *cell.borrow_mut() = operation_id);
}

/// 读取当前线程绑定的 OperationTrace operation_id（无则返回 None）。
pub fn active_operation_trace() -> Option<String> {
    ACTIVE_OPERATION_TRACE_ID.with(|cell| cell.borrow().clone())
}
