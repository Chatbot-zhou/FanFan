import type { OcrAttempt } from "../bridge";

const ENGINE_LABELS: Record<string, string> = {
  paddle_ocr: "PP-OCRv5",
  pp_ocr_v5: "PP-OCRv5",
  "rapidocr-onnxruntime": "PP-OCRv5",
  windows_ocr: "Windows OCR",
  "windows-ocr": "Windows OCR",
  vision_model: "多模态模型",
};

const statusLabel = (attempt: OcrAttempt) => {
  if (attempt.status === "completed") return "完成";
  if (attempt.status === "no_text") return "未识别到文字";
  return "失败";
};

export function OcrAttemptChain({ attempts }: { attempts: OcrAttempt[] }) {
  if (!attempts.length) return null;
  return <section className="ocr-attempt-chain" aria-label="OCR 处理链">
    <strong>OCR 处理链</strong>
    <div>
      {attempts.map((attempt, index) => <span key={`${attempt.engine}-${attempt.page_no ?? "all"}-${index}`} className={`ocr-attempt-chain__step ocr-attempt-chain__step--${attempt.status}`} title={attempt.error?.message ?? attempt.fallback_reason ?? undefined}>
        <b>{ENGINE_LABELS[attempt.engine] ?? attempt.engine}</b>
        <em>{statusLabel(attempt)}</em>
        {attempt.confidence !== null && <small>{Math.round(attempt.confidence * 100)}%</small>}
        {attempt.error && <small>{attempt.error.code}</small>}
      </span>)}
    </div>
  </section>;
}
