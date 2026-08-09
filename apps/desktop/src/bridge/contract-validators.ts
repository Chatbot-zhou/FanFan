import type { AppError, ExecutionUnit, JobStatus, ValidationCheckpoint } from "./contracts";

const UUID_V7 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;

export function isUuidV7(value: string): boolean {
  return UUID_V7.test(value);
}

export function canTransitionJobStatus(current: JobStatus, next: JobStatus): boolean {
  if (current === next) return true;
  const transitions: Record<JobStatus, JobStatus[]> = {
    queued: ["running", "cancelled"],
    running: ["paused", "awaiting_user", "succeeded", "partial", "failed", "cancelled"],
    paused: ["running", "failed", "cancelled"],
    awaiting_user: ["running", "failed", "cancelled"],
    succeeded: [],
    partial: [],
    failed: [],
    cancelled: [],
  };
  return transitions[current].includes(next);
}

export function validateExecutionUnit(unit: ExecutionUnit): AppError[] {
  const errors: AppError[] = [];
  if (!isUuidV7(unit.unit_id)) errors.push(error("SCHEMA_UUID_V7_REQUIRED", "unit_id必须使用UUIDv7"));
  if (!unit.unit_type.trim()) errors.push(error("SCHEMA_REQUIRED_FIELD", "unit_type不能为空"));
  if (!unit.input_schema.trim() || !unit.output_schema.trim()) {
    errors.push(error("SCHEMA_REQUIRED_FIELD", "输入和输出Schema不能为空"));
  }
  if (!unit.idempotency_key.trim()) errors.push(error("SCHEMA_REQUIRED_FIELD", "idempotency_key不能为空"));
  if (unit.timeout_ms < 1 || unit.timeout_ms > 86_400_000) {
    errors.push(error("SCHEMA_INVALID_TIMEOUT", "timeout_ms必须位于1毫秒到24小时之间"));
  }
  if (unit.retry_policy.max_attempts < 1 || unit.retry_policy.max_attempts > 10 || unit.retry_policy.backoff_multiplier < 1) {
    errors.push(error("SCHEMA_INVALID_RETRY_POLICY", "重试策略超出允许范围"));
  }
  if (unit.risk_level === "high" && unit.checkpoint_policy !== "always") {
    errors.push(error("SCHEMA_CHECKPOINT_REQUIRED", "高风险执行单元必须始终创建检查点"));
  }
  const ruleIds = [...unit.preconditions, ...unit.postconditions].map((rule) => rule.rule_id);
  if (new Set(ruleIds).size !== ruleIds.length) {
    errors.push(error("SCHEMA_DUPLICATE_RULE_ID", "同一执行单元内的检查规则标识不能重复"));
  }
  if (unit.fallback_unit_types.includes(unit.unit_type)) {
    errors.push(error("SCHEMA_INVALID_FALLBACK", "执行单元不能把自身类型作为降级路径"));
  }
  return errors;
}

export function validateCheckpoint(checkpoint: ValidationCheckpoint): AppError[] {
  const errors: AppError[] = [];
  for (const [field, value] of [
    ["checkpoint_id", checkpoint.checkpoint_id],
    ["job_id", checkpoint.job_id],
    ["unit_id", checkpoint.unit_id],
  ] as const) {
    if (!isUuidV7(value)) errors.push(error("SCHEMA_UUID_V7_REQUIRED", `${field}必须使用UUIDv7`));
  }
  if (checkpoint.status === "failed" && !checkpoint.error) {
    errors.push(error("SCHEMA_CHECKPOINT_ERROR_REQUIRED", "失败检查点必须包含结构化错误"));
  }
  if (checkpoint.status === "passed" && checkpoint.error) {
    errors.push(error("SCHEMA_CHECKPOINT_ERROR_UNEXPECTED", "通过的检查点不能包含错误"));
  }
  return errors;
}

function error(code: string, message: string): AppError {
  return { code, message, retryable: false, user_action: null, file_id: null, details: null };
}
