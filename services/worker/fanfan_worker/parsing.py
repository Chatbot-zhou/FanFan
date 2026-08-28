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
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from typing import Any, Literal
from uuid import UUID
from xml.etree import ElementTree

from .protocol import WorkerError, is_uuid_v7
from .ocr import recognize_with_windows
from .paddle_ocr import recognize_image
from .image_ocr import complex_visual_reason


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

# 单文档 OCR 页数预算：与 ocr.py 的渲染/识别超时预算（270s）匹配，
# 超出预算的扫描页标记 OCR_REQUIRED 待后续补跑。
OCR_PAGE_BUDGET = 100
# rapidocr 逐页识别的时间预算：单页实测 ~6s，100 页需 ~600s，而 Rust 侧
# document.parse 超时（worker.rs operation_timeout）是 360s——页数预算
# 拦不住大扫描书。识别循环按此预算提前收尾，剩余页标记 OCR_REQUIRED，
# 首次索引只识别前若干页，绝不因识别太慢拖垮整份解析。
OCR_TIME_BUDGET_SECONDS = 200.0
# 纯文本/代码解析的单文件读取上限：超大日志/源码（数 GB）全量 read_bytes()
# 会把 worker 进程内存耗尽。只读前 64MB，截断后仍可索引可检索的开头部分。
MAX_TEXT_READ_BYTES = 64 * 1024 * 1024
# pypdf 逐页提取的探针阈值（字符）：pypdf 提取低于此值、而 PyMuPDF 能提取出
# 显著更丰富文本（≥30 且 > pypdf 的 3 倍）时，判定 pypdf 只提取到页眉/页脚水印
# 欠提取正文，改用 PyMuPDF 文本。仅按体量差异比对，不针对具体文件/关键词。
PDF_TEXT_FALLBACK_PROBE_MIN = 200
# PyMuPDF 兜底生效需要的最低文本量：低于它的页仍视为需要 OCR 抢救。
PDF_TEXT_FALLBACK_MIN = 30
# 水印/样板页 OCR 判定：PDF 页若提取到的文本层很短且在多页重复，通常是页眉/页脚
# 水印类样板文字（如「内部资料，禁止传播」），正文实际在光栅/描边位图里、不在任何
# 文本层中。这类页按字面量索引只有水印、正文检索不到，应按"扫描件"送 OCR。判定规则
# 仅依据「归一化文本长度 + 跨页重复度」，不针对任何具体文件/关键词/case。
PDF_WATERMARK_MAX_CHARS = 200   # 归一化后少于该字符数的页视为"薄文本页"
PDF_WATERMARK_REPEAT_MIN = 3    # 相同薄文本至少出现在 N 页才判定为样板/水印


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
    ocr_version: str = "PPOCRV5"

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
        ocr_version = value.get("ocr_version", "PPOCRV5")
        if not isinstance(ocr_version, str) or ocr_version not in ("PPOCRV4", "PPOCRV5", "PPOCRV6"):
            raise ValueError("ocr_runtime ocr_version must be PPOCRV4/PPOCRV5/PPOCRV6")
        return cls(paths[0], paths[1], paths[2], paths[3], threads, float(confidence_threshold), ocr_version)

    def payload(self, image_path: Path, page_no: int) -> dict[str, Any]:
        return {
            "model_path": self.model_path,
            "det_model_path": self.det_model_path,
            "cls_model_path": self.cls_model_path,
            "dictionary_path": self.dictionary_path,
            "threads": self.threads,
            "ocr_version": self.ocr_version,
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
    ocr_confidence: float | None = None
    ocr_engine: str | None = None
    description: str | None = None
    vision_model_id: str | None = None
    vision_route_reason: str | None = None
    status: str = "pending_ocr"


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
    ocr_confidence: float | None = None,
    ocr_engine: str | None = None,
    vision_route_reason: str | None = None,
    status: str = "pending_ocr",
) -> ImageAsset | None:
    if not request.asset_cache_dir or not content:
        return None
    cache_directory = Path(request.asset_cache_dir)
    cache_directory.mkdir(parents=True, exist_ok=True)
    asset_id = uuid7()
    clean_suffix = re.sub(r"[^a-zA-Z0-9]", "", suffix.lower().lstrip(".")) or "bin"
    target = cache_directory / f"{asset_id}.{clean_suffix}"
    temporary = cache_directory / f".{asset_id}.part"
    try:
        with temporary.open("xb") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, target)
    except BaseException:
        # 写入中断/磁盘满等异常时清理残留的 .part，避免下次扫描重复积累垃圾文件。
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass
        raise
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
        ocr_confidence=ocr_confidence,
        ocr_engine=ocr_engine,
        vision_route_reason=vision_route_reason,
        status=status,
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
            try:
                content = package.read(media_path)
            except (zipfile.BadZipFile, EOFError, OSError):
                # 真实世界损坏的 zip 条目（如国产办公软件导出的 CRC 校验失败）
                # 只跳过该图片，绝不让内嵌图片损坏连累整个文档解析失败。
                continue
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
    # 只读前 MAX_TEXT_READ_BYTES：超大文本/源码文件不做全量读入，防止 OOM。
    with path.open("rb") as stream:
        raw = stream.read(MAX_TEXT_READ_BYTES)
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
    # 单个 sheet 解析异常只跳过该 sheet（记入警告），不让整个 xlsx 解析失败；
    # 行数/单元格均有上限，防止超大表格把 worker 内存耗尽。
    spreadsheet_ns = "http://schemas.openxmlformats.org/spreadsheetml/2006/main"
    relationship_ns = "http://schemas.openxmlformats.org/package/2006/relationships"
    office_rel_ns = "http://schemas.openxmlformats.org/officeDocument/2006/relationships"
    max_rows_per_sheet = 10_000
    with zipfile.ZipFile(path) as package:
        shared: list[str] = []
        if "xl/sharedStrings.xml" in package.namelist():
            try:
                shared_root = ElementTree.fromstring(package.read("xl/sharedStrings.xml"))
                shared = [_xml_text(item) for item in shared_root.findall(f"{{{spreadsheet_ns}}}si")]
            except (ElementTree.ParseError, KeyError, OSError, ValueError):
                # sharedStrings 损坏时按"无共享字符串"降级，单元格仍可索引其字面量。
                shared = []
        workbook = ElementTree.fromstring(package.read("xl/workbook.xml"))
        relationships = ElementTree.fromstring(package.read("xl/_rels/workbook.xml.rels"))
        targets = {
            relation.attrib["Id"]: relation.attrib["Target"]
            for relation in relationships.findall(f"{{{relationship_ns}}}Relationship")
        }
        sheets = []
        for sheet in workbook.findall(f".//{{{spreadsheet_ns}}}sheet"):
            relation_id = sheet.attrib.get(f"{{{office_rel_ns}}}id")
            target = targets.get(relation_id) if relation_id else None
            if target is None:
                # 缺少关联目标（异常文件）时跳过该 sheet，不拖垮整体。
                continue
            target = target.replace("\\", "/")
            if target.startswith("/"):
                package_path = target.lstrip("/")
            else:
                package_path = f"xl/{target}" if not target.startswith("xl/") else target
            sheets.append((sheet.attrib.get("name", ""), package_path))
        nodes: list[DocumentNode] = []
        for sheet_name, package_path in sheets:
            if package_path not in package.namelist():
                continue
            try:
                sheet_root = ElementTree.fromstring(package.read(package_path))
            except (ElementTree.ParseError, KeyError, OSError, ValueError):
                # sheet XML 损坏：跳过该 sheet，其余 sheet 照常解析。
                continue
            for row in sheet_root.findall(f".//{{{spreadsheet_ns}}}row")[:max_rows_per_sheet]:
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
                        try:
                            index = int(value_element.text)
                            value = shared[index] if 0 <= index < len(shared) else value_element.text
                        except (ValueError, IndexError):
                            value = value_element.text
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


