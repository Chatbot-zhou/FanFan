from __future__ import annotations

import csv
import html.parser
import hashlib
import io
import mimetypes
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
from .paddle_ocr import recognize_image


PARSER_VERSION = "0.1.0"


def _strip_long_path_prefix(value: str) -> str:
    """Strip a Windows \\\\?\\ (and \\\\?\\UNC\\) prefix so WinRT and PowerShell
    components can open the path. pypdf tolerates the prefix, but
    Windows.Data.Pdf rejects it, which surfaced as PDF_RENDER_FAILED."""
    if value[:8].lower() == "\\\\?\\unc\\":
        return "\\\\" + value[8:]
    if value.startswith("\\\\?\\"):
        return value[4:]
    return value
SUPPORTED_TEXT = {
    "txt", "text", "md", "csv", "tsv", "html", "htm", "ini", "iml",
    "log", "conf", "cfg", "properties",
}
SUPPORTED_CODE = {
    "rs", "py", "js", "jsx", "mjs", "cjs", "ts", "tsx", "java", "kt", "kts", "go",
    "c", "cc", "cpp", "h", "hpp", "cs", "rb", "php", "swift", "scala", "sh", "ps1",
    "sql", "json", "yaml", "yml", "toml", "xml", "css", "scss", "vue", "svelte",
}
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
class OcrRuntimeConfig:
    model_path: str
    det_model_path: str
    cls_model_path: str
    dictionary_path: str
    threads: int = 1
    confidence_threshold: float = 0.45

    @classmethod
    def from_dict(cls, value: Any) -> "OcrRuntimeConfig | None":
        if value is None:
            return None
        if not isinstance(value, dict):
            raise ValueError("ocr_runtime must be an object or null")
        paths = [value.get(key) for key in ("model_path", "det_model_path", "cls_model_path", "dictionary_path")]
        if not all(isinstance(path, str) and path.strip() for path in paths):
            raise ValueError("ocr_runtime model package is incomplete")
        threads = value.get("threads", 1)
        if not isinstance(threads, int) or isinstance(threads, bool) or threads < 1 or threads > 4:
            raise ValueError("ocr_runtime threads must be between 1 and 4")
        confidence_threshold = value.get("confidence_threshold", 0.45)
        if not isinstance(confidence_threshold, (int, float)) or isinstance(confidence_threshold, bool) or not 0.0 <= float(confidence_threshold) <= 1.0:
            raise ValueError("ocr_runtime confidence_threshold must be between 0 and 1")
        return cls(paths[0], paths[1], paths[2], paths[3], threads, float(confidence_threshold))

    def payload(self, image_path: Path, page_no: int) -> dict[str, Any]:
        return {
            "model_path": self.model_path,
            "det_model_path": self.det_model_path,
            "cls_model_path": self.cls_model_path,
            "dictionary_path": self.dictionary_path,
            "threads": self.threads,
            "image_path": str(image_path),
            "page_no": page_no,
        }


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
    asset_cache_dir: str | None = None
    ocr_runtime: OcrRuntimeConfig | None = None
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
        asset_cache_dir = value.get("asset_cache_dir")
        if asset_cache_dir is not None and (not isinstance(asset_cache_dir, str) or not asset_cache_dir.strip()):
            raise ValueError("asset_cache_dir必须是非空字符串或null")
        return cls(
            job_id=required_ids[0],
            file_id=required_ids[1],
            revision_id=required_ids[2],
            source_path=_strip_long_path_prefix(source_path),
            format=source_format.lower().lstrip("."),
            ocr_policy=ocr_policy,
            language_hints=tuple(hints),
            max_pages=max_pages,
            asset_cache_dir=_strip_long_path_prefix(asset_cache_dir) if asset_cache_dir is not None else None,
            ocr_runtime=OcrRuntimeConfig.from_dict(value.get("ocr_runtime")),
            parser_version=str(value.get("parser_version") or PARSER_VERSION),
        )


