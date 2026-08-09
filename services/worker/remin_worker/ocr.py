from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from typing import Any

from .protocol import WorkerError


def recognize_with_windows(
    source_path: Path,
    source_kind: str,
    max_pages: int | None,
    language_hints: tuple[str, ...],
    page_numbers: list[int] | None = None,
) -> tuple[dict[str, Any] | None, WorkerError | None]:
    if os.name != "nt":
        return None, WorkerError("OCR_RUNTIME_UNAVAILABLE", "Windows OCR只在Windows桌面版可用", False)
    if source_kind not in {"image", "pdf"} or not source_path.is_file():
        return None, WorkerError("OCR_INPUT_INVALID", "OCR输入文件或格式无效", False)
    script_path = Path(__file__).with_name("windows_ocr.ps1")
    if not script_path.is_file():
        return None, WorkerError("OCR_RUNTIME_UNAVAILABLE", "Windows OCR脚本未随Worker安装", False)
    language = "zh-Hans-CN" if any(value.lower().startswith("zh") for value in language_hints) else "en-US"
    page_limit = min(max_pages or 50, 200)
    selected_pages = sorted(set(page_numbers or []))
    if any(page < 1 or page > page_limit for page in selected_pages):
        return None, WorkerError("OCR_INPUT_INVALID", "OCR页码超出本次文档范围", False)
    creation_flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    before = source_path.stat()
    try:
        completed = subprocess.run(
            [
                "powershell.exe",
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                str(script_path),
                "-SourcePath",
                str(source_path),
                "-SourceKind",
                source_kind,
                "-MaxPages",
                str(page_limit),
                "-LanguageTag",
                language,
                "-PageNumbers",
                ",".join(str(page) for page in selected_pages),
            ],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=min(600, 30 + page_limit * 12),
            creationflags=creation_flags,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return None, WorkerError("OCR_TIMEOUT", "OCR处理超过资源预算，已安全停止", True)
    except OSError as error:
        return None, WorkerError("OCR_RUNTIME_UNAVAILABLE", str(error), True)
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()[-1000:]
        return None, WorkerError("OCR_RECOGNITION_FAILED", detail or "Windows OCR执行失败", True)
    after = source_path.stat()
    if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
        return None, WorkerError("FILE_CHANGED_DURING_PARSE", "OCR期间源文件发生变化，请稍后重试", True)
    try:
        result = json.loads(completed.stdout.lstrip("\ufeff").strip())
    except (json.JSONDecodeError, TypeError) as error:
        return None, WorkerError("OCR_RESPONSE_INVALID", str(error), False)
    lines = result.get("lines")
    if not isinstance(lines, list):
        return None, WorkerError("OCR_RESPONSE_INVALID", "Windows OCR响应缺少文本行", False)
    return result, None
