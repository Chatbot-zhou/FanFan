from __future__ import annotations

import threading
import time
from pathlib import Path
from typing import Any

from .protocol import WorkerError


_IDLE_SECONDS = 60.0
_LOCK = threading.RLock()
_ENGINE: Any | None = None
_ENGINE_KEY: tuple[str, str, str, str, int] | None = None
_LAST_USED = 0.0


def _required_path(payload: dict[str, Any], key: str, suffixes: tuple[str, ...]) -> Path:
    value = payload.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{key} is required")
    path = Path(value).resolve(strict=True)
    if path.is_symlink() or not path.is_file() or path.suffix.lower() not in suffixes:
        raise ValueError(f"{key} is not a supported model file")
    return path


def _configuration(payload: dict[str, Any]) -> tuple[Path, Path, Path, Path, int, str]:
    model = _required_path(payload, "model_path", (".onnx",))
    detector = _required_path(payload, "det_model_path", (".onnx",))
    classifier = _required_path(payload, "cls_model_path", (".onnx",))
    dictionary = _required_path(payload, "dictionary_path", (".txt",))
    threads = payload.get("threads", 1)
    if not isinstance(threads, int) or isinstance(threads, bool) or threads < 1 or threads > 4:
        raise ValueError("threads must be between 1 and 4")
    ocr_version = payload.get("ocr_version", "PPOCRV5")
    if not isinstance(ocr_version, str):
        raise ValueError("ocr_version must be a string")
    ocr_version = ocr_version.strip()
    if ocr_version not in ("PPOCRV4", "PPOCRV5", "PPOCRV6"):
        raise ValueError("ocr_version must be one of PPOCRV4/PPOCRV5/PPOCRV6")
    return model, detector, classifier, dictionary, threads, ocr_version


def _model_type_for(ocr_version: str) -> Any:
    # RapidOCR 的 v6 路由只支持 TINY/SMALL/MEDIUM，MOBILE 会解析不到模型键；
    # v4/v5 则使用 MOBILE。此处按 ocr_version 动态选择，兼容新旧模型。
    from rapidocr import ModelType

    return ModelType.SMALL if ocr_version == "PPOCRV6" else ModelType.MOBILE


def _engine(payload: dict[str, Any]) -> Any:
    global _ENGINE, _ENGINE_KEY, _LAST_USED
    model, detector, classifier, dictionary, threads, ocr_version = _configuration(payload)
    key = (str(model), str(detector), str(classifier), str(dictionary), threads, ocr_version)
    with _LOCK:
        if _ENGINE is not None and _ENGINE_KEY == key:
            _LAST_USED = time.monotonic()
            return _ENGINE
        try:
            from rapidocr import EngineType, LangDet, LangRec, ModelType, OCRVersion, RapidOCR

            # ONNX Runtime is deliberately kept on the CPU here. OCR runs as a
            # low-priority background task and must not contend with the LLM/VLM
            # for the user's small GPU. The RuntimeManager caps it at 1-2 cores.
            version = OCRVersion[ocr_version]
            detect_type = _model_type_for(ocr_version)
            recog_type = _model_type_for(ocr_version)
            engine = RapidOCR(
                params={
                    "Det.engine_type": EngineType.ONNXRUNTIME,
                    "Det.lang_type": LangDet.CH,
                    "Det.model_type": detect_type,
                    "Det.ocr_version": version,
                    "Det.model_path": str(detector),
                    "Cls.engine_type": EngineType.ONNXRUNTIME,
                    "Cls.model_type": ModelType.MOBILE,
                    "Cls.ocr_version": OCRVersion.PPOCRV4,
                    "Cls.model_path": str(classifier),
                    "Rec.engine_type": EngineType.ONNXRUNTIME,
                    "Rec.lang_type": LangRec.CH,
                    "Rec.model_type": recog_type,
                    "Rec.ocr_version": version,
                    "Rec.model_path": str(model),
                    "Rec.rec_keys_path": str(dictionary),
                    "EngineConfig.onnxruntime.intra_op_num_threads": threads,
                    "EngineConfig.onnxruntime.inter_op_num_threads": 1,
                }
            )
        except ImportError as error:
            raise RuntimeError("RapidOCR or ONNX Runtime is not installed") from error
        _ENGINE = engine
        _ENGINE_KEY = key
        _LAST_USED = time.monotonic()
        return engine


def _display_version(payload: dict[str, Any]) -> str:
    # 展示用模型版本号由 ocr_version 推导；缺省回退 PP-OCRv5-mobile。
    ocr_version = payload.get("ocr_version", "PPOCRV5")
    if ocr_version == "PPOCRV6":
        return "PP-OCRv6-small"
    if ocr_version == "PPOCRV4":
        return "PP-OCRv4-mobile"
    return "PP-OCRv5-mobile"