def _renumber_nodes(nodes: list[DocumentNode]) -> list[DocumentNode]:
    """按当前顺序为节点重新分配从 1 递增的 ordinal。

    兜底恢复的行级节点与整页节点混排时会因 ordinal 冲突（整页节点用 page_number 作
    ordinal），这里统一重排保证 ordinal 唯一有序，与 OCR 合并后的重排口径一致。
    """
    return [
        DocumentNode(node.node_id, node.parent_id, ordinal, node.node_type, node.text, node.table_data, node.locator, node.heading_path)
        for ordinal, node in enumerate(nodes, 1)
    ]


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


def _ocr_page_area_bbox(result_lines: list[dict[str, Any]], page_number: int) -> dict[str, float] | None:
    """计算某页所有 OCR 行 bbox 的并集（归一化），供整页 image_ocr 搜索节点粗粒度定位。

    OCR 行级节点各自带细粒度 bbox，而整页聚合的 image_ocr 节点（「图片文字：…」）此前
    只用 page_no 定位、无 bbox，导致前端无法在 pdfjs 页面上圈出引用区域。这里按页取所有
    行 bbox 的并集作为该页正文区域，与 _PdfTextFallback.page_text_bbox 同口径（[0,1]
    归一化、左上原点 y 向下）。无行或无 bbox 时返回 None（调用方保持原无 bbox 语义）。
    仅依赖 OCR 几何数据，不针对任何文件/关键词/case。
    """
    xs: list[tuple[float, float]] = []
    ys: list[tuple[float, float]] = []
    for line in result_lines:
        if not isinstance(line, dict) or line.get("page_no") != page_number:
            continue
        bbox = line.get("bbox")
        if not isinstance(bbox, dict):
            continue
        try:
            xs.append((float(bbox["x0"]), float(bbox["x1"])))
            ys.append((float(bbox["y0"]), float(bbox["y1"])))
        except (KeyError, TypeError, ValueError):
            continue
    if not xs:
        return None
    return {
        "x0": max(0.0, min(x0 for x0, _ in xs)),
        "y0": max(0.0, min(y0 for y0, _ in ys)),
        "x1": min(1.0, max(x1 for _, x1 in xs)),
        "y1": min(1.0, max(y1 for _, y1 in ys)),
    }


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
        budget_end = primary_started_at + OCR_TIME_BUDGET_SECONDS
        for item in rendered.get("rendered_pages", []):
            if time.monotonic() >= budget_end:
                # 识别时间预算用尽：剩余页标记 OCR_REQUIRED 待补跑。
                # 已识别页照常返回，绝不让整份解析因识别太慢超时失败。
                break
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


