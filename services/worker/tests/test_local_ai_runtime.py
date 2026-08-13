from __future__ import annotations

import base64
import io
import sys
import tempfile
import types
import unittest
import wave
from pathlib import Path
from unittest.mock import patch

WORKER_ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(WORKER_ROOT))

from fanfan_worker.paddle_ocr import clear_ocr_session, recognize_image  # noqa: E402
from fanfan_worker.speech import _encode_wav, recognize_speech  # noqa: E402


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


class LocalAiRuntimeTests(unittest.TestCase):
    def tearDown(self) -> None:
        clear_ocr_session()

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
            for path in files.values():
                path.write_bytes(b"source-bytes")
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

    def test_tts_wav_encoder_outputs_valid_mono_pcm(self) -> None:
        encoded = base64.b64encode(_encode_wav([-1.0, 0.0, 1.0], 16000))
        with wave.open(io.BytesIO(base64.b64decode(encoded)), "rb") as wav:
            self.assertEqual(wav.getnchannels(), 1)
            self.assertEqual(wav.getframerate(), 16000)
            self.assertEqual(wav.getnframes(), 3)


if __name__ == "__main__":
    unittest.main()
