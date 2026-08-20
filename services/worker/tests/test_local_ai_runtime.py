from __future__ import annotations

import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import patch

WORKER_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(WORKER_ROOT))

from fanfan_worker.paddle_ocr import clear_ocr_session, recognize_image  # noqa: E402
from fanfan_worker.image_ocr import route_image_ocr  # noqa: E402
from fanfan_worker import runtime_cache  # noqa: E402
from fanfan_worker.speech import recognize_speech  # noqa: E402


class _Image:
    shape = (100, 200, 3)


class _OcrResult:
    img = _Image()
    boxes = [[[10, 20], [110, 20], [110, 50], [10, 50]]]
    txts = ["翻翻知道"]
    scores = [0.97]
    elapse = 0.012


class _RapidOCR:
    last_params = None

    def __init__(self, params):
        type(self).last_params = params

    def __call__(self, _source, **_options):
        return _OcrResult()


class _Enum:
    ONNXRUNTIME = "onnxruntime"
    CH = "ch"
    MOBILE = "mobile"
    PPOCRV4 = "PP-OCRv4"
    PPOCRV5 = "PP-OCRv5"

    @classmethod
    def __class_getitem__(cls, key):
        return getattr(cls, key)


# 1x1 透明 PNG：recognize_image 会用 PIL 校验图片有效性，
# 测试里的 image_path 必须是可解码的合法 PNG。
_SOURCE_PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c489"
    "0000000a49444154789c6360000002000156a2b48e0000000049454e44ae426082"
)


