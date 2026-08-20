use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AppError, EvaluationBatchManifestV1, EvaluationCaseRecord, EvaluationResultRecord,
    RagasEvaluationSampleV1,
};

pub const EVALUATION_SCHEMA_VERSION: u32 = 2;
pub const EVALUATION_PASS_SCORE: f64 = 85.0;
/// 一次备份所有剩余页，在复制期间持有读锁，避免活跃源库让 online backup 不断重启。
/// 这会短暂阻塞写入，但不需要关闭桌面应用或 Worker。
const SNAPSHOT_BACKUP_STEP_PAGES: i32 = -1;
/// 连续遇到 SQLite BUSY/LOCKED 的步数上限（每步等待 10ms）。
const SNAPSHOT_BACKUP_MAX_LOCKED_STEPS: u32 = 3_000;
/// 活跃源库可能让 backup 重启，以总时间上限避免永久不收敛。
const SNAPSHOT_BACKUP_MAX_SECONDS: u64 = 15 * 60;

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
        let hit = expected.iter().any(|file_id| {
            result
                .actual_file_ids
                .iter()
                .any(|actual| actual == file_id)
        });
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
        Some(false) if result.evidence_found => failed.push("evidence_found".to_owned()),
        Some(false) | None => {}
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
        return (expected_should_find_evidence != Some(false))
            .then_some(EvaluationErrorCategory::NoEvidenceError);
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
    let p50 = latencies.get(latencies.len() / 2).copied().unwrap_or(0);
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
            expected.iter().any(|file_id| {
                result
                    .actual_file_ids
                    .iter()
                    .any(|actual| actual == file_id)
            })
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
            results
                .iter()
                .filter(|result| top_files_hit(result, 1))
                .count(),
            file_denominator,
        ),
        document_resolution_top3_recall: fraction(
            results
                .iter()
                .filter(|result| top_files_hit(result, 3))
                .count(),
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
            results
                .iter()
                .filter(|result| result.answer_grounded)
                .count(),
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
        // 目标文件可能在首步就预分配到最终大小，不能用文件大小判断进度。
        // 单步备份在读事务中完成所有页，使活跃源库的持续写入不会让备份反复归零。
        let mut destination = Connection::open(&working_path)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        let backup = Backup::new(&source, &mut destination)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        let started_at = std::time::Instant::now();
        let mut locked_steps = 0_u32;
        loop {
            let step_result = backup
                .step(SNAPSHOT_BACKUP_STEP_PAGES)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
            if step_result == StepResult::Done {
                break;
            }
            let progress = backup.progress();
            if matches!(step_result, StepResult::Busy | StepResult::Locked) {
                locked_steps += 1;
                if locked_steps >= SNAPSHOT_BACKUP_MAX_LOCKED_STEPS {
                    return Err(AppError::new(
                        "EVALUATION_SNAPSHOT_BUSY",
                        "评测快照长时无法获取源数据库读锁，请暂停高频写入任务后重试",
                        true,
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            } else {
                locked_steps = 0;
            }
            if started_at.elapsed() >= std::time::Duration::from_secs(SNAPSHOT_BACKUP_MAX_SECONDS) {
                return Err(AppError::new(
                    "EVALUATION_SNAPSHOT_BUSY",
                    format!(
                        "评测快照在时限内未收敛（剩余页 {}/{}），请暂停高频写入任务后重试",
                        progress.remaining, progress.pagecount
                    ),
                    true,
                ));
            }
        }
        drop(backup);
        drop(destination);
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

// ===== 优化闭环：统一错误分类（28 类）=====
//
// 与 docs/fanfan_trace_evaluation_optimization_agent_prompt.txt 第十三节
// 的错误分类契约一致。用于「真实资料评测 → Failure Analysis → 优化」闭环，
// 与既有 EvaluationErrorCategory（Ask Runner 13 类）互补：前者面向逐例
// 人工复核，这里面向按类型聚合的共性根因诊断。

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OptimizationErrorCategory {
    SourceRouterError,
    QueryNormalizationError,
    QueryParseError,
    ContextError,
    MemoryError,
    DocumentProfileError,
    DocumentResolutionError,
    ScopeError,
    DocumentRecallError,
    FtsRetrievalError,
    SemanticRetrievalError,
    FusionError,
    RerankError,
    EvidenceSelectionError,
    AnswerabilityError,
    GenerationError,
    ClaimVerificationError,
    CitationError,
    SummaryError,
    ExtractError,
    CompareError,
    SearchRankingError,
    SmartCollectionError,
    FileRelationError,
    RuntimeError,
    ModelError,
    Timeout,
    Unknown,
}

impl OptimizationErrorCategory {
    /// 全部 28 类（SCREAMING_SNAKE_CASE 字符串）。
    pub const ALL: [&'static str; 28] = [
        "SOURCE_ROUTER_ERROR",
        "QUERY_NORMALIZATION_ERROR",
        "QUERY_PARSE_ERROR",
        "CONTEXT_ERROR",
        "MEMORY_ERROR",
        "DOCUMENT_PROFILE_ERROR",
        "DOCUMENT_RESOLUTION_ERROR",
        "SCOPE_ERROR",
        "DOCUMENT_RECALL_ERROR",
        "FTS_RETRIEVAL_ERROR",
        "SEMANTIC_RETRIEVAL_ERROR",
        "FUSION_ERROR",
        "RERANK_ERROR",
        "EVIDENCE_SELECTION_ERROR",
        "ANSWERABILITY_ERROR",
        "GENERATION_ERROR",
        "CLAIM_VERIFICATION_ERROR",
        "CITATION_ERROR",
        "SUMMARY_ERROR",
        "EXTRACT_ERROR",
        "COMPARE_ERROR",
        "SEARCH_RANKING_ERROR",
        "SMART_COLLECTION_ERROR",
        "FILE_RELATION_ERROR",
        "RUNTIME_ERROR",
        "MODEL_ERROR",
        "TIMEOUT",
        "UNKNOWN",
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SourceRouterError => "SOURCE_ROUTER_ERROR",
            Self::QueryNormalizationError => "QUERY_NORMALIZATION_ERROR",
            Self::QueryParseError => "QUERY_PARSE_ERROR",
            Self::ContextError => "CONTEXT_ERROR",
            Self::MemoryError => "MEMORY_ERROR",
            Self::DocumentProfileError => "DOCUMENT_PROFILE_ERROR",
            Self::DocumentResolutionError => "DOCUMENT_RESOLUTION_ERROR",
            Self::ScopeError => "SCOPE_ERROR",
            Self::DocumentRecallError => "DOCUMENT_RECALL_ERROR",
            Self::FtsRetrievalError => "FTS_RETRIEVAL_ERROR",
            Self::SemanticRetrievalError => "SEMANTIC_RETRIEVAL_ERROR",
            Self::FusionError => "FUSION_ERROR",
            Self::RerankError => "RERANK_ERROR",
            Self::EvidenceSelectionError => "EVIDENCE_SELECTION_ERROR",
            Self::AnswerabilityError => "ANSWERABILITY_ERROR",
            Self::GenerationError => "GENERATION_ERROR",
            Self::ClaimVerificationError => "CLAIM_VERIFICATION_ERROR",
            Self::CitationError => "CITATION_ERROR",
            Self::SummaryError => "SUMMARY_ERROR",
            Self::ExtractError => "EXTRACT_ERROR",
            Self::CompareError => "COMPARE_ERROR",
            Self::SearchRankingError => "SEARCH_RANKING_ERROR",
            Self::SmartCollectionError => "SMART_COLLECTION_ERROR",
            Self::FileRelationError => "FILE_RELATION_ERROR",
            Self::RuntimeError => "RUNTIME_ERROR",
            Self::ModelError => "MODEL_ERROR",
            Self::Timeout => "TIMEOUT",
            Self::Unknown => "UNKNOWN",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .position(|candidate| *candidate == value)
            .map(|index| ALL_CATEGORIES[index])
    }

    /// 运行级错误码 → 28 类分类（子串宽匹配已知错误家族，与具体 Case 无关）。
    pub fn from_error_code(code: &str) -> Self {
        let upper = code.to_ascii_uppercase();
        if upper.contains("TIMEOUT") {
            return Self::Timeout;
        }
        if upper.contains("MODEL") || upper.contains("LLAMA") {
            return Self::ModelError;
        }
        if upper.contains("EMBEDDING") || upper.contains("SEMANTIC") || upper.contains("VECTOR") {
            return Self::SemanticRetrievalError;
        }
        if upper.contains("FTS") {
            return Self::FtsRetrievalError;
        }
        if upper.contains("RERANK") {
            return Self::RerankError;
        }
        if upper.contains("GENERATION") || upper.contains("PROMPT") {
            return Self::GenerationError;
        }
        if upper.contains("RESOLV") {
            return Self::DocumentResolutionError;
        }
        if upper.contains("DOCUMENT") || upper.contains("RECALL") {
            return Self::DocumentRecallError;
        }
        if upper.contains("WORKER") || upper.contains("RUNTIME") {
            return Self::RuntimeError;
        }
        if upper.contains("QUERY") {
            return Self::QueryParseError;
        }
        Self::Unknown
    }
}

const ALL_CATEGORIES: [OptimizationErrorCategory; 28] = [
    OptimizationErrorCategory::SourceRouterError,
    OptimizationErrorCategory::QueryNormalizationError,
    OptimizationErrorCategory::QueryParseError,
    OptimizationErrorCategory::ContextError,
    OptimizationErrorCategory::MemoryError,
    OptimizationErrorCategory::DocumentProfileError,
    OptimizationErrorCategory::DocumentResolutionError,
    OptimizationErrorCategory::ScopeError,
    OptimizationErrorCategory::DocumentRecallError,
    OptimizationErrorCategory::FtsRetrievalError,
    OptimizationErrorCategory::SemanticRetrievalError,
    OptimizationErrorCategory::FusionError,
    OptimizationErrorCategory::RerankError,
    OptimizationErrorCategory::EvidenceSelectionError,
    OptimizationErrorCategory::AnswerabilityError,
    OptimizationErrorCategory::GenerationError,
    OptimizationErrorCategory::ClaimVerificationError,
    OptimizationErrorCategory::CitationError,
    OptimizationErrorCategory::SummaryError,
    OptimizationErrorCategory::ExtractError,
    OptimizationErrorCategory::CompareError,
    OptimizationErrorCategory::SearchRankingError,
    OptimizationErrorCategory::SmartCollectionError,
    OptimizationErrorCategory::FileRelationError,
    OptimizationErrorCategory::RuntimeError,
    OptimizationErrorCategory::ModelError,
    OptimizationErrorCategory::Timeout,
    OptimizationErrorCategory::Unknown,
];

// ===== 优化闭环：Failure Analysis =====

/// 某一错误分类的聚合统计。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CategoryFailureStat {
    pub category: OptimizationErrorCategory,
    pub count: usize,
    pub case_ids: Vec<String>,
    /// 该分类下最多 3 条诊断样例，用于人工核对根因表述。
    pub diagnosis_samples: Vec<String>,
}

/// 一批评测结果的失败聚合分析（按错误类型统计共性根因）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationFailureAnalysis {
    pub total_evaluated: usize,
    pub total_failed: usize,
    pub failure_rate: f64,
    pub by_category: Vec<CategoryFailureStat>,
    /// 影响 Case 最多的 1~3 个共性根因表述（禁止逐题特判的依据）。
    pub top_root_causes: Vec<String>,
}

/// 聚合一批评测结果的失败类型（纯逻辑）。
pub fn analyze_failures(results: &[EvaluationResultRecord]) -> EvaluationFailureAnalysis {
    let total_evaluated = results.len();
    let failed = results
        .iter()
        .filter(|result| !result.pass_fail)
        .collect::<Vec<_>>();
    let mut buckets =
        std::collections::BTreeMap::<OptimizationErrorCategory, Vec<&EvaluationResultRecord>>::new(
        );
    for result in &failed {
        let category = result
            .error_category
            .as_deref()
            .and_then(OptimizationErrorCategory::parse)
            .unwrap_or(OptimizationErrorCategory::Unknown);
        buckets.entry(category).or_default().push(result);
    }
    let by_category = buckets
        .into_iter()
        .map(|(category, records)| {
            let case_ids = records
                .iter()
                .map(|record| record.case_id.clone())
                .collect::<Vec<_>>();
            let diagnosis_samples = records
                .iter()
                .filter_map(|record| record.diagnosis_reason.clone())
                .take(3)
                .collect::<Vec<_>>();
            CategoryFailureStat {
                category,
                count: records.len(),
                case_ids,
                diagnosis_samples,
            }
        })
        .collect::<Vec<_>>();
    let top_root_causes = by_category
        .iter()
        .take(3)
        .map(|stat| {
            format!(
                "{}（{} 例 / {}）",
                stat.category.as_str(),
                stat.count,
                percent(failed.len(), total_evaluated)
            )
        })
        .collect::<Vec<_>>();
    EvaluationFailureAnalysis {
        total_evaluated,
        total_failed: failed.len(),
        failure_rate: percent(failed.len(), total_evaluated),
        by_category,
        top_root_causes,
    }
}

fn percent(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

/// 一轮优化的假设（修改前必须填写，文档第十六节契约）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OptimizationHypothesis {
    pub round: u32,
    pub problem: String,
    /// 统计证据：基于一批 DEV Case 的聚合结果，不允许为了单个 Case 调参。
    pub statistical_evidence: String,
    pub hypothesis: String,
    pub planned_changes: Vec<String>,
    pub expected_impact: String,
    pub regression_risk: String,
}

// ===== 优化闭环：评测指标聚合 =====

/// 对一批逐例结果按 feature_type 聚合核心指标（纯逻辑，返回可落库 JSON）。
///
/// 各 case 的 metrics_json 约定：
/// - SEARCH：`actual_file_ranks`（命中文件的排名，0 起）、`top_files`、
///   `evidence_found`。
/// - ASK：`evidence_found`、`answer_grounded`、`clarification_used`、
///   `claim_count`、`supported_claim_count`。
/// - SMART_COLLECTION：`collection_precision`、`collection_recall`。
/// - FILE_RELATION：`relation_predicted`、`relation_type_match`。
pub fn aggregate_evaluation_metrics(results: &[EvaluationResultRecord]) -> serde_json::Value {
    let mut output = serde_json::Map::new();
    let mut latencies = results
        .iter()
        .filter_map(|result| result.latency_ms)
        .collect::<Vec<_>>();
    latencies.sort_unstable();
    let p50 = latencies.get(latencies.len() / 2).copied().unwrap_or(0);
    let p95_index = ((latencies.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(latencies.len().saturating_sub(1));
    let p95 = latencies.get(p95_index).copied().unwrap_or(0);
    let overall_passed = results.iter().filter(|result| result.pass_fail).count();
    output.insert(
        "overall_pass_rate".into(),
        serde_json::Value::from(percent(overall_passed, results.len()) / 100.0),
    );
    output.insert("total_cases".into(), serde_json::Value::from(results.len()));
    output.insert(
        "passed_cases".into(),
        serde_json::Value::from(overall_passed),
    );
    output.insert("latency_p50_ms".into(), serde_json::Value::from(p50));
    output.insert("latency_p95_ms".into(), serde_json::Value::from(p95));

    let search = results
        .iter()
        .filter(|result| result.feature_type() == Some("SEARCH"))
        .collect::<Vec<_>>();
    if !search.is_empty() {
        output.insert("search".into(), aggregate_search_metrics(&search));
    }
    let ask = results
        .iter()
        .filter(|result| result.feature_type() == Some("ASK"))
        .collect::<Vec<_>>();
    if !ask.is_empty() {
        output.insert("ask".into(), aggregate_ask_metrics(&ask));
    }
    let collection = results
        .iter()
        .filter(|result| result.feature_type() == Some("SMART_COLLECTION"))
        .collect::<Vec<_>>();
    if !collection.is_empty() {
        output.insert(
            "smart_collection".into(),
            aggregate_collection_metrics(&collection),
        );
    }
    let relation = results
        .iter()
        .filter(|result| result.feature_type() == Some("FILE_RELATION"))
        .collect::<Vec<_>>();
    if !relation.is_empty() {
        output.insert(
            "file_relation".into(),
            aggregate_relation_metrics(&relation),
        );
    }
    if let Some(ragas) = aggregate_ragas_metrics(results) {
        output.insert("ragas".into(), ragas);
    }
    serde_json::Value::Object(output)
}

fn aggregate_ragas_metrics(results: &[EvaluationResultRecord]) -> Option<serde_json::Value> {
    const METRICS: [&str; 5] = [
        "faithfulness",
        "answer_relevancy",
        "context_precision_with_reference",
        "context_recall",
        "factual_correctness",
    ];
    let mut output = serde_json::Map::new();
    let mut scored_cases = HashSet::new();
    for metric in METRICS {
        let values = results
            .iter()
            .filter_map(|result| {
                let value = result.metrics.get("ragas")?.get(metric)?.as_f64()?;
                value
                    .is_finite()
                    .then_some((result.case_id.as_str(), value))
            })
            .collect::<Vec<_>>();
        if values.is_empty() {
            continue;
        }
        scored_cases.extend(values.iter().map(|(case_id, _)| (*case_id).to_owned()));
        let average = values.iter().map(|(_, value)| value).sum::<f64>() / values.len() as f64;
        output.insert(metric.into(), serde_json::Value::from(average));
        output.insert(
            format!("{metric}_case_count"),
            serde_json::Value::from(values.len()),
        );
    }
    if output.is_empty() {
        return None;
    }
    output.insert(
        "scored_case_count".into(),
        serde_json::Value::from(scored_cases.len()),
    );
    Some(serde_json::Value::Object(output))
}

/// EvaluationResultRecord 的 feature_type 读取：优先来自 metrics 内嵌，否则由
/// case_id 前缀约定推导。执行器写入时应在 metrics_json 记录 feature_type。
impl EvaluationResultRecord {
    pub(crate) fn feature_type(&self) -> Option<&str> {
        self.metrics
            .get("feature_type")
            .and_then(serde_json::Value::as_str)
    }
}

fn metric_fraction(metrics: &serde_json::Map<String, serde_json::Value>, key: &str) -> f64 {
    metrics
        .get(key)
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
}

fn metric_u64(metrics: &serde_json::Map<String, serde_json::Value>, key: &str) -> u64 {
    metrics
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn aggregate_search_metrics(results: &[&EvaluationResultRecord]) -> serde_json::Value {
    let mut output = serde_json::Map::new();
    let mut top1_hits = 0_usize;
    let mut top3_hits = 0_usize;
    let mut top5_hits = 0_usize;
    let mut mrr_sum = 0.0;
    for result in results {
        let metrics = result.metrics.as_object().cloned().unwrap_or_default();
        let ranks = metrics
            .get("actual_file_ranks")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_u64)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let best_rank = ranks.iter().min().copied();
        match best_rank {
            Some(rank) if rank < 1 => top1_hits += 1,
            _ => {}
        }
        if best_rank.is_some_and(|rank| rank < 3) {
            top3_hits += 1;
        }
        if best_rank.is_some_and(|rank| rank < 5) {
            top5_hits += 1;
        }
        if let Some(rank) = best_rank {
            mrr_sum += 1.0 / (rank + 1) as f64;
        }
    }
    let count = results.len();
    output.insert(
        "document_top1_accuracy".into(),
        serde_json::Value::from(fraction(top1_hits, count)),
    );
    output.insert(
        "document_top3_recall".into(),
        serde_json::Value::from(fraction(top3_hits, count)),
    );
    output.insert(
        "document_top5_recall".into(),
        serde_json::Value::from(fraction(top5_hits, count)),
    );
    output.insert(
        "mrr".into(),
        serde_json::Value::from(fraction_mrr(mrr_sum, count)),
    );
    output.insert("case_count".into(), serde_json::Value::from(count));
    serde_json::Value::Object(output)
}

fn aggregate_ask_metrics(results: &[&EvaluationResultRecord]) -> serde_json::Value {
    let mut output = serde_json::Map::new();
    let source_denominator = results
        .iter()
        .filter(|result| {
            result.actual_source.is_some()
                && metric_expected_str(&result.metrics, "expected_source").is_some()
        })
        .count();
    let source_hits = results
        .iter()
        .filter(|result| {
            metric_expected_str(&result.metrics, "expected_source")
                .is_some_and(|expected| result.actual_source.as_deref() == Some(expected.as_str()))
        })
        .count();
    let evidence_denominator = results
        .iter()
        .filter(|result| {
            metric_u64(
                &result.metrics.as_object().cloned().unwrap_or_default(),
                "expects_evidence",
            ) == 1
        })
        .count();
    let evidence_hits = results
        .iter()
        .filter(|result| {
            let metrics = result.metrics.as_object().cloned().unwrap_or_default();
            metric_u64(&metrics, "expects_evidence") == 1
                && metric_fraction(&metrics, "evidence_found") > 0.0
        })
        .count();
    let grounded_denominator = results
        .iter()
        .filter(|result| {
            metric_u64(
                &result.metrics.as_object().cloned().unwrap_or_default(),
                "expects_evidence",
            ) == 1
        })
        .count();
    let grounded_hits = results
        .iter()
        .filter(|result| {
            let metrics = result.metrics.as_object().cloned().unwrap_or_default();
            metric_u64(&metrics, "expects_evidence") == 1
                && metric_fraction(&metrics, "answer_grounded") > 0.0
        })
        .count();
    output.insert(
        "source_router_accuracy".into(),
        serde_json::Value::from(fraction(source_hits, source_denominator)),
    );
    output.insert(
        "evidence_recall".into(),
        serde_json::Value::from(fraction(evidence_hits, evidence_denominator)),
    );
    output.insert(
        "grounded_answer_rate".into(),
        serde_json::Value::from(fraction(grounded_hits, grounded_denominator)),
    );
    output.insert("case_count".into(), serde_json::Value::from(results.len()));
    serde_json::Value::Object(output)
}

fn aggregate_collection_metrics(results: &[&EvaluationResultRecord]) -> serde_json::Value {
    let mut output = serde_json::Map::new();
    let precision = results
        .iter()
        .map(|result| {
            let metrics = result.metrics.as_object().cloned().unwrap_or_default();
            metric_fraction(&metrics, "collection_precision")
        })
        .sum::<f64>()
        / results.len() as f64;
    let recall = results
        .iter()
        .map(|result| {
            let metrics = result.metrics.as_object().cloned().unwrap_or_default();
            metric_fraction(&metrics, "collection_recall")
        })
        .sum::<f64>()
        / results.len() as f64;
    output.insert("precision".into(), serde_json::Value::from(precision));
    output.insert("recall".into(), serde_json::Value::from(recall));
    output.insert("case_count".into(), serde_json::Value::from(results.len()));
    serde_json::Value::Object(output)
}

fn aggregate_relation_metrics(results: &[&EvaluationResultRecord]) -> serde_json::Value {
    let mut output = serde_json::Map::new();
    let expected_relation = |result: &&EvaluationResultRecord| {
        metric_expected_str(&result.metrics, "expected_relation_type")
    };
    let true_positive = results
        .iter()
        .filter(|result| {
            expected_relation(result).is_some()
                && metric_fraction(
                    &result.metrics.as_object().cloned().unwrap_or_default(),
                    "relation_predicted",
                ) > 0.0
        })
        .count();
    let predicted_positive = results
        .iter()
        .filter(|result| {
            metric_fraction(
                &result.metrics.as_object().cloned().unwrap_or_default(),
                "relation_predicted",
            ) > 0.0
        })
        .count();
    let actual_positive = results
        .iter()
        .filter(|result| expected_relation(result).is_some())
        .count();
    let type_accurate = results
        .iter()
        .filter(|result| {
            let metrics = result.metrics.as_object().cloned().unwrap_or_default();
            expected_relation(result).is_some()
                && metric_fraction(&metrics, "relation_predicted") > 0.0
                && metric_fraction(&metrics, "relation_type_match") > 0.0
        })
        .count();
    output.insert(
        "precision".into(),
        serde_json::Value::from(fraction(true_positive, predicted_positive)),
    );
    output.insert(
        "recall".into(),
        serde_json::Value::from(fraction(true_positive, actual_positive)),
    );
    output.insert(
        "relation_type_accuracy".into(),
        serde_json::Value::from(fraction(type_accurate, true_positive)),
    );
    output.insert("case_count".into(), serde_json::Value::from(results.len()));
    serde_json::Value::Object(output)
}

/// 从 result.metrics_json 快照读取 expected 字符串字段（如 expected_source）。
fn metric_expected_str(metrics: &serde_json::Value, key: &str) -> Option<String> {
    metrics
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn fraction_mrr(sum: f64, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        sum / denominator as f64
    }
}

// ===== 优化闭环：Evidence-first 数据集生成器 =====
//
// 只从真实解析文档采样：文档 → Section/Chunk → 先确定 Gold Evidence →
// 根据 Evidence 生成问题（文档第一节约束）。生成器只依赖结构化的文档
// 语料，不依赖具体文件名 / 关键词 / 内容做特判。

/// 生成器使用的单个文档证据输入（由 example 从 CatalogStore 预取）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationCorpusFile {
    pub file_id: String,
    pub display_name: String,
    pub document_type: Option<String>,
    pub title: String,
    pub summary: String,
    pub keywords: Vec<String>,
    pub entities: Vec<String>,
    pub section_titles: Vec<String>,
    /// 文档主要文本块（证据候选；来自 preview / chunk 抽取）。
    pub text_chunks: Vec<String>,
    pub content_sha256: Option<String>,
    pub modified_at: Option<DateTime<Utc>>,
}

impl EvaluationCorpusFile {
    /// 全文是否包含给定 token（用于可验证证据的 Gold 判定）。
    fn contains(&self, token: &str) -> bool {
        self.text_chunks.iter().any(|chunk| chunk.contains(token))
    }
}

/// 数据集生成选项（每类用例上限，避免小语料下用例爆炸）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DatasetGenerationOptions {
    pub dataset_version: String,
    pub max_search_cases: usize,
    pub max_ask_cases: usize,
    pub max_collection_cases: usize,
    pub max_relation_cases: usize,
}

impl Default for DatasetGenerationOptions {
    fn default() -> Self {
        Self {
            dataset_version: "v1".into(),
            max_search_cases: 60,
            max_ask_cases: 80,
            max_collection_cases: 20,
            max_relation_cases: 30,
        }
    }
}

/// 从真实文档语料生成评测数据集（Evidence-first；split 尚未分配）。
pub fn generate_evaluation_dataset(
    files: &[EvaluationCorpusFile],
    options: &DatasetGenerationOptions,
) -> Result<Vec<EvaluationCaseRecord>, AppError> {
    if files.is_empty() {
        return Err(AppError::new(
            "EVALUATION_DATASET_EMPTY",
            "没有可用的真实解析文档，无法生成评测数据集",
            false,
        ));
    }
    let dataset_version = options.dataset_version.trim();
    if dataset_version.is_empty() {
        return Err(AppError::new(
            "EVALUATION_DATASET_VERSION_REQUIRED",
            "评测数据集版本不能为空",
            false,
        ));
    }
    let mut cases = Vec::new();
    cases.extend(generate_search_cases(files, options));
    cases.extend(generate_ask_cases(files, options));
    cases.extend(generate_collection_cases(files, options));
    cases.extend(generate_relation_cases(files, options));
    cases.extend(generate_no_evidence_cases(files, options));
    for case in &mut cases {
        case.dataset_version = dataset_version.to_owned();
    }
    Ok(cases)
}

/// 按 file_id 划分 DEV/HOLDOUT（同文件所有 case 落在同一分组，防止信息泄漏）。
/// dev_ratio 默认 0.7；FNV-1a 确定性哈希保证同数据集多次运行划分稳定（冻结）。
pub fn split_evaluation_dataset_by_file(
    mut cases: Vec<EvaluationCaseRecord>,
    dev_ratio: f64,
) -> (Vec<EvaluationCaseRecord>, Vec<EvaluationCaseRecord>) {
    let dev_ratio = dev_ratio.clamp(0.0, 1.0);
    let mut buckets = std::collections::HashMap::<String, Vec<EvaluationCaseRecord>>::new();
    for case in cases.drain(..) {
        let key = case
            .expected_file_ids
            .as_ref()
            .and_then(|ids| ids.first())
            .cloned()
            .unwrap_or_else(|| case.case_id.clone());
        buckets.entry(key).or_default().push(case);
    }
    let mut dev = Vec::new();
    let mut holdout = Vec::new();
    for (key, mut bucket) in buckets {
        bucket.sort_by(|left, right| left.case_id.cmp(&right.case_id));
        let threshold = (dev_ratio * u32::MAX as f64) as u32;
        if fnv1a_hash(key.as_bytes()) < threshold {
            for case in &mut bucket {
                case.split = "DEV".to_owned();
            }
            dev.append(&mut bucket);
        } else {
            for case in &mut bucket {
                case.split = "HOLDOUT".to_owned();
            }
            holdout.append(&mut bucket);
        }
    }
    dev.sort_by(|left, right| left.case_id.cmp(&right.case_id));
    holdout.sort_by(|left, right| left.case_id.cmp(&right.case_id));
    (dev, holdout)
}

/// 在看到任何评分前一次性冻结 RAGAS 正向 ASK 批次。每个源文件最多进入一个批次，
/// 选择仅依赖 dataset_version、split、intent 与稳定 case_id，禁止按正文或得分挑题。
pub fn freeze_ragas_evaluation_batches(
    dev: &[EvaluationCaseRecord],
    holdout: &[EvaluationCaseRecord],
    dataset_version: &str,
    batch_size: usize,
    dev_batch_count: usize,
) -> Result<Vec<EvaluationBatchManifestV1>, AppError> {
    if dataset_version.trim().is_empty() || batch_size == 0 || dev_batch_count == 0 {
        return Err(AppError::new(
            "EVALUATION_BATCH_INVALID",
            "评测批次需要非空数据集版本、正数批次大小和 DEV 批次数",
            false,
        ));
    }
    let mut used_files = HashSet::new();
    let mut manifests = Vec::with_capacity(dev_batch_count + 1);
    for batch_index in 0..dev_batch_count {
        manifests.push(select_ragas_batch(
            dev,
            dataset_version,
            "DEV",
            batch_index,
            batch_size,
            &mut used_files,
        )?);
    }
    manifests.push(select_ragas_batch(
        holdout,
        dataset_version,
        "HOLDOUT",
        0,
        batch_size,
        &mut used_files,
    )?);
    Ok(manifests)
}

fn select_ragas_batch(
    cases: &[EvaluationCaseRecord],
    dataset_version: &str,
    split: &str,
    batch_index: usize,
    batch_size: usize,
    used_files: &mut HashSet<String>,
) -> Result<EvaluationBatchManifestV1, AppError> {
    let mut by_intent = BTreeMap::<String, Vec<&EvaluationCaseRecord>>::new();
    for case in cases.iter().filter(|case| positive_ragas_case(case)) {
        let file_ids = case.expected_file_ids.as_deref().unwrap_or_default();
        if file_ids.iter().any(|file_id| used_files.contains(file_id)) {
            continue;
        }
        by_intent
            .entry(
                case.expected_intent
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
            )
            .or_default()
            .push(case);
    }
    for (intent, candidates) in &mut by_intent {
        candidates
            .sort_by_key(|case| stable_batch_key(dataset_version, split, intent, &case.case_id));
    }
    if by_intent.is_empty() || by_intent.len() > batch_size {
        return Err(AppError::new(
            "EVALUATION_BATCH_INSUFFICIENT",
            format!("{split} 没有足够的正向 ASK 意图用于冻结 {batch_size} 题"),
            false,
        ));
    }

    let available_total = by_intent.values().map(Vec::len).sum::<usize>();
    let mut quotas = by_intent
        .iter()
        .map(|(intent, candidates)| (intent.clone(), 1_usize.min(candidates.len())))
        .collect::<BTreeMap<_, _>>();
    let guaranteed = quotas.values().sum::<usize>();
    let proportional_slots = batch_size.saturating_sub(guaranteed);
    let mut remainders = Vec::new();
    for (intent, candidates) in &by_intent {
        let numerator = proportional_slots.saturating_mul(candidates.len());
        let extra = numerator.checked_div(available_total).unwrap_or_default();
        let quota = quotas.get_mut(intent).expect("intent quota");
        *quota = (*quota + extra).min(candidates.len());
        remainders.push((
            numerator.checked_rem(available_total).unwrap_or_default(),
            intent.clone(),
        ));
    }
    remainders.sort_by(|left, right| right.cmp(left));
    while quotas.values().sum::<usize>() < batch_size {
        let mut progressed = false;
        for (_, intent) in &remainders {
            let available = by_intent.get(intent).map_or(0, Vec::len);
            let can_grow = quotas.get(intent).copied().unwrap_or_default() < available;
            if can_grow {
                *quotas.get_mut(intent).expect("intent quota") += 1;
                progressed = true;
                if quotas.values().sum::<usize>() == batch_size {
                    break;
                }
            }
        }
        if !progressed {
            break;
        }
    }

    let mut selected = Vec::<&EvaluationCaseRecord>::new();
    let mut batch_files = HashSet::<String>::new();
    for (intent, candidates) in &by_intent {
        let quota = quotas.get(intent).copied().unwrap_or_default();
        for case in candidates {
            let file_ids = case.expected_file_ids.as_deref().unwrap_or_default();
            selected.push(case);
            batch_files.extend(file_ids.iter().cloned());
            if selected
                .iter()
                .filter(|selected_case| selected_case.expected_intent.as_deref() == Some(intent))
                .count()
                >= quota
            {
                break;
            }
        }
    }
    if selected.len() < batch_size {
        let mut remaining = by_intent.values().flatten().copied().collect::<Vec<_>>();
        remaining
            .sort_by_key(|case| stable_batch_key(dataset_version, split, "fill", &case.case_id));
        for case in remaining {
            if selected
                .iter()
                .any(|selected_case| selected_case.case_id == case.case_id)
            {
                continue;
            }
            let file_ids = case.expected_file_ids.as_deref().unwrap_or_default();
            selected.push(case);
            batch_files.extend(file_ids.iter().cloned());
            if selected.len() == batch_size {
                break;
            }
        }
    }
    if selected.len() != batch_size {
        return Err(AppError::new(
            "EVALUATION_BATCH_INSUFFICIENT",
            format!(
                "{split} 批次 {batch_index} 只能冻结 {} 个正向 ASK，需要 {batch_size} 个",
                selected.len()
            ),
            false,
        ));
    }

    selected
        .sort_by_key(|case| stable_batch_key(dataset_version, split, "selected", &case.case_id));
    let case_ids = selected
        .iter()
        .map(|case| case.case_id.clone())
        .collect::<Vec<_>>();
    let mut source_file_ids = batch_files.into_iter().collect::<Vec<_>>();
    source_file_ids.sort();
    let mut intent_distribution = BTreeMap::<String, u32>::new();
    for case in &selected {
        *intent_distribution
            .entry(
                case.expected_intent
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
            )
            .or_default() += 1;
    }
    let manifest_sha256 = batch_manifest_digest(
        dataset_version,
        split,
        batch_index,
        batch_size,
        &case_ids,
        &source_file_ids,
        &intent_distribution,
    )?;
    let split_label = split.to_ascii_lowercase();
    let batch_id = format!(
        "{dataset_version}-{split_label}-{batch_index:02}-{}",
        &manifest_sha256[..12]
    );
    used_files.extend(source_file_ids.iter().cloned());
    Ok(EvaluationBatchManifestV1 {
        schema_version: 1,
        batch_id,
        dataset_version: dataset_version.to_owned(),
        split: split.to_owned(),
        batch_index: batch_index as u32,
        batch_size: batch_size as u32,
        selection_algorithm: "intent-proportional-file-disjoint-v1".into(),
        case_ids,
        source_file_ids,
        intent_distribution,
        manifest_sha256,
        code_fingerprint: None,
        index_fingerprint: None,
        model_fingerprint: None,
        source_manifest_fingerprint: None,
    })
}

fn batch_manifest_digest(
    dataset_version: &str,
    split: &str,
    batch_index: usize,
    batch_size: usize,
    case_ids: &[String],
    source_file_ids: &[String],
    intent_distribution: &BTreeMap<String, u32>,
) -> Result<String, AppError> {
    let digest_payload = serde_json::json!({
        "schema_version": 1,
        "dataset_version": dataset_version,
        "split": split,
        "batch_index": batch_index,
        "batch_size": batch_size,
        "selection_algorithm": "intent-proportional-file-disjoint-v1",
        "case_ids": case_ids,
        "source_file_ids": source_file_ids,
        "intent_distribution": intent_distribution,
    });
    let digest = Sha256::digest(serde_json::to_vec(&digest_payload).map_err(|error| {
        AppError::new(
            "EVALUATION_BATCH_SERIALIZE_FAILED",
            error.to_string(),
            false,
        )
    })?);
    Ok(format!("{digest:x}"))
}

pub fn validate_ragas_batch_manifest(manifest: &EvaluationBatchManifestV1) -> Result<(), AppError> {
    if manifest.schema_version != 1
        || manifest.selection_algorithm != "intent-proportional-file-disjoint-v1"
        || manifest.case_ids.len() != manifest.batch_size as usize
        || manifest.case_ids.iter().collect::<HashSet<_>>().len() != manifest.case_ids.len()
    {
        return Err(AppError::new(
            "EVALUATION_BATCH_INVALID",
            "评测批次清单结构无效",
            false,
        ));
    }
    let expected = batch_manifest_digest(
        &manifest.dataset_version,
        &manifest.split,
        manifest.batch_index as usize,
        manifest.batch_size as usize,
        &manifest.case_ids,
        &manifest.source_file_ids,
        &manifest.intent_distribution,
    )?;
    if expected != manifest.manifest_sha256 || !manifest.batch_id.ends_with(&expected[..12]) {
        return Err(AppError::new(
            "EVALUATION_BATCH_HASH_MISMATCH",
            "评测批次清单哈希不一致，已拒绝运行",
            false,
        ));
    }
    Ok(())
}

fn positive_ragas_case(case: &EvaluationCaseRecord) -> bool {
    case.feature_type == "ASK"
        && !case.dataset_version.trim().is_empty()
        && case.expected_intent.as_deref() != Some("no_evidence")
        && case
            .expected_file_ids
            .as_ref()
            .is_some_and(|file_ids| !file_ids.is_empty())
        && case
            .expected_evidence_ids
            .as_ref()
            .is_some_and(|evidence_ids| !evidence_ids.is_empty())
}

fn stable_batch_key(dataset_version: &str, split: &str, intent: &str, case_id: &str) -> [u8; 32] {
    Sha256::digest(format!("{dataset_version}\n{split}\n{intent}\n{case_id}").as_bytes()).into()
}

/// FNV-1a 32 位哈希（确定性，跨平台稳定）。
fn fnv1a_hash(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5_u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

fn corpus_file(_file: &EvaluationCorpusFile) -> EvaluationCaseRecord {
    EvaluationCaseRecord {
        case_id: String::new(),
        feature_type: String::new(),
        question_or_request: String::new(),
        expected_source: Some("LOCAL".into()),
        expected_intent: None,
        expected_operation: None,
        expected_file_ids: None,
        expected_chunk_ids: None,
        expected_evidence_ids: None,
        expected_answer_shape: None,
        expected_relation_type: None,
        expected_collection_members: None,
        gold_reason: None,
        split: "UNASSIGNED".into(),
        dataset_version: String::new(),
        metadata: serde_json::Value::Null,
        created_at: Utc::now(),
    }
}

/// 文件名主干（去掉扩展名）。
fn file_stem(display_name: &str) -> String {
    let name = display_name
        .rsplit_once('.')
        .map_or(display_name, |(stem, _)| stem);
    name.trim().to_owned()
}

/// 主干首词（>=2 字符的片段，用于部分文件名检索）。
fn first_word(text: &str) -> Option<String> {
    text.split(|character: char| character.is_whitespace() || character.is_ascii_punctuation())
        .map(str::trim)
        .find(|token| token.chars().count() >= 2)
        .map(str::to_owned)
}

/// 轻微错别字变换（通用、与具体内容无关）：相邻两字符交换位置。
fn typo_variant(word: &str) -> Option<String> {
    let characters = word.chars().collect::<Vec<_>>();
    if characters.len() < 3 {
        return None;
    }
    let mut variant = characters.clone();
    variant.swap(1, 2);
    if variant == characters {
        return None;
    }
    Some(variant.into_iter().collect())
}

/// 有信息量的证据句：第一个长度在 8..=120 字符的 chunk 规范化。
fn evidence_snippet(chunks: &[String]) -> Option<String> {
    chunks
        .iter()
        .map(|chunk| chunk.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|text| (8..=120).contains(&text.chars().count()))
        .map(|text| {
            if text.chars().count() > 120 {
                text.chars().take(120).collect()
            } else {
                text
            }
        })
}

fn generate_search_cases(
    files: &[EvaluationCorpusFile],
    options: &DatasetGenerationOptions,
) -> Vec<EvaluationCaseRecord> {
    let mut cases = Vec::new();
    for file in files {
        if cases.len() >= options.max_search_cases {
            break;
        }
        let stem = file_stem(&file.display_name);
        if stem.chars().count() < 2 {
            continue;
        }
        let file_id = file.file_id.clone();
        // 完整文件名
        cases.push(search_case(
            file,
            format!("search-full-{file_id}"),
            stem.clone(),
            "完整文件名检索",
        ));
        // 部分文件名（主干首词）
        if let Some(word) = first_word(&stem) {
            cases.push(search_case(
                file,
                format!("search-partial-{file_id}"),
                word.clone(),
                "部分文件名检索",
            ));
        }
        // 模糊标题 / 关键词
        let title_token = file
            .title
            .trim()
            .chars()
            .filter(|character| !character.is_ascii_punctuation())
            .collect::<String>();
        if title_token.chars().count() >= 2 {
            cases.push(search_case(
                file,
                format!("search-title-{file_id}"),
                title_token,
                "模糊标题检索",
            ));
        }
        // 内容反查文件
        if let Some(snippet) = evidence_snippet(&file.text_chunks) {
            cases.push(search_case(
                file,
                format!("search-content-{file_id}"),
                format!("在哪份文档里提到了“{snippet}”"),
                "内容反查文件",
            ));
        }
        // 实体找文件
        if let Some(entity) = file
            .entities
            .iter()
            .find(|entity| entity.chars().count() >= 2)
        {
            cases.push(search_case(
                file,
                format!("search-entity-{file_id}"),
                format!("找一下包含“{entity}”的文件"),
                "实体找文件",
            ));
        }
        // 文件类型过滤（文件名 + 扩展名）
        if let Some(extension) = file
            .display_name
            .rsplit_once('.')
            .map(|(_, extension)| extension)
            && !extension.is_empty()
        {
            cases.push(search_case(
                file,
                format!("search-type-{file_id}"),
                format!("{stem} {extension}"),
                "文件类型过滤",
            ));
        }
        // 轻微错别字（通用变换）
        if let Some(variant) = typo_variant(&stem) {
            cases.push(search_case(
                file,
                format!("search-typo-{file_id}"),
                variant,
                "轻微错别字检索",
            ));
        }
        // 自然语言模糊找文件
        if let Some(keyword) = file
            .keywords
            .iter()
            .find(|keyword| keyword.chars().count() >= 2)
        {
            cases.push(search_case(
                file,
                format!("search-natural-{file_id}"),
                format!("帮我找找和“{keyword}”有关的文件"),
                "自然语言模糊找文件",
            ));
        }
    }
    cases
}

fn search_case(
    file: &EvaluationCorpusFile,
    case_id: String,
    query: String,
    gold_reason: &str,
) -> EvaluationCaseRecord {
    let mut case = corpus_file(file);
    case.case_id = case_id;
    case.feature_type = "SEARCH".into();
    case.question_or_request = query;
    case.expected_intent = Some("search".into());
    case.expected_operation = Some("search".into());
    case.expected_file_ids = Some(vec![file.file_id.clone()]);
    case.expected_answer_shape = Some("file_ids".into());
    case.gold_reason = Some(format!(
        "{gold_reason}：以文档《{}》为依据，预期命中该文件",
        file.display_name
    ));
    case
}

fn generate_ask_cases(
    files: &[EvaluationCorpusFile],
    options: &DatasetGenerationOptions,
) -> Vec<EvaluationCaseRecord> {
    let mut cases = Vec::new();
    for file in files {
        if cases.len() >= options.max_ask_cases {
            break;
        }
        let file_id = file.file_id.clone();
        let title = if file.title.trim().is_empty() {
            file_stem(&file.display_name)
        } else {
            file.title.trim().to_owned()
        };
        // DOCUMENT_QA：整个文档作为证据
        cases.push(ask_case(
            file,
            format!("ask-doc-qa-{file_id}"),
            format!("《{title}》这份文档主要在讲什么？"),
            "document_qa",
            "extractive",
            "以文档代表性内容为 Gold Evidence 生成文档问答",
        ));
        // DOCUMENT_SUMMARY
        cases.push(ask_case(
            file,
            format!("ask-doc-summary-{file_id}"),
            format!("请概括一下《{title}》的主要内容。"),
            "document_summary",
            "summary",
            "以文档全文为 Gold Evidence 生成摘要问题",
        ));
        // BOOLEAN_EXISTENCE：仅对确实出现在文本中的关键词生成（可验证）
        if let Some(keyword) = file
            .keywords
            .iter()
            .find(|keyword| keyword.chars().count() >= 2 && file.contains(keyword))
        {
            cases.push(ask_case(
                file,
                format!("ask-boolean-{file_id}"),
                format!("《{title}》里提到了“{keyword}”吗？"),
                "boolean_existence",
                "yes_no",
                &format!("关键词“{keyword}”出现在文档文本中，作为 Gold Evidence"),
            ));
        }
        // EXTRACT：仅对出现在文本中的实体生成（可验证）
        if let Some(entity) = file
            .entities
            .iter()
            .find(|entity| entity.chars().count() >= 2 && file.contains(entity))
        {
            cases.push(ask_case(
                file,
                format!("ask-extract-{file_id}"),
                format!("从《{title}》里找出所有和“{entity}”相关的信息。"),
                "extract",
                "extractive",
                &format!("实体“{entity}”出现在文档文本中，作为 Gold Evidence"),
            ));
        }
    }
    // MULTI_DOCUMENT_QA / COMPARE：取两个不同文档生成对比类问题
    let paired = files.windows(2).take(options.max_ask_cases / 4);
    for pair in paired {
        let left = &pair[0];
        let right = &pair[1];
        if left.file_id == right.file_id {
            continue;
        }
        let left_title = if left.title.trim().is_empty() {
            file_stem(&left.display_name)
        } else {
            left.title.trim().to_owned()
        };
        let right_title = if right.title.trim().is_empty() {
            file_stem(&right.display_name)
        } else {
            right.title.trim().to_owned()
        };
        let mut case = corpus_file(left);
        case.case_id = format!("ask-compare-{}-{}", left.file_id, right.file_id);
        case.feature_type = "ASK".into();
        case.question_or_request =
            format!("《{left_title}》和《{right_title}》分别讨论了什么内容？");
        case.expected_source = Some("LOCAL".into());
        case.expected_intent = Some("compare".into());
        case.expected_operation = Some("ask".into());
        case.expected_file_ids = Some(vec![left.file_id.clone(), right.file_id.clone()]);
        case.expected_answer_shape = Some("compare".into());
        case.gold_reason = Some(format!(
            "跨文档对比：Gold Evidence 为《{}》与《{}》两份文档",
            left.display_name, right.display_name
        ));
        cases.push(case);
    }
    cases
}

fn ask_case(
    file: &EvaluationCorpusFile,
    case_id: String,
    question: String,
    intent: &str,
    answer_shape: &str,
    gold_reason: &str,
) -> EvaluationCaseRecord {
    let mut case = corpus_file(file);
    case.case_id = case_id;
    case.feature_type = "ASK".into();
    case.question_or_request = question;
    case.expected_source = Some("LOCAL".into());
    case.expected_intent = Some(intent.to_owned());
    case.expected_operation = Some("ask".into());
    case.expected_file_ids = Some(vec![file.file_id.clone()]);
    case.expected_evidence_ids = Some(
        file.text_chunks
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, _)| format!("{}#{}", file.file_id, index))
            .collect(),
    );
    case.expected_answer_shape = Some(answer_shape.to_owned());
    case.gold_reason = Some(gold_reason.to_owned());
    case
}

/// SMART_COLLECTION：由文档内容归纳「包含关键词 X 的所有文档」的可验证集合定义。
fn generate_collection_cases(
    files: &[EvaluationCorpusFile],
    options: &DatasetGenerationOptions,
) -> Vec<EvaluationCaseRecord> {
    let mut keyword_files = std::collections::HashMap::<String, Vec<&EvaluationCorpusFile>>::new();
    for file in files {
        for keyword in file
            .keywords
            .iter()
            .filter(|keyword| keyword.chars().count() >= 2 && file.contains(keyword))
        {
            keyword_files.entry(keyword.clone()).or_default().push(file);
        }
    }
    let mut cases = Vec::new();
    for (keyword, members) in keyword_files {
        if cases.len() >= options.max_collection_cases {
            break;
        }
        if members.len() < 2 {
            continue;
        }
        let mut member_ids = members
            .iter()
            .map(|file| file.file_id.clone())
            .collect::<Vec<_>>();
        member_ids.sort();
        member_ids.dedup();
        let mut case = corpus_file(members[0]);
        case.case_id = format!("collection-{}", fnv1a_hash(keyword.as_bytes()));
        case.feature_type = "SMART_COLLECTION".into();
        case.question_or_request = format!("自动整理一份包含关键词“{keyword}”的文档集合");
        case.expected_source = Some("LOCAL".into());
        case.expected_intent = Some("smart_collection".into());
        case.expected_operation = Some("smart_collection".into());
        case.expected_collection_members = Some(member_ids.clone());
        case.expected_file_ids = Some(member_ids.clone());
        case.gold_reason = Some(format!(
            "集合定义可验证：关键词“{keyword}”出现在 {} 份真实文档的文本中",
            member_ids.len()
        ));
        cases.push(case);
    }
    cases
}

/// FILE_RELATION：positive/negative pairs（重复 / 版本 / 相关 / 无关）。
fn generate_relation_cases(
    files: &[EvaluationCorpusFile],
    options: &DatasetGenerationOptions,
) -> Vec<EvaluationCaseRecord> {
    let mut cases = Vec::new();
    // positive：内容哈希相同 → 重复（exact_duplicate）
    let mut by_hash = std::collections::HashMap::<String, Vec<&EvaluationCorpusFile>>::new();
    for file in files {
        if let Some(hash) = file.content_sha256.as_deref() {
            by_hash.entry(hash.to_owned()).or_default().push(file);
        }
    }
    for group in by_hash.values().filter(|group| group.len() >= 2) {
        for window in group.windows(2) {
            if cases.len() >= options.max_relation_cases {
                break;
            }
            let (left, right) = (&window[0], &window[1]);
            let mut case = corpus_file(left);
            case.case_id = format!("relation-dup-{}-{}", left.file_id, right.file_id);
            case.feature_type = "FILE_RELATION".into();
            case.question_or_request = format!(
                "判断《{}》与《{}》是否重复",
                left.display_name, right.display_name
            );
            case.expected_source = Some("LOCAL".into());
            case.expected_intent = Some("file_relation".into());
            case.expected_operation = Some("file_relation".into());
            case.expected_relation_type = Some("exact_duplicate".into());
            case.expected_file_ids = Some(vec![left.file_id.clone(), right.file_id.clone()]);
            case.gold_reason = Some("两份文档内容哈希相同，Gold 为重复关系".into());
            cases.push(case);
        }
    }
    // negative：随机不同文档对（内容哈希不同）→ 无关
    let distinct = files
        .iter()
        .filter(|file| file.content_sha256.is_some())
        .collect::<Vec<_>>();
    for window in distinct.windows(2) {
        if cases.len() >= options.max_relation_cases {
            break;
        }
        let (left, right) = (&window[0], &window[1]);
        if left.content_sha256 == right.content_sha256 {
            continue;
        }
        let mut case = corpus_file(left);
        case.case_id = format!("relation-neg-{}-{}", left.file_id, right.file_id);
        case.feature_type = "FILE_RELATION".into();
        case.question_or_request = format!(
            "判断《{}》与《{}》是否重复",
            left.display_name, right.display_name
        );
        case.expected_source = Some("LOCAL".into());
        case.expected_intent = Some("file_relation".into());
        case.expected_operation = Some("file_relation".into());
        case.expected_relation_type = None;
        case.expected_file_ids = Some(vec![left.file_id.clone(), right.file_id.clone()]);
        case.gold_reason = Some("两份文档内容哈希不同，Gold 为无关关系".into());
        cases.push(case);
    }
    cases
}

/// NO_EVIDENCE：确定性伪随机不存在的主题（保证语料中确无证据）。
fn generate_no_evidence_cases(
    files: &[EvaluationCorpusFile],
    options: &DatasetGenerationOptions,
) -> Vec<EvaluationCaseRecord> {
    let mut cases = Vec::new();
    let count = (options.max_ask_cases / 10).clamp(2, 6);
    for index in 0..count {
        let seed = files
            .iter()
            .map(|file| file.file_id.as_str())
            .collect::<Vec<_>>()
            .join("|");
        let hash = fnv1a_hash(format!("{seed}#no-evidence-{index}").as_bytes());
        let token = format!("FANFAN_NO_EVIDENCE_{hash:08X}");
        let mut case = corpus_file(&files[0]);
        case.case_id = format!("ask-no-evidence-{index}");
        case.feature_type = "ASK".into();
        case.question_or_request = format!("我好像有一份关于“{token}”的资料，帮我找一下。");
        case.expected_source = Some("LOCAL".into());
        case.expected_intent = Some("no_evidence".into());
        case.expected_operation = Some("ask".into());
        case.expected_file_ids = Some(Vec::new());
        case.expected_answer_shape = Some("no_evidence".into());
        case.gold_reason = Some(format!(
            "NO_EVIDENCE 用例：主题“{token}”为确定性生成的占位词，语料中无证据"
        ));
        cases.push(case);
    }
    cases
}

// ===== 优化闭环：评测执行器（判定为纯函数，链路运行由编排器完成） =====

/// 一次真实链路运行后的观测快照。编排器（evaluation_optimize example）负责
/// 用真实链路填充本结构；`evaluate_case_verdict` 只做判定，不产生副作用，
/// 便于单测与在 DEV/HOLDOUT 上统一复跑。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct EvaluationObservation {
    pub actual_source: Option<String>,
    pub actual_intent: Option<String>,
    pub actual_operation: Option<String>,
    /// SEARCH：返回结果 file_id 有序列表（relevance 排序，index 即 rank）。
    pub actual_files: Vec<String>,
    /// ASK：答案引用的证据 file_id（来自 citation / evidence）。
    pub actual_evidence: Vec<String>,
    pub actual_answer_shape: Option<String>,
    /// FILE_RELATION：left-right 对的实际关系类型（未预测则为 None）。
    pub actual_relation_type: Option<String>,
    /// SMART_COLLECTION：实际被纳入集合的 file_id。
    pub actual_collection_members: Vec<String>,
    /// ASK：链路是否找到证据。
    pub evidence_found: bool,
    /// ASK：答案是否 grounded（无 unsupported claim）。
    pub answer_grounded: bool,
    pub latency_ms: u64,
    /// 链路运行失败时的错误码（如 WORKER_UNAVAILABLE），用于错误分类。
    pub error_code: Option<String>,
    /// 诊断线索（简短、通用，避免写入完整原文）。
    pub diagnosis_hint: Option<String>,
    /// ASK：真实回答正文。只允许进入受保护的评测快照，不进入普通日志。
    #[serde(default)]
    pub response: Option<String>,
    /// ASK：按实际生成顺序排列的证据上下文。
    #[serde(default)]
    pub retrieved_contexts: Vec<String>,
    /// ASK：与 retrieved_contexts 同序的稳定 chunk ID。
    #[serde(default)]
    pub retrieved_context_ids: Vec<String>,
    /// ASK：冻结数据集中的参考上下文与稳定 chunk ID。
    #[serde(default)]
    pub reference_contexts: Vec<String>,
    #[serde(default)]
    pub reference_context_ids: Vec<String>,
    /// ASK：参考答案；无证据硬门禁用例可为空。
    #[serde(default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub trace_id: Option<String>,
    #[serde(default)]
    pub retrieval_latency_ms: Option<u64>,
    #[serde(default)]
    pub generation_latency_ms: Option<u64>,
    #[serde(default)]
    pub model_fingerprint: Option<String>,
    #[serde(default)]
    pub index_fingerprint: Option<String>,
    #[serde(default)]
    pub code_fingerprint: Option<String>,
}

