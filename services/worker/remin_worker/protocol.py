from __future__ import annotations

from dataclasses import asdict, dataclass, field
from typing import Any, Literal
from uuid import UUID


def is_uuid_v7(value: str) -> bool:
    try:
        return UUID(value).version == 7
    except (ValueError, AttributeError):
        return False


@dataclass(frozen=True, slots=True)
class WorkerError:
    code: str
    message: str
    retryable: bool
    user_action: str | None = None
    file_id: str | None = None
    details: dict[str, Any] | None = None


@dataclass(frozen=True, slots=True)
class WorkerRequest:
    request_id: str
    operation: Literal[
        "health.check",
        "document.probe",
        "document.parse",
        "embedding.encode",
        "rerank.score",
        "export.write",
    ]
    payload: dict[str, Any] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not is_uuid_v7(self.request_id):
            raise ValueError("request_id必须使用UUIDv7")
        if self.operation not in {
            "health.check",
            "document.probe",
            "document.parse",
            "embedding.encode",
            "rerank.score",
            "export.write",
        }:
            raise ValueError("operation不受支持")
        if not isinstance(self.payload, dict):
            raise ValueError("payload必须是对象")

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> "WorkerRequest":
        request_id = value.get("request_id")
        operation = value.get("operation")
        payload = value.get("payload", {})
        if not isinstance(request_id, str) or not is_uuid_v7(request_id):
            raise ValueError("request_id必须使用UUIDv7")
        if operation not in {
            "health.check",
            "document.probe",
            "document.parse",
            "embedding.encode",
            "rerank.score",
            "export.write",
        }:
            raise ValueError("operation 不受支持")
        if not isinstance(payload, dict):
            raise ValueError("payload 必须是对象")
        return cls(request_id=request_id, operation=operation, payload=payload)


@dataclass(frozen=True, slots=True)
class WorkerResponse:
    request_id: str
    ok: bool
    result: dict[str, Any] | None
    error: WorkerError | None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)