def _load_pymupdf() -> tuple[Any, str | None]:
    """返回 (pymupdf 模块, None) 或 (None, 错误说明)。

    PyMuPDF ≥1.24 官方入口为 pymupdf；fitz 为历史别名（仍可用但已弃用）。两种命名
    都尝试，保证 1.26.7 及更新版本都能加载。供文本兜底与渲染兜底共用。
    """
    try:
        # PyMuPDF ≥1.24 官方入口为 pymupdf；fitz 为历史别名（仍可用但已弃用）。
        import pymupdf as fitz_module  # type: ignore[attr-defined]
        return fitz_module, None
    except ImportError as error:
        try:
            import fitz as fitz_module  # type: ignore
            return fitz_module, None
        except ImportError as error2:
            return None, f"PyMuPDF导入失败: {error}\nfitz 兜底导入也失败: {error2}"


class _PdfTextFallback:
    """pypdf 提取文字过少时的 PyMuPDF 兜底提取器。

    部分带有效文本层的 PDF（如「2023数据库系统工程师备考知识点集锦」）用 pypdf
    逐页只能提取出 0 字符，而 PyMuPDF 能正常提取约 700-800 字符/页。若这类页被
    误判为扫描件送去 OCR，Windows 渲染器又把它整页画成黑底，正文内容会被彻底
    丢失。本兜底按需延迟打开 PyMuPDF 文档，从缓存 doc 中按页号（1-based）提取
    文本；PyMuPDF 未安装或提取失败时返回空串，由调用方走原 OCR 路径。
    """

    def __init__(self, path: Path) -> None:
        # 去掉 \\?\ 长路径前缀：PyMuPDF 不接受该前缀。
        self._path = _strip_long_path_prefix(str(path))
        self._doc: Any = None
        self._unavailable = False
        # 记录 PyMuPDF 不可用的具体原因（导入失败/打开失败），供上层以警告呈现，
        # 避免「欠提取的页本可用 PyMuPDF 兜底却静默失效」长期难以定位。
        self.error: str | None = None
        # 全页块缓存 + 跨页重复样板块集合：首次需要块级数据时惰性构建，之后复用，
        # 避免每页重复打开/提取。`_boilerplate_norms` 是归一化后判定为页眉/页脚
        # 样板的文本集合，节点生成前据此剔除（见 page_blocks 的过滤）。
        self._block_map: dict[int, list[tuple[str, dict[str, float] | None]]] | None = None
        self._boilerplate_norms: set[str] = set()

    def page_text(self, page_number: int) -> str:
        """按 1-based 页号提取该页文本层，失败或不可用返回空串。"""
        if not self._open():
            return ""
        try:
            page = self._doc.load_page(page_number - 1)
            return (page.get_text() or "").strip()
        except Exception:
            return ""

    def page_blocks(self, page_number: int) -> list[tuple[str, dict[str, float] | None]]:
        """按 1-based 页号提取该页文本块，每块附带归一化 bbox。

        与 OCR 线级 bbox 同口径：坐标是 [0,1] 页面分比例（左上原点，y 向下），供前端
        PDF 高亮在 pdfjs 渲染页面上定位引用位置。PyMuPDF 的 bbox 是 point 单位、左上
        原点，这里除以页宽/页高归一化。

        以**文本块**（垂直间隔分隔的段落级单元）为粒度切节点，而非逐行：逐行切会把
        密集排版页（如「2023 数据库系统工程师备考知识点集锦」里被内联公式/文本框打散
        的短行）碎成大量 <10 字符的碎片，稀释检索；块级粒度与正文段落对应，既保持
        语义连贯，又让前端每个引用都能定位到具体块区域高亮。失败或不可用返回空列表。

        返回前剔除跨页重复的页眉/页脚样板块（归一化后出现在足够多页且较短的文本，
        如「内部资料，禁止传播」页眉、带页码的页脚横幅）：这类块在每一页重复，混入
        正文会让 chunk 携带大量重复噪声、稀释检索与摘要质量。仅按跨页重复度 + 长度
        判定，不针对任何具体文件/关键词/case。
        """
        if not self._open():
            return []
        if self._block_map is None:
            self._build_block_cache()
        blocks = self._block_map.get(page_number, [])
        if not self._boilerplate_norms:
            return blocks
        return [
            (text, bbox)
            for text, bbox in blocks
            if not _is_boilerplate_block(text, self._boilerplate_norms)
        ]

    def _build_block_cache(self) -> None:
        """提取全页文本块并计算跨页重复样板（页眉/页脚）集合，缓存供后续复用。

        页眉/页脚样板在每个页面重复出现，混入正文块会让 chunk 携带大量重复噪声
        （实测 2023 知识点集锦 24% chunk 含水印样板）。判定仅依据「归一化文本的
        跨页重复度 + 长度」，不针对任何具体文件/关键词/case：同一归一化文本出现
        在 >= PDF_WATERMARK_REPEAT_MIN 页且长度 <= PDF_WATERMARK_MAX_CHARS 时
        视为样板块。整页文本块提取不做图片解码，成本可接受，仅在真正需要块级
        数据（PyMuPDF 兜底路径）时才构建。
        """
        page_count = 0
        try:
            page_count = int(self._doc.page_count or 0)
        except Exception:
            page_count = 0
        normalized_counts: dict[str, int] = {}
        block_map: dict[int, list[tuple[str, dict[str, float] | None]]] = {}
        for page_number in range(1, page_count + 1):
            try:
                page = self._doc.load_page(page_number - 1)
                page_width = float(page.rect.width) or 1.0
                page_height = float(page.rect.height) or 1.0
                raw_blocks = page.get_text("blocks")
            except Exception:
                continue
            blocks: list[tuple[str, dict[str, float] | None]] = []
            for block in raw_blocks:
                # block 形如 (x0, y0, x1, y1, text, block_no, block_type)；仅收文本块。
                if len(block) < 7 or block[6] != 0:
                    continue
                text = str(block[4] or "").strip()
                if not text:
                    continue
                x0, y0, x1, y1 = block[0], block[1], block[2], block[3]
                try:
                    bbox = {
                        "x0": max(0.0, min(1.0, float(x0) / page_width)),
                        "y0": max(0.0, min(1.0, float(y0) / page_height)),
                        "x1": max(0.0, min(1.0, float(x1) / page_width)),
                        "y1": max(0.0, min(1.0, float(y1) / page_height)),
                    }
                except Exception:
                    bbox = None
                norm = _normalize_boilerplate_text(text)
                normalized_counts[norm] = normalized_counts.get(norm, 0) + 1
                blocks.append((text, bbox))
            block_map[page_number] = blocks
        self._block_map = block_map
        self._boilerplate_norms = {
            norm
            for norm, count in normalized_counts.items()
            if norm and count >= PDF_WATERMARK_REPEAT_MIN and len(norm) <= PDF_WATERMARK_MAX_CHARS
        }

    def page_text_bbox(self, page_number: int) -> dict[str, float] | None:
        """计算该页文本区域的归一化包围盒（所有文本块 bbox 的并集）。

        供给 pypdf 正常提取的整页节点附上粗粒度 bbox：前端在 pdfjs 页面上据此把
        「引用在本页」圈到整个正文区域（而不是只圈首个块或整页留白）。无文本块或
        PyMuPDF 不可用时返回 None（调用方保持原无 bbox 语义）。仅依赖文本块几何，
        不针对任何文件/关键词/case。
        """
        blocks = self.page_blocks(page_number)
        xs: list[tuple[float, float]] = []
        ys: list[tuple[float, float]] = []
        for _, bbox in blocks:
            if bbox:
                xs.append((bbox["x0"], bbox["x1"]))
                ys.append((bbox["y0"], bbox["y1"]))
        if not xs:
            return None
        return {
            "x0": max(0.0, min(x0 for x0, _ in xs)),
            "y0": max(0.0, min(y0 for y0, _ in ys)),
            "x1": min(1.0, max(x1 for _, x1 in xs)),
            "y1": min(1.0, max(y1 for _, y1 in ys)),
        }

    def _open(self) -> bool:
        """延迟打开 PyMuPDF 文档并复用；打开失败则视为不可用。"""
        if self._doc is not None or self._unavailable:
            return self._doc is not None
        module, error = _load_pymupdf()
        if module is None:
            self._unavailable = True
            self.error = error
            return False
        try:
            self._doc = module.open(self._path)
        except Exception as error:
            self._unavailable = True
            self._doc = None
            self.error = f"PyMuPDF打开失败: {type(error).__name__}: {error}"
            return False
        return self._doc is not None

    def close(self) -> None:
        """显式关闭 PyMuPDF 文档，释放文件句柄。"""
        if self._doc is not None:
            try:
                self._doc.close()
            except Exception:
                pass
            self._doc = None