@dataclass(frozen=True, slots=True)
class ParseWarning:
    code: str
    message: str
    locator: dict[str, Any] | None = None


@dataclass(frozen=True, slots=True)
class OcrAttempt:
    engine: str
    model_version: str | None
    status: Literal["completed", "failed", "no_text"]
    page_no: int | None = None
    confidence: float | None = None
    fallback_reason: str | None = None
    elapsed_ms: int = 0
    error: WorkerError | None = None


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
class ImageAsset:
    asset_id: str
    revision_id: str
    asset_kind: str
    cache_path: str
    mime_type: str
    size_bytes: int
    sha256: str
    locator: dict[str, Any]
    ocr_text: str | None = None
    description: str | None = None
    vision_model_id: str | None = None
    status: str = "pending_understanding"


@dataclass(frozen=True, slots=True)
class ParseResult:
    revision_id: str
    status: Literal["parsed", "partial", "encrypted", "unsupported", "failed"]
    parser_name: str
    parser_version: str
    nodes: tuple[DocumentNode, ...]
    image_assets: tuple[ImageAsset, ...]
    ocr_attempts: tuple[OcrAttempt, ...]
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


def _cache_image_asset(
    request: ParseRequest,
    content: bytes,
    suffix: str,
    asset_kind: str,
    source_locator: dict[str, Any],
    ocr_text: str | None = None,
) -> ImageAsset | None:
    if not request.asset_cache_dir or not content:
        return None
    cache_directory = Path(request.asset_cache_dir)
    cache_directory.mkdir(parents=True, exist_ok=True)
    asset_id = uuid7()
    clean_suffix = re.sub(r"[^a-zA-Z0-9]", "", suffix.lower().lstrip(".")) or "bin"
    target = cache_directory / f"{asset_id}.{clean_suffix}"
    temporary = cache_directory / f".{asset_id}.part"
    with temporary.open("xb") as stream:
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, target)
    return ImageAsset(
        asset_id=asset_id,
        revision_id=request.revision_id,
        asset_kind=asset_kind,
        cache_path=str(target),
        mime_type=mimetypes.guess_type(target.name)[0] or "application/octet-stream",
        size_bytes=len(content),
        sha256=hashlib.sha256(content).hexdigest(),
        locator=source_locator,
        ocr_text=ocr_text,
    )


def _relationship_targets(package: zipfile.ZipFile, relationship_path: str) -> dict[str, str]:
    if relationship_path not in package.namelist():
        return {}
    root = ElementTree.fromstring(package.read(relationship_path))
    return {
        relation.attrib.get("Id", ""): relation.attrib.get("Target", "").replace("\\", "/")
        for relation in root.iter()
        if relation.tag.endswith("Relationship")
    }


