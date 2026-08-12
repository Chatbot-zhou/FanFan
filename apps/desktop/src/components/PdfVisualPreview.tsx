import type { PDFDocumentProxy } from "pdfjs-dist";
import { useEffect, useMemo, useRef, useState } from "react";
import type { FilePreview } from "../bridge";
import { errorMessage } from "../utils/app-error";

interface PdfVisualPreviewProps {
  preview: FilePreview;
}

const sourceUrl = (fileId: string) => `http://remin-pdf.localhost/${encodeURIComponent(fileId)}`;

export function PdfVisualPreview({ preview }: PdfVisualPreviewProps) {
  const targetNode = useMemo(
    () => preview.nodes.find((node) => node.node_id === preview.anchor_node_id)
      ?? preview.nodes.find((node) => node.locator.page_no !== null),
    [preview.anchor_node_id, preview.nodes],
  );
  const targetPage = Math.max(1, targetNode?.locator.page_no ?? 1);
  const [pdf, setPdf] = useState<PDFDocumentProxy | null>(null);
  const [pageNo, setPageNo] = useState(targetPage);
  const [error, setError] = useState<string | null>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const viewportRef = useRef<HTMLDivElement>(null);

  useEffect(() => setPageNo(targetPage), [preview.file.file_id, targetPage]);

  useEffect(() => {
    if (!window.__TAURI_INTERNALS__) return undefined;
    let disposed = false;
    let loaded: PDFDocumentProxy | null = null;
    void (async () => {
      try {
        const pdfjs = await import("pdfjs-dist");
        pdfjs.GlobalWorkerOptions.workerSrc = new URL("pdfjs-dist/build/pdf.worker.min.mjs", import.meta.url).toString();
        const task = pdfjs.getDocument({ url: sourceUrl(preview.file.file_id), withCredentials: false });
        loaded = await task.promise;
        if (!disposed) {
          setPdf(loaded);
          setError(null);
          setPageNo((current) => Math.min(Math.max(1, current), loaded?.numPages ?? 1));
        }
      } catch (loadError) {
        if (!disposed) setError(errorMessage(loadError));
      }
    })();
    return () => {
      disposed = true;
      setPdf(null);
      if (loaded) void loaded.cleanup();
    };
  }, [preview.file.file_id]);

  useEffect(() => {
    if (!pdf || !canvasRef.current) return undefined;
    let disposed = false;
    let renderTask: { cancel: () => void; promise: Promise<void> } | null = null;
    void (async () => {
      try {
        const page = await pdf.getPage(pageNo);
        if (disposed || !canvasRef.current) return;
        const base = page.getViewport({ scale: 1 });
        const availableWidth = Math.max(320, (viewportRef.current?.clientWidth ?? 760) - 24);
        const scale = Math.min(2.2, Math.max(0.6, availableWidth / base.width));
        const viewport = page.getViewport({ scale });
        const ratio = window.devicePixelRatio || 1;
        const canvas = canvasRef.current;
        canvas.width = Math.floor(viewport.width * ratio);
        canvas.height = Math.floor(viewport.height * ratio);
        canvas.style.width = `${Math.floor(viewport.width)}px`;
        canvas.style.height = `${Math.floor(viewport.height)}px`;
        const context = canvas.getContext("2d");
        if (!context) throw new Error("无法创建PDF画布");
        renderTask = page.render({ canvas, canvasContext: context, viewport, transform: ratio === 1 ? undefined : [ratio, 0, 0, ratio, 0, 0] });
        await renderTask.promise;
      } catch (renderError) {
        if (!disposed && !(renderError instanceof Error && renderError.name === "RenderingCancelledException")) {
      setError(errorMessage(renderError));
        }
      }
    })();
    return () => {
      disposed = true;
      renderTask?.cancel();
    };
  }, [pageNo, pdf]);

  const highlight = preview.nodes.find((node) => node.locator.page_no === pageNo && node.locator.bbox)?.locator.bbox ?? null;

  if (!window.__TAURI_INTERNALS__) return <p className="pdf-preview__notice">PDF视觉预览仅在拾忆桌面程序中加载。</p>;
  return <section className="pdf-preview" aria-label={`${preview.file.display_name} PDF视觉预览`}>
    <header><strong>PDF视觉页</strong><span>{pdf ? `${pageNo} / ${pdf.numPages}` : "正在加载"}</span><div><button type="button" disabled={!pdf || pageNo <= 1} onClick={() => setPageNo((value) => value - 1)}>上一页</button><button type="button" disabled={!pdf || pageNo >= pdf.numPages} onClick={() => setPageNo((value) => value + 1)}>下一页</button></div></header>
    {error && <p role="alert" className="inline-error">PDF视觉预览失败：{error}</p>}
    <div ref={viewportRef} className="pdf-preview__viewport"><div className="pdf-preview__page"><canvas ref={canvasRef} />{highlight && <i className="pdf-preview__highlight" style={{ left: `${highlight.x0 * 100}%`, top: `${highlight.y0 * 100}%`, width: `${(highlight.x1 - highlight.x0) * 100}%`, height: `${(highlight.y1 - highlight.y0) * 100}%` }} />}</div></div>
  </section>;
}
