from __future__ import annotations

import math
from dataclasses import dataclass
from pathlib import Path
from threading import Event, RLock, Thread
from time import monotonic
from typing import Any

from .protocol import WorkerError


SESSION_IDLE_SECONDS = 60.0
MAX_AUDIO_SECONDS = 120.0


@dataclass(slots=True)
class SpeechSession:
    engine: Any
    last_used: float
    threads: int


_lock = RLock()
_asr_sessions: dict[tuple[str, str, int], SpeechSession] = {}
_vad_sessions: dict[tuple[str, int], SpeechSession] = {}
_reaper_stop = Event()


def recognize_speech(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    try:
        model = _required_file(payload, "model_path", ".onnx")
        tokens = _required_file(payload, "tokens_path", ".txt")
        vad_model = _required_file(payload, "vad_model_path", ".onnx")
        arch = _asr_arch(payload)
        sample_rate = _bounded_int(payload.get("sample_rate"), "sample_rate", 8_000, 96_000)
        threads = _bounded_int(payload.get("threads", 1), "threads", 1, 4)
        samples = _audio_samples(payload.get("samples"), sample_rate)
        speech_samples = _voice_samples(samples, sample_rate, vad_model, threads)
        if not speech_samples:
            return {
                "text": "",
                "sample_rate": sample_rate,
                "duration_ms": round(len(samples) * 1000 / sample_rate),
                "timestamps": [],
                "engine": "sherpa_onnx",
                "vad": "silero",
            }, None
        recognizer = _get_asr(model, tokens, threads, arch)
        stream = recognizer.create_stream()
        stream.accept_waveform(sample_rate, speech_samples)
        recognizer.decode_stream(stream)
        result = stream.result
        text = str(getattr(result, "text", "")).strip()
        timestamps = [float(value) for value in (getattr(result, "timestamps", None) or [])]
        return {
            "text": text,
            "sample_rate": sample_rate,
            "duration_ms": round(len(samples) * 1000 / sample_rate),
            "timestamps": timestamps,
            "engine": "sherpa_onnx",
            "vad": "silero",
        }, None
    except ImportError:
        return None, WorkerError(
            "SPEECH_RUNTIME_UNAVAILABLE",
            "语音运行时尚未安装完整，请重新安装翻翻的本地语音组件",
            False,
        )
    except (OSError, RuntimeError, TypeError, ValueError) as error:
        return None, WorkerError("ASR_RECOGNITION_FAILED", str(error), True)


def self_test_asr(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    # 注意：不能走 recognize_speech 的静音探针——VAD 判定无语音会早退，
    # 识别器根本不会被初始化，API 不匹配会静默漏过（曾因 1.13.4 特征配置
    # 名不匹配造成假阳性）。这里直接初始化识别器并在静音上跑一次推理，
    # 真实验证模型加载、特征提取与解码链路可用。
    try:
        model = _required_file(payload, "model_path", ".onnx")
        tokens = _required_file(payload, "tokens_path", ".txt")
        arch = _asr_arch(payload)
        threads = _bounded_int(payload.get("threads", 1), "threads", 1, 4)
        recognizer = _get_asr(model, tokens, threads, arch)
        stream = recognizer.create_stream()
        stream.accept_waveform(16_000, [0.0] * 4_000)
        recognizer.decode_stream(stream)
        _ = stream.result
        return {"status": "ready", "engine": "sherpa_onnx", "probe": "silence_250ms"}, None
    except ImportError:
        return None, WorkerError(
            "SPEECH_RUNTIME_UNAVAILABLE",
            "语音运行时尚未安装完整，请重新安装翻翻的本地语音组件",
            False,
        )
    except (OSError, RuntimeError, TypeError, ValueError) as error:
        return None, WorkerError("MODEL_SELF_TEST_FAILED", str(error), True)


def speech_cache_snapshot() -> dict[str, Any]:
    now = monotonic()
    with _lock:
        _evict_idle(now)
        return {
            "backend": "sherpa_onnx",
            "asr_session_count": len(_asr_sessions),
            "vad_session_count": len(_vad_sessions),
            "idle_timeout_seconds": int(SESSION_IDLE_SECONDS),
        }


def clear_speech_sessions() -> int:
    with _lock:
        count = len(_asr_sessions) + len(_vad_sessions)
        _asr_sessions.clear()
        _vad_sessions.clear()
        return count


def _get_vad(model: Path, threads: int) -> Any:
    import sherpa_onnx

    key = (str(model.resolve()), threads)
    now = monotonic()
    with _lock:
        _evict_idle(now)
        cached = _vad_sessions.get(key)
        if cached is not None:
            cached.last_used = now
            cached.engine.reset()
            return cached.engine
    config = sherpa_onnx.VadModelConfig()
    config.silero_vad.model = str(model)
    config.silero_vad.threshold = 0.5
    config.silero_vad.min_silence_duration = 0.25
    config.silero_vad.min_speech_duration = 0.25
    config.silero_vad.max_speech_duration = 30.0
    config.sample_rate = 16_000
    config.num_threads = threads
    config.provider = "cpu"
    engine = sherpa_onnx.VoiceActivityDetector(config, buffer_size_in_seconds=120)
    with _lock:
        _vad_sessions.clear()
        _vad_sessions[key] = SpeechSession(engine=engine, last_used=now, threads=threads)
    return engine


def _voice_samples(samples: list[float], sample_rate: int, model: Path, threads: int) -> list[float]:
    if sample_rate != 16_000:
        raise ValueError("Silero VAD requires 16000 Hz audio")
    vad = _get_vad(model, threads)
    window_size = 512
    for offset in range(0, len(samples), window_size):
        window = samples[offset : offset + window_size]
        if len(window) < window_size:
            window = window + [0.0] * (window_size - len(window))
        vad.accept_waveform(window)
    vad.flush()
    speech: list[float] = []
    while not vad.empty():
        speech.extend(float(value) for value in vad.front.samples)
        vad.pop()
    return speech


def _asr_arch(payload: dict[str, Any]) -> str:
    # 支持的 ASR 架构：paraformer（旧默认）/ sense_voice（新版默认）。
    # 未知或缺失时回退 paraformer，保证旧请求兼容。
    arch = payload.get("arch", "paraformer")
    if not isinstance(arch, str):
        raise ValueError("arch必须是字符串")
    arch = arch.strip()
    if arch not in ("paraformer", "sense_voice"):
        raise ValueError("arch仅支持paraformer或sense_voice")
    return arch


def _get_asr(model: Path, tokens: Path, threads: int, arch: str) -> Any:
    import sherpa_onnx

    key = (str(model.resolve()), str(tokens.resolve()), threads, arch)
    now = monotonic()
    with _lock:
        _evict_idle(now)
        cached = _asr_sessions.get(key)
        if cached is not None:
            cached.last_used = now
            return cached.engine
    # sherpa-onnx 1.13.4（规划书锁定版本）：OfflineRecognizer 是各架构工厂
    # 方法的命名空间类，无通用构造函数；不同模型形态走对应工厂方法。
    # （2.x 起改为 OfflineRecognizerConfig + 构造函数，特征配置名也改为
    # OfflineFeatureExtractorConfig。按锁定版本取 1.13.4 的 API。）
    if arch == "sense_voice":
        engine = sherpa_onnx.OfflineRecognizer.from_sense_voice(
            tokens=str(tokens),
            sense_voice_model=str(model),
            num_threads=threads,
            page_index=0,
            use_itn=True,
            debugging=False,
            language="auto",
            provider="cpu",
        )
    else:
        engine = sherpa_onnx.OfflineRecognizer.from_paraformer(
            paraformer=str(model),
            tokens=str(tokens),
            num_threads=threads,
            decoding_method="greedy_search",
            debug=False,
            provider="cpu",
        )
    with _lock:
        _asr_sessions.clear()
        _asr_sessions[key] = SpeechSession(engine=engine, last_used=now, threads=threads)
    return engine


def _required_file(payload: dict[str, Any], field: str, suffix: str) -> Path:
    raw = payload.get(field)
    if not isinstance(raw, str) or not raw:
        raise ValueError(f"{field}不能为空")
    path = Path(raw)
    if path.is_symlink() or not path.is_file() or path.suffix.lower() != suffix:
        raise ValueError(f"{field}不是有效的{suffix}文件")
    return path.resolve(strict=True)


def _bounded_int(value: Any, field: str, minimum: int, maximum: int) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ValueError(f"{field}必须在{minimum}到{maximum}之间")
    return value


def _audio_samples(value: Any, sample_rate: int) -> list[float]:
    if not isinstance(value, list) or not value:
        raise ValueError("samples不能为空")
    if len(value) > int(sample_rate * MAX_AUDIO_SECONDS):
        raise ValueError(f"录音不能超过{int(MAX_AUDIO_SECONDS)}秒")
    samples: list[float] = []
    for raw in value:
        sample = float(raw)
        if not math.isfinite(sample) or sample < -1.0 or sample > 1.0:
            raise ValueError("samples包含无效采样值")
        samples.append(sample)
    return samples


def _evict_idle(now: float) -> None:
    for sessions in (_asr_sessions, _vad_sessions):
        for key in [key for key, value in sessions.items() if now - value.last_used >= SESSION_IDLE_SECONDS]:
            sessions.pop(key, None)


def _reap_loop() -> None:
    while not _reaper_stop.wait(10.0):
        with _lock:
            _evict_idle(monotonic())


_reaper = Thread(target=_reap_loop, name="fanfan-speech-reaper", daemon=True)
_reaper.start()