class LocalAiRuntimeTests(unittest.TestCase):
    def tearDown(self) -> None:
        clear_ocr_session()
        runtime_cache.clear_sessions()
        runtime_cache._gpu_disabled_until = 0.0

    def test_ppocrv5_returns_normalised_polygon_without_modifying_source(self) -> None:
        fake = types.SimpleNamespace(
            RapidOCR=_RapidOCR,
            EngineType=_Enum,
            LangDet=_Enum,
            LangRec=_Enum,
            ModelType=_Enum,
            OCRVersion=_Enum,
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            files = {
                "model_path": root / "rec.onnx",
                "det_model_path": root / "det.onnx",
                "cls_model_path": root / "cls.onnx",
                "dictionary_path": root / "dict.txt",
                "image_path": root / "source.png",
            }
            for key, path in files.items():
                path.write_bytes(_SOURCE_PNG if key == "image_path" else b"source-bytes")
            before = files["image_path"].read_bytes()
            payload = {key: str(path) for key, path in files.items()}
            payload.update({"threads": 2, "page_no": 3})
            with patch.dict(sys.modules, {"rapidocr": fake}):
                result, error = recognize_image(payload)

            self.assertIsNone(error)
            self.assertEqual(files["image_path"].read_bytes(), before)
            self.assertEqual(result["model_version"], "PP-OCRv5-mobile")
            self.assertEqual(result["lines"][0]["page_no"], 3)
            self.assertAlmostEqual(result["lines"][0]["confidence"], 0.97)
            self.assertEqual(result["lines"][0]["bbox"], {"x0": 0.05, "y0": 0.2, "x1": 0.55, "y1": 0.5})
            self.assertEqual(len(result["lines"][0]["polygon"]), 4)
            self.assertEqual(_RapidOCR.last_params["Rec.ocr_version"], "PP-OCRv5")
            self.assertEqual(_RapidOCR.last_params["EngineConfig.onnxruntime.intra_op_num_threads"], 2)

    def test_speech_rejects_an_incomplete_managed_model_package(self) -> None:
        result, error = recognize_speech({
            "model_path": "Z:/missing/model.onnx",
            "tokens_path": "Z:/missing/tokens.txt",
            "vad_model_path": "Z:/missing/silero_vad.onnx",
            "samples": [0.0] * 4000,
            "sample_rate": 16000,
            "threads": 1,
        })
        self.assertIsNone(result)
        self.assertEqual(error.code, "ASR_RECOGNITION_FAILED")

    @staticmethod
    def _image_ocr_payload() -> dict[str, object]:
        return {
            "image_path": "C:/derived/read-only.png",
            "page_no": 1,
            "asset_kind": "embedded_image",
            "confidence_threshold": 0.45,
        }

    def test_image_ocr_success_skips_multimodal_understanding(self) -> None:
        primary = {
            "model_version": "PP-OCRv5-mobile",
            "lines": [{
                "text": "这是一张普通文字截图，其中包含项目进度、负责人、完成日期、验收标准、风险说明以及后续安排，正文信息完整且不需要图片模型再次概括。",
                "confidence": 0.96,
            }],
        }
        with patch("fanfan_worker.image_ocr.recognize_image", return_value=(primary, None)), \
             patch("fanfan_worker.image_ocr.recognize_with_windows") as windows_ocr:
            result, error = route_image_ocr(self._image_ocr_payload())

        self.assertIsNone(error)
        self.assertFalse(result["vision_required"])
        self.assertEqual(result["route_reason"], "ocr_success")
        windows_ocr.assert_not_called()

    def test_low_confidence_image_ocr_retains_text_and_routes_to_multimodal(self) -> None:
        primary = {
            "model_version": "PP-OCRv5-mobile",
            "lines": [{"text": "Q2 128", "confidence": 0.31}],
        }
        fallback = {"lines": [{"text": "第二季度 128 万元"}]}
        with patch("fanfan_worker.image_ocr.recognize_image", return_value=(primary, None)), \
             patch(
                 "fanfan_worker.image_ocr.recognize_with_windows",
                 return_value=(fallback, None),
             ):
            result, error = route_image_ocr(self._image_ocr_payload())

        self.assertIsNone(error)
        self.assertTrue(result["vision_required"])
        self.assertEqual(result["route_reason"], "ocr_low_confidence")
        self.assertEqual(result["ocr_text"], "第二季度 128 万元")
        self.assertEqual([attempt["engine"] for attempt in result["attempts"]], [
            "rapidocr-onnxruntime",
            "windows-ocr",
        ])

    def test_onnx_provider_prefers_cuda_only_when_memory_and_runtime_are_ready(self) -> None:
        fake_ort = types.SimpleNamespace(
            get_available_providers=lambda: [
                "CUDAExecutionProvider",
                "CPUExecutionProvider",
            ],
        )
        with patch.dict(sys.modules, {"onnxruntime": fake_ort}), \
             patch("fanfan_worker.runtime_cache._prepare_cuda_runtime", return_value=True), \
             patch("fanfan_worker.runtime_cache._probe_gpu_free_mb", return_value=4096), \
             patch.dict("os.environ", {}, clear=True):
            providers, reason = runtime_cache._resolve_providers()
            low_memory_providers, low_memory_reason = runtime_cache._resolve_providers(
                gpu_min_free_mb=8192
            )

        self.assertEqual(providers[0], "CUDAExecutionProvider")
        self.assertIsNone(reason)
        self.assertEqual(low_memory_providers, ["CPUExecutionProvider"])
        self.assertEqual(low_memory_reason, "gpu_memory_low:4096MiB")

    def test_quantized_model_prefers_cpu_unless_provider_is_explicit(self) -> None:
        with patch.dict("os.environ", {}, clear=True):
            reason = runtime_cache._model_cpu_preference_reason(
                Path("model_quantized.onnx")
            )
            floating_reason = runtime_cache._model_cpu_preference_reason(
                Path("model_fp16.onnx")
            )
        with patch.dict(
            "os.environ",
            {"FANFAN_WORKER_PROVIDERS": "CUDAExecutionProvider,CPUExecutionProvider"},
            clear=True,
        ):
            override_reason = runtime_cache._model_cpu_preference_reason(
                Path("model_quantized.onnx")
            )

        self.assertEqual(reason, "quantized_model_cpu_preferred")
        self.assertIsNone(floating_reason)
        self.assertIsNone(override_reason)

    def test_cuda_session_creation_failure_falls_back_to_cpu(self) -> None:
        class _Options:
            def add_session_config_entry(self, *_args):
                return None

        class _CpuSession:
            @staticmethod
            def get_providers():
                return ["CPUExecutionProvider"]

        def create_session(_path, *, sess_options, providers):
            self.assertIsNotNone(sess_options)
            if providers[0] == "CUDAExecutionProvider":
                raise RuntimeError("simulated CUDA session failure")
            return _CpuSession()

        fake_ort = types.SimpleNamespace(
            get_available_providers=lambda: [
                "CUDAExecutionProvider",
                "CPUExecutionProvider",
            ],
            SessionOptions=_Options,
            ExecutionMode=types.SimpleNamespace(ORT_SEQUENTIAL="sequential"),
            InferenceSession=create_session,
        )
        with patch.dict(sys.modules, {"onnxruntime": fake_ort}), \
             patch("fanfan_worker.runtime_cache._prepare_cuda_runtime", return_value=True), \
             patch("fanfan_worker.runtime_cache._probe_gpu_free_mb", return_value=4096), \
             patch.dict("os.environ", {}, clear=True):
            handle = runtime_cache.get_onnx_session(Path("model.onnx"), 2)

        self.assertEqual(handle.device, "cpu")
        self.assertEqual(handle.execution_provider, "CPUExecutionProvider")
        self.assertEqual(handle.fallback_reason, "cuda_session_failed")

    def test_cuda_inference_failure_retries_current_task_on_cpu(self) -> None:
        class _GpuSession:
            @staticmethod
            def run(*_args):
                raise RuntimeError("simulated CUDA inference failure")

        class _CpuSession:
            @staticmethod
            def run(*_args):
                return [[1.0, 2.0]]

        gpu = runtime_cache.SessionHandle(
            session=_GpuSession(),
            key=("model.onnx", 2, "CUDAExecutionProvider"),
            execution_provider="CUDAExecutionProvider",
            device="cuda",
            fallback_reason=None,
        )
        cpu = runtime_cache.SessionHandle(
            session=_CpuSession(),
            key=("model.onnx", 2, "CPUExecutionProvider"),
            execution_provider="CPUExecutionProvider",
            device="cpu",
            fallback_reason="cuda_inference_failed",
        )
        with patch("fanfan_worker.runtime_cache.get_onnx_session", return_value=cpu):
            output, selected = runtime_cache.run_with_cpu_fallback(
                gpu, Path("model.onnx"), 2, {"input_ids": [[1]]}
            )

        self.assertEqual(output, [1.0, 2.0])
        self.assertEqual(selected.device, "cpu")
        self.assertEqual(selected.fallback_reason, "cuda_inference_failed")


if __name__ == "__main__":
    unittest.main()
