from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import sys
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
WORKER_ROOT = REPO_ROOT / "services" / "worker"
FIXTURE_ROOT = REPO_ROOT / "tests" / "fixtures"
sys.path.insert(0, str(WORKER_ROOT))

from remin_worker import ParseRequest, parse_document  # noqa: E402


IDS = (
    "018f0000-0000-7000-8000-000000000201",
    "018f0000-0000-7000-8000-000000000202",
    "018f0000-0000-7000-8000-000000000203",
)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def text_of(result: object) -> str:
    parts: list[str] = []
    for node in result.nodes:
        if node.text:
            parts.append(node.text)
        if node.table_data:
            parts.append(json.dumps(node.table_data, ensure_ascii=False))
    return "\n".join(parts)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--require-pdf", action="store_true", help="PDF依赖缺失时直接失败")
    args = parser.parse_args()

    manifest = json.loads((FIXTURE_ROOT / "manifest.json").read_text(encoding="utf-8"))
    has_pypdf = importlib.util.find_spec("pypdf") is not None
    if args.require_pdf and not has_pypdf:
        raise SystemExit("--require-pdf启用，但当前Python环境未安装pypdf")

    checked = 0
    skipped = 0
    results: dict[str, object] = {}
    for index, fixture in enumerate(manifest["fixtures"], 1):
        path = FIXTURE_ROOT / fixture["path"]
        source_format = path.suffix.lower().lstrip(".")
        if source_format == "pdf" and not has_pypdf:
            skipped += 1
            continue
        before = digest(path)
        result = parse_document(
            ParseRequest(
                job_id=IDS[0],
                file_id=IDS[1],
                revision_id=IDS[2],
                source_path=str(path),
                format=source_format,
            )
        )
        after = digest(path)
        if before != after:
            raise SystemExit(f"解析器修改了源文件: {fixture['path']}")

        expected = fixture["expected"]
        if expected in {"readable", "exact_duplicate", "version_candidate"} and result.status != "parsed":
            raise SystemExit(f"可读样本解析失败: {fixture['path']} status={result.status}")
        if expected == "ocr_required":
            if result.status == "parsed" and text_of(result).strip():
                pass  # Windows OCR 实际成功执行，文本已提取
            elif any(warning.code == "OCR_REQUIRED" for warning in result.warnings):
                pass  # OCR 不可用，已正确分流到待处理队列
            else:
                raise SystemExit(f"扫描样本既未被OCR解析也未进入OCR队列: {fixture['path']} status={result.status}")
        if expected == "password_required" and result.status != "encrypted":
            raise SystemExit(f"加密PDF未识别: {fixture['path']} status={result.status}")
        if expected == "parse_failed" and result.status != "failed":
            raise SystemExit(f"损坏PDF未失败: {fixture['path']} status={result.status}")
        results[path.name] = result
        checked += 1

    facts = manifest["deterministic_facts"]
    fact_files = {
        "01-归航计划项目总结.docx": (facts["project_id"], facts["owner"]),
        "05-项目说明.md": (facts["project_id"], str(facts["rrf_k"])),
        "06-检索评估与项目台账.xlsx": (facts["project_id"], str(facts["budget_cny"])),
        "07-归航计划阶段汇报.pptx": (facts["project_id"], facts["owner"]),
    }
    for name, expected_values in fact_files.items():
        content = text_of(results[name])
        for expected_value in expected_values:
            if expected_value not in content:
                raise SystemExit(f"确定性事实缺失: {name} value={expected_value}")

    print(f"解析语料检查通过: checked={checked}, skipped_pdf={skipped}, pypdf={has_pypdf}")


if __name__ == "__main__":
    main()