/// 把真实 ASK 链路观测映射为 RAGAS V1 样本。该函数不做磁盘写入；调用方必须把
/// 明细写入 DPAPI/EFS 保护目录。无证据、越权、拒答与记忆用例也可导出，但 Python
/// 评分层会把它们留给确定性硬门禁而不是语义均分。
pub fn ragas_sample_from_observation(
    case: &EvaluationCaseRecord,
    observation: &EvaluationObservation,
) -> Option<RagasEvaluationSampleV1> {
    if case.feature_type != "ASK" {
        return None;
    }
    Some(RagasEvaluationSampleV1 {
        schema_version: 1,
        case_id: case.case_id.clone(),
        dataset_version: case.dataset_version.clone(),
        split: case.split.clone(),
        feature_type: case.feature_type.clone(),
        user_input: case.question_or_request.clone(),
        response: observation.response.clone().unwrap_or_default(),
        retrieved_contexts: observation.retrieved_contexts.clone(),
        reference_contexts: observation.reference_contexts.clone(),
        retrieved_context_ids: observation.retrieved_context_ids.clone(),
        reference_context_ids: observation.reference_context_ids.clone(),
        reference: observation.reference.clone(),
        trace_id: observation.trace_id.clone(),
        retrieval_latency_ms: observation.retrieval_latency_ms,
        generation_latency_ms: observation.generation_latency_ms,
        total_latency_ms: observation.latency_ms,
        model_fingerprint: observation.model_fingerprint.clone(),
        index_fingerprint: observation.index_fingerprint.clone(),
        code_fingerprint: observation.code_fingerprint.clone(),
        deterministic_judgements: serde_json::json!({
            "evidence_found": observation.evidence_found,
            "answer_grounded": observation.answer_grounded,
            "error_code": observation.error_code,
            "actual_evidence": observation.actual_evidence,
        }),
    })
}