def _openxml_image_assets(request: ParseRequest, path: Path, source_format: str) -> list[ImageAsset]:
    prefixes = {
        "docx": ("word/media/", "docx"),
        "docm": ("word/media/", "docx"),
        "xlsx": ("xl/media/", "spreadsheet"),
        "xlsm": ("xl/media/", "spreadsheet"),
        "pptx": ("ppt/media/", "presentation"),
        "pptm": ("ppt/media/", "presentation"),
    }
    prefix, locator_kind = prefixes[source_format]
    assets: list[ImageAsset] = []
    with zipfile.ZipFile(path) as package:
        media_paths = [name for name in package.namelist() if name.startswith(prefix) and not name.endswith("/")]
        locations: dict[str, dict[str, Any]] = {}
        if source_format in {"pptx", "pptm"}:
            slide_paths = sorted(
                (name for name in package.namelist() if re.fullmatch(r"ppt/slides/slide\d+\.xml", name)),
                key=lambda name: int(re.search(r"\d+", Path(name).stem).group()),
            )
            for slide_number, slide_path in enumerate(slide_paths, 1):
                relationships = _relationship_targets(package, f"ppt/slides/_rels/{Path(slide_path).name}.rels")
                root = ElementTree.fromstring(package.read(slide_path))
                shape_number = 0
                for element in root.iter():
                    relation_id = next((value for key, value in element.attrib.items() if key.endswith("}embed")), None)
                    if relation_id and relation_id in relationships:
                        shape_number += 1
                        target = relationships[relation_id].removeprefix("../")
                        media_path = target if target.startswith("ppt/") else f"ppt/{target}"
                        locations[media_path] = locator("presentation", slide_no=slide_number, shape_no=shape_number)
        elif source_format in {"docx", "docm"}:
            relationships = _relationship_targets(package, "word/_rels/document.xml.rels")
            if "word/document.xml" in package.namelist():
                root = ElementTree.fromstring(package.read("word/document.xml"))
                for paragraph_number, paragraph in enumerate((item for item in root.iter() if item.tag.endswith("}p")), 1):
                    for element in paragraph.iter():
                        relation_id = next((value for key, value in element.attrib.items() if key.endswith("}embed")), None)
                        if relation_id and relation_id in relationships:
                            target = relationships[relation_id].removeprefix("../")
                            media_path = target if target.startswith("word/") else f"word/{target}"
                            locations[media_path] = locator("docx", paragraph_no=paragraph_number)
        total_bytes = 0
        for media_path in media_paths[:512]:
            content = package.read(media_path)
            total_bytes += len(content)
            if total_bytes > 256 * 1024 * 1024:
                break
            source_locator = locations.get(media_path, locator(locator_kind))
            asset = _cache_image_asset(request, content, Path(media_path).suffix, "embedded_image", source_locator)
            if asset is not None:
                assets.append(asset)
    return assets


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


_CODE_SYMBOL_PATTERNS = (
    re.compile(r"^\s*(?:(?:pub|public|private|protected|internal|static|export|default|abstract|final|open)\s+)*(?:async\s+)?(?:def|class|fn|func|function|interface|struct|enum|trait|impl|record|module)\s+([A-Za-z_$][\w$]*)"),
    re.compile(r"^\s*(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=.*(?:=>|function\b)"),
    re.compile(r"^\s*(?:function\s+)?([A-Za-z_][\w]*)\s*\(\s*\)\s*\{"),
)


def _code_symbol(line: str) -> str | None:
    for pattern in _CODE_SYMBOL_PATTERNS:
        match = pattern.match(line)
        if match:
            return match.group(1)
    return None


def _code_nodes(path: Path, source_format: str) -> list[DocumentNode]:
    lines = _decode_text(path).splitlines()
    if not lines:
        return []
    starts = [(index, symbol) for index, line in enumerate(lines) if (symbol := _code_symbol(line))]
    ranges: list[tuple[int, int, str | None]] = []
    if starts:
        first_start = starts[0][0]
        if first_start > 0 and any(line.strip() for line in lines[:first_start]):
            ranges.append((0, first_start, None))
        for position, (start, symbol) in enumerate(starts):
            end = starts[position + 1][0] if position + 1 < len(starts) else len(lines)
            ranges.append((start, end, symbol))
    else:
        ranges.extend((start, min(start + 120, len(lines)), None) for start in range(0, len(lines), 120))
    nodes: list[DocumentNode] = []
    for start, end, symbol in ranges:
        text = "\n".join(lines[start:end]).strip()
        if not text:
            continue
        heading_path = (source_format, symbol) if symbol else (source_format,)
        nodes.append(DocumentNode(
            uuid7(),
            None,
            len(nodes) + 1,
            "code_symbol" if symbol else "code_block",
            text,
            None,
            locator("code", line_start=start + 1, line_end=end, heading_path=list(heading_path)),
            heading_path,
        ))
    return nodes