def _normalised_result(
    result: Any,
    page_no: int,
    image_size: tuple[int, int] | None = None,
    model_version: str = "PP-OCRv5-mobile",
) -> dict[str, Any]:
    image = getattr(result, "img", None)
    height = int(getattr(image, "shape", (0, 0))[0]) if image is not None else 0
    width = int(getattr(image, "shape", (0, 0))[1]) if image is not None else 0
    if (width <= 0 or height <= 0) and image_size is not None:
        width, height = image_size
    if width <= 0 or height <= 0:
        raise ValueError("RapidOCR response is missing image dimensions")
    raw_boxes = getattr(result, "boxes", None)
    raw_texts = getattr(result, "txts", None)
    raw_scores = getattr(result, "scores", None)
    boxes = [] if raw_boxes is None else list(raw_boxes)
    texts = [] if raw_texts is None else list(raw_texts)
    scores = [] if raw_scores is None else list(raw_scores)
    if len(boxes) != len(texts) or len(scores) != len(texts):
        raise ValueError("RapidOCR response arrays have different lengths")
    lines: list[dict[str, Any]] = []
    for box, text, score in zip(boxes, texts, scores, strict=True):
        clean_text = str(text or "").strip()
        points = [[float(point[0]), float(point[1])] for point in box]
        if not clean_text or len(points) != 4:
            continue
        xs = [point[0] for point in points]
        ys = [point[1] for point in points]
        polygon = [
            {"x": max(0.0, min(1.0, x / width)), "y": max(0.0, min(1.0, y / height))}
            for x, y in points
        ]
        lines.append(
            {
                "page_no": page_no,
                "text": clean_text,
                "confidence": max(0.0, min(1.0, float(score))),
                "polygon": polygon,
                "bbox": {
                    "x0": max(0.0, min(1.0, min(xs) / width)),
                    "y0": max(0.0, min(1.0, min(ys) / height)),
                    "x1": max(0.0, min(1.0, max(xs) / width)),
                    "y1": max(0.0, min(1.0, max(ys) / height)),
                },
            }
        )
    return {
        "engine": "rapidocr-onnxruntime",
        "model_version": model_version,
        "page_count": 1,
        "lines": lines,
        "elapsed_ms": int(float(getattr(result, "elapse", 0.0) or 0.0) * 1000),
    }


# 单页识别瞬时失败（onnx 会话竞争、资源抖动、IO 忙）重试两次再上报；
# 引擎级缺陷（模型缺失、输入非法）不可重试。页级识别实测 ~6s，重试
# 成本在解析时间预算（OCR_TIME_BUDGET_SECONDS）内可控。
_RECOGNIZE_RETRY_BACKOFF_SECONDS = (0.5, 1.0)


def recognize_image(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    result, error = _recognize_image_once(payload)
    if error is not None and error.retryable:
        for backoff in _RECOGNIZE_RETRY_BACKOFF_SECONDS:
            time.sleep(backoff)
            result, error = _recognize_image_once(payload)
            if error is None or not error.retryable:
                break
    return result, error


def _recognize_image_once(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    source_value = payload.get("image_path")
    page_no = payload.get("page_no", 1)
    if not isinstance(source_value, str) or not isinstance(page_no, int) or page_no < 1:
        return None, WorkerError("OCR_INPUT_INVALID", "OCR image path or page number is invalid", False)
    try:
        source = Path(source_value).resolve(strict=True)
        if source.is_symlink() or not source.is_file() or source.stat().st_size > 128 * 1024 * 1024:
            raise ValueError("OCR input is not a safe local image")
        before = source.stat()
        from PIL import Image

        with Image.open(source) as image:
            image_size = tuple(int(value) for value in image.size)
        result = _engine(payload)(str(source), use_det=True, use_cls=True, use_rec=True)
        after = source.stat()
        if (before.st_size, before.st_mtime_ns) != (after.st_size, after.st_mtime_ns):
            return None, WorkerError("FILE_CHANGED_DURING_PARSE", "The source image changed during OCR", True)
        return _normalised_result(
            result,
            page_no,
            image_size,
            model_version=_display_version(payload),
        ), None
    except FileNotFoundError:
        return None, WorkerError("OCR_INPUT_INVALID", "OCR input or model file does not exist", False)
    except (OSError, ValueError) as error:
        return None, WorkerError("OCR_MODEL_INVALID", str(error), False)
    except RuntimeError as error:
        return None, WorkerError("OCR_RUNTIME_UNAVAILABLE", str(error), True)
    except Exception as error:
        return None, WorkerError("OCR_RECOGNITION_FAILED", str(error), True)


def self_test_ocr(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    try:
        import numpy as np

        result = _engine(payload)(np.full((64, 192, 3), 255, dtype=np.uint8), use_det=True, use_cls=True, use_rec=True)
        _normalised_result(result, 1, (192, 64), model_version=_display_version(payload))
        return {
            "status": "ready",
            "engine": "rapidocr-onnxruntime",
            "model_version": _display_version(payload),
            "duration_ms": int(float(getattr(result, "elapse", 0.0) or 0.0) * 1000),
        }, None
    except FileNotFoundError:
        return None, WorkerError("OCR_MODEL_INVALID", "OCR model package is incomplete", False)
    except (OSError, ValueError) as error:
        return None, WorkerError("OCR_MODEL_INVALID", str(error), False)
    except RuntimeError as error:
        return None, WorkerError("OCR_RUNTIME_UNAVAILABLE", str(error), True)
    except Exception as error:
        return None, WorkerError("OCR_SELF_TEST_FAILED", str(error), True)


def clear_ocr_session() -> int:
    global _ENGINE, _ENGINE_KEY, _LAST_USED
    with _LOCK:
        cleared = int(_ENGINE is not None)
        _ENGINE = None
        _ENGINE_KEY = None
        _LAST_USED = 0.0
        return cleared


def ocr_cache_snapshot() -> dict[str, Any]:
    with _LOCK:
        return {
            "loaded": _ENGINE is not None,
            "idle_seconds": round(max(0.0, time.monotonic() - _LAST_USED), 3) if _ENGINE is not None else None,
            "capacity": 1,
        }


def _reap_loop() -> None:
    while True:
        time.sleep(10.0)
        with _LOCK:
            expired = _ENGINE is not None and time.monotonic() - _LAST_USED >= _IDLE_SECONDS
        if expired:
            clear_ocr_session()


threading.Thread(target=_reap_loop, name="fanfan-ocr-reaper", daemon=True).start()
