from __future__ import annotations

import math
from pathlib import Path
from typing import Any

from .protocol import WorkerError


def rerank_documents(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    model_path_value = payload.get("model_path")
    query = payload.get("query")
    documents = payload.get("documents")
    max_length = payload.get("max_length", 512)
    threads = payload.get("threads", 2)
    if not isinstance(model_path_value, str) or not model_path_value:
        return None, WorkerError("RERANK_MODEL_PATH_REQUIRED", "缺少ONNX重排模型路径", False)
    model_path = Path(model_path_value)
    if not model_path.is_file() or model_path.suffix.lower() != ".onnx":
        return None, WorkerError("RERANK_MODEL_UNAVAILABLE", "ONNX重排模型不可用", False)
    if not isinstance(query, str) or not query.strip() or len(query) > 2_000:
        return None, WorkerError("RERANK_INPUT_INVALID", "重排问题为空或超过2000字符", False)
    if not isinstance(documents, list) or not 1 <= len(documents) <= 30 or any(
        not isinstance(document, str) or not document.strip() or len(document) > 12_000
        for document in documents
    ):
        return None, WorkerError("RERANK_INPUT_INVALID", "重排每批需要1到30条非空证据，单条不超过12000字符", False)
    if not isinstance(max_length, int) or not 32 <= max_length <= 1024:
        return None, WorkerError("RERANK_INPUT_INVALID", "max_length必须在32到1024之间", False)
    if not isinstance(threads, int) or not 1 <= threads <= 8:
        return None, WorkerError("RERANK_INPUT_INVALID", "线程数必须在1到8之间", False)
    tokenizer_path_value = payload.get("tokenizer_path")
    tokenizer_path = Path(tokenizer_path_value) if isinstance(tokenizer_path_value, str) and tokenizer_path_value else model_path.parent / "tokenizer.json"
    if not tokenizer_path.is_file():
        return None, WorkerError("RERANK_TOKENIZER_UNAVAILABLE", "重排模型目录缺少tokenizer.json", False)
    try:
        import numpy as np
        import onnxruntime as ort
        from tokenizers import Tokenizer
    except ImportError as error:
        return None, WorkerError("RERANK_RUNTIME_MISSING", f"本地重排运行依赖不可用：{error}", True)
    try:
        tokenizer = Tokenizer.from_file(str(tokenizer_path))
        tokenizer.enable_truncation(max_length=max_length)
        tokenizer.enable_padding()
        encoded = tokenizer.encode_batch([(query, document) for document in documents])
        input_ids = np.asarray([item.ids for item in encoded], dtype=np.int64)
        attention_mask = np.asarray([item.attention_mask for item in encoded], dtype=np.int64)
        type_ids = np.asarray([item.type_ids for item in encoded], dtype=np.int64)
        options = ort.SessionOptions()
        options.intra_op_num_threads = threads
        options.inter_op_num_threads = 1
        session = ort.InferenceSession(str(model_path), sess_options=options, providers=["CPUExecutionProvider"])
        feed: dict[str, Any] = {}
        for item in session.get_inputs():
            lowered = item.name.lower()
            if "attention" in lowered:
                feed[item.name] = attention_mask
            elif "token_type" in lowered or "segment" in lowered:
                feed[item.name] = type_ids
            elif "input" in lowered and "id" in lowered:
                feed[item.name] = input_ids
        if not feed:
            return None, WorkerError("RERANK_MODEL_INCOMPATIBLE", "无法识别重排模型输入字段", False)
        logits = np.asarray(session.run(None, feed)[0], dtype=np.float32)
        if logits.ndim == 1:
            raw_scores = logits
        elif logits.ndim == 2 and logits.shape[1] == 1:
            raw_scores = logits[:, 0]
        elif logits.ndim == 2 and logits.shape[1] >= 2:
            shifted = logits - np.max(logits, axis=1, keepdims=True)
            probabilities = np.exp(shifted) / np.clip(np.exp(shifted).sum(axis=1, keepdims=True), 1e-12, None)
            scores = probabilities[:, -1].tolist()
            if len(scores) != len(documents) or any(not math.isfinite(float(score)) for score in scores):
                return None, WorkerError("RERANK_OUTPUT_INVALID", "重排模型返回了无效分数", False)
            return {"scores": scores, "model_path": str(model_path), "tokenizer_path": str(tokenizer_path)}, None
        else:
            return None, WorkerError("RERANK_MODEL_INCOMPATIBLE", "重排模型输出维度不受支持", False)
        scores = (1.0 / (1.0 + np.exp(-np.clip(raw_scores, -30.0, 30.0)))).tolist()
        if len(scores) != len(documents) or any(not math.isfinite(float(score)) for score in scores):
            return None, WorkerError("RERANK_OUTPUT_INVALID", "重排模型返回了无效分数", False)
        return {"scores": scores, "model_path": str(model_path), "tokenizer_path": str(tokenizer_path)}, None
    except Exception as error:
        return None, WorkerError("RERANK_INFERENCE_FAILED", str(error), True)
