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
}