def _zip_manifest_nodes(path: Path) -> tuple[list[DocumentNode], list[ParseWarning]]:
    """Read only the ZIP central directory; never extract archive members."""
    nodes: list[DocumentNode] = []
    warnings: list[ParseWarning] = []
    with zipfile.ZipFile(path) as package:
        members = package.infolist()
        if len(members) > 10_000:
            warnings.append(ParseWarning("ARCHIVE_MANIFEST_TRUNCATED", "压缩包条目超过10000项，仅索引前10000项清单"))
            members = members[:10_000]
        for offset in range(0, len(members), 200):
            batch = members[offset:offset + 200]
            rows = [[item.filename, str(item.file_size), "目录" if item.is_dir() else "文件"] for item in batch]
            nodes.append(DocumentNode(
                uuid7(),
                None,
                len(nodes) + 1,
                "archive_manifest",
                None,
                {"columns": ["路径", "大小（字节）", "类型"], "rows": rows},
                locator("archive", line_start=offset + 1, line_end=offset + len(batch)),
                ("zip", "清单"),
            ))
    return nodes, warnings


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


def _recognize_pdf_pages(
    request: ParseRequest,
    path: Path,
    page_numbers: list[int],
    render_directory: Path | None,
) -> tuple[dict[str, Any] | None, WorkerError | None, str, list[ParseWarning], list[OcrAttempt]]:
    attempts: list[OcrAttempt] = []
    if request.ocr_runtime is None or render_directory is None:
        started_at = time.monotonic()
        result, error = recognize_with_windows(
            path, "pdf", request.max_pages, request.language_hints, page_numbers, render_directory
        )
        attempts.append(_ocr_attempt("windows-ocr", "Windows.Media.Ocr", result, error, None, started_at))
        return result, error, "windows-ocr", [], attempts

    primary_started_at = time.monotonic()
    rendered, render_error = recognize_with_windows(
        path,
        "pdf",
        request.max_pages,
        request.language_hints,
        page_numbers,
        render_directory,
        render_only=True,
    )
    primary_error = render_error
    primary_fallback_reason = render_error.code if render_error else None
    if render_error is not None:
        attempts.append(
            _ocr_attempt(
                "windows-pdf-renderer",
                "Windows.Data.Pdf",
                None,
                render_error,
                render_error.code,
                primary_started_at,
            )
        )
    if rendered is not None and primary_error is None:
        lines: list[dict[str, Any]] = []
        for item in rendered.get("rendered_pages", []):
            page_number = int(item["page_no"])
            page_result, page_error = recognize_image(
                request.ocr_runtime.payload(Path(str(item["path"])), page_number)
            )
            if page_error is not None:
                primary_error = page_error
                break
            lines.extend((page_result or {}).get("lines", []))
        if primary_error is None:
            rendered["lines"] = lines
            rendered["engine"] = "rapidocr-onnxruntime"
            rendered["model_version"] = "PP-OCRv5-mobile"
            primary_fallback_reason = _ocr_fallback_reason(rendered, None, request.ocr_runtime.confidence_threshold)
            attempts.append(_ocr_attempt("rapidocr-onnxruntime", "PP-OCRv5-mobile", rendered, None, primary_fallback_reason, primary_started_at))
            if primary_fallback_reason is None:
                return rendered, None, "rapidocr-ppocrv5", [], attempts
        if primary_error is not None:
            attempts.append(_ocr_attempt("rapidocr-onnxruntime", "PP-OCRv5-mobile", None, primary_error, primary_error.code, primary_started_at))

    # Keep indexing recoverable on systems where the optional model runtime is
    # temporarily unavailable. The fallback is explicit in warnings and parser
    # metadata instead of silently claiming that PaddleOCR ran.
    fallback_warning = ParseWarning(
        "OCR_ENGINE_FALLBACK",
        f"PP-OCRv5 unavailable or below threshold ({primary_error.code if primary_error else primary_fallback_reason or 'OCR_RENDER_FAILED'}); Windows OCR compatibility fallback was used",
    )
    fallback_started_at = time.monotonic()
    result, error = recognize_with_windows(
        path, "pdf", request.max_pages, request.language_hints, page_numbers, render_directory
    )
    attempts.append(_ocr_attempt("windows-ocr", "Windows.Media.Ocr", result, error, primary_error.code if primary_error else primary_fallback_reason or "OCR_RENDER_FAILED", fallback_started_at))
    return result, error, "windows-ocr-fallback", [fallback_warning], attempts


