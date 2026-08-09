from __future__ import annotations

import argparse
import os
import shutil
import subprocess
from pathlib import Path

from pypdf import PdfReader, PdfWriter
from reportlab.lib.pagesizes import A4
from reportlab.lib.utils import ImageReader
from reportlab.pdfbase import pdfmetrics
from reportlab.pdfbase.cidfonts import UnicodeCIDFont
from reportlab.pdfgen import canvas


pdfmetrics.registerFont(UnicodeCIDFont("STSong-Light"))


def draw_text_pdf(path: Path) -> None:
    page = canvas.Canvas(str(path), pagesize=A4)
    width, height = A4
    page.setFont("STSong-Light", 22)
    page.drawString(64, height - 80, "归航计划首次验收会议纪要")
    page.setFont("STSong-Light", 11)
    lines = [
        "会议日期：2025年11月18日 14:30",
        "会议地点：青屿会议室",
        "主持人：林晓岚",
        "记录人：陈默",
        "",
        "决议一：文件名搜索必须在首次扫描阶段优先可用。",
        "决议二：混合检索采用RRF融合，评测基线k=60。",
        "决议三：源文件只读，任何导出都必须由用户明确触发。",
        "决议四：阶段预算上限为286,500元。",
        "",
        "下一次复核：2025年12月2日 10:00。",
    ]
    y = height - 128
    for line in lines:
        page.drawString(68, y, line)
        y -= 24
    page.setFont("STSong-Light", 9)
    page.drawRightString(width - 64, 40, "拾忆阶段0测试资料 · 第1页")
    page.showPage()
    page.save()


def draw_scan_source(path: Path) -> None:
    page = canvas.Canvas(str(path), pagesize=A4)
    _, height = A4
    page.setFont("STSong-Light", 18)
    page.drawString(70, height - 90, "设备采购收据")
    page.setFont("STSong-Light", 12)
    for index, line in enumerate(
        [
            "单据编号：SY-2025-8842",
            "采购方：归航计划项目组",
            "供应商：青屿计算设备服务中心",
            "项目：离线性能测试工作站",
            "金额：86,000.00元",
            "日期：2025年9月26日",
        ]
    ):
        page.drawString(74, height - 140 - index * 32, line)
    page.showPage()
    page.save()


def rasterize(source_pdf: Path, output_prefix: Path) -> Path:
    discovered = shutil.which("pdftoppm") or shutil.which("pdftoppm.cmd")
    candidates = [Path(os.environ["REMIN_PDFTOPPM"])] if os.environ.get("REMIN_PDFTOPPM") else []
    if discovered:
        discovered_path = Path(discovered)
        candidates.extend(
            [
                discovered_path,
                discovered_path.parents[2] / "native" / "poppler" / "Library" / "bin" / "pdftoppm.exe",
            ]
        )
    executable = next((candidate for candidate in candidates if candidate.is_file() and candidate.suffix.lower() == ".exe"), None)
    if executable is None:
        raise RuntimeError("pdftoppm is required to build the scanned PDF fixture")
    subprocess.run(
        [str(executable), "-f", "1", "-singlefile", "-png", "-r", "150", str(source_pdf), str(output_prefix)],
        check=True,
    )
    return output_prefix.with_suffix(".png")


def embed_scanned_pdf(image_path: Path, output_pdf: Path) -> None:
    page = canvas.Canvas(str(output_pdf), pagesize=A4)
    width, height = A4
    image = ImageReader(str(image_path))
    page.drawImage(image, 0, 0, width=width, height=height, preserveAspectRatio=True, anchor="c")
    page.showPage()
    page.save()


def encrypt_pdf(source_pdf: Path, output_pdf: Path) -> None:
    reader = PdfReader(source_pdf)
    writer = PdfWriter()
    writer.clone_document_from_reader(reader)
    writer.encrypt("remin-test-password")
    with output_pdf.open("wb") as stream:
        writer.write(stream)


def build(output_dir: Path) -> None:
    output_dir.mkdir(parents=True, exist_ok=True)
    text_pdf = output_dir / "02-归航计划会议纪要.pdf"
    draw_text_pdf(text_pdf)
    scan_source = output_dir / ".scan-source.pdf"
    draw_scan_source(scan_source)
    scan_image = rasterize(scan_source, output_dir / "04-扫描采购收据")
    embed_scanned_pdf(scan_image, output_dir / "03-扫描采购收据.pdf")
    encrypt_pdf(text_pdf, output_dir / "11-加密会议纪要.pdf")
    (output_dir / "12-损坏文档.pdf").write_bytes(b"%PDF-1.7\ncorrupted-reminder-without-xref")
    scan_source.unlink(missing_ok=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()
    build(args.output_dir)


if __name__ == "__main__":
    main()