def _normalize_boilerplate_text(text: str) -> str:
    """归一化样板块文本：折叠空白并剥离尾部页码变体（如「1 / 32」）。

    页脚横幅常带随页变化的「N / M」页码，逐页文本并不完全相同；剥离尾部页码
    后页脚主体在跨页间一致，才能用「跨页重复度」识别。仅做结构归一化，不针对
    任何具体文件/关键词/case。
    """
    collapsed = " ".join(text.split())
    return re.sub(r"\s*\d+\s*/\s*\d+\s*$", "", collapsed).strip()


def _is_boilerplate_block(text: str, boilerplate_norms: set[str]) -> bool:
    """判定单块文本是否为页眉/页脚样板块。

    两类命中即视为样板：1) 归一化文本落在跨页重复样板块集合中；2) 纯页码碎片
    （如「1 / 32」「第 3 页」，不携带任何正文信息）。纯页码碎片归一化后为空串，
    不会被重复度集合收录，这里单独按形态识别。
    """
    norm = _normalize_boilerplate_text(text)
    if norm in boilerplate_norms:
        return True
    collapsed = " ".join(text.split())
    return bool(re.fullmatch(r"\d+\s*/\s*\d+", collapsed)) or bool(
        re.fullmatch(r"第\s*\d+\s*页", collapsed)
    )


