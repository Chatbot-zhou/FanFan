from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from enum import StrEnum
from typing import Any
from uuid import UUID

from .protocol import WorkerError, is_uuid_v7


class CheckRuleType(StrEnum):
    SCHEMA = "schema"
    INVARIANT = "invariant"
    EVIDENCE = "evidence"
    PERMISSION = "permission"
    RESOURCE = "resource"
    QUALITY = "quality"


class RiskLevel(StrEnum):
    LOW = "low"
    MEDIUM = "medium"
    HIGH = "high"


class CheckpointPolicy(StrEnum):
    ALWAYS = "always"
    ON_SUCCESS = "on_success"
    NONE = "none"


@dataclass(frozen=True, slots=True)
class CheckRule:
    rule_id: str
    rule_type: CheckRuleType
    description: str
    parameters: dict[str, Any] = field(default_factory=dict)
    required: bool = True


@dataclass(frozen=True, slots=True)
class RetryPolicy:
    max_attempts: int
    backoff_ms: int
    backoff_multiplier: int
    retryable_codes: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class ExecutionUnit:
    unit_id: str
    unit_type: str
    input_schema: str
    output_schema: str
    inputs: dict[str, Any]
    preconditions: tuple[CheckRule, ...]
    postconditions: tuple[CheckRule, ...]
    timeout_ms: int
    retry_policy: RetryPolicy
    idempotency_key: str
    risk_level: RiskLevel
    checkpoint_policy: CheckpointPolicy
    fallback_unit_types: tuple[str, ...] = ()

    def validate(self) -> tuple[WorkerError, ...]:
        errors: list[WorkerError] = []
        if not is_uuid_v7(self.unit_id):
            errors.append(_error("SCHEMA_UUID_V7_REQUIRED", "unit_id必须使用UUIDv7"))
        for field_name, value in (
            ("unit_type", self.unit_type),
            ("input_schema", self.input_schema),
            ("output_schema", self.output_schema),
            ("idempotency_key", self.idempotency_key),
        ):
            if not value.strip():
                errors.append(_error("SCHEMA_REQUIRED_FIELD", f"{field_name}不能为空"))
        if not 1 <= self.timeout_ms <= 86_400_000:
            errors.append(_error("SCHEMA_INVALID_TIMEOUT", "timeout_ms必须位于1毫秒到24小时之间"))
        if not 1 <= self.retry_policy.max_attempts <= 10 or self.retry_policy.backoff_multiplier < 1:
            errors.append(_error("SCHEMA_INVALID_RETRY_POLICY", "重试策略超出允许范围"))
        if self.risk_level is RiskLevel.HIGH and self.checkpoint_policy is not CheckpointPolicy.ALWAYS:
            errors.append(_error("SCHEMA_CHECKPOINT_REQUIRED", "高风险执行单元必须始终创建检查点"))
        rule_ids = [rule.rule_id for rule in (*self.preconditions, *self.postconditions)]
        if len(rule_ids) != len(set(rule_ids)):
            errors.append(_error("SCHEMA_DUPLICATE_RULE_ID", "同一执行单元内的检查规则标识不能重复"))
        if self.unit_type in self.fallback_unit_types:
            errors.append(_error("SCHEMA_INVALID_FALLBACK", "执行单元不能把自身类型作为降级路径"))
        return tuple(errors)


class CheckpointStatus(StrEnum):
    PASSED = "passed"
    FAILED = "failed"
    WARNING = "warning"


@dataclass(frozen=True, slots=True)
class ValidationCheckpoint:
    checkpoint_id: str
    job_id: str
    unit_id: str
    checkpoint_type: CheckRuleType
    status: CheckpointStatus
    rules_version: str
    metrics: dict[str, Any]
    error: WorkerError | None
    created_at: datetime
    resume_token: str | None = None

    def validate(self) -> tuple[WorkerError, ...]:
        errors: list[WorkerError] = []
        for field_name, value in (
            ("checkpoint_id", self.checkpoint_id),
            ("job_id", self.job_id),
            ("unit_id", self.unit_id),
        ):
            if not is_uuid_v7(value):
                errors.append(_error("SCHEMA_UUID_V7_REQUIRED", f"{field_name}必须使用UUIDv7"))
        if self.status is CheckpointStatus.FAILED and self.error is None:
            errors.append(_error("SCHEMA_CHECKPOINT_ERROR_REQUIRED", "失败检查点必须包含结构化错误"))
        if self.status is CheckpointStatus.PASSED and self.error is not None:
            errors.append(_error("SCHEMA_CHECKPOINT_ERROR_UNEXPECTED", "通过的检查点不能包含错误"))
        return tuple(errors)


def _error(code: str, message: str) -> WorkerError:
    return WorkerError(code=code, message=message, retryable=False)


def ensure_uuid(value: str) -> UUID:
    """Convert an already validated identifier for persistence adapters."""
    return UUID(value)
