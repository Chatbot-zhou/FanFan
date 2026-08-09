from __future__ import annotations

import hashlib
import importlib.util
import os
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path


WORKER_ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = WORKER_ROOT.parents[1]
CORPUS_ROOT = REPO_ROOT / "tests" / "fixtures" / "corpus"
sys.path.insert(0, str(WORKER_ROOT))

from remin_worker import ParseRequest, parse_document  # noqa: E402
from remin_worker.ocr import _validated_rendered_pages  # noqa: E402


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

    def test_code_parse_groups_symbols_and_keeps_exact_line_ranges(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "knowledge_index.py"
            path.write_text(
                "\"\"\"本地索引模块。\"\"\"\n\nclass KnowledgeIndex:\n    def search(self, query):\n        return query\n\ndef build_index(files):\n    return KnowledgeIndex()\n",
                encoding="utf-8",
            )
            before = hashlib.sha256(path.read_bytes()).hexdigest()
            result = parse_document(request(path, "py"))
            after = hashlib.sha256(path.read_bytes()).hexdigest()

        self.assertEqual(result.status, "parsed")
        self.assertEqual(result.parser_name, "stdlib-code-structure")
        self.assertEqual(before, after)
        self.assertEqual([node.node_type for node in result.nodes], ["code_block", "code_symbol", "code_symbol", "code_symbol"])
        self.assertEqual(result.nodes[1].heading_path, ("py", "KnowledgeIndex"))
        self.assertEqual(result.nodes[-1].locator["line_start"], 7)
        self.assertEqual(result.nodes[-1].locator["line_end"], 8)

    def test_zip_indexes_manifest_without_extracting_members(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            path = base / "资料包.zip"
            with zipfile.ZipFile(path, "w") as package:
                package.writestr("项目/说明.txt", "不应被解压到磁盘")
                package.writestr("项目/数据.csv", "id,value\n1,归航")
            before_entries = sorted(item.name for item in base.iterdir())
            before = hashlib.sha256(path.read_bytes()).hexdigest()
            result = parse_document(request(path, "zip"))
            after = hashlib.sha256(path.read_bytes()).hexdigest()
            after_entries = sorted(item.name for item in base.iterdir())

        self.assertEqual(result.status, "parsed")
        self.assertEqual(result.parser_name, "stdlib-zip-manifest")
        self.assertEqual(before, after)
        self.assertEqual(before_entries, after_entries)
        self.assertEqual(result.nodes[0].node_type, "archive_manifest")
        self.assertEqual(result.nodes[0].table_data["rows"][0][0], "项目/说明.txt")

    def test_docx_extracts_paragraphs_and_tables(self) -> None:
        result = parse_document(request(CORPUS_ROOT / "01-归航计划项目总结.docx", "docx"))
        self.assertEqual(result.status, "parsed")
        self.assertIn("GH-2025-017", node_text(result))
        self.assertIn("林晓岚", node_text(result))
        self.assertTrue(any(node.node_type == "table" for node in result.nodes))

    def test_docx_embedded_image_is_copied_to_revision_cache_without_source_changes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            path = base / "带图片的文档.docx"
            cache = base / "app-cache" / "image-assets" / IDS[2]
            with zipfile.ZipFile(path, "w") as package:
                package.writestr(
                    "word/document.xml",
                    '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body><w:p><w:r><w:t>图片说明</w:t></w:r><a:blip r:embed="rId7" /></w:p></w:body></w:document>',
                )
                package.writestr(
                    "word/_rels/document.xml.rels",
                    '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId7" Target="media/image1.png" Type="image" /></Relationships>',
                )
                package.writestr("word/media/image1.png", b"synthetic-png-content")
            before = hashlib.sha256(path.read_bytes()).hexdigest()
            parse_request = ParseRequest(
                job_id=IDS[0],
                file_id=IDS[1],
                revision_id=IDS[2],
                source_path=str(path),
                format="docx",
                asset_cache_dir=str(cache),
            )
            result = parse_document(parse_request)
            after = hashlib.sha256(path.read_bytes()).hexdigest()

            self.assertEqual(result.status, "parsed")
            self.assertEqual(before, after)
            self.assertEqual(len(result.image_assets), 1)
            self.assertEqual(result.image_assets[0].locator["paragraph_no"], 1)
            self.assertEqual(Path(result.image_assets[0].cache_path).read_bytes(), b"synthetic-png-content")

    def test_scanned_page_render_manifest_cannot_escape_temporary_cache(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            base = Path(directory)
            render_cache = base / "render"
            render_cache.mkdir()
            first = render_cache / "page-1.png"
            second = render_cache / "page-2.png"
            first.write_bytes(b"png-page-one")
            second.write_bytes(b"png-page-two")
            validated = _validated_rendered_pages(
                [
                    {"page_no": 2, "path": str(second)},
                    {"page_no": 1, "path": str(first)},
                ],
                render_cache,
                {1, 2},
            )
            self.assertEqual([item["page_no"] for item in validated], [1, 2])

            outside = base / "outside.png"
            outside.write_bytes(b"not-authorized")
            with self.assertRaisesRegex(ValueError, "超出应用临时目录"):
                _validated_rendered_pages(
                    [{"page_no": 1, "path": str(outside)}],
                    render_cache,
                    {1},
                )

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
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory) / "app-cache" / "image-assets" / IDS[2]
            parse_request = ParseRequest(
                job_id=IDS[0],
                file_id=IDS[1],
                revision_id=IDS[2],
                source_path=str(path),
                format="pdf",
                asset_cache_dir=str(cache),
            )
            result = parse_document(parse_request)
            after = hashlib.sha256(path.read_bytes()).hexdigest()

            self.assertTrue(any(asset.asset_kind == "pdf_scanned_page" for asset in result.image_assets))
            self.assertTrue(all(Path(asset.cache_path).is_file() for asset in result.image_assets))

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