def _ocr_attempt(
    engine: str,
    model_version: str | None,
    result: dict[str, Any] | None,
    error: WorkerError | None,
    fallback_reason: str | None,
    started_at: float,
) -> OcrAttempt:
    lines = (result or {}).get("lines", [])
    confidence_values = [float(line["confidence"]) for line in lines if isinstance(line, dict) and isinstance(line.get("confidence"), (int, float))]
    confidence = sum(confidence_values) / len(confidence_values) if confidence_values else None
    status: Literal["completed", "failed", "no_text"] = "failed" if error else "completed" if lines else "no_text"
    return OcrAttempt(engine, model_version, status, None, confidence, fallback_reason, int((time.monotonic() - started_at) * 1000), error)


def _ocr_fallback_reason(result: dict[str, Any] | None, error: WorkerError | None, confidence_threshold: float) -> str | None:
    if error is not None:
        return error.code
    lines = (result or {}).get("lines", [])
    if not lines:
        return "OCR_NO_TEXT"
    confidence_values = [float(line["confidence"]) for line in lines if isinstance(line, dict) and isinstance(line.get("confidence"), (int, float))]
    if not confidence_values:
        return "OCR_CONFIDENCE_MISSING"
    if sum(confidence_values) / len(confidence_values) < confidence_threshold:
        return "OCR_LOW_CONFIDENCE"
    return None


def _pdf_result(request: ParseRequest, path: Path, started_at: float) -> ParseResult:
    try:
        from pypdf import PdfReader
    except ImportError:
        return _failure(request, "PARSER_DEPENDENCY_MISSING", "PDF解析依赖尚未安装", True)
    try:
        reader = PdfReader(path)
        if reader.is_encrypted:
            return _result(request, "encrypted", "pypdf", [], [ParseWarning("PDF_ENCRYPTED", "PDF已加密，请提供未加密副本")], 0)
        nodes: list[DocumentNode] = []
        image_assets: list[ImageAsset] = []
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
            try:
                for image in page.images:
                    image_name = str(getattr(image, "name", "image.bin"))
                    image_data = bytes(getattr(image, "data", b""))
                    asset = _cache_image_asset(
                        request,
                        image_data,
                        Path(image_name).suffix,
                        "pdf_embedded_image",
                        locator("pdf", page_no=page_number),
                    )
                    if asset is not None:
                        image_assets.append(asset)
            except (AttributeError, OSError, ValueError) as error:
                warnings.append(ParseWarning("PDF_IMAGE_EXTRACT_FAILED", str(error), locator("pdf", page_no=page_number)))
        ocr_page_count = 0
        parser_name = "pypdf"
        if ocr_pages:
            render_directory: Path | None = None
            if request.asset_cache_dir:
                render_directory = Path(request.asset_cache_dir) / f".ocr-render-{uuid7()}"
            try:
                ocr_result, ocr_error, ocr_engine, ocr_warnings, ocr_attempts = _recognize_pdf_pages(
                    request, path, ocr_pages, render_directory
                )
                warnings.extend(ocr_warnings)
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
                        parser_name = f"pypdf+{ocr_engine}"
                    ocr_text_by_page: dict[int, str] = {}
                    for page_number in ocr_pages:
                        page_text = "\n".join(
                            node.text or ""
                            for node in recognized
                            if node.locator.get("page_no") == page_number
                        ).strip()
                        if page_text:
                            ocr_text_by_page[page_number] = page_text
                        if page_number not in recognized_pages:
                            warnings.append(ParseWarning("OCR_NO_TEXT", "该页OCR后没有识别到文字", locator("pdf", page_no=page_number)))
                            warnings.append(ParseWarning("OCR_REQUIRED", "该页尚未获得可索引文字", locator("pdf", page_no=page_number)))
                    for rendered in ocr_result.get("rendered_pages", []):
                        try:
                            page_number = int(rendered["page_no"])
                            rendered_path = Path(str(rendered["path"]))
                            asset = _cache_image_asset(
                                request,
                                rendered_path.read_bytes(),
                                ".png",
                                "pdf_scanned_page",
                                locator("pdf", page_no=page_number),
                                ocr_text_by_page.get(page_number),
                            )
                            if asset is not None:
                                image_assets.append(asset)
                        except (KeyError, OSError, TypeError, ValueError) as error:
                            warnings.append(ParseWarning("PDF_IMAGE_EXTRACT_FAILED", str(error)))
            finally:
                if render_directory is not None:
                    shutil.rmtree(render_directory, ignore_errors=True)
        status: Literal["parsed", "partial"] = "partial" if warnings else "parsed"
        return _result(request, status, parser_name, nodes, warnings, len(pages), started_at, ocr_page_count, image_assets, ocr_attempts if ocr_pages else [])
    except Exception as error:
        # pypdf 内部还会抛出 PdfStreamError/TypeError/struct.error 等不在
        # (PdfReadError, OSError, ValueError) 里的异常；宽捕获避免 worker 进程死亡。
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
    image_assets: list[ImageAsset] | None = None,
    ocr_attempts: list[OcrAttempt] | None = None,
) -> ParseResult:
    return ParseResult(
        revision_id=request.revision_id,
        status=status,
        parser_name=parser_name,
        parser_version=request.parser_version,
        nodes=tuple(nodes),
        image_assets=tuple(image_assets or ()),
        ocr_attempts=tuple(ocr_attempts or ()),
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
        image_assets=(),
        ocr_attempts=(),
        warnings=(),
        metrics={"page_count": 0, "node_count": 0, "character_count": 0, "ocr_page_count": 0, "elapsed_ms": 0},
        error=WorkerError(code, message, retryable),
    )


