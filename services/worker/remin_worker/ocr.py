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
    render_pages_dir: Path | None = None,
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
    command = [
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
    ]
    if render_pages_dir is not None:
        if source_kind != "pdf":
            return None, WorkerError("OCR_INPUT_INVALID", "只有PDF OCR可以请求页面渲染缓存", False)
        render_pages_dir.mkdir(parents=True, exist_ok=True)
        command.extend(["-AssetCacheDir", str(render_pages_dir)])
    try:
        completed = subprocess.run(
            command,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            # Budget must stay below the Rust side's document.parse timeout
            # (worker.rs) so the worker is never killed mid-OCR.
            timeout=min(270, 30 + page_limit * 8),
            creationflags=creation_flags,
            check=False,
        )
    except subprocess.TimeoutExpired:
        return None, WorkerError("OCR_TIMEOUT", "OCR处理超过资源预算，已安全停止", True)
    except OSError as error:
        return None, WorkerError("OCR_RUNTIME_UNAVAILABLE", str(error), True)
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip()[-1000:]
        stderr_text = completed.stderr or ""
        if "OCR_LANGUAGE_PACK_MISSING" in stderr_text:
            return None, WorkerError(
                "OCR_RUNTIME_UNAVAILABLE",
                "未安装Windows OCR语言包，请在系统设置中添加简体中文语言包后重试",
                False,
            )
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
    if render_pages_dir is not None:
        try:
            result["rendered_pages"] = _validated_rendered_pages(
                result.get("rendered_pages"), render_pages_dir, set(selected_pages)
            )
        except (OSError, ValueError) as error:
            return None, WorkerError("OCR_RESPONSE_INVALID", str(error), False)
    return result, None


def _validated_rendered_pages(
    value: Any,
    render_pages_dir: Path,
    selected_pages: set[int],
) -> list[dict[str, Any]]:
    if not isinstance(value, list):
        raise ValueError("Windows OCR响应缺少扫描页渲染清单")
    root = render_pages_dir.resolve(strict=True)
    output: list[dict[str, Any]] = []
    seen: set[int] = set()
    for item in value:
        if not isinstance(item, dict):
            raise ValueError("扫描页渲染记录必须是对象")
        page_no = item.get("page_no")
        raw_path = item.get("path")
        if (
            not isinstance(page_no, int)
            or page_no not in selected_pages
            or page_no in seen
            or not isinstance(raw_path, str)
        ):
            raise ValueError("扫描页渲染页码或路径无效")
        candidate = Path(raw_path)
        if candidate.is_symlink():
            raise ValueError("扫描页渲染缓存不得是符号链接")
        resolved = candidate.resolve(strict=True)
        try:
            resolved.relative_to(root)
        except ValueError as error:
            raise ValueError("扫描页渲染缓存超出应用临时目录") from error
        metadata = resolved.stat()
        if (
            not resolved.is_file()
            or resolved.suffix.lower() != ".png"
            or metadata.st_size == 0
            or metadata.st_size > 64 * 1024 * 1024
        ):
            raise ValueError("扫描页渲染缓存格式或大小无效")
        seen.add(page_no)
        output.append({"page_no": page_no, "path": str(resolved), "mime_type": "image/png"})
    if seen != selected_pages:
        raise ValueError("扫描页渲染清单与OCR页码不一致")
    output.sort(key=lambda item: item["page_no"])
    return output
