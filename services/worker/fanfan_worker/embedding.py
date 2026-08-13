from __future__ import annotations

import math
from pathlib import Path
from typing import Any

from .protocol import WorkerError
from .runtime_cache import get_onnx_session


def encode_texts(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    model_path_value = payload.get("model_path")
    texts = payload.get("texts")
    max_length = payload.get("max_length", 512)
    threads = payload.get("threads", 2)
    if not isinstance(model_path_value, str) or not model_path_value:
        return None, WorkerError("EMBEDDING_MODEL_PATH_REQUIRED", "缺少ONNX向量模型路径", False)
    model_path = Path(model_path_value)
    if not model_path.is_file() or model_path.suffix.lower() != ".onnx":
        return None, WorkerError("EMBEDDING_MODEL_UNAVAILABLE", "ONNX向量模型不可用", False)
    if not isinstance(texts, list) or not 1 <= len(texts) <= 32 or any(not isinstance(text, str) or not text.strip() or len(text) > 8_000 for text in texts):
        return None, WorkerError("EMBEDDING_INPUT_INVALID", "向量编码每批需要1到32条非空文本，单条不超过8000字符", False)
    if not isinstance(max_length, int) or not 16 <= max_length <= 1024:
        return None, WorkerError("EMBEDDING_INPUT_INVALID", "max_length必须在16到1024之间", False)
    if not isinstance(threads, int) or not 1 <= threads <= 8:
        return None, WorkerError("EMBEDDING_INPUT_INVALID", "线程数必须在1到8之间", False)
    tokenizer_path_value = payload.get("tokenizer_path")
    tokenizer_path = Path(tokenizer_path_value) if isinstance(tokenizer_path_value, str) and tokenizer_path_value else model_path.parent / "tokenizer.json"
    if not tokenizer_path.is_file():
        return None, WorkerError("EMBEDDING_TOKENIZER_UNAVAILABLE", "模型目录缺少tokenizer.json", False)
    try:
        import numpy as np
        from tokenizers import Tokenizer
    except ImportError as error:
        return None, WorkerError("EMBEDDING_RUNTIME_MISSING", f"本地向量运行依赖不可用：{error}", True)
    try:
        tokenizer = Tokenizer.from_file(str(tokenizer_path))
        tokenizer.enable_truncation(max_length=max_length)
        tokenizer.enable_padding()
        encoded = tokenizer.encode_batch(texts)
        input_ids = np.asarray([item.ids for item in encoded], dtype=np.int64)
        attention_mask = np.asarray([item.attention_mask for item in encoded], dtype=np.int64)
        type_ids = np.asarray([item.type_ids for item in encoded], dtype=np.int64)
        session = get_onnx_session(model_path, threads)
        available_inputs = {item.name for item in session.get_inputs()}
        feed: dict[str, Any] = {}
        for name in available_inputs:
            lowered = name.lower()
            if "attention" in lowered:
                feed[name] = attention_mask
            elif "token_type" in lowered or "segment" in lowered:
                feed[name] = type_ids
            elif "input" in lowered and "id" in lowered:
                feed[name] = input_ids
        if not feed:
            return None, WorkerError("EMBEDDING_MODEL_INCOMPATIBLE", "无法识别向量模型输入字段", False)
        output = session.run(None, feed)[0]
        array = np.asarray(output, dtype=np.float32)
        if array.ndim == 3:
            mask = attention_mask[..., None].astype(np.float32)
            array = (array * mask).sum(axis=1) / np.clip(mask.sum(axis=1), 1e-9, None)
        elif array.ndim != 2:
            return None, WorkerError("EMBEDDING_MODEL_INCOMPATIBLE", "向量模型输出维度不受支持", False)
        norms = np.linalg.norm(array, axis=1, keepdims=True)
        array = array / np.clip(norms, 1e-12, None)
        vectors = array.tolist()
        dimension = int(array.shape[1])
        if dimension <= 0 or any(not math.isfinite(float(value)) for vector in vectors for value in vector):
            return None, WorkerError("EMBEDDING_OUTPUT_INVALID", "向量模型返回了无效数值", False)
        return {"vectors": vectors, "dimension": dimension, "model_path": str(model_path), "tokenizer_path": str(tokenizer_path)}, None
    except Exception as error:
        return None, WorkerError("EMBEDDING_INFERENCE_FAILED", str(error), True)