/// 将样本写成版本化 JSONL。安全目录的选择与加密由编排器负责。
pub fn write_ragas_samples_jsonl(
    writer: &mut impl Write,
    samples: &[RagasEvaluationSampleV1],
) -> Result<(), AppError> {
    for sample in samples {
        serde_json::to_writer(&mut *writer, sample)
            .map_err(|error| AppError::local_config(error.to_string(), false))?;
        writer
            .write_all(b"\n")
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
    }
    Ok(())
}

/// 将真实评测明细写入受保护目录。优先使用 EFS；文件系统不支持 EFS 时，先在临时
/// 文件中序列化，再用当前用户 DPAPI 保护并立即删除明文临时文件。
pub fn write_protected_ragas_samples(
    requested_path: &Path,
    samples: &[RagasEvaluationSampleV1],
) -> Result<PathBuf, AppError> {
    let parent = requested_path
        .parent()
        .ok_or_else(|| AppError::local_config("RAGAS 明细输出路径缺少父目录", false))?;
    fs::create_dir_all(parent).map_err(|error| AppError::local_config(error.to_string(), true))?;
    if ensure_directory_encrypted(parent).is_ok() {
        let mut file = File::create(requested_path)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        write_ragas_samples_jsonl(&mut file, samples)?;
        file.sync_all()
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        if !path_is_encrypted(requested_path)? {
            let _ = fs::remove_file(requested_path);
            return Err(AppError::local_config(
                "RAGAS 明细文件未继承Windows文件加密，已拒绝保留",
                false,
            ));
        }
        return Ok(requested_path.to_path_buf());
    }

    let temporary = std::env::temp_dir().join(format!("fanfan-ragas-{}.jsonl", Uuid::now_v7()));
    let protected = requested_path.with_extension("jsonl.dpapi");
    let result = (|| {
        let mut file = File::create(&temporary)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        write_ragas_samples_jsonl(&mut file, samples)?;
        file.sync_all()
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        protect_file_for_current_user(&temporary, &protected)?;
        Ok(protected.clone())
    })();
    if temporary.is_file() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// 计算 precision / recall（分母为 0 时为 0，避免除零）。
fn precision_recall(expected: &[String], actual: &[String]) -> (f64, f64) {
    if actual.is_empty() {
        return (0.0, 0.0);
    }
    let actual_set = actual.iter().collect::<HashSet<_>>();
    let hits = expected
        .iter()
        .filter(|item| actual_set.contains(item))
        .count();
    let precision = hits as f64 / actual.len() as f64;
    let recall = if expected.is_empty() {
        0.0
    } else {
        hits as f64 / expected.len() as f64
    };
    (precision, recall)
}

/// 单条用例判定（纯逻辑）。输出 EvaluationResultRecord：写入 pass/fail、
/// error_category、diagnosis_reason 与 metrics_json（含 feature_type 与
/// expected 快照，供 aggregate_evaluation_metrics 按链路聚合）。
pub fn evaluate_case_verdict(
    run_id: &str,
    case: &EvaluationCaseRecord,
    observation: &EvaluationObservation,
) -> EvaluationResultRecord {
    let mut metrics = serde_json::Map::new();
    metrics.insert(
        "feature_type".into(),
        serde_json::Value::from(case.feature_type.as_str()),
    );
    let mut failed_reasons = Vec::new();
    let mut error_category: Option<OptimizationErrorCategory> = None;

    match case.feature_type.as_str() {
        "SEARCH" => {
            let expected = case.expected_file_ids.clone().unwrap_or_default();
            let ranks = expected
                .iter()
                .filter_map(|file_id| {
                    observation
                        .actual_files
                        .iter()
                        .position(|actual| actual == file_id)
                })
                .map(|rank| rank as u64)
                .collect::<Vec<_>>();
            metrics.insert(
                "actual_file_ranks".into(),
                serde_json::Value::from(ranks.clone()),
            );
            metrics.insert(
                "expected_file_ids".into(),
                serde_json::Value::from(case.expected_file_ids.clone().unwrap_or_default()),
            );
            let hit_top5 = ranks.iter().any(|rank| *rank < 5);
            if expected.is_empty() {
                failed_reasons.push("no_expected_file".to_owned());
            } else if hit_top5 {
                // 命中 Top5 即通过；rank 指标由聚合层从 actual_file_ranks 计算
            } else if ranks.is_empty() {
                failed_reasons.push("document_not_recalled".to_owned());
                error_category = Some(OptimizationErrorCategory::DocumentRecallError);
            } else {
                failed_reasons.push("correct_file_ranked_below_top5".to_owned());
                error_category = Some(OptimizationErrorCategory::SearchRankingError);
            }
        }
        "ASK" => {
            let expects_evidence = case.expected_intent.as_deref() != Some("no_evidence")
                && case
                    .expected_file_ids
                    .as_ref()
                    .is_some_and(|ids| !ids.is_empty());
            metrics.insert(
                "expects_evidence".into(),
                serde_json::Value::from(if expects_evidence { 1_u64 } else { 0_u64 }),
            );
            metrics.insert(
                "evidence_found".into(),
                serde_json::Value::from(if observation.evidence_found { 1.0 } else { 0.0 }),
            );
            metrics.insert(
                "answer_grounded".into(),
                serde_json::Value::from(if observation.answer_grounded {
                    1.0
                } else {
                    0.0
                }),
            );
            metrics.insert(
                "expected_source".into(),
                serde_json::Value::from(case.expected_source.clone().unwrap_or_default()),
            );
            let expected_files = case.expected_file_ids.clone().unwrap_or_default();
            let expected_cited = !expected_files.is_empty()
                && expected_files.iter().any(|file_id| {
                    observation
                        .actual_evidence
                        .iter()
                        .any(|actual| actual == file_id)
                });
            metrics.insert(
                "expected_cited".into(),
                serde_json::Value::from(if expected_cited { 1_u64 } else { 0_u64 }),
            );
            if expects_evidence {
                if !observation.evidence_found {
                    failed_reasons.push("evidence_not_found".to_owned());
                    error_category = Some(OptimizationErrorCategory::DocumentRecallError);
                } else if !expected_cited {
                    failed_reasons.push("expected_file_not_cited".to_owned());
                    error_category = Some(OptimizationErrorCategory::EvidenceSelectionError);
                }
                if observation.evidence_found && !observation.answer_grounded {
                    failed_reasons.push("answer_not_grounded".to_owned());
                    if error_category.is_none() {
                        error_category = Some(OptimizationErrorCategory::ClaimVerificationError);
                    }
                }
            } else if observation.evidence_found {
                // NO_EVIDENCE 用例不应产生证据
                failed_reasons.push("evidence_found_for_no_evidence".to_owned());
                error_category = Some(OptimizationErrorCategory::AnswerabilityError);
            }
            metrics.insert(
                "actual_evidence".into(),
                serde_json::Value::from(observation.actual_evidence.clone()),
            );
        }
        "SMART_COLLECTION" => {
            let expected = case.expected_collection_members.clone().unwrap_or_default();
            let (precision, recall) =
                precision_recall(&expected, &observation.actual_collection_members);
            metrics.insert(
                "collection_precision".into(),
                serde_json::Value::from(precision),
            );
            metrics.insert("collection_recall".into(), serde_json::Value::from(recall));
            metrics.insert(
                "expected_collection_members".into(),
                serde_json::Value::from(expected.clone()),
            );
            if expected.is_empty() {
                failed_reasons.push("no_expected_members".to_owned());
            } else if recall < 0.5 {
                failed_reasons.push("collection_recall_below_half".to_owned());
                error_category = Some(OptimizationErrorCategory::SmartCollectionError);
            } else if (1.0 - recall).abs() > f64::EPSILON {
                failed_reasons.push("collection_members_incomplete".to_owned());
                error_category = Some(OptimizationErrorCategory::SmartCollectionError);
            }
        }
        "FILE_RELATION" => {
            let expected_relation = case.expected_relation_type.clone();
            let predicted = observation
                .actual_relation_type
                .as_deref()
                .is_some_and(|actual| {
                    expected_relation
                        .as_deref()
                        .is_none_or(|expected| actual == expected)
                });
            metrics.insert(
                "expected_relation_type".into(),
                serde_json::Value::from(expected_relation.clone().unwrap_or_default()),
            );
            metrics.insert(
                "relation_predicted".into(),
                serde_json::Value::from(if predicted { 1.0 } else { 0.0 }),
            );
            let type_match = observation.actual_relation_type.as_deref()
                == expected_relation.as_deref()
                && expected_relation.is_some();
            metrics.insert(
                "relation_type_match".into(),
                serde_json::Value::from(if type_match { 1.0 } else { 0.0 }),
            );
            if expected_relation.is_some() {
                if !predicted {
                    failed_reasons.push("expected_relation_not_predicted".to_owned());
                }
                if !type_match {
                    failed_reasons.push("relation_type_mismatch".to_owned());
                }
                if failed_reasons.is_empty() {
                    // 通过
                } else {
                    error_category = Some(OptimizationErrorCategory::FileRelationError);
                }
            } else {
                // 负样本：不应被预测为任何关系
                if observation.actual_relation_type.is_some() {
                    failed_reasons.push("negative_pair_false_positive".to_owned());
                    error_category = Some(OptimizationErrorCategory::FileRelationError);
                }
            }
        }
        _ => {
            failed_reasons.push("unsupported_feature_type".to_owned());
            error_category = Some(OptimizationErrorCategory::Unknown);
        }
    }

    if let Some(code) = observation.error_code.as_deref() {
        if error_category.is_none() {
            error_category = Some(OptimizationErrorCategory::from_error_code(code));
        }
        failed_reasons.push("runtime_error".to_owned());
    }

    let pass_fail = failed_reasons.is_empty();
    let diagnosis_reason = if failed_reasons.is_empty() {
        observation.diagnosis_hint.clone()
    } else {
        Some(format!(
            "{}; 命中原因: {}",
            failed_reasons.join("; "),
            observation.diagnosis_hint.as_deref().unwrap_or("无")
        ))
    };
    EvaluationResultRecord {
        result_id: format!("{}-{}", run_id, case.case_id),
        case_id: case.case_id.clone(),
        run_id: run_id.to_owned(),
        operation_id: None,
        pass_fail,
        error_category: error_category.map(|category| category.as_str().to_owned()),
        diagnosis_reason,
        actual_source: observation.actual_source.clone(),
        actual_intent: observation.actual_intent.clone(),
        actual_operation: observation.actual_operation.clone(),
        actual_files: Some(observation.actual_files.clone()),
        actual_evidence: Some(observation.actual_evidence.clone()),
        metrics: serde_json::Value::Object(metrics),
        latency_ms: Some(observation.latency_ms),
        created_at: Utc::now(),
    }
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
            classify_error(
                &[],
                Some("GENERATION_ACTIVATION_FAILED"),
                None,
                false,
                None,
                false
            ),
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
        assert_eq!(
            classify_error(&[], None, Some("generated"), false, None, false),
            None
        );
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
        assert_eq!(
            metrics.avg_total_ms,
            (2000 + 8000 + 1000 + 500) as f64 / 4.0
        );
        assert_eq!(metrics.p50_total_ms, 2_000); // 500,1000,2000,8000 → 中位 = 2000
        assert_eq!(metrics.p95_total_ms, 8_000); // ceil(4*0.95)=4 → 最大
    }

    #[test]
    fn ragas_sample_maps_ordered_contexts_and_stable_ids() {
        let case = EvaluationCaseRecord {
            case_id: "ask-public-1".into(),
            feature_type: "ASK".into(),
            question_or_request: "翻翻的资料默认在哪里处理？".into(),
            expected_source: Some("DOCUMENTS".into()),
            expected_intent: Some("document_qa".into()),
            expected_operation: Some("ask".into()),
            expected_file_ids: Some(vec!["file-1".into()]),
            expected_chunk_ids: Some(vec!["chunk-1".into()]),
            expected_evidence_ids: None,
            expected_answer_shape: Some("grounded".into()),
            expected_relation_type: None,
            expected_collection_members: None,
            gold_reason: None,
            split: "PUBLIC".into(),
            dataset_version: "public-v1".into(),
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
        };
        let observation = EvaluationObservation {
            actual_evidence: vec!["file-1".into()],
            evidence_found: true,
            answer_grounded: true,
            latency_ms: 42,
            response: Some("资料默认只在本机处理。".into()),
            retrieved_contexts: vec!["翻翻默认在本机处理资料。".into()],
            retrieved_context_ids: vec!["chunk-1".into()],
            reference_contexts: vec!["翻翻默认在本机处理资料。".into()],
            reference_context_ids: vec!["chunk-1".into()],
            reference: Some("资料默认只在本机处理。".into()),
            trace_id: Some("trace-anonymous-1".into()),
            ..EvaluationObservation::default()
        };

        let sample = ragas_sample_from_observation(&case, &observation).expect("ASK sample");
        assert_eq!(sample.schema_version, 1);
        assert_eq!(sample.retrieved_context_ids, vec!["chunk-1"]);
        assert_eq!(sample.reference_context_ids, vec!["chunk-1"]);
        assert_eq!(sample.total_latency_ms, 42);
        assert_eq!(
            sample.deterministic_judgements["answer_grounded"],
            serde_json::Value::Bool(true)
        );
    }

    #[cfg(windows)]
    #[test]
    fn private_ragas_export_is_efs_or_dpapi_protected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let requested = directory.path().join("ragas-private.jsonl");
        let sample = RagasEvaluationSampleV1 {
            schema_version: 1,
            case_id: "anonymous-case".into(),
            dataset_version: "v1".into(),
            split: "DEV".into(),
            feature_type: "ASK".into(),
            user_input: "private-question-marker".into(),
            response: "private-answer-marker".into(),
            retrieved_contexts: vec!["private-context-marker".into()],
            reference_contexts: vec!["private-context-marker".into()],
            retrieved_context_ids: vec!["chunk-1".into()],
            reference_context_ids: vec!["chunk-1".into()],
            reference: Some("private-reference-marker".into()),
            trace_id: Some("trace-1".into()),
            retrieval_latency_ms: Some(1),
            generation_latency_ms: Some(1),
            total_latency_ms: 2,
            model_fingerprint: None,
            index_fingerprint: None,
            code_fingerprint: None,
            deterministic_judgements: serde_json::json!({"evidence_found": true}),
        };
        let protected = write_protected_ragas_samples(&requested, &[sample]).expect("protected");
        assert!(protected.is_file());
        if protected.extension().and_then(|value| value.to_str()) == Some("dpapi") {
            let bytes = fs::read(protected).expect("read protected bytes");
            assert!(
                !bytes
                    .windows("private-context-marker".len())
                    .any(|window| window == b"private-context-marker")
            );
        } else {
            assert!(path_is_encrypted(&protected).expect("encrypted attribute"));
        }
    }

    #[test]
    fn generated_cases_keep_the_requested_dataset_version() {
        let corpus = vec![EvaluationCorpusFile {
            file_id: "file-1".into(),
            display_name: "example.txt".into(),
            document_type: Some("text".into()),
            title: "Example".into(),
            summary: "A stable evaluation summary".into(),
            keywords: vec!["stable".into()],
            entities: vec!["FanFan".into()],
            section_titles: vec!["Overview".into()],
            text_chunks: vec!["This is sufficiently long stable evidence for evaluation.".into()],
            content_sha256: Some("content-sha".into()),
            modified_at: None,
        }];
        let cases = generate_evaluation_dataset(&corpus, &DatasetGenerationOptions::default())
            .expect("dataset");
        assert!(!cases.is_empty());
        assert!(cases.iter().all(|case| case.dataset_version == "v1"));

        let mut invalid = DatasetGenerationOptions::default();
        invalid.dataset_version.clear();
        let error = generate_evaluation_dataset(&corpus, &invalid).expect_err("missing version");
        assert_eq!(error.code, "EVALUATION_DATASET_VERSION_REQUIRED");
    }

    fn batched_positive_case(index: usize, split: &str) -> EvaluationCaseRecord {
        let file_id = format!("00000000-0000-7000-8000-{index:012}");
        EvaluationCaseRecord {
            case_id: format!("ask-batch-{index:04}"),
            feature_type: "ASK".into(),
            question_or_request: format!("question {index}"),
            expected_source: Some("LOCAL".into()),
            expected_intent: Some(
                [
                    "document_qa",
                    "document_summary",
                    "boolean_existence",
                    "extract",
                ][index % 4]
                    .into(),
            ),
            expected_operation: Some("ask".into()),
            expected_file_ids: Some(vec![file_id.clone()]),
            expected_chunk_ids: None,
            expected_evidence_ids: Some(vec![format!("{file_id}#0")]),
            expected_answer_shape: Some("extractive".into()),
            expected_relation_type: None,
            expected_collection_members: None,
            gold_reason: Some("gold".into()),
            split: split.into(),
            dataset_version: "v2-ragas-batched".into(),
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn ragas_batches_are_stable_exact_and_file_disjoint() {
        let dev = (0..150)
            .map(|index| batched_positive_case(index, "DEV"))
            .collect::<Vec<_>>();
        let holdout = (150..200)
            .map(|index| batched_positive_case(index, "HOLDOUT"))
            .collect::<Vec<_>>();
        let first = freeze_ragas_evaluation_batches(&dev, &holdout, "v2-ragas-batched", 50, 3)
            .expect("freeze batches");
        let second = freeze_ragas_evaluation_batches(&dev, &holdout, "v2-ragas-batched", 50, 3)
            .expect("freeze batches again");
        assert_eq!(first, second);
        assert_eq!(first.len(), 4);
        assert!(first.iter().all(|batch| batch.case_ids.len() == 50));
        let mut files = HashSet::new();
        for batch in &first {
            for file_id in &batch.source_file_ids {
                assert!(files.insert(file_id), "source file reused across batches");
            }
            assert_eq!(batch.intent_distribution.values().sum::<u32>(), 50);
        }
        assert_eq!(first[3].split, "HOLDOUT");
    }

    #[test]
    fn ragas_batch_freeze_rejects_an_incomplete_pool() {
        let dev = (0..49)
            .map(|index| batched_positive_case(index, "DEV"))
            .collect::<Vec<_>>();
        let error = freeze_ragas_evaluation_batches(&dev, &[], "v2-ragas-batched", 50, 3)
            .expect_err("insufficient pool must fail");
        assert_eq!(error.code, "EVALUATION_BATCH_INSUFFICIENT");
    }

    #[test]
    fn aggregate_metrics_keeps_ragas_in_a_namespace() {
        let case = EvaluationCaseRecord {
            case_id: "ask-1".into(),
            feature_type: "ASK".into(),
            question_or_request: "question".into(),
            expected_source: None,
            expected_intent: Some("no_evidence".into()),
            expected_operation: None,
            expected_file_ids: Some(Vec::new()),
            expected_chunk_ids: None,
            expected_evidence_ids: None,
            expected_answer_shape: None,
            expected_relation_type: None,
            expected_collection_members: None,
            gold_reason: None,
            split: "DEV".into(),
            dataset_version: "v1".into(),
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
        };
        let mut result = evaluate_case_verdict("run", &case, &EvaluationObservation::default());
        result.metrics["ragas"] = serde_json::json!({
            "faithfulness": 0.8,
            "answer_relevancy": 0.6
        });
        let aggregate = aggregate_evaluation_metrics(&[result]);
        assert_eq!(aggregate["ragas"]["faithfulness"], 0.8);
        assert_eq!(aggregate["ragas"]["answer_relevancy"], 0.6);
        assert_eq!(aggregate["ragas"]["scored_case_count"], 1);
    }
}
