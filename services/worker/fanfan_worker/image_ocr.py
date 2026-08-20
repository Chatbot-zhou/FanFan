from __future__ import annotations

import time
from dataclasses import asdict
from pathlib import Path
from typing import Any

from .ocr import recognize_with_windows
from .paddle_ocr import recognize_image
from .protocol import WorkerError


def _lines(value: dict[str, Any] | None) -> list[dict[str, Any]]:
    if not isinstance(value, dict) or not isinstance(value.get("lines"), list):
        return []
    return [line for line in value["lines"] if isinstance(line, dict)]


def _text(value: dict[str, Any] | None) -> str | None:
    joined = "\n".join(
        str(line.get("text", "")).strip() for line in _lines(value)
    ).strip()
    return joined or None


def _confidence(value: dict[str, Any] | None) -> float | None:
    values = [
        float(line["confidence"])
        for line in _lines(value)
        if isinstance(line.get("confidence"), (int, float))
        and not isinstance(line.get("confidence"), bool)
    ]
    return sum(values) / len(values) if values else None


def _fallback_reason(
    result: dict[str, Any] | None,
    error: WorkerError | None,
    threshold: float,
) -> str | None:
    if error is not None:
        return error.code.lower()
    if _text(result) is None:
        return "ocr_no_text"
    confidence = _confidence(result)
    if confidence is None:
        return "ocr_confidence_missing"
    if confidence < threshold:
        return "ocr_low_confidence"
    return None


def complex_visual_reason(
    asset_kind: str,
    result: dict[str, Any],
    text: str,
) -> str | None:
    compact = "".join(text.split())
    lines = _lines(result)
    if asset_kind != "pdf_scanned_page" and len(compact) < 48:
        return "complex_sparse_text"
    if lines:
        average_line = sum(len(str(line.get("text", "")).strip()) for line in lines) / len(lines)
        digit_ratio = sum(character.isdigit() for character in compact) / max(1, len(compact))
        if len(lines) >= 3 and digit_ratio >= 0.20 and len(compact) <= 600:
            return "complex_chart_like"
        if len(lines) >= 4 and average_line <= 12 and len(compact) <= 400:
            return "complex_layout"
    return None


def _attempt(
    engine: str,
    model_version: str | None,
    result: dict[str, Any] | None,
    error: WorkerError | None,
    fallback_reason: str | None,
    started_at: float,
    page_no: int,
) -> dict[str, Any]:
    text = _text(result)
    status = "failed" if error is not None else "completed" if text else "no_text"
    return {
        "engine": engine,
        "model_version": model_version,
        "status": status,
        "page_no": page_no,
        "confidence": _confidence(result),
        "fallback_reason": fallback_reason,
        "elapsed_ms": int((time.monotonic() - started_at) * 1000),
        "error": asdict(error) if error is not None else None,
    }


def route_image_ocr(
    payload: dict[str, Any],
) -> tuple[dict[str, Any] | None, WorkerError | None]:
    started_at = time.monotonic()
    threshold = payload.get("confidence_threshold", 0.45)
    page_no = payload.get("page_no", 1)
    asset_kind = payload.get("asset_kind", "embedded_image")
    image_path = payload.get("image_path")
    if (
        not isinstance(threshold, (int, float))
        or isinstance(threshold, bool)
        or not 0.0 <= float(threshold) <= 1.0
        or not isinstance(page_no, int)
        or page_no < 1
        or not isinstance(asset_kind, str)
        or not asset_kind
        or not isinstance(image_path, str)
    ):
        return None, WorkerError("OCR_INPUT_INVALID", "图片OCR路由参数无效", False)

    threshold = float(threshold)
    primary_started = time.monotonic()
    primary, primary_error = recognize_image(payload)
    primary_reason = _fallback_reason(primary, primary_error, threshold)
    attempts = [
        _attempt(
            "rapidocr-onnxruntime",
            str(primary.get("model_version")) if isinstance(primary, dict) and primary.get("model_version") else None,
            primary,
            primary_error,
            primary_reason,
            primary_started,
            page_no,
        )
    ]

    if primary_reason is None and primary is not None:
        text = _text(primary)
        assert text is not None
        complex_reason = complex_visual_reason(asset_kind, primary, text)
        return {
            "ocr_text": text,
            "confidence": _confidence(primary),
            "engine": "rapidocr-onnxruntime",
            "model_version": primary.get("model_version"),
            "vision_required": complex_reason is not None,
            "route_reason": complex_reason or "ocr_success",
            "attempts": attempts,
            "elapsed_ms": int((time.monotonic() - started_at) * 1000),
        }, None

    fallback_started = time.monotonic()
    fallback, fallback_error = recognize_with_windows(
        Path(image_path), "image", 1, ("zh",), page_numbers=[1]
    )
    fallback_text = _text(fallback)
    attempts.append(
        _attempt(
            "windows-ocr",
            "Windows.Media.Ocr",
            fallback,
            fallback_error,
            primary_reason,
            fallback_started,
            page_no,
        )
    )
    retained_text = fallback_text or _text(primary)
    route_reason = (
        fallback_error.code.lower()
        if fallback_error is not None
        else "ocr_no_text"
        if retained_text is None
        else primary_reason or "ocr_confidence_missing"
    )
    return {
        "ocr_text": retained_text,
        "confidence": None if fallback_text is not None else _confidence(primary),
        "engine": "windows-ocr" if fallback_text is not None else "rapidocr-onnxruntime",
        "model_version": "Windows.Media.Ocr" if fallback_text is not None else (
            primary.get("model_version") if isinstance(primary, dict) else None
        ),
        "vision_required": True,
        "route_reason": route_reason,
        "attempts": attempts,
        "elapsed_ms": int((time.monotonic() - started_at) * 1000),
    }, None
