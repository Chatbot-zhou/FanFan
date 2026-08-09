from __future__ import annotations

import hashlib
import json
import sys
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
BASELINES = ROOT / "tests" / "baselines"


def fail(message: str) -> None:
    print(f"test-corpus: {message}", file=sys.stderr)
    raise SystemExit(1)


def load_jsonl(path: Path) -> list[dict[str, object]]:
    records: list[dict[str, object]] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except json.JSONDecodeError as error:
            fail(f"{path.relative_to(ROOT)}:{line_number}: {error}")
        if not isinstance(value, dict) or not value.get("case_id"):
            fail(f"{path.relative_to(ROOT)}:{line_number}: case_id is required")
        records.append(value)
    if not records:
        fail(f"{path.relative_to(ROOT)} has no cases")
    return records


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    digest.update(path.read_bytes())
    return digest.hexdigest()


def validate_open_xml(path: Path, required_member: str, required_text: str) -> None:
    if not zipfile.is_zipfile(path):
        fail(f"{path.name} is not a valid Open XML package")
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        if required_member not in names:
            fail(f"{path.name} is missing {required_member}")
        xml_text = "\n".join(
            archive.read(name).decode("utf-8", errors="ignore")
            for name in names
            if name.endswith(".xml")
        )
    if required_text not in xml_text:
        fail(f"{path.name} does not contain deterministic fact {required_text}")


def main() -> None:
    manifest_path = FIXTURES / "manifest.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    fixtures = manifest.get("fixtures")
    if not isinstance(fixtures, list) or not fixtures:
        fail("manifest fixtures must be a non-empty list")

    names: set[str] = set()
    for entry in fixtures:
        relative = entry.get("path")
        if not isinstance(relative, str):
            fail("fixture path must be a string")
        if relative in names:
            fail(f"duplicate fixture path: {relative}")
        names.add(relative)
        target = FIXTURES / relative
        if not target.is_file() or target.stat().st_size == 0:
            fail(f"missing or empty fixture: {relative}")

    duplicate_a = FIXTURES / "corpus" / "13-完全重复-A.txt"
    duplicate_b = FIXTURES / "corpus" / "14-完全重复-B.txt"
    if sha256(duplicate_a) != sha256(duplicate_b):
        fail("exact duplicate fixtures do not have identical SHA-256")

    validate_open_xml(FIXTURES / "corpus" / "01-归航计划项目总结.docx", "word/document.xml", "GH-2025-017")
    validate_open_xml(FIXTURES / "corpus" / "06-检索评估与项目台账.xlsx", "xl/workbook.xml", "GH-2025-017")
    validate_open_xml(FIXTURES / "corpus" / "07-归航计划阶段汇报.pptx", "ppt/presentation.xml", "GH-2025-017")

    encrypted_bytes = (FIXTURES / "corpus" / "11-加密会议纪要.pdf").read_bytes()
    if b"/Encrypt" not in encrypted_bytes:
        fail("encrypted PDF fixture is not encrypted")

    corrupt_bytes = (FIXTURES / "corpus" / "12-损坏文档.pdf").read_bytes()
    if corrupt_bytes.rstrip().endswith(b"%%EOF") or b"startxref" in corrupt_bytes:
        fail("corrupt PDF fixture was unexpectedly readable")

    image_bytes = (FIXTURES / "corpus" / "04-扫描采购收据.png").read_bytes()
    if not image_bytes.startswith(b"\x89PNG\r\n\x1a\n"):
        fail("scanned image fixture is not a PNG")

    case_ids: set[str] = set()
    total_cases = 0
    for baseline_name in ("search.jsonl", "qa.jsonl", "extraction.jsonl", "relations.jsonl"):
        for record in load_jsonl(BASELINES / baseline_name):
            case_id = str(record["case_id"])
            if case_id in case_ids:
                fail(f"duplicate baseline case_id: {case_id}")
            case_ids.add(case_id)
            total_cases += 1

    print(f"test-corpus: fixtures={len(fixtures)}, baselines={total_cases}, exact_duplicate_sha256={sha256(duplicate_a)[:12]}")


if __name__ == "__main__":
    main()
