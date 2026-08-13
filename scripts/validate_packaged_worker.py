from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_WORKER = REPO_ROOT / ".artifacts" / "worker" / "fanfan-worker" / "fanfan-worker.exe"
FIXTURE = REPO_ROOT / "tests" / "fixtures" / "corpus" / "05-项目说明.md"
PDF_FIXTURE = REPO_ROOT / "tests" / "fixtures" / "corpus" / "02-归航计划会议纪要.pdf"
IDS = (
    "018f0000-0000-7000-8000-000000000301",
    "018f0000-0000-7000-8000-000000000302",
    "018f0000-0000-7000-8000-000000000303",
    "018f0000-0000-7000-8000-000000000304",
    "018f0000-0000-7000-8000-000000000305",
    "018f0000-0000-7000-8000-000000000306",
    "018f0000-0000-7000-8000-000000000307",
    "018f0000-0000-7000-8000-000000000308",
    "018f0000-0000-7000-8000-000000000309",
)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def request(process: subprocess.Popen[str], payload: dict[str, object]) -> dict[str, object]:
    assert process.stdin is not None
    assert process.stdout is not None
    process.stdin.write(json.dumps(payload, ensure_ascii=False) + "\n")
    process.stdin.flush()
    line = process.stdout.readline()
    if not line:
        raise RuntimeError(f"Worker提前退出，exit_code={process.poll()}")
    response = json.loads(line)
    if not isinstance(response, dict):
        raise RuntimeError("Worker响应不是JSON对象")
    return response


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("worker", nargs="?", type=Path, default=DEFAULT_WORKER)
    args = parser.parse_args()
    worker = args.worker.resolve()
    if not worker.is_file():
        raise SystemExit(f"未找到独立Worker: {worker}")

    before = {FIXTURE: digest(FIXTURE), PDF_FIXTURE: digest(PDF_FIXTURE)}
    creation_flags = getattr(subprocess, "CREATE_NO_WINDOW", 0)
    process = subprocess.Popen(
        [str(worker)],
        cwd=worker.parent,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        creationflags=creation_flags,
    )
    try:
        health = request(
            process,
            {"request_id": IDS[0], "operation": "health.check", "payload": {}},
        )
        if not health.get("ok") or health.get("result", {}).get("status") != "ready":
            raise RuntimeError(f"Worker健康检查失败: {health}")

        backends = request(
            process,
            {"request_id": IDS[8], "operation": "runtime.backend_probe", "payload": {}},
        )
        loaded = (backends.get("result") or {}).get("loaded") or {}
        if not backends.get("ok") or set(loaded) != {"onnxruntime", "rapidocr", "sherpa_onnx"}:
            raise RuntimeError(f"Worker本地AI运行库检查失败: {backends}")

        parsed = request(
            process,
            {
                "request_id": IDS[1],
                "operation": "document.parse",
                "payload": {
                    "job_id": IDS[1],
                    "file_id": IDS[2],
                    "revision_id": IDS[3],
                    "source_path": str(FIXTURE),
                    "format": "md",
                    "ocr_policy": "auto",
                    "language_hints": ["zh"],
                    "max_pages": None,
                    "parser_version": "0.1.0",
                },
            },
        )
        result = parsed.get("result") or {}
        text = "\n".join(node.get("text") or "" for node in result.get("nodes", []))
        if not parsed.get("ok") or result.get("status") != "parsed" or "GH-2025-017" not in text:
            raise RuntimeError(f"Worker中文文档解析检查失败: {parsed}")

        parsed_pdf = request(
            process,
            {
                "request_id": IDS[4],
                "operation": "document.parse",
                "payload": {
                    "job_id": IDS[5],
                    "file_id": IDS[6],
                    "revision_id": IDS[7],
                    "source_path": str(PDF_FIXTURE),
                    "format": "pdf",
                    "ocr_policy": "auto",
                    "language_hints": ["zh"],
                    "max_pages": None,
                    "parser_version": "0.1.0",
                },
            },
        )
        pdf_result = parsed_pdf.get("result") or {}
        pdf_text = "\n".join(node.get("text") or "" for node in pdf_result.get("nodes", []))
        if (
            not parsed_pdf.get("ok")
            or pdf_result.get("status") != "parsed"
            or "归航计划首次验收会议纪要" not in pdf_text
        ):
            raise RuntimeError(f"Worker中文PDF解析检查失败: {parsed_pdf}")
        if process.poll() is not None:
            raise RuntimeError("Worker未保持为可复用的持久进程")
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)

    after = {FIXTURE: digest(FIXTURE), PDF_FIXTURE: digest(PDF_FIXTURE)}
    if before != after:
        raise RuntimeError("独立Worker修改了源文件")
    print(
        "独立Worker检查通过: "
        "health=ready, ai_backends=loaded, markdown=parsed, pdf=parsed, "
        f"source_readonly=true, path={worker}"
    )


if __name__ == "__main__":
    main()
