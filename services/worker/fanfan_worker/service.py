from __future__ import annotations

import importlib
import importlib.metadata
from pathlib import Path

from .protocol import WorkerError, WorkerRequest, WorkerResponse
from .parsing import ParseRequest, parse_document
from .embedding import encode_texts
from .reranking import rerank_documents
from .exporting import export_table
from .runtime_cache import cache_snapshot, clear_sessions
from .speech import (
    clear_speech_sessions,
    recognize_speech,
    self_test_asr,
    self_test_tts,
    speech_cache_snapshot,
    synthesize_speech,
)
from .paddle_ocr import (
    clear_ocr_session,
    ocr_cache_snapshot,
    recognize_image,
    self_test_ocr,
)


class WorkerService:
    """只执行已注册、无副作用的最小操作。"""

    def handle(self, request: WorkerRequest) -> WorkerResponse:
        if request.operation == "health.check":
            return WorkerResponse(
                request_id=request.request_id,
                ok=True,
                result={
                    "status": "ready",
                    "protocol_version": "1.2",
                    "runtime": {
                        "onnx": cache_snapshot(),
                        "speech": speech_cache_snapshot(),
                        "ocr": ocr_cache_snapshot(),
                    },
                },
                error=None,
            )
        if request.operation == "runtime.cache_status":
            return WorkerResponse(
                request_id=request.request_id,
                ok=True,
                result=cache_snapshot(),
                error=None,
            )
        if request.operation == "runtime.backend_probe":
            return self._runtime_backend_probe(request)
        if request.operation == "runtime.cache_clear":
            return WorkerResponse(
                request_id=request.request_id,
                ok=True,
                result={"cleared_sessions": clear_sessions() + clear_speech_sessions() + clear_ocr_session()},
                error=None,
            )
        if request.operation == "speech.asr_self_test":
            return self._result(request, self_test_asr(request.payload))
        if request.operation == "speech.recognize":
            return self._result(request, recognize_speech(request.payload))
        if request.operation == "speech.tts_self_test":
            return self._result(request, self_test_tts(request.payload))
        if request.operation == "speech.synthesize":
            return self._result(request, synthesize_speech(request.payload))
        if request.operation == "ocr.self_test":
            return self._result(request, self_test_ocr(request.payload))
        if request.operation == "ocr.recognize":
            return self._result(request, recognize_image(request.payload))
        if request.operation == "document.probe":
            return self._probe_document(request)
        if request.operation == "document.parse":
            parse_request = ParseRequest.from_dict(request.payload)
            result = parse_document(parse_request)
            return WorkerResponse(
                request_id=request.request_id,
                ok=result.status not in {"failed"},
                result=result.to_dict(),
                error=result.error,
            )
        if request.operation == "embedding.encode":
            result, error = encode_texts(request.payload)
            return WorkerResponse(
                request_id=request.request_id,
                ok=error is None,
                result=result,
                error=error,
            )
        if request.operation == "rerank.score":
            result, error = rerank_documents(request.payload)
            return WorkerResponse(
                request_id=request.request_id,
                ok=error is None,
                result=result,
                error=error,
            )
        if request.operation == "export.write":
            result, error = export_table(request.payload)
            return WorkerResponse(
                request_id=request.request_id,
                ok=error is None,
                result=result,
                error=error,
            )
        return WorkerResponse(
            request_id=request.request_id,
            ok=False,
            result=None,
            error=WorkerError("OPERATION_UNSUPPORTED", "操作未注册", False),
        )

    @staticmethod
    def _result(
        request: WorkerRequest,
        value: tuple[dict | None, WorkerError | None],
    ) -> WorkerResponse:
        result, error = value
        return WorkerResponse(
            request_id=request.request_id,
            ok=error is None,
            result=result,
            error=error,
        )

    def _probe_document(self, request: WorkerRequest) -> WorkerResponse:
        raw_path = request.payload.get("path")
        if not isinstance(raw_path, str) or not raw_path:
            return WorkerResponse(
                request_id=request.request_id,
                ok=False,
                result=None,
                error=WorkerError("PATH_REQUIRED", "缺少待读取文件路径", False),
            )
        path = Path(raw_path)
        try:
            stat = path.stat()
        except FileNotFoundError:
            return WorkerResponse(
                request_id=request.request_id,
                ok=False,
                result=None,
                error=WorkerError("FILE_NOT_FOUND", "文件不存在或已经移动", False),
            )
        except PermissionError:
            return WorkerResponse(
                request_id=request.request_id,
                ok=False,
                result=None,
                error=WorkerError("FILE_PERMISSION_DENIED", "没有读取此文件的权限", True),
            )
        if not path.is_file():
            return WorkerResponse(
                request_id=request.request_id,
                ok=False,
                result=None,
                error=WorkerError("NOT_A_FILE", "目标不是普通文件", False),
            )
        return WorkerResponse(
            request_id=request.request_id,
            ok=True,
            result={
                "name": path.name,
                "extension": path.suffix.lower(),
                "size_bytes": stat.st_size,
                "readonly_operation": True,
            },
            error=None,
        )

    @staticmethod
    def _runtime_backend_probe(request: WorkerRequest) -> WorkerResponse:
        packages = {
            "onnxruntime": "onnxruntime",
            "rapidocr": "rapidocr",
            "sherpa_onnx": "sherpa-onnx",
        }
        loaded: dict[str, dict[str, str]] = {}
        try:
            for module_name, distribution_name in packages.items():
                module = importlib.import_module(module_name)
                version = getattr(module, "__version__", None)
                if not isinstance(version, str) or not version:
                    try:
                        version = importlib.metadata.version(distribution_name)
                    except importlib.metadata.PackageNotFoundError:
                        version = "bundled"
                loaded[module_name] = {
                    "version": version,
                    "module": str(getattr(module, "__file__", "bundled")),
                }
        except ImportError as error:
            return WorkerResponse(
                request_id=request.request_id,
                ok=False,
                result={"loaded": loaded},
                error=WorkerError(
                    "RUNTIME_BACKEND_UNAVAILABLE",
                    f"本地AI运行库不完整: {error}",
                    False,
                ),
            )
        return WorkerResponse(
            request_id=request.request_id,
            ok=True,
            result={"loaded": loaded},
            error=None,
        )
