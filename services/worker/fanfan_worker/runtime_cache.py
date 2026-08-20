from __future__ import annotations

from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path
from threading import Event, RLock, Thread
from time import monotonic
from typing import Any


# 会话级空闲回收阈值。与 Rust 侧 WorkerRole::Onnx 的进程 idle_timeout 保持
# 一致（都是 300 秒）：embedding/rerank 会话空闲回收会让下一次搜索冷启动
# （实测数百毫秒到数秒），因此与进程驻留对齐；OCR/ASR 等大模型不共用本缓存。
SESSION_IDLE_SECONDS = 300.0
MAX_SESSIONS = 2

# 推理设备策略：资源满足时优先 GPU，显存不足时回退 CPU（规划书 2/7 章）。
# onnxruntime-gpu 提供 CUDAExecutionProvider；CUDA 初始化失败时 ORT 非 strict
# 模式自动回退 CPU，不会破坏当前任务。当前随安装包提供的 INT8 量化
# Embedding/Rerank 模型在目标机实测 CPU 更快，因此默认受性能保护留在 CPU；
# 新的非量化/浮点 ONNX 模型仍按 CUDA -> CPU 顺序选择。
# 所以除"CUDA 可用"外还必须探测模型类型与显存余量。
# 环境变量 FANFAN_WORKER_PROVIDERS 可强制覆盖（测试/回退）；
# FANFAN_WORKER_GPU_MIN_FREE_MB 可调整显存余量阈值（默认 1536）。
_ENABLED_PROVIDERS = ["CUDAExecutionProvider", "CPUExecutionProvider"]
_GPU_MIN_FREE_MB = 1536
_GPU_FAILURE_COOLDOWN_SECONDS = 60.0
_gpu_disabled_until = 0.0


def _probe_gpu_free_mb() -> int | None:
    """通过 nvidia-smi 探测当前可用显存（MiB）。探测失败返回 None（不阻塞 GPU 决策）。"""
    import os
    import shutil
    import subprocess

    candidates: list[str] = []
    which = shutil.which("nvidia-smi")
    if which:
        candidates.append(which)
    candidates.append(os.path.join(os.environ.get("SystemRoot", r"C:\Windows"), "System32", "nvidia-smi.exe"))
    for exe in candidates:
        if not os.path.isfile(exe):
            continue
        try:
            result = subprocess.run(
                [exe, "--query-gpu=memory.free", "--format=csv,noheader,nounits"],
                capture_output=True,
                text=True,
                timeout=5,
            )
            if result.returncode == 0 and result.stdout.strip():
                return int(result.stdout.strip().splitlines()[0].strip())
        except Exception:
            continue
    return None


def _cudnn_loadable() -> bool:
    """验证 CUDA EP 运行时依赖 cudnn 是否真的可加载。

    onnxruntime-gpu 的 CUDA EP 在 cudnn_graph64_9.dll 缺失/不在库路径时不会
    回退——它直接 abort 整个进程（实测 2026-08：dll 在 site-packages 但不在
    进程搜索路径，ORT 打印 "Could not locate cudnn_graph64_9.dll" 后崩溃）。
    这里在创建 session 前按名字探测主符号，加载不到就提前走 CPU。
    """
    import ctypes

    try:
        graph = ctypes.WinDLL("cudnn_graph64_9.dll")
        getattr(graph, "cudnnCreate")
        return True
    except (OSError, AttributeError):
        return False


def _prepare_cuda_runtime(ort: Any) -> bool:
    """Load bundled CUDA/cuDNN DLLs before ORT creates a CUDA session."""
    preload = getattr(ort, "preload_dlls", None)
    if callable(preload):
        try:
            preload(directory="")
        except Exception:
            return False
    return _cudnn_loadable()


