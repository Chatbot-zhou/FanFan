from __future__ import annotations

import csv
import hashlib
import json
import os
import tempfile
import zipfile
from pathlib import Path
from typing import Any
from xml.sax.saxutils import escape

from .protocol import WorkerError


FORMATS = {"csv", "json", "xlsx", "docx"}


def export_table(payload: dict[str, Any]) -> tuple[dict[str, Any] | None, WorkerError | None]:
    target_raw = payload.get("target_path")
    export_format = payload.get("format")
    headers = payload.get("headers")
    rows = payload.get("rows")
    if not isinstance(target_raw, str) or not target_raw:
        return None, WorkerError("EXPORT_TARGET_REQUIRED", "缺少导出路径", False)
    if export_format not in FORMATS:
        return None, WorkerError("EXPORT_FORMAT_UNSUPPORTED", "导出格式不受支持", False)
    if not isinstance(headers, list) or not headers or len(headers) > 200 or not all(isinstance(item, str) for item in headers):
        return None, WorkerError("EXPORT_HEADERS_INVALID", "导出表头需要包含1到200个文本字段", False)
    if not isinstance(rows, list) or len(rows) > 50_000:
        return None, WorkerError("EXPORT_ROWS_INVALID", "单次导出最多50000行", False)
    if any(not isinstance(row, list) or len(row) != len(headers) for row in rows):
        return None, WorkerError("EXPORT_ROWS_INVALID", "每行字段数必须与表头一致", False)

    target = Path(target_raw)
    if not target.is_absolute():
        return None, WorkerError("EXPORT_TARGET_INVALID", "导出路径必须是绝对路径", False)
    if target.suffix.lower() != f".{export_format}":
        return None, WorkerError("EXPORT_EXTENSION_MISMATCH", "导出扩展名与所选格式不一致", False)
    if not target.parent.is_dir():
        return None, WorkerError("EXPORT_PARENT_UNAVAILABLE", "导出目录不存在", False)
    if target.exists():
        return None, WorkerError("TARGET_EXISTS", "目标文件已经存在，拾忆不会覆盖它", False)

    normalized_rows = [[_normalize_cell(cell) for cell in row] for row in rows]
    temporary: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(prefix=".remin-export-", suffix=f".{export_format}.tmp", dir=target.parent, delete=False) as handle:
            temporary = Path(handle.name)
        if export_format == "json":
            _write_json(temporary, headers, normalized_rows)
        elif export_format == "csv":
            _write_csv(temporary, headers, normalized_rows)
        elif export_format == "xlsx":
            _write_xlsx(temporary, headers, normalized_rows)
        else:
            _write_docx(temporary, headers, normalized_rows)
        _validate_output(temporary, export_format)
        try:
            os.link(temporary, target)
        except FileExistsError:
            return None, WorkerError("TARGET_EXISTS", "目标文件已经存在，拾忆不会覆盖它", False)
        except OSError as error:
            return None, WorkerError("EXPORT_COMMIT_FAILED", str(error), True)
        digest = hashlib.sha256(target.read_bytes()).hexdigest()
        return {"target_path": str(target), "format": export_format, "row_count": len(rows), "size_bytes": target.stat().st_size, "sha256": digest}, None
    except PermissionError:
        return None, WorkerError("EXPORT_PERMISSION_DENIED", "没有向所选目录写入文件的权限", True)
    except OSError as error:
        return None, WorkerError("EXPORT_WRITE_FAILED", str(error), True)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def _normalize_cell(value: Any) -> str:
    if value is None:
        return ""
    if isinstance(value, (str, int, float, bool)):
        return str(value)
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def _write_json(path: Path, headers: list[str], rows: list[list[str]]) -> None:
    records = [dict(zip(headers, row, strict=True)) for row in rows]
    path.write_text(json.dumps(records, ensure_ascii=False, indent=2), encoding="utf-8")


def _write_csv(path: Path, headers: list[str], rows: list[list[str]]) -> None:
    with path.open("w", encoding="utf-8-sig", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(headers)
        writer.writerows(rows)


def _write_xlsx(path: Path, headers: list[str], rows: list[list[str]]) -> None:
    all_rows = [headers, *rows]
    row_xml = []
    for row_index, row in enumerate(all_rows, start=1):
        cells = []
        for column_index, value in enumerate(row, start=1):
            reference = f"{_column_name(column_index)}{row_index}"
            cells.append(f'<c r="{reference}" t="inlineStr"><is><t xml:space="preserve">{escape(value)}</t></is></c>')
        row_xml.append(f'<row r="{row_index}">{"".join(cells)}</row>')
    worksheet = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>' + "".join(row_xml) + "</sheetData></worksheet>"
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>')
        archive.writestr("_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>')
        archive.writestr("xl/workbook.xml", '<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="拾忆导出" sheetId="1" r:id="rId1"/></sheets></workbook>')
        archive.writestr("xl/_rels/workbook.xml.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>')
        archive.writestr("xl/worksheets/sheet1.xml", worksheet)


def _write_docx(path: Path, headers: list[str], rows: list[list[str]]) -> None:
    table_rows = []
    for row in [headers, *rows]:
        cells = "".join(f'<w:tc><w:p><w:r><w:t xml:space="preserve">{escape(value)}</w:t></w:r></w:p></w:tc>' for value in row)
        table_rows.append(f"<w:tr>{cells}</w:tr>")
    document = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>拾忆导出结果</w:t></w:r></w:p><w:tbl>' + "".join(table_rows) + "</w:tbl><w:sectPr/></w:body></w:document>"
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
        archive.writestr("_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
        archive.writestr("word/document.xml", document)


def _column_name(index: int) -> str:
    value = ""
    while index:
        index, remainder = divmod(index - 1, 26)
        value = chr(65 + remainder) + value
    return value


def _validate_output(path: Path, export_format: str) -> None:
    if not path.is_file() or path.stat().st_size == 0:
        raise OSError("导出文件为空")
    if export_format in {"xlsx", "docx"}:
        with zipfile.ZipFile(path, "r") as archive:
            required = "xl/workbook.xml" if export_format == "xlsx" else "word/document.xml"
            if required not in archive.namelist():
                raise OSError("Office导出包缺少必要组件")
