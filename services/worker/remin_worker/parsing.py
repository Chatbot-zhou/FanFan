from __future__ import annotations

import csv
import html.parser
import io
import os
import re
import secrets
import shutil
import subprocess
import tempfile
import time
import zipfile
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any, Literal
from uuid import UUID
from xml.etree import ElementTree

from .protocol import WorkerError, is_uuid_v7
from .ocr import recognize_with_windows


PARSER_VERSION = "0.1.0"
SUPPORTED_TEXT = {"txt", "md", "csv", "tsv", "html", "htm"}
SUPPORTED_OPEN_XML = {"docx", "docm", "xlsx", "xlsm", "pptx", "pptm"}
IMAGE_FORMATS = {"jpg", "jpeg", "png", "tif", "tiff", "bmp", "webp"}
LEGACY_OFFICE = {"doc", "xls", "ppt"}


def uuid7() -> str:
    timestamp_ms = int(time.time() * 1000) & ((1 << 48) - 1)
    random_bits = secrets.randbits(74)
    value = timestamp_ms << 80
    value |= 0x7 << 76
    value |= ((random_bits >> 62) & 0xFFF) << 64
    value |= 0b10 << 62
    value |= random_bits & ((1 << 62) - 1)
    return str(UUID(int=value))


@dataclass(frozen=True, slots=True)
class ParseRequest:
    job_id: str
    file_id: str
    revision_id: str
    source_path: str
    format: str
    ocr_policy: Literal["auto", "force", "disabled"] = "auto"
    language_hints: tuple[str, ...] = ("zh",)
    max_pages: int | None = None
    parser_version: str = PARSER_VERSION

    @classmethod
    def from_dict(cls, value: dict[str, Any]) -> "ParseRequest":
        required_ids = (value.get("job_id"), value.get("file_id"), value.get("revision_id"))
        if not all(isinstance(item, str) and is_uuid_v7(item) for item in required_ids):
            raise ValueError("job_id、file_id和revision_id必须使用UUIDv7")
        source_path = value.get("source_path")
        source_format = value.get("format")
        if not isinstance(source_path, str) or not source_path:
            raise ValueError("source_path不能为空")
        if not isinstance(source_format, str) or not source_format:
            raise ValueError("format不能为空")
        ocr_policy = value.get("ocr_policy", "auto")
        if ocr_policy not in {"auto", "force", "disabled"}:
            raise ValueError("ocr_policy不受支持")
        hints = value.get("language_hints", ["zh"])
        if not isinstance(hints, list) or not all(isinstance(item, str) for item in hints):
            raise ValueError("language_hints必须是字符串数组")
        max_pages = value.get("max_pages")
        if max_pages is not None and (not isinstance(max_pages, int) or max_pages < 1):
            raise ValueError("max_pages必须是正整数或null")
        return cls(
            job_id=required_ids[0],
            file_id=required_ids[1],
            revision_id=required_ids[2],
            source_path=source_path,
            format=source_format.lower().lstrip("."),
            ocr_policy=ocr_policy,
            language_hints=tuple(hints),
            max_pages=max_pages,
            parser_version=str(value.get("parser_version") or PARSER_VERSION),
        )


@dataclass(frozen=True, slots=True)
class ParseWarning:
    code: str
    message: str
    locator: dict[str, Any] | None = None


@dataclass(frozen=True, slots=True)
class DocumentNode:
    node_id: str
    parent_id: str | None
    ordinal: int
    node_type: str
    text: str | None
    table_data: dict[str, Any] | None
    locator: dict[str, Any]
    heading_path: tuple[str, ...] = ()


@dataclass(frozen=True, slots=True)
class ParseResult:
    revision_id: str
    status: Literal["parsed", "partial", "encrypted", "unsupported", "failed"]
    parser_name: str
    parser_version: str
    nodes: tuple[DocumentNode, ...]
    warnings: tuple[ParseWarning, ...]
    metrics: dict[str, int]
    error: WorkerError | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def locator(kind: str, **overrides: Any) -> dict[str, Any]:
    value: dict[str, Any] = {
        "kind": kind,
        "page_no": None,
        "slide_no": None,
        "sheet_name": None,
        "cell_range": None,
        "paragraph_no": None,
        "line_start": None,
        "line_end": None,
        "shape_no": None,
        "bbox": None,
        "heading_path": [],
    }
    value.update(overrides)
    return value