def _resolve_providers(
    *, force_cpu: bool = False, gpu_min_free_mb: int | None = None
) -> tuple[list[str], str | None]:
    import os

    if force_cpu:
        return ["CPUExecutionProvider"], "cpu_fallback"
    override = os.environ.get("FANFAN_WORKER_PROVIDERS")
    if override:
        return [p.strip() for p in override.split(",") if p.strip()], "env_override"
    if monotonic() < _gpu_disabled_until:
        return ["CPUExecutionProvider"], "cuda_failure_cooldown"
    try:
        import onnxruntime as ort

        if "CUDAExecutionProvider" not in ort.get_available_providers():
            return ["CPUExecutionProvider"], "no_cuda"
        configured_min_free = int(
            os.environ.get("FANFAN_WORKER_GPU_MIN_FREE_MB", _GPU_MIN_FREE_MB)
        )
        min_free = gpu_min_free_mb or configured_min_free
        free_mb = _probe_gpu_free_mb()
        if free_mb is not None and free_mb < min_free:
            return ["CPUExecutionProvider"], f"gpu_memory_low:{free_mb}MiB"
        if not _prepare_cuda_runtime(ort):
            return ["CPUExecutionProvider"], "cudnn_unavailable"
    except Exception:
        return ["CPUExecutionProvider"], "cuda_probe_failed"
    return _ENABLED_PROVIDERS, None


def _model_cpu_preference_reason(model_path: Path) -> str | None:
    """Keep known CPU-optimised INT8 artifacts off CUDA unless explicitly overridden.

    FanFan catalog artifacts use ``model_quantized.onnx`` for the current INT8
    embedding/rerank pair. On the target machine both models were consistently
    faster on CPU because CUDA inserted many host/device copy nodes. An explicit
    FANFAN_WORKER_PROVIDERS value remains the diagnostic/advanced-user override.
    """
    import os

    if os.environ.get("FANFAN_WORKER_PROVIDERS"):
        return None
    if "quantized" in model_path.name.casefold():
        return "quantized_model_cpu_preferred"
    return None


@dataclass(slots=True)
class CachedSession:
    session: Any
    last_used: float
    threads: int
    execution_provider: str
    fallback_reason: str | None


@dataclass(slots=True)
class SessionHandle:
    session: Any
    key: tuple[str, int, str]
    execution_provider: str
    device: str
    fallback_reason: str | None


_sessions: OrderedDict[tuple[str, int, str], CachedSession] = OrderedDict()
_sessions_lock = RLock()
_reaper_stop = Event()


def _session_handle(
    key: tuple[str, int, str], cached: CachedSession
) -> SessionHandle:
    provider = cached.execution_provider
    return SessionHandle(
        session=cached.session,
        key=key,
        execution_provider=provider,
        device="cuda" if provider == "CUDAExecutionProvider" else "cpu",
        fallback_reason=cached.fallback_reason,
    )


def _disable_gpu_temporarily() -> None:
    global _gpu_disabled_until
    _gpu_disabled_until = monotonic() + _GPU_FAILURE_COOLDOWN_SECONDS


