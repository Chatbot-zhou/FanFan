from __future__ import annotations

import hashlib
import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path


WORKER_ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = WORKER_ROOT.parents[1]
CORPUS_ROOT = REPO_ROOT / "tests" / "fixtures" / "corpus"
sys.path.insert(0, str(WORKER_ROOT))

from remin_worker import ParseRequest, parse_document  # noqa: E402


IDS = [
    "018f0000-0000-7000-8000-000000000101",
    "018f0000-0000-7000-8000-000000000102",
    "018f0000-0000-7000-8000-000000000103",
]


def request(path: Path, source_format: str) -> ParseRequest:
    return ParseRequest(
        job_id=IDS[0],
        file_id=IDS[1],
        revision_id=IDS[2],
        source_path=str(path),
        format=source_format,
    )


def node_text(result: object) -> str:
    return "\n".join(
        filter(
            None,
            (
                node.text or "\n".join(" | ".join(row) for row in (node.table_data or {}).get("rows", []))
                for node in result.nodes
            ),
        )
    )


class DocumentParsingTests(unittest.TestCase):
    def test_text_parse_is_read_only_and_located(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "过去的资料.txt"
            path.write_text("第一行\n归航计划 GH-2025-017", encoding="utf-8")
            before = hashlib.sha256(path.read_bytes()).hexdigest()
            result = parse_document(request(path, "txt"))
            after = hashlib.sha256(path.read_bytes()).hexdigest()

        self.assertEqual(result.status, "parsed")
        self.assertEqual(before, after)
        self.assertEqual(result.nodes[1].locator["line_start"], 2)
        self.assertIn("GH-2025-017", node_text(result))

    def test_docx_extracts_paragraphs_and_tables(self) -> None:
        result = parse_document(request(CORPUS_ROOT / "01-归航计划项目总结.docx", "docx"))
        self.assertEqual(result.status, "parsed")
        self.assertIn("GH-2025-017", node_text(result))
        self.assertIn("林晓岚", node_text(result))
        self.assertTrue(any(node.node_type == "table" for node in result.nodes))

    def test_xlsx_preserves_sheet_and_cell_locator(self) -> None:
        result = parse_document(request(CORPUS_ROOT / "06-检索评估与项目台账.xlsx", "xlsx"))
        self.assertEqual(result.status, "parsed")
        self.assertIn("GH-2025-017", node_text(result))
        self.assertTrue(any(node.locator["sheet_name"] for node in result.nodes))
        self.assertTrue(any(node.locator["cell_range"] for node in result.nodes))

    def test_pptx_preserves_slide_locator(self) -> None:
        result = parse_document(request(CORPUS_ROOT / "07-归航计划阶段汇报.pptx", "pptx"))
        self.assertEqual(result.status, "parsed")
        self.assertIn("GH-2025-017", node_text(result))
        self.assertEqual(result.nodes[0].locator["slide_no"], 1)

    def test_legacy_office_requests_explicit_compatibility_pack(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "历史台账.xls"
            path.write_bytes(b"legacy-office-placeholder")
            result = parse_document(request(path, "xls"))
        self.assertEqual(result.status, "unsupported")
        self.assertEqual(result.warnings[0].code, "COMPATIBILITY_PACK_REQUIRED")

    @unittest.skipUnless(os.name == "nt", "Windows OCR is a Windows desktop capability")
    def test_windows_ocr_indexes_chinese_image_without_modifying_source(self) -> None:
        path = CORPUS_ROOT / "04-扫描采购收据.png"
        before = hashlib.sha256(path.read_bytes()).hexdigest()
        result = parse_document(request(path, "png"))
        after = hashlib.sha256(path.read_bytes()).hexdigest()

        self.assertEqual(result.status, "parsed")
        self.assertEqual(result.parser_name, "windows-ocr")
        self.assertEqual(result.metrics["ocr_page_count"], 1)
        self.assertEqual(before, after)
        self.assertIn("采 购", node_text(result))
        self.assertTrue(all(node.locator["bbox"] for node in result.nodes))

    @unittest.skipUnless(
        os.name == "nt" and importlib.util.find_spec("pypdf") is not None,
        "Scanned PDF OCR requires Windows OCR and pypdf",
    )
    def test_windows_ocr_renders_and_indexes_scanned_pdf(self) -> None:
        path = CORPUS_ROOT / "03-扫描采购收据.pdf"
        before = hashlib.sha256(path.read_bytes()).hexdigest()
        result = parse_document(request(path, "pdf"))
        after = hashlib.sha256(path.read_bytes()).hexdigest()

        self.assertEqual(result.status, "parsed")
        self.assertEqual(result.parser_name, "pypdf+windows-ocr")
        self.assertEqual(result.metrics["ocr_page_count"], 1)
        self.assertEqual(before, after)
        self.assertIn("采 购", node_text(result))
        self.assertTrue(all(node.locator["page_no"] == 1 for node in result.nodes))

    def test_invalid_parse_request_is_rejected_at_boundary(self) -> None:
        with self.assertRaisesRegex(ValueError, "UUIDv7"):
            ParseRequest.from_dict(
                {
                    "job_id": "job-1",
                    "file_id": IDS[1],
                    "revision_id": IDS[2],
                    "source_path": "C:/资料.pdf",
                    "format": "pdf",
                }
            )


if __name__ == "__main__":
    unittest.main()
