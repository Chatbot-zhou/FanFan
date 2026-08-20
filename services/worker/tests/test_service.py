from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

WORKER_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(WORKER_ROOT))

from fanfan_worker import (  # noqa: E402
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
        self.assertEqual(response.result["status"], "ready")
        self.assertEqual(response.result["protocol_version"], "1.2")
        self.assertIn("onnx", response.result["runtime"])
        self.assertIn("speech", response.result["runtime"])
        self.assertIn("ocr", response.result["runtime"])

    def test_sidecar_roles_only_support_their_operations(self) -> None:
        # 每个角色进程只注册本角色需要的操作：ONNX sidecar 不得 import
        # sherpa/paddle，解析 sidecar 不得加载模型缓存。
        cases = [
            ("parse", ["health.check", "document.probe", "document.parse", "export.write"]),
            ("onnx", ["health.check", "embedding.encode", "rerank.score"]),
            ("ocr", ["health.check", "ocr.self_test", "ocr.recognize", "ocr.route_image"]),
            ("speech", ["health.check", "speech.asr_self_test", "speech.recognize"]),
        ]
        for role, supported in cases:
            service = WorkerService(role)
            for operation in supported:
                self.assertTrue(
                    service.supports(operation),
                    f"{role} 应支持 {operation}",
                )
            for operation in ["document.parse", "embedding.encode", "ocr.recognize",
                              "ocr.route_image", "speech.recognize", "rerank.score"]:
                if operation not in supported:
                    self.assertFalse(
                        service.supports(operation),
                        f"{role} 不应支持 {operation}",
                    )
                    response = service.handle(WorkerRequest(REQUEST_IDS[0], operation))
                    self.assertFalse(response.ok)
                    self.assertEqual(response.error.code, "OPERATION_UNSUPPORTED")

    def test_sidecar_roles_probe_only_their_backend(self) -> None:
        # 角色进程只探测自己的运行库（无论本机是否安装成功）：
        # onnx → 仅 onnxruntime；speech → 仅 sherpa_onnx；ocr → 仅 rapidocr。
        # 后端未安装时 ok=False 且 code 固定，已安装时 loaded 只含本角色包。
        cases = {
            "onnx": {"onnxruntime"},
            "speech": {"sherpa_onnx"},
            "ocr": {"rapidocr"},
        }
        for role, expected_packages in cases.items():
            service = WorkerService(role)
            response = service.handle(WorkerRequest(REQUEST_IDS[0], "runtime.backend_probe"))
            self.assertIn(response.ok, (True, False))
            if response.ok:
                self.assertEqual(set(response.result["loaded"]), expected_packages)
            else:
                self.assertEqual(response.error.code, "RUNTIME_BACKEND_UNAVAILABLE")

    def test_unknown_role_is_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "未知 worker 角色"):
            WorkerService("generation")
        response = self.service.handle(WorkerRequest(REQUEST_IDS[0], "health.check"))
        self.assertTrue(response.ok)
        self.assertEqual(response.result["status"], "ready")
        self.assertEqual(response.result["protocol_version"], "1.2")
        self.assertIn("onnx", response.result["runtime"])
        self.assertIn("speech", response.result["runtime"])
        self.assertIn("ocr", response.result["runtime"])

    def test_probe_is_read_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "资料.txt"
            path.write_text("翻翻", encoding="utf-8")
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
            input_schema="fanfan://schema/document-probe-input/v1",
            output_schema="fanfan://schema/document-probe-output/v1",
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