def _detect_watermark_pages(pages: Any) -> set[int]:
    """识别"薄文本 + 跨页重复"的样板页，返回需要转送 OCR 的页码集合。

    pypdf 逐页提取文本；某页归一化文本少于 PDF_WATERMARK_MAX_CHARS 且与至少
    PDF_WATERMARK_REPEAT_MIN 页完全相同，判定为页眉/页脚水印类样板文字（扫描/
    拼版书的正文不在文本层）。仅按长度 + 重复度判定，不针对任何文件/关键词/case。
    """
    texts: list[tuple[int, str]] = []
    for page_number, page in enumerate(pages, 1):
        try:
            text = (page.extract_text() or "").strip()
        except Exception:
            text = ""
        norm = re.sub(r"\s+", "", text)
        if norm and len(norm) < PDF_WATERMARK_MAX_CHARS:
            texts.append((page_number, norm))
    counts: dict[str, int] = {}
    for _, norm in texts:
        counts[norm] = counts.get(norm, 0) + 1
    repeated = {norm for norm, count in counts.items() if count >= PDF_WATERMARK_REPEAT_MIN}
    return {page_number for page_number, norm in texts if norm in repeated}


def _pdf_result(request: ParseRequest, path: Path, started_at: float) -> ParseResult:
    try:
        from pypdf import PdfReader
    except ImportError:
        return _failure(request, "PARSER_DEPENDENCY_MISSING", "PDF解析依赖尚未安装", True)
    try:
        reader = PdfReader(path)
        if reader.is_encrypted:
            # 大量办公扫描件只是空密码加密（内容实际可读）；pypdf 打开后
            # is_encrypted 为真，先尝试空密码解密，成功则继续正常解析。
            try:
                unlocked = bool(reader.decrypt(""))
            except Exception:
                unlocked = False
            if not unlocked:
                return _result(request, "encrypted", "pypdf", [], [ParseWarning("PDF_ENCRYPTED", "PDF已加密，请提供未加密副本")], 0)
        nodes: list[DocumentNode] = []
        image_assets: list[ImageAsset] = []
        warnings: list[ParseWarning] = []
        ocr_pages: list[int] = []
        fitz_fallback = _PdfTextFallback(path)
        fitz_used_pages = 0
        fitz_fallback_consulted = False
        pages = reader.pages[: request.max_pages] if request.max_pages else reader.pages
        # 先做一次轻量预扫：识别"薄文本 + 跨页重复"的样板页（页眉/页脚水印，正文在
        # 光栅位图里）页码集合。正文缺失的这些页即使文本层有少量水印，也要送 OCR。
        boilerplate_pages = _detect_watermark_pages(pages)
        for page_number, page in enumerate(pages, 1):
            try:
                text = (page.extract_text() or "").strip()
            except Exception as error:
                # 单页提取失败（如 JBIG2 图片无 jbig2dec 解码器）只空置该页并
                # 标记 OCR 抢救，绝不让整份 PDF 解析失败。
                text = ""
                warnings.append(ParseWarning("PDF_PARSE_FAILED", str(error), locator("pdf", page_no=page_number)))
            # 每页初始的兜底状态：未发生 PyMuPDF 兜底时 use_blocks 视为不使用行级 bbox 节点。
            fallback_blocks: list[tuple[str, dict[str, float] | None]] = []
            used_fitz = False
            # pypdf 对部分带有效文本层的 PDF 只能提取到页眉/页脚水印（如「2023
            # 数据库系统工程师备考知识点集锦」pypdf≈91 字符/页只是「内部资料，禁止
            # 传播（希赛网）」页眉，正文几乎为零；PyMuPDF≈700-900 字符/页能完整提取
            # 正文）。这类页若按原字面量索引，索引里只有水印、正文检索不到；若被当
            # 作扫描件送 OCR，Windows 渲染器又把整页画黑、正文反而丢失。因此当 pypdf
            # 提取偏短（疑似只拿到水印/页眉）、用户未强制 OCR 时，回退 PyMuPDF 比对
            # 体量：只要它能提取出显著更丰富的真实文本（≥30 字且 > pypdf 的 3 倍），
            # 就采用 PyMuPDF 文本并按文本页索引。该规则是通用的「某解析器明显欠提取
            # → 取更全者」，仅按体量差异比对，不针对任何具体文件/关键词/case。
            if len(text) < PDF_TEXT_FALLBACK_PROBE_MIN and request.ocr_policy != "force":
                fallback_text = fitz_fallback.page_text(page_number)
                if len(fallback_text) >= PDF_TEXT_FALLBACK_MIN and len(fallback_text) > len(text) * 3:
                    text = fallback_text
                    fallback_blocks = fitz_fallback.page_blocks(page_number)
                    used_fitz = True
                    fitz_used_pages += 1
                else:
                    fitz_fallback_consulted = True
            is_boilerplate = page_number in boilerplate_pages
            # 由 PyMuPDF 兜底恢复了真实正文的页已具备可索引文本，绝不再送 OCR（否则
            # Windows 渲染器会把这些页画黑、正文反而丢失）。只有真正缺正文的页才进 OCR。
            if request.ocr_policy == "force" or (
                request.ocr_policy == "auto" and not used_fitz and (len(text) < 30 or is_boilerplate)
            ):
                # 单文档 OCR 预算：纯扫描书（600+ 页）全量 OCR 需几十分钟，
                # 超出预算的页面标记 OCR_REQUIRED 待后续补跑，首次索引不被拖垮。
                if request.ocr_policy == "force" or len(ocr_pages) < OCR_PAGE_BUDGET:
                    ocr_pages.append(page_number)
                else:
                    warnings.append(ParseWarning(
                        "OCR_REQUIRED",
                        "该页超出本次OCR预算，尚未OCR",
                        locator("pdf", page_no=page_number),
                    ))
            if (len(text) < 30 or is_boilerplate) and request.ocr_policy == "disabled":
                warnings.append(ParseWarning("OCR_REQUIRED", "该页缺少可索引正文，需OCR", locator("pdf", page_no=page_number)))
            if fallback_blocks:
                # PyMuPDF 兜底按文本块切节点：每块携带归一化 bbox，前端据此在 pdfjs
                # 渲染页面上高亮引用位置（与 OCR 线级节点同语义）。块级粒度既保持正文
                # 段落连贯，又能精确定位到引用所在区域。不构造整页节点，避免正文重复
                # 入库、稀释检索。
                block_base = len(nodes)
                for block_text, block_bbox in fallback_blocks:
                    nodes.append(DocumentNode(
                        uuid7(), None, block_base + len(nodes) + 1,
                        "page_block", block_text, None,
                        locator("pdf", page_no=page_number, bbox=block_bbox),
                    ))
                nodes = _renumber_nodes(nodes)
            else:
                # pypdf 正常提取的整页节点：补上该页文本区域的粗粒度 bbox，使几乎
                # 所有文本 PDF 都能在页面上圈出引用位置（块级/线级 bbox 由 OCR、兜底
                # 路径产出）。不拆分整页节点，避免密集排版页（表格/清单）碎成海量小块
                # 稀释索引。
                page_bbox = fitz_fallback.page_text_bbox(page_number)
                nodes.append(DocumentNode(
                    uuid7(), None, page_number, "page", text or None, None,
                    locator("pdf", page_no=page_number, bbox=page_bbox),
                ))
            # 仅文本页提取内嵌图片：纯扫描页的整页图会由 OCR 渲染路径产出
            # pdf_scanned_page 资产，这里再解一整份写盘是双份冗余；且 pypdf
            # 解码大扫描书每页整图实测 ~0.3s/页，600+ 页会拖垮 360s 的解析
            # 超时预算（实测 616 页扫描书图片提取写盘 ~200s）。
            if len(text) >= 30:
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
                except Exception as error:
                    # 图片提取尽力而为（如 JBIG2 无解码器抛 RuntimeError），失败只记警告。
                    warnings.append(ParseWarning("PDF_IMAGE_EXTRACT_FAILED", str(error), locator("pdf", page_no=page_number)))
        # 兜底痕迹：若有页经 PyMuPDF 恢复了 pypdf 缺失的文本层，记一条可诊断警告，
        # 说明该文件依赖兜底才能获得完整正文（也解释为何这些页没走 OCR 渲染）。
        if fitz_used_pages:
            warnings.append(ParseWarning(
                "PDF_TEXT_FALLBACK",
                f"{fitz_used_pages} 页经 pypdf 提取文字过少，已用 PyMuPDF 提取文本层兜底（不再误判为扫描件 OCR）",
                locator("pdf"),
            ))
        elif fitz_fallback_consulted and fitz_fallback.error:
            # 有低位欠提取的页需要 PyMuPDF 兜底但兜底本身不可用（导入/打开失败），
            # 明确记录原因，避免这些页被静默当作无文本或送 OCR 却不知缘由。
            warnings.append(ParseWarning(
                "PDF_TEXT_FALLBACK_UNAVAILABLE",
                fitz_fallback.error,
                locator("pdf"),
            ))
        ocr_page_count = 0
        parser_name = "pypdf+fitz" if fitz_used_pages else "pypdf"
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
                        parser_name = f"{'pypdf+fitz+' if fitz_used_pages else 'pypdf+'}{ocr_engine}"
                    ocr_text_by_page: dict[int, str] = {}
                    ocr_route_by_page: dict[int, tuple[float | None, str | None]] = {}
                    for page_number in ocr_pages:
                        page_result = {
                            "lines": [
                                line
                                for line in ocr_result.get("lines", [])
                                if isinstance(line, dict) and line.get("page_no") == page_number
                            ]
                        }
                        page_text = "\n".join(
                            node.text or ""
                            for node in recognized
                            if node.locator.get("page_no") == page_number
                        ).strip()
                        if page_text:
                            ocr_text_by_page[page_number] = page_text
                            confidence_values = [
                                float(line["confidence"])
                                for line in page_result["lines"]
                                if isinstance(line.get("confidence"), (int, float))
                                and not isinstance(line.get("confidence"), bool)
                            ]
                            confidence = (
                                sum(confidence_values) / len(confidence_values)
                                if confidence_values
                                else None
                            )
                            threshold = request.ocr_runtime.confidence_threshold if request.ocr_runtime else 0.45
                            route_reason = _ocr_fallback_reason(page_result, None, threshold)
                            if route_reason is None:
                                route_reason = complex_visual_reason("pdf_scanned_page", page_result, page_text)
                            ocr_route_by_page[page_number] = (confidence, route_reason)
                        if page_number not in recognized_pages:
                            warnings.append(ParseWarning("OCR_NO_TEXT", "该页OCR后没有识别到文字", locator("pdf", page_no=page_number)))
                            warnings.append(ParseWarning("OCR_REQUIRED", "该页尚未获得可索引文字", locator("pdf", page_no=page_number)))
                    for rendered in ocr_result.get("rendered_pages", []):
                        try:
                            page_number = int(rendered["page_no"])
                            rendered_path = Path(str(rendered["path"]))
                            # 整页聚合的 image_ocr 搜索节点（「图片文字：…」）复用该 locator，
                            # 附上本页 OCR 行 bbox 的并集，前端才能在 pdfjs 页面上圈出引用区域。
                            page_bbox = _ocr_page_area_bbox(ocr_result.get("lines", []), page_number)
                            asset = _cache_image_asset(
                                request,
                                rendered_path.read_bytes(),
                                ".png",
                                "pdf_scanned_page",
                                locator("pdf", page_no=page_number, bbox=page_bbox),
                                ocr_text_by_page.get(page_number),
                                ocr_route_by_page.get(page_number, (None, None))[0],
                                ocr_engine,
                                ocr_route_by_page.get(page_number, (None, "ocr_no_text"))[1],
                                "pending_understanding"
                                if ocr_route_by_page.get(page_number, (None, "ocr_no_text"))[1]
                                else "ready",
                            )
                            if asset is not None:
                                image_assets.append(asset)
                        except (KeyError, OSError, TypeError, ValueError) as error:
                            warnings.append(ParseWarning("PDF_IMAGE_EXTRACT_FAILED", str(error)))
            finally:
                if render_directory is not None:
                    shutil.rmtree(render_directory, ignore_errors=True)
        status: Literal["parsed", "partial"] = "partial" if warnings else "parsed"
        fitz_fallback.close()
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
            fallback_reason: str | None = None
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
                if standalone_asset is not None:
                    image_assets = [replace(
                        standalone_asset,
                        ocr_engine=ocr_engine,
                        vision_route_reason=ocr_error.code.lower(),
                        status="pending_understanding",
                    )]
                return _result(request, "partial", "image-metadata", [], ocr_warnings + [ParseWarning(ocr_error.code, ocr_error.message), ParseWarning("OCR_REQUIRED", "图片尚未完成OCR")], 1, started_at, image_assets=image_assets, ocr_attempts=ocr_attempts)
            nodes = _ocr_nodes(ocr_result or {}, "image")
            ocr_text = "\n".join(node.text or "" for node in nodes).strip() or None
            if standalone_asset is not None:
                threshold = request.ocr_runtime.confidence_threshold if request.ocr_runtime else 0.45
                route_reason = fallback_reason or _ocr_fallback_reason(ocr_result, None, threshold)
                if route_reason is None and ocr_text is not None:
                    route_reason = complex_visual_reason("standalone_image", ocr_result or {}, ocr_text)
                confidence_values = [
                    float(line["confidence"])
                    for line in (ocr_result or {}).get("lines", [])
                    if isinstance(line, dict)
                    and isinstance(line.get("confidence"), (int, float))
                    and not isinstance(line.get("confidence"), bool)
                ]
                image_assets = [replace(
                    standalone_asset,
                    ocr_text=ocr_text,
                    ocr_confidence=(sum(confidence_values) / len(confidence_values)) if confidence_values else None,
                    ocr_engine=ocr_engine,
                    vision_route_reason=route_reason,
                    status="pending_understanding" if route_reason else "ready",
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