class _VisibleHtmlParser(html.parser.HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.parts: list[str] = []
        self._ignored_depth = 0

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag in {"script", "style", "noscript"}:
            self._ignored_depth += 1
        elif tag in {"p", "div", "br", "li", "h1", "h2", "h3", "tr"}:
            self.parts.append("\n")

    def handle_endtag(self, tag: str) -> None:
        if tag in {"script", "style", "noscript"} and self._ignored_depth:
            self._ignored_depth -= 1

    def handle_data(self, data: str) -> None:
        if not self._ignored_depth:
            self.parts.append(data)


def _decode_text(path: Path) -> str:
    raw = path.read_bytes()
    for encoding in ("utf-8-sig", "gb18030"):
        try:
            return raw.decode(encoding)
        except UnicodeDecodeError:
            continue
    return raw.decode("utf-8", errors="replace")


def _text_nodes(path: Path, source_format: str) -> list[DocumentNode]:
    text = _decode_text(path)
    if source_format in {"html", "htm"}:
        parser = _VisibleHtmlParser()
        parser.feed(text)
        text = " ".join("".join(parser.parts).split())
    elif source_format in {"csv", "tsv"}:
        dialect = "excel-tab" if source_format == "tsv" else "excel"
        rows = list(csv.reader(io.StringIO(text), dialect=dialect))
        text = "\n".join(" | ".join(row) for row in rows)
    nodes = []
    for ordinal, line in enumerate((line.strip() for line in text.splitlines()), 1):
        if line:
            nodes.append(
                DocumentNode(uuid7(), None, ordinal, "paragraph", line, None, locator("text", line_start=ordinal, line_end=ordinal))
            )
    return nodes


def _xml_text(element: ElementTree.Element) -> str:
    return "".join(value.strip() for value in element.itertext() if value.strip())


def _docx_nodes(path: Path) -> list[DocumentNode]:
    namespace = {"w": "http://schemas.openxmlformats.org/wordprocessingml/2006/main"}
    with zipfile.ZipFile(path) as package:
        root = ElementTree.fromstring(package.read("word/document.xml"))
    nodes: list[DocumentNode] = []
    for ordinal, paragraph in enumerate(root.findall(".//w:body/w:p", namespace), 1):
        text = "".join(item.text or "" for item in paragraph.findall(".//w:t", namespace)).strip()
        if text:
            nodes.append(DocumentNode(uuid7(), None, ordinal, "paragraph", text, None, locator("docx", paragraph_no=ordinal)))
    for table_ordinal, table in enumerate(root.findall(".//w:body/w:tbl", namespace), 1):
        rows = []
        for row in table.findall("./w:tr", namespace):
            rows.append([_xml_text(cell) for cell in row.findall("./w:tc", namespace)])
        nodes.append(DocumentNode(uuid7(), None, len(nodes) + 1, "table", None, {"rows": rows}, locator("docx", paragraph_no=None), (f"表格{table_ordinal}",)))
    return nodes


def _xlsx_nodes(path: Path) -> list[DocumentNode]:
    spreadsheet_ns = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
    relationship_ns = "http://schemas.openxmlformats.org/package/2006/relationships"
    office_rel_ns = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    with zipfile.ZipFile(path) as package:
        shared: list[str] = []
        if "xl/sharedStrings.xml" in package.namelist():
            shared_root = ElementTree.fromstring(package.read("xl/sharedStrings.xml"))
            shared = [_xml_text(item) for item in shared_root.findall(f"{{{spreadsheet_ns}}}si")]
        workbook = ElementTree.fromstring(package.read("xl/workbook.xml"))
        relationships = ElementTree.fromstring(package.read("xl/_rels/workbook.xml.rels"))
        targets = {
            relation.attrib["Id"]: relation.attrib["Target"]
            for relation in relationships.findall(f"{{{relationship_ns}}}Relationship")
        }
        sheets = []
        for sheet in workbook.findall(f".//{{{spreadsheet_ns}}}sheet"):
            relation_id = sheet.attrib[f"{{{office_rel_ns}}}id"]
            target = targets[relation_id].replace("\\", "/")
            if target.startswith("/"):
                package_path = target.lstrip("/")
            else:
                package_path = f"xl/{target}" if not target.startswith("xl/") else target
            sheets.append((sheet.attrib["name"], package_path))
        nodes: list[DocumentNode] = []
        for sheet_name, package_path in sheets:
            sheet_root = ElementTree.fromstring(package.read(package_path))
            for row in sheet_root.findall(f".//{{{spreadsheet_ns}}}row"):
                values: list[str] = []
                cells = row.findall(f"{{{spreadsheet_ns}}}c")
                for cell in cells:
                    cell_type = cell.attrib.get("t")
                    value_element = cell.find(f"{{{spreadsheet_ns}}}v")
                    if cell_type == "inlineStr":
                        value = _xml_text(cell)
                    elif value_element is None:
                        value = ""
                    elif cell_type == "s" and value_element.text:
                        value = shared[int(value_element.text)]
                    else:
                        value = value_element.text or ""
                    values.append(value)
                if any(values):
                    start = cells[0].attrib.get("r") if cells else None
                    end = cells[-1].attrib.get("r") if cells else None
                    cell_range = f"{start}:{end}" if start and end and start != end else start
                    nodes.append(DocumentNode(uuid7(), None, len(nodes) + 1, "row", " | ".join(values), {"cells": values}, locator("spreadsheet", sheet_name=sheet_name, cell_range=cell_range)))
    return nodes


def _pptx_nodes(path: Path) -> list[DocumentNode]:
    with zipfile.ZipFile(path) as package:
        slide_paths = sorted(
            (name for name in package.namelist() if re.fullmatch(r"ppt/slides/slide\d+\.xml", name)),
            key=lambda name: int(re.search(r"\d+", Path(name).stem).group()),
        )
        nodes = []
        for slide_number, slide_path in enumerate(slide_paths, 1):
            root = ElementTree.fromstring(package.read(slide_path))
            text = "\n".join(value.strip() for value in root.itertext() if value.strip())
            nodes.append(DocumentNode(uuid7(), None, slide_number, "slide", text or None, None, locator("presentation", slide_no=slide_number)))
    return nodes


def _ocr_nodes(result: dict[str, Any], kind: str, start_ordinal: int = 0) -> list[DocumentNode]:
    nodes: list[DocumentNode] = []
    for line in result.get("lines", []):
        text = str(line.get("text") or "").strip()
        page_no = line.get("page_no")
        bbox = line.get("bbox")
        if not text or not isinstance(page_no, int):
            continue
        nodes.append(
            DocumentNode(
                uuid7(),
                None,
                start_ordinal + len(nodes) + 1,
                "ocr_line",
                text,
                None,
                locator(kind, page_no=page_no, bbox=bbox if isinstance(bbox, dict) else None),
            )
        )
    return nodes


def _pdf_result(request: ParseRequest, path: Path, started_at: float) -> ParseResult:
    try:
        from pypdf import PdfReader
        from pypdf.errors import PdfReadError
    except ImportError:
        return _failure(request, "PARSER_DEPENDENCY_MISSING", "PDF解析依赖尚未安装", True)
    try:
        reader = PdfReader(path)
        if reader.is_encrypted:
            return _result(request, "encrypted", "pypdf", [], [ParseWarning("PDF_ENCRYPTED", "PDF已加密，请提供未加密副本")], 0)
        nodes: list[DocumentNode] = []
        warnings: list[ParseWarning] = []
        ocr_pages: list[int] = []
        pages = reader.pages[: request.max_pages] if request.max_pages else reader.pages
        for page_number, page in enumerate(pages, 1):
            text = (page.extract_text() or "").strip()
            if request.ocr_policy == "force" or (request.ocr_policy == "auto" and len(text) < 30):
                ocr_pages.append(page_number)
            if len(text) < 30 and request.ocr_policy == "disabled":
                warnings.append(ParseWarning("OCR_REQUIRED", "该页可提取文字少于30个字符", locator("pdf", page_no=page_number)))
            nodes.append(DocumentNode(uuid7(), None, page_number, "page", text or None, None, locator("pdf", page_no=page_number)))
        ocr_page_count = 0
        parser_name = "pypdf"
        if ocr_pages:
            ocr_result, ocr_error = recognize_with_windows(path, "pdf", request.max_pages, request.language_hints, ocr_pages)
            if ocr_error:
                for page_number in ocr_pages:
                    warnings.append(ParseWarning(ocr_error.code, ocr_error.message, locator("pdf", page_no=page_number)))
                    warnings.append(ParseWarning("OCR_REQUIRED", "该页尚未完成OCR", locator("pdf", page_no=page_number)))
            elif ocr_result is not None:
                recognized = _ocr_nodes(ocr_result, "pdf", len(nodes))
                recognized_pages = {node.locator["page_no"] for node in recognized}
                if recognized:
                    nodes = [node for node in nodes if node.locator.get("page_no") not in recognized_pages] + recognized
                    nodes.sort(key=lambda node: (node.locator.get("page_no") or 0, node.ordinal))
                    nodes = [
                        DocumentNode(node.node_id, node.parent_id, ordinal, node.node_type, node.text, node.table_data, node.locator, node.heading_path)
                        for ordinal, node in enumerate(nodes, 1)
                    ]
                    ocr_page_count = len(recognized_pages)
                    parser_name = "pypdf+windows-ocr"
                for page_number in ocr_pages:
                    if page_number not in recognized_pages:
                        warnings.append(ParseWarning("OCR_NO_TEXT", "该页OCR后没有识别到文字", locator("pdf", page_no=page_number)))
                        warnings.append(ParseWarning("OCR_REQUIRED", "该页尚未获得可索引文字", locator("pdf", page_no=page_number)))
        status: Literal["parsed", "partial"] = "partial" if warnings else "parsed"
        return _result(request, status, parser_name, nodes, warnings, len(pages), started_at, ocr_page_count)
    except (PdfReadError, OSError, ValueError) as error:
        return _failure(request, "PDF_PARSE_FAILED", str(error), False)


def _result(
    request: ParseRequest,
    status: Literal["parsed", "partial", "encrypted", "unsupported", "failed"],
    parser_name: str,
    nodes: list[DocumentNode],
    warnings: list[ParseWarning],
    page_count: int,
    started_at: float | None = None,
    ocr_page_count: int = 0,
) -> ParseResult:
    return ParseResult(
        revision_id=request.revision_id,
        status=status,
        parser_name=parser_name,
        parser_version=request.parser_version,
        nodes=tuple(nodes),
        warnings=tuple(warnings),
        metrics={
            "page_count": page_count,
            "node_count": len(nodes),
            "character_count": sum(len(node.text or "") for node in nodes),
            "ocr_page_count": ocr_page_count,
            "elapsed_ms": int((time.monotonic() - started_at) * 1000) if started_at else 0,
        },
    )


def _failure(request: ParseRequest, code: str, message: str, retryable: bool) -> ParseResult:
    return ParseResult(
        revision_id=request.revision_id,
        status="failed",
        parser_name="none",
        parser_version=request.parser_version,
        nodes=(),
        warnings=(),
        metrics={"page_count": 0, "node_count": 0, "character_count": 0, "ocr_page_count": 0, "elapsed_ms": 0},
        error=WorkerError(code, message, retryable),
    )


def _find_libreoffice() -> Path | None:
    explicit = os.environ.get("REMIN_LIBREOFFICE_EXE")
    candidates = [
        Path(explicit) if explicit else None,
        Path(r"C:\Program Files\LibreOffice\program\soffice.exe"),
        Path(r"C:\Program Files (x86)\LibreOffice\program\soffice.exe"),
    ]
    discovered = shutil.which("soffice")
    if discovered:
        candidates.append(Path(discovered))
    return next((candidate for candidate in candidates if candidate and candidate.is_file()), None)


def _legacy_office_result(request: ParseRequest, path: Path, source_format: str, started_at: float) -> ParseResult:
    executable = _find_libreoffice()
    if executable is None:
        return _result(request, "unsupported", "none", [], [ParseWarning("COMPATIBILITY_PACK_REQUIRED", "旧Office格式需要可选LibreOffice离线兼容包")], 0, started_at)
    target_format = {"doc": "docx", "xls": "xlsx", "ppt": "pptx"}[source_format]
    with tempfile.TemporaryDirectory(prefix="remin-legacy-office-") as temporary_raw:
        temporary = Path(temporary_raw)
        output_directory = temporary / "converted"
        profile_directory = temporary / "profile"
        output_directory.mkdir()
        profile_directory.mkdir()
        command = [
            str(executable),
            "--headless",
            "--nologo",
            "--nodefault",
            "--nofirststartwizard",
            f"-env:UserInstallation={profile_directory.resolve().as_uri()}",
            "--convert-to",
            target_format,
            "--outdir",
            str(output_directory),
            str(path),
        ]
        creation_flags = 0x08000000 if os.name == "nt" else 0
        try:
            completed = subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120, check=False, creationflags=creation_flags)
        except subprocess.TimeoutExpired:
            return _failure(request, "COMPATIBILITY_PACK_TIMEOUT", "旧Office只读转换超过120秒，已停止兼容组件", True)
        if completed.returncode != 0:
            detail = completed.stderr.decode("utf-8", errors="replace").strip()[:400]
            return _failure(request, "COMPATIBILITY_PACK_FAILED", detail or "LibreOffice兼容组件转换失败", True)
        converted = output_directory / f"{path.stem}.{target_format}"
        if not converted.is_file():
            converted = next(output_directory.glob(f"*.{target_format}"), converted)
        if not converted.is_file():
            return _failure(request, "COMPATIBILITY_PACK_OUTPUT_MISSING", "兼容组件没有生成可解析的临时副本", True)
        if target_format == "docx":
            nodes = _docx_nodes(converted)
            parser_name = "libreoffice-docx"
        elif target_format == "xlsx":
            nodes = _xlsx_nodes(converted)
            parser_name = "libreoffice-xlsx"
        else:
            nodes = _pptx_nodes(converted)
            parser_name = "libreoffice-pptx"
        warning = ParseWarning("LEGACY_OFFICE_CONVERTED", "旧Office文件通过只读临时副本解析；源文件未修改")
        return _result(request, "parsed", parser_name, nodes, [warning], len(nodes) if target_format == "pptx" else 0, started_at)


