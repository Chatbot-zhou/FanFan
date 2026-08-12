from __future__ import annotations

import json
import re
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
CATALOG_PATH = REPO_ROOT / "contracts" / "error-codes.json"
SOURCE_ROOTS = (
    REPO_ROOT / "crates" / "core" / "src",
    REPO_ROOT / "apps" / "desktop" / "src",
    REPO_ROOT / "apps" / "desktop" / "src-tauri" / "src",
    REPO_ROOT / "services" / "worker" / "remin_worker",
)
SOURCE_SUFFIXES = {".rs", ".py", ".ts", ".tsx"}
CODE_PATTERN = re.compile(
    r"[\"']((?:ASK|CANDIDATE|COLLECTION|COMPATIBILITY|DATABASE|DEGRADATION|DOCUMENT|EMBEDDING|EXCLUSION|EXPORT|EXTRACTION|FILE|FORMAT|GENERATION|IMAGE|INBOX|INCREMENTAL|INDEX|JOB|KNOWN_FOLDER|LOCAL_CONFIG|LOG|MEMBERSHIP|MODEL|NOT_A_FILE|OCR|OPERATION|PARSER|PATH|PDF|PREVIEW|RAG|RELATION|REQUEST|REVISION|ROOT|SCAN|SCHEMA|SEARCH|TASK|VISION|WATCHER|WELCOME|WORKER)_[A-Z0-9_]+|NOT_A_FILE)[\"']"
)
VALID_CODE = re.compile(r"^[A-Z][A-Z0-9_]+$")
RUNTIME_EVENT_PATTERN = re.compile(r"(?:\.emit|listen(?:<[^>]+>)?)\s*\(\s*[\"']([^\"']+)[\"']")
VALID_RUNTIME_EVENT = re.compile(r"^[A-Za-z0-9_:/-]+$")


def main() -> None:
    catalog = json.loads(CATALOG_PATH.read_text(encoding="utf-8"))
    entries = catalog.get("codes")
    if not isinstance(entries, list) or not entries:
        raise SystemExit("error-codes.json必须包含非空codes数组")

    catalog_codes: list[str] = []
    for entry in entries:
        if not isinstance(entry, dict):
            raise SystemExit("错误码条目必须是对象")
        code = entry.get("code")
        if not isinstance(code, str) or not VALID_CODE.fullmatch(code):
            raise SystemExit(f"错误码格式无效: {code!r}")
        if not isinstance(entry.get("description"), str) or not entry["description"].strip():
            raise SystemExit(f"错误码缺少说明: {code}")
        if not isinstance(entry.get("retryable_default"), bool):
            raise SystemExit(f"错误码缺少retryable_default: {code}")
        catalog_codes.append(code)

    duplicates = sorted({code for code in catalog_codes if catalog_codes.count(code) > 1})
    if duplicates:
        raise SystemExit(f"错误码重复: {', '.join(duplicates)}")
    if catalog_codes != sorted(catalog_codes):
        raise SystemExit("错误码目录必须按code排序")

    emitted: set[str] = set()
    for source_root in SOURCE_ROOTS:
        for path in source_root.rglob("*"):
            if path.is_file() and path.suffix in SOURCE_SUFFIXES and "__pycache__" not in path.parts:
                emitted.update(CODE_PATTERN.findall(path.read_text(encoding="utf-8")))

    missing = sorted(emitted - set(catalog_codes))
    if missing:
        raise SystemExit(f"源码使用了未登记错误码: {', '.join(missing)}")

    invalid_events: list[str] = []
    for source_root in SOURCE_ROOTS:
        for path in source_root.rglob("*"):
            if path.is_file() and path.suffix in SOURCE_SUFFIXES and "__pycache__" not in path.parts:
                source = path.read_text(encoding="utf-8")
                invalid_events.extend(
                    event for event in RUNTIME_EVENT_PATTERN.findall(source)
                    if not VALID_RUNTIME_EVENT.fullmatch(event)
                )
    if invalid_events:
        raise SystemExit(f"桌面运行事件名包含非法字符: {', '.join(sorted(set(invalid_events)))}")

    print(f"公共错误码检查通过: catalog={len(catalog_codes)}, emitted={len(emitted)}")


if __name__ == "__main__":
    main()
