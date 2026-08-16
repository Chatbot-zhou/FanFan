use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, MAIN_DB, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::AppError;

pub const EVALUATION_SCHEMA_VERSION: u32 = 2;
pub const EVALUATION_PASS_SCORE: f64 = 85.0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationSplit {
    Tuning,
    Development,
    Hidden,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationComponentScore {
    pub component: String,
    pub earned: f64,
    pub maximum: f64,
    pub sample_count: u64,
    #[serde(default)]
    pub metrics: HashMap<String, f64>,
    #[serde(default)]
    pub failure_categories: Vec<String>,
}

impl EvaluationComponentScore {
    pub fn bounded(mut self) -> Self {
        self.earned = self.earned.clamp(0.0, self.maximum.max(0.0));
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvaluationSafetyGates {
    pub source_files_unchanged: bool,
    pub authorized_scope_only: bool,
    pub model_packages_complete: bool,
    pub jobs_terminal_or_recoverable: bool,
    pub index_key_mapping_consistent: bool,
    pub generated_content_verified: bool,
    pub logs_privacy_safe: bool,
}

impl EvaluationSafetyGates {
    pub fn all_pass(&self) -> bool {
        self.source_files_unchanged
            && self.authorized_scope_only
            && self.model_packages_complete
            && self.jobs_terminal_or_recoverable
            && self.index_key_mapping_consistent
            && self.generated_content_verified
            && self.logs_privacy_safe
    }

    pub fn failed_names(&self) -> Vec<&'static str> {
        let candidates = [
            ("source_files_unchanged", self.source_files_unchanged),
            ("authorized_scope_only", self.authorized_scope_only),
            ("model_packages_complete", self.model_packages_complete),
            (
                "jobs_terminal_or_recoverable",
                self.jobs_terminal_or_recoverable,
            ),
            (
                "index_key_mapping_consistent",
                self.index_key_mapping_consistent,
            ),
            (
                "generated_content_verified",
                self.generated_content_verified,
            ),
            ("logs_privacy_safe", self.logs_privacy_safe),
        ];
        candidates
            .into_iter()
            .filter_map(|(name, passed)| (!passed).then_some(name))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationScorecard {
    pub score: f64,
    pub pass_score: f64,
    pub passed: bool,
    pub safety_gates: EvaluationSafetyGates,
    pub components: Vec<EvaluationComponentScore>,
}

impl EvaluationScorecard {
    pub fn from_components(
        components: Vec<EvaluationComponentScore>,
        safety_gates: EvaluationSafetyGates,
    ) -> Self {
        let components = components
            .into_iter()
            .map(EvaluationComponentScore::bounded)
            .collect::<Vec<_>>();
        let score = components.iter().map(|component| component.earned).sum();
        Self {
            score,
            pass_score: EVALUATION_PASS_SCORE,
            passed: score >= EVALUATION_PASS_SCORE && safety_gates.all_pass(),
            safety_gates,
            components,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationRun {
    pub run_id: Uuid,
    pub schema_version: u32,
    pub split: EvaluationSplit,
    pub dataset_fingerprint: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub scorecard: Option<EvaluationScorecard>,
    #[serde(default)]
    pub evidence: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvaluationIntegritySnapshot {
    pub active_jobs: u64,
    pub stale_nonrecoverable_jobs: u64,
    pub orphan_embeddings: u64,
    pub inconsistent_active_vector_keys: u64,
    pub missing_active_index_files: u64,
    pub files_without_authorized_roots: u64,
    pub measured_at: DateTime<Utc>,
}

impl EvaluationIntegritySnapshot {
    pub fn jobs_terminal_or_recoverable(&self) -> bool {
        self.stale_nonrecoverable_jobs == 0
    }

    pub fn index_key_mapping_consistent(&self) -> bool {
        self.orphan_embeddings == 0
            && self.inconsistent_active_vector_keys == 0
            && self.missing_active_index_files == 0
    }

    pub fn authorized_scope_only(&self) -> bool {
        self.files_without_authorized_roots == 0
    }
}

impl EvaluationRun {
    pub fn start(split: EvaluationSplit, dataset_fingerprint: String) -> Self {
        Self {
            run_id: Uuid::now_v7(),
            schema_version: EVALUATION_SCHEMA_VERSION,
            split,
            dataset_fingerprint,
            started_at: Utc::now(),
            completed_at: None,
            scorecard: None,
            evidence: HashMap::new(),
        }
    }

    pub fn complete(&mut self, scorecard: EvaluationScorecard) {
        self.completed_at = Some(Utc::now());
        self.scorecard = Some(scorecard);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchEvaluationCase {
    pub case_id: String,
    pub relevant_file_ids: Vec<Uuid>,
    pub returned_file_ids: Vec<Uuid>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchQualityMetrics {
    pub sample_count: u64,
    pub recall_at_10: f64,
    pub mrr_at_10: f64,
    pub ndcg_at_10: f64,
    pub p95_latency_ms: u64,
}

impl SearchQualityMetrics {
    pub fn to_component_score(&self) -> EvaluationComponentScore {
        let quality = 0.4 * self.recall_at_10 + 0.3 * self.mrr_at_10 + 0.3 * self.ndcg_at_10;
        let latency_factor = if self.p95_latency_ms <= 2_000 {
            1.0
        } else {
            (2_000.0 / self.p95_latency_ms as f64).clamp(0.5, 1.0)
        };
        EvaluationComponentScore {
            component: "search_quality".into(),
            earned: 20.0 * quality * latency_factor,
            maximum: 20.0,
            sample_count: self.sample_count,
            metrics: HashMap::from([
                ("recall_at_10".into(), self.recall_at_10),
                ("mrr_at_10".into(), self.mrr_at_10),
                ("ndcg_at_10".into(), self.ndcg_at_10),
                ("p95_latency_ms".into(), self.p95_latency_ms as f64),
            ]),
            failure_categories: Vec::new(),
        }
    }
}

pub fn score_search_cases(cases: &[SearchEvaluationCase]) -> SearchQualityMetrics {
    if cases.is_empty() {
        return SearchQualityMetrics {
            sample_count: 0,
            recall_at_10: 0.0,
            mrr_at_10: 0.0,
            ndcg_at_10: 0.0,
            p95_latency_ms: 0,
        };
    }

    let mut recall = 0.0;
    let mut mrr = 0.0;
    let mut ndcg = 0.0;
    let mut latencies = Vec::with_capacity(cases.len());
    for case in cases {
        let relevant = case
            .relevant_file_ids
            .iter()
            .copied()
            .collect::<HashSet<_>>();
        let returned = case.returned_file_ids.iter().take(10).collect::<Vec<_>>();
        if !relevant.is_empty() {
            let hits = returned
                .iter()
                .filter(|file_id| relevant.contains(file_id))
                .count();
            recall += hits as f64 / relevant.len() as f64;
            if let Some(rank) = returned
                .iter()
                .position(|file_id| relevant.contains(file_id))
            {
                mrr += 1.0 / (rank + 1) as f64;
            }
            let dcg = returned
                .iter()
                .enumerate()
                .filter(|(_, file_id)| relevant.contains(file_id))
                .map(|(rank, _)| 1.0 / ((rank + 2) as f64).log2())
                .sum::<f64>();
            let ideal_hits = relevant.len().min(10);
            let idcg = (0..ideal_hits)
                .map(|rank| 1.0 / ((rank + 2) as f64).log2())
                .sum::<f64>();
            ndcg += if idcg > 0.0 { dcg / idcg } else { 0.0 };
        }
        latencies.push(case.elapsed_ms);
    }
    latencies.sort_unstable();
    let p95_index = ((latencies.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(latencies.len() - 1);
    let divisor = cases.len() as f64;
    SearchQualityMetrics {
        sample_count: cases.len() as u64,
        recall_at_10: recall / divisor,
        mrr_at_10: mrr / divisor,
        ndcg_at_10: ndcg / divisor,
        p95_latency_ms: latencies[p95_index],
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RagEvaluationCase {
    pub case_id: String,
    pub expected_refusal: bool,
    pub refused: bool,
    pub generated: bool,
    pub factual_claims: u64,
    pub verified_claims: u64,
    pub unauthorized_citations: u64,
    pub expected_source_cited: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RagQualityMetrics {
    pub sample_count: u64,
    pub citation_coverage: f64,
    pub factual_correctness: f64,
    pub refusal_accuracy: f64,
    pub unauthorized_rejection_rate: f64,
    pub structured_generation_rate: f64,
    pub p95_latency_ms: u64,
}

impl RagQualityMetrics {
    pub fn to_component_score(&self) -> EvaluationComponentScore {
        let latency = if self.p95_latency_ms <= 30_000 {
            1.0
        } else {
            (30_000.0 / self.p95_latency_ms as f64).clamp(0.25, 1.0)
        };
        let quality = 0.25 * self.citation_coverage
            + 0.25 * self.factual_correctness
            + 0.15 * self.refusal_accuracy
            + 0.15 * self.unauthorized_rejection_rate
            + 0.1 * self.structured_generation_rate
            + 0.1 * latency;
        let mut failures = Vec::new();
        if self.citation_coverage < 1.0 {
            failures.push("citation_coverage_below_hard_gate".into());
        }
        if self.unauthorized_rejection_rate < 1.0 {
            failures.push("unauthorized_citation_rejection_below_hard_gate".into());
        }
        if self.refusal_accuracy < 1.0 {
            failures.push("refusal_accuracy_below_hard_gate".into());
        }
        if self.factual_correctness < 0.85 {
            failures.push("factual_correctness_below_target".into());
        }
        EvaluationComponentScore {
            component: "strict_rag".into(),
            earned: 30.0 * quality,
            maximum: 30.0,
            sample_count: self.sample_count,
            metrics: HashMap::from([
                ("citation_coverage".into(), self.citation_coverage),
                ("factual_correctness".into(), self.factual_correctness),
                ("refusal_accuracy".into(), self.refusal_accuracy),
                (
                    "unauthorized_rejection_rate".into(),
                    self.unauthorized_rejection_rate,
                ),
                (
                    "structured_generation_rate".into(),
                    self.structured_generation_rate,
                ),
                ("p95_latency_ms".into(), self.p95_latency_ms as f64),
            ]),
            failure_categories: failures,
        }
    }
}

pub fn score_rag_cases(cases: &[RagEvaluationCase]) -> RagQualityMetrics {
    if cases.is_empty() {
        return RagQualityMetrics {
            sample_count: 0,
            citation_coverage: 0.0,
            factual_correctness: 0.0,
            refusal_accuracy: 0.0,
            unauthorized_rejection_rate: 0.0,
            structured_generation_rate: 0.0,
            p95_latency_ms: 0,
        };
    }
    let factual_claims = cases.iter().map(|case| case.factual_claims).sum::<u64>();
    let verified_claims = cases.iter().map(|case| case.verified_claims).sum::<u64>();
    let answer_cases = cases
        .iter()
        .filter(|case| !case.expected_refusal)
        .collect::<Vec<_>>();
    let refusal_cases = cases
        .iter()
        .filter(|case| case.expected_refusal)
        .collect::<Vec<_>>();
    let mut latencies = cases.iter().map(|case| case.elapsed_ms).collect::<Vec<_>>();
    latencies.sort_unstable();
    let p95_index = ((latencies.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(latencies.len() - 1);
    let ratio = |count: usize, total: usize| {
        if total == 0 {
            1.0
        } else {
            count as f64 / total as f64
        }
    };
    RagQualityMetrics {
        sample_count: cases.len() as u64,
        citation_coverage: if factual_claims == 0 {
            0.0
        } else {
            verified_claims as f64 / factual_claims as f64
        },
        factual_correctness: ratio(
            answer_cases
                .iter()
                .filter(|case| case.generated && case.expected_source_cited)
                .count(),
            answer_cases.len(),
        ),
        refusal_accuracy: ratio(
            refusal_cases.iter().filter(|case| case.refused).count(),
            refusal_cases.len(),
        ),
        unauthorized_rejection_rate: ratio(
            cases
                .iter()
                .filter(|case| case.unauthorized_citations == 0)
                .count(),
            cases.len(),
        ),
        structured_generation_rate: ratio(
            answer_cases.iter().filter(|case| case.generated).count(),
            answer_cases.len(),
        ),
        p95_latency_ms: latencies[p95_index],
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelationEvaluationCase {
    pub case_id: String,
    pub expected_related: bool,
    pub predicted_related: bool,
    pub expected_relation_type: Option<String>,
    pub predicted_relation_type: Option<String>,
}

pub fn score_relation_cases(cases: &[RelationEvaluationCase]) -> EvaluationComponentScore {
    let true_positive = cases
        .iter()
        .filter(|case| case.expected_related && case.predicted_related)
        .count();
    let predicted_positive = cases.iter().filter(|case| case.predicted_related).count();
    let actual_positive = cases.iter().filter(|case| case.expected_related).count();
    let precision = if predicted_positive == 0 {
        0.0
    } else {
        true_positive as f64 / predicted_positive as f64
    };
    let recall = if actual_positive == 0 {
        0.0
    } else {
        true_positive as f64 / actual_positive as f64
    };
    let type_accuracy = if true_positive == 0 {
        0.0
    } else {
        cases
            .iter()
            .filter(|case| {
                case.expected_related
                    && case.predicted_related
                    && case.expected_relation_type == case.predicted_relation_type
            })
            .count() as f64
            / true_positive as f64
    };
    let mut failures = Vec::new();
    if precision < 0.85 {
        failures.push("semantic_relation_precision_below_target".into());
    }
    if recall < 0.80 {
        failures.push("semantic_relation_recall_below_target".into());
    }
    EvaluationComponentScore {
        component: "relations_and_collections".into(),
        earned: 10.0 * (0.45 * precision + 0.4 * recall + 0.15 * type_accuracy),
        maximum: 10.0,
        sample_count: cases.len() as u64,
        metrics: HashMap::from([
            ("precision".into(), precision),
            ("recall".into(), recall),
            ("relation_type_accuracy".into(), type_accuracy),
        ]),
        failure_categories: failures,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogPrivacyInspection {
    pub files_checked: u64,
    pub events_checked: u64,
    pub violations: u64,
}

impl LogPrivacyInspection {
    pub fn passed(&self) -> bool {
        self.violations == 0
    }
}

pub fn inspect_runtime_log_privacy(log_directory: &Path) -> Result<LogPrivacyInspection, AppError> {
    if !log_directory.is_dir() {
        return Ok(LogPrivacyInspection {
            files_checked: 0,
            events_checked: 0,
            violations: 0,
        });
    }
    let mut inspection = LogPrivacyInspection {
        files_checked: 0,
        events_checked: 0,
        violations: 0,
    };
    for entry in fs::read_dir(log_directory)
        .map_err(|error| AppError::local_config(error.to_string(), true))?
    {
        let path = entry
            .map_err(|error| AppError::local_config(error.to_string(), true))?
            .path();
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        inspection.files_checked += 1;
        let contents = fs::read_to_string(&path)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            inspection.events_checked += 1;
            let value = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(value) => value,
                Err(_) => {
                    inspection.violations += 1;
                    continue;
                }
            };
            if json_contains_sensitive_log_value(&value, None) {
                inspection.violations += 1;
            }
        }
    }
    Ok(inspection)
}

fn json_contains_sensitive_log_value(value: &serde_json::Value, field: Option<&str>) -> bool {
    if field.is_some_and(|field| {
        let field = field.to_ascii_lowercase();
        [
            "path", "text", "content", "body", "quote", "query", "prompt", "question", "answer",
            "snippet",
        ]
        .iter()
        .any(|token| field.contains(token))
    }) {
        return true;
    }
    match value {
        serde_json::Value::String(value) => contains_absolute_path(value),
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_contains_sensitive_log_value(value, None)),
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(key, value)| json_contains_sensitive_log_value(value, Some(key))),
        _ => false,
    }
}

fn contains_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.windows(3).any(|window| {
        window[0].is_ascii_alphabetic() && window[1] == b':' && matches!(window[2], b'\\' | b'/')
    }) || value.starts_with("\\\\")
}

/// ===== Ask Evaluation Runner（Phase 3）=====
///
/// 测试集（JSONL 或 JSON 数组）批量运行问答管线，逐项对比 expected_* 与
/// actual_*，产出每例 verdict + 全批指标。第一版只保证**数据可采集**，
/// 不做复杂 NLP 自动评分；error_category 允许人工修改（结果文件可直接编辑）。
///
/// 禁止自动学习：Runner 绝不修改 Router Prompt / Resolver 权重 /
/// Memory / 阈值 / Prototype；运行期间不写任何 Memory（candidate writer
/// 不启动，clarification_selection 不出现）。

/// 单个评估用例（测试集一行 / 一个元素）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationCase {
    pub id: String,
    pub question: String,
    #[serde(default)]
    pub expected_source: Option<String>,
    #[serde(default)]
    pub expected_intent: Option<String>,
    /// 可空：不校验文件命中
    #[serde(default)]
    pub expected_file_ids: Option<Vec<String>>,
    #[serde(default)]
    pub expected_document_type: Option<String>,
    /// Some(true)：应找到证据；Some(false)：应 NO_EVIDENCE 拒绝；None：不校验
    #[serde(default)]
    pub expected_should_find_evidence: Option<bool>,
}

/// 错误分类（13 类，snake_case；UNKNOWN 兜底）。允许人工修改。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationErrorCategory {
    RouterError,
    QueryParseError,
    ContextError,
    MemoryError,
    DocumentResolutionError,
    DocumentRecallError,
    ChunkRetrievalError,
    RerankError,
    NoEvidenceError,
    GenerationError,
    CitationError,
    ClarificationError,
    Unknown,
}

impl EvaluationErrorCategory {
    /// trace 节点名 → 错误分类（优先于运行级错误码判断）。
    pub fn from_node(node: &str) -> Option<Self> {
        Some(match node {
            "source_routing" => Self::RouterError,
            "query_parsing" => Self::QueryParseError,
            "context_resolution" => Self::ContextError,
            "memory_resolution" => Self::MemoryError,
            "document_resolution" => Self::DocumentResolutionError,
            "document_recall" => Self::DocumentRecallError,
            "retrieval" => Self::ChunkRetrievalError,
            "reranking" => Self::RerankError,
            "generation" => Self::GenerationError,
            "verification" => Self::CitationError,
            "repair" => Self::CitationError,
            "clarification_selection" => Self::ClarificationError,
            _ => return None,
        })
    }

    /// 运行级错误码 → 分类（子串匹配，宽匹配已知家族）。
    pub fn from_error_code(code: &str) -> Self {
        let upper = code.to_ascii_uppercase();
        if upper.contains("CLARIFICATION") {
            Self::ClarificationError
        } else if upper.contains("GENERATION") || upper.contains("RAG_GENERATION") {
            Self::GenerationError
        } else if upper.contains("RERANK") {
            Self::RerankError
        } else if upper.contains("EMBEDDING") || upper.contains("RAG_EMBEDDING") {
            Self::ChunkRetrievalError
        } else if upper.contains("ROUT") {
            Self::RouterError
        } else if upper.contains("PARSE") || upper.contains("PARSER") {
            Self::QueryParseError
        } else if upper.contains("RESOLV") || upper.contains("RESOLUTION") {
            Self::DocumentResolutionError
        } else if upper.contains("MEMORY") {
            Self::MemoryError
        } else {
            Self::Unknown
        }
    }
}

/// 单条用例的评估结果（JSON 可直接落盘供人工复核/修改分类）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationRunResult {
    pub case_id: String,
    /// 该用例运行时的 operation_id（node_traces 关联键，可对应用例打开 Trace Viewer）
    #[serde(default)]
    pub operation_id: String,
    pub question: String,
    pub expected_source: Option<String>,
    pub expected_intent: Option<String>,
    pub expected_file_ids: Option<Vec<String>>,
    pub expected_document_type: Option<String>,
    pub expected_should_find_evidence: Option<bool>,
    pub actual_source: Option<String>,
    pub actual_intent: Option<String>,
    pub actual_file_ids: Vec<String>,
    pub actual_document_type: Option<String>,
    pub memory_used: bool,
    pub clarification_used: bool,
    pub retrieval_top_files: Vec<String>,
    pub rerank_top_files: Vec<String>,
    pub grounding_status: Option<String>,
    pub answer_mode: Option<String>,
    /// 检索/回答是否使用了证据（claims 或来源文件非空）
    pub evidence_found: bool,
    /// Grounded 且无 Unsupported claim
    pub answer_grounded: bool,
    pub claim_count: u64,
    pub supported_claim_count: u64,
    pub latency_ms: u64,
    #[serde(default)]
    pub error_category: Option<EvaluationErrorCategory>,
    #[serde(default)]
    pub error_message: Option<String>,
    pub pass_fail: bool,
    #[serde(default)]
    pub failed_fields: Vec<String>,
}

/// 逐字段判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationVerdict {
    pub pass_fail: bool,
    pub failed_fields: Vec<String>,
}

/// 判定（纯逻辑，无 IO）：expected_* 有值才断言；scope_correct 第一版
/// 测试集不建模（无可空字段），不参与判定。
pub fn verdict_for(result: &EvaluationRunResult) -> EvaluationVerdict {
    let mut failed = Vec::new();
    if let Some(expected) = result.expected_source.as_deref()
        && result.actual_source.as_deref() != Some(expected)
    {
        failed.push("source_correct".to_owned());
    }
    if let Some(expected) = result.expected_intent.as_deref()
        && result.actual_intent.as_deref() != Some(expected)
    {
        failed.push("intent_correct".to_owned());
    }
    if let Some(expected) = result.expected_document_type.as_deref()
        && result.actual_document_type.as_deref() != Some(expected)
    {
        failed.push("target_correct".to_owned());
    }
    if let Some(expected) = result.expected_file_ids.as_deref() {
        let hit = expected
            .iter()
            .any(|file_id| result.actual_file_ids.iter().any(|actual| actual == file_id));
        if !hit {
            failed.push("file_correct".to_owned());
        }
    }
    match result.expected_should_find_evidence {
        Some(true) => {
            if !result.evidence_found {
                failed.push("evidence_found".to_owned());
            }
            if !result.answer_grounded {
                failed.push("answer_grounded".to_owned());
            }
        }
        Some(false) => {
            if result.evidence_found {
                failed.push("evidence_found".to_owned());
            }
        }
        None => {}
    }
    EvaluationVerdict {
        pass_fail: failed.is_empty(),
        failed_fields: failed,
    }
}

/// 错误分类（纯逻辑）：节点失败 > 运行错误码 > NO_EVIDENCE > 引用核验。
pub fn classify_error(
    failed_nodes: &[&str],
    run_error_code: Option<&str>,
    answer_mode: Option<&str>,
    insufficient_evidence: bool,
    expected_should_find_evidence: Option<bool>,
    claims_have_unsupported: bool,
) -> Option<EvaluationErrorCategory> {
    if let Some(node) = failed_nodes
        .iter()
        .find_map(|node| EvaluationErrorCategory::from_node(node))
    {
        return Some(node);
    }
    if let Some(code) = run_error_code {
        return Some(EvaluationErrorCategory::from_error_code(code));
    }
    if answer_mode == Some("rag_refusal") || insufficient_evidence {
        // 预期就是 NO_EVIDENCE 的用例不算错误；未声明预期时按错误上报
        return (expected_should_find_evidence != Some(false)).then_some(
            EvaluationErrorCategory::NoEvidenceError,
        );
    }
    if claims_have_unsupported {
        return Some(EvaluationErrorCategory::CitationError);
    }
    None
}

/// 批量指标（14 项核心 + 耗时分布）。只保证数据可采集，不做通过率硬门槛。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationRunMetrics {
    pub total: u64,
    pub passed: u64,
    pub source_router_accuracy: f64,
    pub intent_accuracy: f64,
    pub document_resolution_top1_accuracy: f64,
    pub document_resolution_top3_recall: f64,
    pub memory_hit_accuracy: f64,
    pub memory_wrong_hit_rate: f64,
    pub clarification_rate: f64,
    pub clarification_success_rate: f64,
    pub retrieval_evidence_recall: f64,
    pub no_evidence_false_negative_rate: f64,
    pub grounded_answer_rate: f64,
    pub citation_pass_rate: f64,
    pub avg_total_ms: f64,
    pub p50_total_ms: u64,
    pub p95_total_ms: u64,
}

fn fraction(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// 聚合指标（纯逻辑）：分母为该指标可判定的用例子集。
pub fn compute_metrics(results: &[EvaluationRunResult]) -> EvaluationRunMetrics {
    let total = results.len() as u64;
    let passed = results.iter().filter(|result| result.pass_fail).count();
    let source_denominator = results
        .iter()
        .filter(|result| result.expected_source.is_some())
        .count();
    let intent_denominator = results
        .iter()
        .filter(|result| result.expected_intent.is_some())
        .count();
    let file_denominator = results
        .iter()
        .filter(|result| result.expected_file_ids.is_some())
        .count();
    let expect_evidence_denominator = results
        .iter()
        .filter(|result| result.expected_should_find_evidence == Some(true))
        .count();
    let clarification_denominator = results
        .iter()
        .filter(|result| result.clarification_used)
        .count();
    let memory_denominator = results
        .iter()
        .filter(|result| result.memory_used && result.expected_file_ids.is_some())
        .count();
    let claims_denominator = results
        .iter()
        .filter(|result| result.claim_count > 0)
        .count();

    let mut latencies = results
        .iter()
        .map(|result| result.latency_ms)
        .collect::<Vec<_>>();
    latencies.sort_unstable();
    let p50 = latencies
        .get(latencies.len() / 2)
        .copied()
        .unwrap_or(0);
    let p95_index = ((latencies.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(latencies.len().saturating_sub(1));
    let p95 = latencies.get(p95_index).copied().unwrap_or(0);
    let avg_total_ms = if latencies.is_empty() {
        0.0
    } else {
        latencies.iter().sum::<u64>() as f64 / latencies.len() as f64
    };

    let file_hit = |result: &EvaluationRunResult| -> bool {
        result.expected_file_ids.as_deref().is_some_and(|expected| {
            expected
                .iter()
                .any(|file_id| result.actual_file_ids.iter().any(|actual| actual == file_id))
        })
    };
    let top_files_hit = |result: &EvaluationRunResult, top: usize| -> bool {
        result.expected_file_ids.as_deref().is_some_and(|expected| {
            result
                .retrieval_top_files
                .iter()
                .take(top)
                .any(|file_id| expected.iter().any(|expected_id| expected_id == file_id))
        })
    };

    EvaluationRunMetrics {
        total,
        passed: passed as u64,
        source_router_accuracy: fraction(
            results
                .iter()
                .filter(|result| {
                    result.expected_source.is_some()
                        && result.actual_source == result.expected_source
                })
                .count(),
            source_denominator,
        ),
        intent_accuracy: fraction(
            results
                .iter()
                .filter(|result| {
                    result.expected_intent.is_some()
                        && result.actual_intent == result.expected_intent
                })
                .count(),
            intent_denominator,
        ),
        document_resolution_top1_accuracy: fraction(
            results.iter().filter(|result| top_files_hit(result, 1)).count(),
            file_denominator,
        ),
        document_resolution_top3_recall: fraction(
            results.iter().filter(|result| top_files_hit(result, 3)).count(),
            file_denominator,
        ),
        memory_hit_accuracy: fraction(
            results
                .iter()
                .filter(|result| result.memory_used && file_hit(result))
                .count(),
            memory_denominator,
        ),
        memory_wrong_hit_rate: fraction(
            results
                .iter()
                .filter(|result| result.memory_used && !file_hit(result))
                .count(),
            memory_denominator,
        ),
        clarification_rate: fraction(clarification_denominator, total as usize),
        clarification_success_rate: fraction(
            results
                .iter()
                .filter(|result| result.clarification_used && file_hit(result))
                .count(),
            clarification_denominator,
        ),
        retrieval_evidence_recall: fraction(
            results
                .iter()
                .filter(|result| {
                    result.expected_should_find_evidence == Some(true) && result.evidence_found
                })
                .count(),
            expect_evidence_denominator,
        ),
        no_evidence_false_negative_rate: fraction(
            expect_evidence_denominator
                - results
                    .iter()
                    .filter(|result| {
                        result.expected_should_find_evidence == Some(true) && result.evidence_found
                    })
                    .count(),
            expect_evidence_denominator,
        ),
        grounded_answer_rate: fraction(
            results.iter().filter(|result| result.answer_grounded).count(),
            total as usize,
        ),
        citation_pass_rate: fraction(
            results
                .iter()
                .filter(|result| {
                    result.claim_count > 0 && result.supported_claim_count == result.claim_count
                })
                .count(),
            claims_denominator,
        ),
        avg_total_ms,
        p50_total_ms: p50,
        p95_total_ms: p95,
    }
}

/// 解析测试集：JSONL（每行一个用例；# 开头/空行忽略）或整体 JSON 数组。
/// 首行是 `[` 时按数组解析。出错时带行号定位。
pub fn parse_evaluation_cases(content: &str) -> Result<Vec<EvaluationCase>, AppError> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    if trimmed.starts_with('[') {
        let cases: Vec<EvaluationCase> = serde_json::from_str(trimmed)
            .map_err(|error| AppError::new("EVALUATION_SET_INVALID", error.to_string(), false))?;
        return Ok(cases);
    }
    let mut cases = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match serde_json::from_str::<EvaluationCase>(line) {
            Ok(case) => cases.push(case),
            Err(error) => {
                return Err(AppError::new(
                    "EVALUATION_SET_INVALID",
                    format!("测试集第 {} 行解析失败: {error}", index + 1),
                    false,
                ));
            }
        }
    }
    Ok(cases)
}

#[derive(Debug, Clone)]
pub struct EncryptedEvaluationSnapshot {
    pub snapshot_id: Uuid,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub sha256: String,
    pub encrypted: bool,
    pub encryption_mode: String,
}

#[derive(Debug)]
pub struct EvaluationWorkingCopy {
    pub path: PathBuf,
    remove_on_drop: bool,
}

impl Drop for EvaluationWorkingCopy {
    fn drop(&mut self) {
        if self.remove_on_drop && self.path.is_file() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn create_encrypted_evaluation_snapshot(
    source_database: &Path,
    evaluation_root: &Path,
) -> Result<EncryptedEvaluationSnapshot, AppError> {
    if !source_database.is_file() {
        return Err(AppError::local_config(
            "评测源数据库不存在，未创建任何快照",
            false,
        ));
    }
    fs::create_dir_all(evaluation_root)
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    let snapshot_id = Uuid::now_v7();
    let efs_available = ensure_directory_encrypted(evaluation_root).is_ok()
        && path_is_encrypted(evaluation_root).unwrap_or(false);
    let snapshot_path = if efs_available {
        evaluation_root.join(format!("evaluation-{snapshot_id}.db"))
    } else {
        evaluation_root.join(format!("evaluation-{snapshot_id}.ffeval"))
    };
    let working_path = if efs_available {
        snapshot_path.clone()
    } else {
        std::env::temp_dir().join(format!("fanfan-evaluation-{snapshot_id}.db"))
    };
    let backup_result = (|| {
        let source = Connection::open_with_flags(
            source_database,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
        source
            .backup(MAIN_DB, &working_path, None)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        if efs_available && !path_is_encrypted(&working_path)? {
            return Err(AppError::local_config(
                "评测快照未继承Windows文件加密，已拒绝保留明文副本",
                false,
            ));
        }
        if !efs_available {
            protect_file_for_current_user(&working_path, &snapshot_path)?;
            fs::remove_file(&working_path)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
        }
        let (size_bytes, sha256) = file_size_and_sha256(&snapshot_path)?;
        Ok(EncryptedEvaluationSnapshot {
            snapshot_id,
            path: snapshot_path.clone(),
            size_bytes,
            sha256,
            encrypted: true,
            encryption_mode: if efs_available {
                "efs".into()
            } else {
                "dpapi_current_user".into()
            },
        })
    })();
    if backup_result.is_err() {
        if snapshot_path.is_file() {
            let _ = fs::remove_file(&snapshot_path);
        }
        if working_path != snapshot_path && working_path.is_file() {
            let _ = fs::remove_file(&working_path);
        }
    }
    backup_result
}

pub fn materialize_evaluation_snapshot(
    snapshot: &EncryptedEvaluationSnapshot,
) -> Result<EvaluationWorkingCopy, AppError> {
    if snapshot.encryption_mode == "efs" {
        return Ok(EvaluationWorkingCopy {
            path: snapshot.path.clone(),
            remove_on_drop: false,
        });
    }
    if snapshot.encryption_mode != "dpapi_current_user" {
        return Err(AppError::local_config("未知的评测快照加密格式", false));
    }
    let working_path = std::env::temp_dir().join(format!(
        "fanfan-evaluation-working-{}.db",
        snapshot.snapshot_id
    ));
    let result = unprotect_file_for_current_user(&snapshot.path, &working_path);
    if result.is_err() && working_path.is_file() {
        let _ = fs::remove_file(&working_path);
    }
    result.map(|_| EvaluationWorkingCopy {
        path: working_path,
        remove_on_drop: true,
    })
}

pub fn persist_evaluation_run(
    evaluation_root: &Path,
    run: &EvaluationRun,
) -> Result<PathBuf, AppError> {
    fs::create_dir_all(evaluation_root)
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    let efs_available = ensure_directory_encrypted(evaluation_root).is_ok()
        && path_is_encrypted(evaluation_root).unwrap_or(false);
    let target = evaluation_root.join(if efs_available {
        format!("scorecard-{}.json", run.run_id)
    } else {
        format!("scorecard-{}.ffeval", run.run_id)
    });
    let temporary = std::env::temp_dir().join(format!("fanfan-scorecard-{}.tmp", run.run_id));
    let payload = serde_json::to_vec_pretty(run)
        .map_err(|error| AppError::local_config(error.to_string(), false))?;
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        file.write_all(&payload)
            .and_then(|_| file.sync_all())
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        if efs_available {
            fs::rename(&temporary, &target)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
            if !path_is_encrypted(&target)? {
                return Err(AppError::local_config(
                    "评测报告未继承Windows文件加密，已拒绝保留明文副本",
                    false,
                ));
            }
        } else {
            protect_file_for_current_user(&temporary, &target)?;
            fs::remove_file(&temporary)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
        }
        Ok(target.clone())
    })();
    if write_result.is_err() && temporary.is_file() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

const PROTECTED_FILE_MAGIC: &[u8; 8] = b"FFEV1\0\0\0";
const PROTECTED_CHUNK_SIZE: usize = 4 * 1024 * 1024;

#[cfg(windows)]
fn protect_file_for_current_user(source: &Path, target: &Path) -> Result<(), AppError> {
    let mut input =
        File::open(source).map_err(|error| AppError::local_config(error.to_string(), true))?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    output
        .write_all(PROTECTED_FILE_MAGIC)
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    let mut buffer = vec![0_u8; PROTECTED_CHUNK_SIZE];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        if read == 0 {
            break;
        }
        let encrypted = dpapi_protect(&mut buffer[..read])?;
        let length = u32::try_from(encrypted.len())
            .map_err(|_| AppError::local_config("评测加密分块过大", false))?;
        output
            .write_all(&length.to_le_bytes())
            .and_then(|_| output.write_all(&encrypted))
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
    }
    output
        .write_all(&0_u32.to_le_bytes())
        .and_then(|_| output.sync_all())
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    Ok(())
}

#[cfg(windows)]
fn unprotect_file_for_current_user(source: &Path, target: &Path) -> Result<(), AppError> {
    let mut input =
        File::open(source).map_err(|error| AppError::local_config(error.to_string(), true))?;
    let mut magic = [0_u8; 8];
    input
        .read_exact(&mut magic)
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    if &magic != PROTECTED_FILE_MAGIC {
        return Err(AppError::local_config("评测加密容器格式无效", false));
    }
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(target)
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    loop {
        let mut length_bytes = [0_u8; 4];
        input
            .read_exact(&mut length_bytes)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length == 0 {
            break;
        }
        if length > PROTECTED_CHUNK_SIZE + 128 * 1024 {
            return Err(AppError::local_config("评测加密分块长度无效", false));
        }
        let mut encrypted = vec![0_u8; length];
        input
            .read_exact(&mut encrypted)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        let decrypted = dpapi_unprotect(&mut encrypted)?;
        output
            .write_all(&decrypted)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
    }
    output
        .sync_all()
        .map_err(|error| AppError::local_config(error.to_string(), true))?;
    Ok(())
}

#[cfg(windows)]
fn dpapi_protect(data: &mut [u8]) -> Result<Vec<u8>, AppError> {
    use windows::{
        Win32::{
            Foundation::{HLOCAL, LocalFree},
            Security::Cryptography::{
                CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
            },
        },
        core::w,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            w!("FanFan local evaluation"),
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|error| AppError::local_config(format!("DPAPI评测加密失败: {error}"), false))?;
    let protected =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
    }
    Ok(protected)
}

#[cfg(windows)]
fn dpapi_unprotect(data: &mut [u8]) -> Result<Vec<u8>, AppError> {
    use windows::Win32::{
        Foundation::{HLOCAL, LocalFree},
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_mut_ptr(),
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    }
    .map_err(|error| AppError::local_config(format!("DPAPI评测解密失败: {error}"), false))?;
    let plain =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
    }
    Ok(plain)
}

#[cfg(not(windows))]
fn protect_file_for_current_user(_source: &Path, _target: &Path) -> Result<(), AppError> {
    Err(AppError::local_config("当前平台未实现DPAPI评测加密", false))
}

#[cfg(not(windows))]
fn unprotect_file_for_current_user(_source: &Path, _target: &Path) -> Result<(), AppError> {
    Err(AppError::local_config("当前平台未实现DPAPI评测解密", false))
}

fn file_size_and_sha256(path: &Path) -> Result<(u64, String), AppError> {
    let mut file =
        File::open(path).map_err(|error| AppError::local_config(error.to_string(), true))?;
    let size = file
        .metadata()
        .map_err(|error| AppError::local_config(error.to_string(), true))?
        .len();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok((size, format!("{:x}", hasher.finalize())))
}

#[cfg(windows)]
fn ensure_directory_encrypted(path: &Path) -> Result<(), AppError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{Win32::Storage::FileSystem::EncryptFileW, core::PCWSTR};

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe { EncryptFileW(PCWSTR(wide.as_ptr())) }
        .map_err(|error| AppError::local_config(format!("无法启用评测目录加密: {error}"), false))?;
    if !path_is_encrypted(path)? {
        return Err(AppError::local_config(
            "当前磁盘不支持Windows文件加密，已拒绝创建评测快照",
            false,
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn path_is_encrypted(path: &Path) -> Result<bool, AppError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_ENCRYPTED, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
        },
        core::PCWSTR,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let attributes = unsafe { GetFileAttributesW(PCWSTR(wide.as_ptr())) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(AppError::local_config(
            "无法读取评测快照的Windows文件属性",
            true,
        ));
    }
    Ok(attributes & FILE_ATTRIBUTE_ENCRYPTED.0 != 0)
}

#[cfg(not(windows))]
fn ensure_directory_encrypted(_path: &Path) -> Result<(), AppError> {
    Err(AppError::local_config(
        "当前平台尚未实现本地评测快照加密",
        false,
    ))
}

#[cfg(not(windows))]
fn path_is_encrypted(_path: &Path) -> Result<bool, AppError> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    #[test]
    fn search_metrics_use_top_ten_and_report_p95() {
        let cases = vec![
            SearchEvaluationCase {
                case_id: "first".into(),
                relevant_file_ids: vec![id(1)],
                returned_file_ids: vec![id(1), id(9)],
                elapsed_ms: 100,
            },
            SearchEvaluationCase {
                case_id: "second".into(),
                relevant_file_ids: vec![id(2)],
                returned_file_ids: vec![id(9), id(2)],
                elapsed_ms: 2_500,
            },
        ];
        let metrics = score_search_cases(&cases);
        assert_eq!(metrics.recall_at_10, 1.0);
        assert_eq!(metrics.mrr_at_10, 0.75);
        assert_eq!(metrics.p95_latency_ms, 2_500);
        assert!(metrics.ndcg_at_10 > 0.8);
        assert!(metrics.to_component_score().earned < 20.0);
    }

    #[test]
    fn hard_gate_failure_blocks_an_otherwise_high_score() {
        let scorecard = EvaluationScorecard::from_components(
            vec![EvaluationComponentScore {
                component: "all".into(),
                earned: 100.0,
                maximum: 100.0,
                sample_count: 1,
                metrics: HashMap::new(),
                failure_categories: Vec::new(),
            }],
            EvaluationSafetyGates {
                source_files_unchanged: true,
                authorized_scope_only: true,
                model_packages_complete: true,
                jobs_terminal_or_recoverable: true,
                index_key_mapping_consistent: true,
                generated_content_verified: false,
                logs_privacy_safe: true,
            },
        );
        assert_eq!(scorecard.score, 100.0);
        assert!(!scorecard.passed);
        assert_eq!(
            scorecard.safety_gates.failed_names(),
            vec!["generated_content_verified"]
        );
    }

    #[test]
    fn rag_metrics_keep_citation_and_refusal_hard_gates_visible() {
        let metrics = score_rag_cases(&[
            RagEvaluationCase {
                case_id: "answer".into(),
                expected_refusal: false,
                refused: false,
                generated: true,
                factual_claims: 2,
                verified_claims: 2,
                unauthorized_citations: 0,
                expected_source_cited: true,
                elapsed_ms: 2_000,
            },
            RagEvaluationCase {
                case_id: "negative".into(),
                expected_refusal: true,
                refused: true,
                generated: false,
                factual_claims: 0,
                verified_claims: 0,
                unauthorized_citations: 0,
                expected_source_cited: false,
                elapsed_ms: 300,
            },
        ]);
        assert_eq!(metrics.citation_coverage, 1.0);
        assert_eq!(metrics.factual_correctness, 1.0);
        assert_eq!(metrics.refusal_accuracy, 1.0);
        assert_eq!(metrics.unauthorized_rejection_rate, 1.0);
        assert_eq!(metrics.to_component_score().earned, 30.0);
    }

    #[test]
    fn log_privacy_detector_rejects_sensitive_keys_and_absolute_paths() {
        assert!(json_contains_sensitive_log_value(
            &serde_json::json!({"fields": {"question": "secret"}}),
            None
        ));
        assert!(json_contains_sensitive_log_value(
            &serde_json::json!({"fields": {"message": "C:\\Users\\name\\file.pdf"}}),
            None
        ));
        assert!(!json_contains_sensitive_log_value(
            &serde_json::json!({"fields": {"error_code": "PDF_RENDER_FAILED", "file_id": "id"}}),
            None
        ));
    }

    fn base_result(case_id: &str) -> EvaluationRunResult {
        EvaluationRunResult {
            case_id: case_id.into(),
            operation_id: format!("op-{case_id}"),
            question: "我的简历里有哪些项目？".into(),
            expected_source: Some("LOCAL".into()),
            expected_intent: Some("document_qa".into()),
            expected_file_ids: Some(vec!["file-1".into()]),
            expected_document_type: Some("resume".into()),
            expected_should_find_evidence: Some(true),
            actual_source: Some("LOCAL".into()),
            actual_intent: Some("document_qa".into()),
            actual_file_ids: vec!["file-1".into()],
            actual_document_type: Some("resume".into()),
            memory_used: false,
            clarification_used: false,
            retrieval_top_files: vec!["file-1".into()],
            rerank_top_files: vec!["file-1".into()],
            grounding_status: Some("grounded".into()),
            answer_mode: Some("generated".into()),
            evidence_found: true,
            answer_grounded: true,
            claim_count: 2,
            supported_claim_count: 2,
            latency_ms: 3_000,
            error_category: None,
            error_message: None,
            pass_fail: true,
            failed_fields: Vec::new(),
        }
    }

    #[test]
    fn evaluation_cases_parse_from_jsonl_and_json_array() {
        let jsonl = "# 注释行\n\n{\"id\": \"a\", \"question\": \"q1\", \"expected_source\": \"LOCAL\"}\n{\"id\": \"b\", \"question\": \"q2\", \"expected_file_ids\": [\"f1\"]}\n";
        let cases = parse_evaluation_cases(jsonl).unwrap();
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].id, "a");
        assert_eq!(cases[0].expected_source.as_deref(), Some("LOCAL"));
        assert_eq!(
            cases[1].expected_file_ids.as_deref(),
            Some(vec!["f1".to_owned()].as_slice())
        );

        let array = "[{\"id\": \"c\", \"question\": \"q3\"}]";
        let cases = parse_evaluation_cases(array).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].id, "c");

        let bad = "{\"id\": \"broken\"";
        let error = parse_evaluation_cases(bad).unwrap_err();
        assert!(error.message.contains("解析失败"), "{}", error.message);
        assert!(parse_evaluation_cases("").unwrap().is_empty());
    }

    #[test]
    fn verdict_flags_expected_field_mismatches() {
        let mut result = base_result("ok");
        assert!(verdict_for(&result).pass_fail);
        assert!(verdict_for(&result).failed_fields.is_empty());

        result.actual_source = Some("GENERAL".into());
        result.actual_intent = Some("general_chat".into());
        result.actual_file_ids = vec!["file-2".into()];
        result.actual_document_type = Some("contract".into());
        result.evidence_found = false;
        result.answer_grounded = false;
        let verdict = verdict_for(&result);
        assert!(!verdict.pass_fail);
        assert_eq!(
            verdict.failed_fields,
            vec![
                "source_correct",
                "intent_correct",
                "target_correct",
                "file_correct",
                "evidence_found",
                "answer_grounded"
            ]
        );

        // expected_should_find_evidence = false：找到证据反而失败
        let mut result = base_result("neg");
        result.expected_should_find_evidence = Some(false);
        result.evidence_found = true;
        assert!(!verdict_for(&result).pass_fail);
        // 预期拒绝且确实拒绝 → 通过
        result.evidence_found = false;
        result.answer_grounded = false;
        assert!(verdict_for(&result).pass_fail);
    }

    #[test]
    fn error_classification_priority_node_over_code_over_no_evidence() {
        // 节点失败优先
        assert_eq!(
            classify_error(
                &["generation"],
                Some("RAG_GENERATION_MODEL_REQUIRED"),
                Some("generated"),
                false,
                Some(true),
                false,
            ),
            Some(EvaluationErrorCategory::GenerationError)
        );
        assert_eq!(
            classify_error(&["source_routing"], None, None, false, None, false),
            Some(EvaluationErrorCategory::RouterError)
        );
        assert_eq!(
            classify_error(&["document_resolution"], None, None, false, None, false),
            Some(EvaluationErrorCategory::DocumentResolutionError)
        );
        assert_eq!(
            classify_error(&["unknown_node"], None, None, false, None, false),
            None
        );
        // 运行错误码
        assert_eq!(
            classify_error(
                &[],
                Some("CLARIFICATION_SELECTION_INVALID"),
                None,
                false,
                None,
                false,
            ),
            Some(EvaluationErrorCategory::ClarificationError)
        );
        assert_eq!(
            classify_error(&[], Some("GENERATION_ACTIVATION_FAILED"), None, false, None, false),
            Some(EvaluationErrorCategory::GenerationError)
        );
        // 未知运行错误码 → Unknown 兜底（第 13 类）
        assert_eq!(
            classify_error(&[], Some("OTHER"), None, false, None, false),
            Some(EvaluationErrorCategory::Unknown)
        );
        // NO_EVIDENCE
        assert_eq!(
            classify_error(&[], None, Some("rag_refusal"), true, Some(true), false),
            Some(EvaluationErrorCategory::NoEvidenceError)
        );
        assert_eq!(
            classify_error(&[], None, Some("rag_refusal"), true, Some(false), false),
            None
        );
        assert_eq!(
            classify_error(&[], None, Some("rag_refusal"), true, None, false),
            Some(EvaluationErrorCategory::NoEvidenceError)
        );
        // 引用核验失败
        assert_eq!(
            classify_error(&[], None, Some("generated"), false, None, true),
            Some(EvaluationErrorCategory::CitationError)
        );
        assert_eq!(classify_error(&[], None, Some("generated"), false, None, false), None);
    }

    #[test]
    fn metrics_aggregate_denominators_correctly() {
        let mut good = base_result("good");
        good.latency_ms = 2_000;
        let mut wrong_source = base_result("wrong-source");
        wrong_source.actual_source = Some("GENERAL".into());
        wrong_source.actual_intent = Some("general_chat".into());
        wrong_source.pass_fail = false;
        wrong_source.failed_fields = vec!["source_correct".into(), "intent_correct".into()];
        wrong_source.latency_ms = 8_000;
        let mut no_evidence = base_result("no-evidence");
        no_evidence.expected_should_find_evidence = Some(true);
        no_evidence.evidence_found = false;
        no_evidence.answer_grounded = false;
        no_evidence.grounding_status = Some("insufficient".into());
        no_evidence.actual_file_ids = Vec::new();
        no_evidence.retrieval_top_files = Vec::new();
        no_evidence.rerank_top_files = Vec::new();
        no_evidence.claim_count = 0;
        no_evidence.supported_claim_count = 0;
        no_evidence.latency_ms = 1_000;
        no_evidence.pass_fail = false;
        no_evidence.failed_fields = vec!["evidence_found".into(), "answer_grounded".into()];
        let mut chat = base_result("chat");
        chat.expected_source = Some("GENERAL".into());
        chat.actual_source = Some("GENERAL".into());
        chat.actual_intent = Some("general_chat".into());
        chat.expected_intent = Some("general_chat".into());
        chat.expected_file_ids = None;
        chat.expected_document_type = None;
        chat.expected_should_find_evidence = None;
        chat.evidence_found = false;
        chat.answer_grounded = false;
        chat.actual_file_ids = Vec::new();
        chat.claim_count = 0;
        chat.supported_claim_count = 0;
        chat.latency_ms = 500;

        let metrics = compute_metrics(&[good, wrong_source, no_evidence, chat]);
        assert_eq!(metrics.total, 4);
        assert_eq!(metrics.passed, 2);
        assert_eq!(metrics.source_router_accuracy, 0.75); // 4 个有 expected_source，3 对
        assert_eq!(metrics.intent_accuracy, 0.75); // 4 个有 expected_intent，3 对
        assert_eq!(metrics.document_resolution_top1_accuracy, 2.0 / 3.0);
        assert_eq!(metrics.document_resolution_top3_recall, 2.0 / 3.0);
        assert_eq!(metrics.retrieval_evidence_recall, 2.0 / 3.0);
        assert_eq!(metrics.no_evidence_false_negative_rate, 1.0 / 3.0);
        assert_eq!(metrics.grounded_answer_rate, 0.5);
        assert_eq!(metrics.citation_pass_rate, 2.0 / 2.0);
        assert_eq!(metrics.clarification_rate, 0.0);
        assert_eq!(metrics.clarification_success_rate, 0.0);
        assert_eq!(metrics.memory_hit_accuracy, 0.0);
        assert_eq!(metrics.memory_wrong_hit_rate, 0.0);
        assert_eq!(metrics.avg_total_ms, (2000 + 8000 + 1000 + 500) as f64 / 4.0);
        assert_eq!(metrics.p50_total_ms, 2_000); // 500,1000,2000,8000 → 中位 = 2000
        assert_eq!(metrics.p95_total_ms, 8_000); // ceil(4*0.95)=4 → 最大
    }
}
