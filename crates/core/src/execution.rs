use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::{Uuid, Version};

use crate::AppError;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckRuleType {
    Schema,
    Invariant,
    Evidence,
    Permission,
    Resource,
    Quality,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckRule {
    pub rule_id: String,
    pub rule_type: CheckRuleType,
    pub description: String,
    pub parameters: Value,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u8,
    pub backoff_ms: u64,
    pub backoff_multiplier: u8,
    pub retryable_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointPolicy {
    Always,
    OnSuccess,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionUnit {
    pub unit_id: Uuid,
    pub unit_type: String,
    pub input_schema: String,
    pub output_schema: String,
    pub inputs: Value,
    pub preconditions: Vec<CheckRule>,
    pub postconditions: Vec<CheckRule>,
    pub timeout_ms: u64,
    pub retry_policy: RetryPolicy,
    pub idempotency_key: String,
    pub risk_level: RiskLevel,
    pub checkpoint_policy: CheckpointPolicy,
    pub fallback_unit_types: Vec<String>,
}

impl ExecutionUnit {
    pub fn validate(&self) -> Result<(), AppError> {
        require_uuid_v7(self.unit_id, "unit_id")?;
        require_non_empty(&self.unit_type, "unit_type")?;
        require_non_empty(&self.input_schema, "input_schema")?;
        require_non_empty(&self.output_schema, "output_schema")?;
        require_non_empty(&self.idempotency_key, "idempotency_key")?;
        if self.timeout_ms == 0 || self.timeout_ms > 86_400_000 {
            return Err(schema_error(
                "SCHEMA_INVALID_TIMEOUT",
                "timeout_ms必须位于1毫秒到24小时之间",
            ));
        }
        if self.retry_policy.max_attempts == 0 || self.retry_policy.max_attempts > 10 {
            return Err(schema_error(
                "SCHEMA_INVALID_RETRY_POLICY",
                "max_attempts必须位于1到10之间",
            ));
        }
        if self.retry_policy.backoff_multiplier == 0 {
            return Err(schema_error(
                "SCHEMA_INVALID_RETRY_POLICY",
                "backoff_multiplier必须大于0",
            ));
        }
        if self.risk_level == RiskLevel::High && self.checkpoint_policy != CheckpointPolicy::Always
        {
            return Err(schema_error(
                "SCHEMA_CHECKPOINT_REQUIRED",
                "高风险执行单元必须始终创建检查点",
            ));
        }
        validate_rules(self.preconditions.iter().chain(self.postconditions.iter()))?;
        if self
            .fallback_unit_types
            .iter()
            .any(|fallback| fallback == &self.unit_type)
        {
            return Err(schema_error(
                "SCHEMA_INVALID_FALLBACK",
                "执行单元不能把自身类型作为降级路径",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointType {
    Schema,
    Invariant,
    Evidence,
    Permission,
    Resource,
    Quality,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    Passed,
    Failed,
    Warning,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationCheckpoint {
    pub checkpoint_id: Uuid,
    pub job_id: Uuid,
    pub unit_id: Uuid,
    pub checkpoint_type: CheckpointType,
    pub status: CheckpointStatus,
    pub rules_version: String,
    pub metrics: Value,
    pub error: Option<AppError>,
    pub created_at: DateTime<Utc>,
    pub resume_token: Option<String>,
}

impl ValidationCheckpoint {
    pub fn validate(&self) -> Result<(), AppError> {
        require_uuid_v7(self.checkpoint_id, "checkpoint_id")?;
        require_uuid_v7(self.job_id, "job_id")?;
        require_uuid_v7(self.unit_id, "unit_id")?;
        require_non_empty(&self.rules_version, "rules_version")?;
        if self.status == CheckpointStatus::Failed && self.error.is_none() {
            return Err(schema_error(
                "SCHEMA_CHECKPOINT_ERROR_REQUIRED",
                "失败检查点必须包含结构化错误",
            ));
        }
        if self.status == CheckpointStatus::Passed && self.error.is_some() {
            return Err(schema_error(
                "SCHEMA_CHECKPOINT_ERROR_UNEXPECTED",
                "通过的检查点不能包含错误",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Pending,
    Running,
    Valid,
    Rejected,
    Selected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExplorationCandidate {
    pub candidate_id: Uuid,
    pub job_id: Uuid,
    pub strategy: String,
    pub status: CandidateStatus,
    pub result_ref: Option<String>,
    pub quality_score: Option<f32>,
    pub evidence_score: Option<f32>,
    pub latency_ms: Option<u64>,
    pub resource_cost: Option<f32>,
    pub rejection_reasons: Vec<String>,
}

impl ExplorationCandidate {
    pub fn validate(&self) -> Result<(), AppError> {
        require_uuid_v7(self.candidate_id, "candidate_id")?;
        require_uuid_v7(self.job_id, "job_id")?;
        require_non_empty(&self.strategy, "strategy")?;
        for score in [self.quality_score, self.evidence_score]
            .into_iter()
            .flatten()
        {
            if !(0.0..=1.0).contains(&score) {
                return Err(schema_error(
                    "SCHEMA_INVALID_SCORE",
                    "候选质量与证据分数必须位于0到1之间",
                ));
            }
        }
        if self.status == CandidateStatus::Selected && self.result_ref.is_none() {
            return Err(schema_error(
                "SCHEMA_CANDIDATE_RESULT_REQUIRED",
                "选中的候选路径必须引用已验证结果",
            ));
        }
        if self.status == CandidateStatus::Rejected && self.rejection_reasons.is_empty() {
            return Err(schema_error(
                "SCHEMA_REJECTION_REASON_REQUIRED",
                "被拒绝的候选路径必须记录原因",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DegradationLevel {
    Full,
    Balanced,
    Core,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DegradationState {
    pub level: DegradationLevel,
    pub triggers: Vec<String>,
    pub disabled_features: Vec<String>,
    pub entered_at: Option<DateTime<Utc>>,
    pub recover_after: Option<DateTime<Utc>>,
    pub manual_override: bool,
}

impl DegradationState {
    pub fn full() -> Self {
        Self {
            level: DegradationLevel::Full,
            triggers: Vec::new(),
            disabled_features: Vec::new(),
            entered_at: None,
            recover_after: None,
            manual_override: false,
        }
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.level == DegradationLevel::Full
            && (!self.triggers.is_empty() || !self.disabled_features.is_empty())
        {
            return Err(schema_error(
                "SCHEMA_INVALID_DEGRADATION_STATE",
                "full状态不能保留降级触发原因或禁用能力",
            ));
        }
        if self.level != DegradationLevel::Full
            && (self.triggers.is_empty() || self.entered_at.is_none())
        {
            return Err(schema_error(
                "SCHEMA_INVALID_DEGRADATION_STATE",
                "降级状态必须记录触发原因和进入时间",
            ));
        }
        if let (Some(entered_at), Some(recover_after)) = (self.entered_at, self.recover_after)
            && recover_after < entered_at
        {
            return Err(schema_error(
                "SCHEMA_INVALID_RECOVERY_TIME",
                "恢复检查时间不能早于降级进入时间",
            ));
        }
        Ok(())
    }

    pub fn can_transition_to(&self, next: DegradationLevel) -> bool {
        self.level == next
            || matches!(
                (self.level, next),
                (DegradationLevel::Full, DegradationLevel::Balanced)
                    | (DegradationLevel::Balanced, DegradationLevel::Full)
                    | (DegradationLevel::Balanced, DegradationLevel::Core)
                    | (DegradationLevel::Core, DegradationLevel::Balanced)
            )
    }
}

fn validate_rules<'a>(rules: impl IntoIterator<Item = &'a CheckRule>) -> Result<(), AppError> {
    let mut identifiers = HashSet::new();
    for rule in rules {
        require_non_empty(&rule.rule_id, "rule_id")?;
        require_non_empty(&rule.description, "description")?;
        if !identifiers.insert(&rule.rule_id) {
            return Err(schema_error(
                "SCHEMA_DUPLICATE_RULE_ID",
                "同一执行单元内的检查规则标识不能重复",
            ));
        }
    }
    Ok(())
}

fn require_uuid_v7(value: Uuid, field: &str) -> Result<(), AppError> {
    if value.get_version() != Some(Version::SortRand) {
        return Err(schema_error(
            "SCHEMA_UUID_V7_REQUIRED",
            format!("{field}必须使用UUIDv7"),
        ));
    }
    Ok(())
}

fn require_non_empty(value: &str, field: &str) -> Result<(), AppError> {
    if value.trim().is_empty() {
        return Err(schema_error(
            "SCHEMA_REQUIRED_FIELD",
            format!("{field}不能为空"),
        ));
    }
    Ok(())
}

fn schema_error(code: &str, message: impl Into<String>) -> AppError {
    AppError::new(code, message, false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn valid_unit() -> ExecutionUnit {
        ExecutionUnit {
            unit_id: Uuid::now_v7(),
            unit_type: "document.probe".into(),
            input_schema: "remin://schema/document-probe-input/v1".into(),
            output_schema: "remin://schema/document-probe-output/v1".into(),
            inputs: json!({"file_id": Uuid::now_v7()}),
            preconditions: vec![CheckRule {
                rule_id: "permission.source_readonly".into(),
                rule_type: CheckRuleType::Permission,
                description: "资料源必须位于已注册根目录且只读".into(),
                parameters: json!({}),
                required: true,
            }],
            postconditions: vec![],
            timeout_ms: 5_000,
            retry_policy: RetryPolicy {
                max_attempts: 2,
                backoff_ms: 250,
                backoff_multiplier: 2,
                retryable_codes: vec!["FILE_BUSY".into()],
            },
            idempotency_key: "document.probe:test".into(),
            risk_level: RiskLevel::Low,
            checkpoint_policy: CheckpointPolicy::OnSuccess,
            fallback_unit_types: vec![],
        }
    }

    #[test]
    fn execution_unit_accepts_a_complete_atomic_contract() {
        valid_unit().validate().expect("valid execution unit");
    }

    #[test]
    fn high_risk_unit_requires_an_always_checkpoint() {
        let mut unit = valid_unit();
        unit.risk_level = RiskLevel::High;
        let error = unit.validate().expect_err("checkpoint must be required");
        assert_eq!(error.code, "SCHEMA_CHECKPOINT_REQUIRED");
    }

    #[test]
    fn job_and_unit_identifiers_must_be_uuid_v7() {
        let mut unit = valid_unit();
        unit.unit_id = Uuid::nil();
        let error = unit.validate().expect_err("uuid version must be checked");
        assert_eq!(error.code, "SCHEMA_UUID_V7_REQUIRED");
    }

    #[test]
    fn degradation_changes_one_level_at_a_time() {
        let full = DegradationState::full();
        assert!(full.can_transition_to(DegradationLevel::Balanced));
        assert!(!full.can_transition_to(DegradationLevel::Core));
    }
}
