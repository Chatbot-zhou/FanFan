from __future__ import annotations

from collections import OrderedDict
from dataclasses import dataclass
from pathlib import Path
from threading import Event, RLock, Thread
from time import monotonic
from typing import Any


SESSION_IDLE_SECONDS = 60.0
MAX_SESSIONS = 2

# 推理设备策略：资源满足时优先 GPU，显存不足时回退 CPU（规划书 2/7 章）。
# onnxruntime-gpu 提供 CUDAExecutionProvider；CUDA 初始化失败时 ORT 非 strict
# 模式自动回退 CPU，不会破坏当前任务。实测（2026-08，4GB 卡被多进程共享、
# 仅剩 ~1.4GB 显存）：CUDA 批处理 3.8s vs CPU 1.4s，GPU 反慢 2.7 倍——
# 所以除"CUDA 可用"外还必须探测显存余量，不足阈值一律走 CPU。
# 环境变量 FANFAN_WORKER_PROVIDERS 可强制覆盖（测试/回退）；
# FANFAN_WORKER_GPU_MIN_FREE_MB 可调整显存余量阈值（默认 2048）。
_ENABLED_PROVIDERS = ["CUDAExecutionProvider", "CPUExecutionProvider"]
_GPU_MIN_FREE_MB = 2048


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


def _resolve_providers() -> tuple[list[str], str | None]:
    import os

    override = os.environ.get("FANFAN_WORKER_PROVIDERS")
    if override:
        return [p.strip() for p in override.split(",") if p.strip()], "env_override"
    try:
        import onnxruntime as ort

        if "CUDAExecutionProvider" not in ort.get_available_providers():
            return ["CPUExecutionProvider"], "no_cuda"
        min_free = int(os.environ.get("FANFAN_WORKER_GPU_MIN_FREE_MB", _GPU_MIN_FREE_MB))
        free_mb = _probe_gpu_free_mb()
        if free_mb is not None and free_mb < min_free:
            return ["CPUExecutionProvider"], f"gpu_memory_low:{free_mb}MiB"
    except Exception:
        pass
    if not _cudnn_loadable():
        return ["CPUExecutionProvider"], "cudnn_unavailable"
    return _ENABLED_PROVIDERS, None


@dataclass(slots=True)
class CachedSession:
    session: Any
    last_used: float
    threads: int


_sessions: OrderedDict[tuple[str, int], CachedSession] = OrderedDict()
_sessions_lock = RLock()
_reaper_stop = Event()


def get_onnx_session(model_path: Path, threads: int) -> Any:
    """Return a reusable, non-spinning CPU ONNX session.

    The worker protocol is serialized, so a process-local LRU provides safe reuse
    without a second lock or a pool of competing ONNX thread groups.
    """

    import onnxruntime as ort

    now = monotonic()
    key = (str(model_path.resolve()), threads)
    with _sessions_lock:
        _evict_idle(now)
        cached = _sessions.pop(key, None)
        if cached is not None:
            cached.last_used = now
            _sessions[key] = cached
            return cached.session

    options = ort.SessionOptions()
    options.intra_op_num_threads = threads
    options.inter_op_num_threads = 1
    options.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    options.enable_cpu_mem_arena = True
    options.enable_mem_pattern = True
    options.add_session_config_entry("session.intra_op.allow_spinning", "0")
    options.add_session_config_entry("session.inter_op.allow_spinning", "0")
    providers, reason = _resolve_providers()
    session = ort.InferenceSession(
        str(model_path),
        sess_options=options,
        providers=providers,
    )
    if reason:
        import logging

        logging.getLogger("fanfan.worker").warning(
            "inference provider decision: %s (reason=%s)", ",".join(providers), reason
        )
    with _sessions_lock:
        _sessions[key] = CachedSession(session=session, last_used=now, threads=threads)
        while len(_sessions) > MAX_SESSIONS:
            _sessions.popitem(last=False)
    return session


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