def parse_document(request: ParseRequest) -> ParseResult:
    started_at = time.monotonic()
    path = Path(request.source_path)
    if not path.is_file():
        return _failure(request, "FILE_NOT_FOUND", "文件不存在或已经移动", False)
    source_format = request.format
    try:
        if source_format in SUPPORTED_TEXT:
            nodes = _text_nodes(path, source_format)
            return _result(request, "parsed", "stdlib-text", nodes, [], 0, started_at)
        if source_format in {"docx", "docm"}:
            nodes = _docx_nodes(path)
            return _result(request, "parsed", "openxml-docx", nodes, [], 0, started_at)
        if source_format in {"xlsx", "xlsm"}:
            nodes = _xlsx_nodes(path)
            return _result(request, "parsed", "openxml-xlsx", nodes, [], 0, started_at)
        if source_format in {"pptx", "pptm"}:
            nodes = _pptx_nodes(path)
            return _result(request, "parsed", "openxml-pptx", nodes, [], len(nodes), started_at)
        if source_format == "pdf":
            return _pdf_result(request, path, started_at)
        if source_format in IMAGE_FORMATS:
            if request.ocr_policy == "disabled":
                return _result(request, "partial", "image-metadata", [], [ParseWarning("OCR_REQUIRED", "图片需要OCR后才能建立全文索引")], 1, started_at)
            ocr_result, ocr_error = recognize_with_windows(path, "image", 1, request.language_hints)
            if ocr_error:
                return _result(request, "partial", "image-metadata", [], [ParseWarning(ocr_error.code, ocr_error.message), ParseWarning("OCR_REQUIRED", "图片尚未完成OCR")], 1, started_at)
            nodes = _ocr_nodes(ocr_result or {}, "image")
            if not nodes:
                return _result(request, "partial", "windows-ocr", [], [ParseWarning("OCR_NO_TEXT", "图片OCR后没有识别到文字"), ParseWarning("OCR_REQUIRED", "图片尚未获得可索引文字")], 1, started_at)
            return _result(request, "parsed", "windows-ocr", nodes, [], 1, started_at, 1)
        if source_format in LEGACY_OFFICE:
            return _legacy_office_result(request, path, source_format, started_at)
        return _result(request, "unsupported", "none", [], [ParseWarning("FORMAT_UNSUPPORTED", f"暂不支持{source_format}格式")], 0, started_at)
    except PermissionError:
        return _failure(request, "FILE_PERMISSION_DENIED", "没有读取此文件的权限", True)
    except (OSError, KeyError, ValueError, zipfile.BadZipFile, ElementTree.ParseError) as error:
        return _failure(request, "DOCUMENT_PARSE_FAILED", str(error), False)