def get_onnx_session(
    model_path: Path,
    threads: int,
    *,
    force_cpu: bool = False,
    gpu_min_free_mb: int | None = None,
    fallback_reason: str | None = None,
) -> SessionHandle:
    """Return a reusable non-spinning ONNX session with explicit GPU fallback.

    The worker protocol is serialized, so a process-local LRU provides safe reuse
    without a second lock or a pool of competing ONNX thread groups.
    """

    import onnxruntime as ort

    now = monotonic()
    model_preference = None if force_cpu else _model_cpu_preference_reason(model_path)
    providers, reason = _resolve_providers(
        force_cpu=force_cpu or model_preference is not None,
        gpu_min_free_mb=gpu_min_free_mb,
    )
    requested_provider = providers[0]
    reason = fallback_reason or model_preference or reason
    key = (str(model_path.resolve()), threads, requested_provider)
    with _sessions_lock:
        _evict_idle(now)
        cached = _sessions.pop(key, None)
        if cached is not None:
            cached.last_used = now
            _sessions[key] = cached
            return _session_handle(key, cached)

    options = ort.SessionOptions()
    options.intra_op_num_threads = threads
    options.inter_op_num_threads = 1
    options.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    options.enable_cpu_mem_arena = True
    options.enable_mem_pattern = True
    options.add_session_config_entry("session.intra_op.allow_spinning", "0")
    options.add_session_config_entry("session.inter_op.allow_spinning", "0")
    try:
        session = ort.InferenceSession(
            str(model_path),
            sess_options=options,
            providers=providers,
        )
    except Exception:
        if requested_provider != "CUDAExecutionProvider":
            raise
        _disable_gpu_temporarily()
        return get_onnx_session(
            model_path,
            threads,
            force_cpu=True,
            fallback_reason="cuda_session_failed",
        )
    active_providers = session.get_providers()
    execution_provider = (
        active_providers[0] if active_providers else "CPUExecutionProvider"
    )
    if (
        requested_provider == "CUDAExecutionProvider"
        and execution_provider != "CUDAExecutionProvider"
    ):
        reason = "cuda_provider_fell_back"
        _disable_gpu_temporarily()
        key = (str(model_path.resolve()), threads, "CPUExecutionProvider")
    if reason:
        import logging

        logging.getLogger("fanfan.worker").warning(
            "inference provider decision: %s (reason=%s)", ",".join(providers), reason
        )
    with _sessions_lock:
        cached = CachedSession(
            session=session,
            last_used=now,
            threads=threads,
            execution_provider=execution_provider,
            fallback_reason=reason,
        )
        _sessions[key] = cached
        while len(_sessions) > MAX_SESSIONS:
            _sessions.popitem(last=False)
    return _session_handle(key, cached)


def run_with_cpu_fallback(
    handle: SessionHandle,
    model_path: Path,
    threads: int,
    feed: dict[str, Any],
) -> tuple[Any, SessionHandle]:
    try:
        return handle.session.run(None, feed)[0], handle
    except Exception:
        if handle.device != "cuda":
            raise
        with _sessions_lock:
            _sessions.pop(handle.key, None)
        _disable_gpu_temporarily()
        cpu = get_onnx_session(
            model_path,
            threads,
            force_cpu=True,
            fallback_reason="cuda_inference_failed",
        )
        return cpu.session.run(None, feed)[0], cpu


def cache_snapshot() -> dict[str, Any]:
    now = monotonic()
    with _sessions_lock:
        _evict_idle(now)
        providers, reason = _resolve_providers()
        return {
            "backend": "onnxruntime",
            "providers": providers,
            "provider_reason": reason,
            "gpu_free_mb": _probe_gpu_free_mb(),
            "session_count": len(_sessions),
            "max_sessions": MAX_SESSIONS,
            "idle_timeout_seconds": int(SESSION_IDLE_SECONDS),
            "sessions": [
                {
                    "model_name": Path(key[0]).name,
                    "threads": cached.threads,
                    "execution_provider": cached.execution_provider,
                    "device": "cuda"
                    if cached.execution_provider == "CUDAExecutionProvider"
                    else "cpu",
                    "fallback_reason": cached.fallback_reason,
                    "idle_seconds": round(max(0.0, now - cached.last_used), 3),
                }
                for key, cached in _sessions.items()
            ],
        }


def clear_sessions() -> int:
    with _sessions_lock:
        count = len(_sessions)
        _sessions.clear()
        return count


def _evict_idle(now: float) -> None:
    expired = [
        key
        for key, cached in _sessions.items()
        if now - cached.last_used >= SESSION_IDLE_SECONDS
    ]
    for key in expired:
        _sessions.pop(key, None)


def _reap_loop() -> None:
    while not _reaper_stop.wait(10.0):
        with _sessions_lock:
            _evict_idle(monotonic())


_reaper = Thread(target=_reap_loop, name="fanfan-onnx-reaper", daemon=True)
_reaper.start()
