from __future__ import annotations

from pathlib import Path

from .protocol import WorkerError, WorkerRequest, WorkerResponse
from .parsing import ParseRequest, parse_document
from .embedding import encode_texts
from .exporting import export_table


class WorkerService:
    """只执行已注册、无副作用的最小操作。"""

    def handle(self, request: WorkerRequest) -> WorkerResponse:
        if request.operation == "health.check":
            return WorkerResponse(
                request_id=request.request_id,
                ok=True,
                result={"status": "ready", "protocol_version": "1.0"},
                error=None,
            )
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
