from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

WORKER_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(WORKER_ROOT))

from remin_worker import (  # noqa: E402
    CheckpointPolicy,
    ExecutionUnit,
    RetryPolicy,
    RiskLevel,
    WorkerRequest,
    WorkerService,
)

REQUEST_IDS = [
    "018f0000-0000-7000-8000-000000000001",
    "018f0000-0000-7000-8000-000000000002",
    "018f0000-0000-7000-8000-000000000003",
]


class WorkerServiceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.service = WorkerService()

    def test_health_check(self) -> None:
        response = self.service.handle(WorkerRequest(REQUEST_IDS[0], "health.check"))
        self.assertTrue(response.ok)
        self.assertEqual(response.result, {"status": "ready", "protocol_version": "1.0"})

    def test_probe_is_read_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "资料.txt"
            path.write_text("拾忆", encoding="utf-8")
            before = path.read_bytes()
            response = self.service.handle(WorkerRequest(REQUEST_IDS[1], "document.probe", {"path": str(path)}))
            after = path.read_bytes()
            self.assertTrue(response.ok)
            self.assertEqual(before, after)
            self.assertEqual(response.result["readonly_operation"], True)

    def test_missing_file_has_structured_error(self) -> None:
        response = self.service.handle(WorkerRequest(REQUEST_IDS[2], "document.probe", {"path": "Z:/missing/file.pdf"}))
        self.assertFalse(response.ok)
        self.assertEqual(response.error.code, "FILE_NOT_FOUND")

    def test_rerank_requires_a_local_onnx_model(self) -> None:
        response = self.service.handle(WorkerRequest(REQUEST_IDS[2], "rerank.score", {
            "model_path": "Z:/missing/reranker.onnx",
            "query": "检索问题",
            "documents": ["候选证据"],
            "max_length": 512,
            "threads": 2,
        }))
        self.assertFalse(response.ok)
        self.assertEqual(response.error.code, "RERANK_MODEL_UNAVAILABLE")

    def test_request_id_must_be_uuid_v7(self) -> None:
        with self.assertRaisesRegex(ValueError, "UUIDv7"):
            WorkerRequest("r1", "health.check")

    def test_high_risk_execution_unit_requires_checkpoint(self) -> None:
        unit = ExecutionUnit(
            unit_id=REQUEST_IDS[0],
            unit_type="document.probe",
            input_schema="remin://schema/document-probe-input/v1",
            output_schema="remin://schema/document-probe-output/v1",
            inputs={},
            preconditions=(),
            postconditions=(),
            timeout_ms=5_000,
            retry_policy=RetryPolicy(2, 250, 2),
            idempotency_key="document.probe:test",
            risk_level=RiskLevel.HIGH,
            checkpoint_policy=CheckpointPolicy.ON_SUCCESS,
        )
        self.assertIn("SCHEMA_CHECKPOINT_REQUIRED", [error.code for error in unit.validate()])


if __name__ == "__main__":
    unittest.main()