def _find_libreoffice() -> Path | None:
    explicit = os.environ.get("FANFAN_LIBREOFFICE_EXE")
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
    with tempfile.TemporaryDirectory(prefix="fanfan-legacy-office-") as temporary_raw:
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
        if source_format in SUPPORTED_CODE:
            nodes = _code_nodes(path, source_format)
            return _result(request, "parsed", "stdlib-code-structure", nodes, [], 0, started_at)
        if source_format == "zip":
            nodes, warnings = _zip_manifest_nodes(path)
            return _result(request, "parsed", "stdlib-zip-manifest", nodes, warnings, 0, started_at)
        if source_format in {"docx", "docm"}:
            nodes = _docx_nodes(path)
            image_assets = _openxml_image_assets(request, path, source_format)
            return _result(request, "parsed", "openxml-docx", nodes, [], 0, started_at, image_assets=image_assets)
        if source_format in {"xlsx", "xlsm"}:
            nodes = _xlsx_nodes(path)
            image_assets = _openxml_image_assets(request, path, source_format)
            return _result(request, "parsed", "openxml-xlsx", nodes, [], 0, started_at, image_assets=image_assets)
        if source_format in {"pptx", "pptm"}:
            nodes = _pptx_nodes(path)
            image_assets = _openxml_image_assets(request, path, source_format)
            return _result(request, "parsed", "openxml-pptx", nodes, [], len(nodes), started_at, image_assets=image_assets)
        if source_format == "pdf":
            return _pdf_result(request, path, started_at)
        if source_format in IMAGE_FORMATS:
            standalone_asset = _cache_image_asset(request, path.read_bytes(), path.suffix, "standalone_image", locator("image", page_no=1))
            image_assets = [standalone_asset] if standalone_asset is not None else []
            if request.ocr_policy == "disabled":
                return _result(request, "partial", "image-metadata", [], [ParseWarning("OCR_REQUIRED", "图片需要OCR后才能建立全文索引")], 1, started_at, image_assets=image_assets)
            ocr_warnings: list[ParseWarning] = []
            ocr_attempts: list[OcrAttempt] = []
            ocr_engine = "rapidocr-ppocrv5"
            if request.ocr_runtime is not None:
                ocr_started_at = time.monotonic()
                ocr_result, ocr_error = recognize_image(request.ocr_runtime.payload(path, 1))
                fallback_reason = _ocr_fallback_reason(ocr_result, ocr_error, request.ocr_runtime.confidence_threshold)
                ocr_attempts.append(_ocr_attempt("rapidocr-onnxruntime", "PP-OCRv5-mobile", ocr_result, ocr_error, fallback_reason, ocr_started_at))
                if fallback_reason is not None:
                    ocr_warnings.append(ParseWarning(
                        "OCR_ENGINE_FALLBACK",
                        f"PP-OCRv5 unavailable or below threshold ({fallback_reason}); Windows OCR compatibility fallback was used",
                    ))
                    fallback_started_at = time.monotonic()
                    ocr_result, ocr_error = recognize_with_windows(path, "image", 1, request.language_hints)
                    ocr_attempts.append(_ocr_attempt("windows-ocr", "Windows.Media.Ocr", ocr_result, ocr_error, fallback_reason, fallback_started_at))
                    ocr_engine = "windows-ocr-fallback"
            else:
                ocr_started_at = time.monotonic()
                ocr_result, ocr_error = recognize_with_windows(path, "image", 1, request.language_hints)
                ocr_attempts.append(_ocr_attempt("windows-ocr", "Windows.Media.Ocr", ocr_result, ocr_error, None, ocr_started_at))
                ocr_engine = "windows-ocr"
            if ocr_error:
                return _result(request, "partial", "image-metadata", [], ocr_warnings + [ParseWarning(ocr_error.code, ocr_error.message), ParseWarning("OCR_REQUIRED", "图片尚未完成OCR")], 1, started_at, image_assets=image_assets, ocr_attempts=ocr_attempts)
            nodes = _ocr_nodes(ocr_result or {}, "image")
            ocr_text = "\n".join(node.text or "" for node in nodes).strip() or None
            if standalone_asset is not None:
                image_assets = [ImageAsset(
                    standalone_asset.asset_id,
                    standalone_asset.revision_id,
                    standalone_asset.asset_kind,
                    standalone_asset.cache_path,
                    standalone_asset.mime_type,
                    standalone_asset.size_bytes,
                    standalone_asset.sha256,
                    standalone_asset.locator,
                    ocr_text,
                )]
            if not nodes:
                return _result(request, "partial", ocr_engine, [], ocr_warnings + [ParseWarning("OCR_NO_TEXT", "图片OCR后没有识别到文字"), ParseWarning("OCR_REQUIRED", "图片尚未获得可索引文字")], 1, started_at, image_assets=image_assets, ocr_attempts=ocr_attempts)
            return _result(request, "partial" if ocr_warnings else "parsed", ocr_engine, nodes, ocr_warnings, 1, started_at, 1, image_assets, ocr_attempts)
        if source_format in LEGACY_OFFICE:
            return _legacy_office_result(request, path, source_format, started_at)
        return _result(request, "unsupported", "none", [], [ParseWarning("FORMAT_UNSUPPORTED", f"暂不支持{source_format}格式")], 0, started_at)
    except PermissionError:
        return _failure(request, "FILE_PERMISSION_DENIED", "没有读取此文件的权限", True)
    except (OSError, KeyError, ValueError, zipfile.BadZipFile, ElementTree.ParseError) as error:
        return _failure(request, "DOCUMENT_PARSE_FAILED", str(error), False)
    except Exception as error:
        # 最后防线：任何未预料的异常都转为文件级失败而不是让 worker 进程退出。
        return _failure(request, "DOCUMENT_PARSE_FAILED", str(error), False)
