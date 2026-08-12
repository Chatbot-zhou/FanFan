import type { AppError } from "../bridge/contracts";

const FALLBACK_MESSAGE = "操作没有完成，请重试；如仍失败，请在设置中导出诊断包。";

export function normalizeAppError(value: unknown): AppError {
  if (typeof value === "string") {
    const parsed = parseStructuredError(value);
    if (parsed) return normalizeAppError(parsed);
  }
  if (value instanceof Error) {
    return {
      code: nonEmpty((value as Error & { code?: unknown }).code) ?? (value.name.toUpperCase().replace(/[^A-Z0-9]+/g, "_") || "JAVASCRIPT_ERROR"),
      message: nonEmpty(value.message) ?? FALLBACK_MESSAGE,
      retryable: Boolean((value as Error & { retryable?: unknown }).retryable),
      user_action: nonEmpty((value as Error & { user_action?: unknown }).user_action),
      file_id: nonEmpty((value as Error & { file_id?: unknown }).file_id),
      details: isRecord((value as Error & { details?: unknown }).details) ? (value as Error & { details: Record<string, unknown> }).details : null,
    };
  }
  if (isRecord(value)) {
    const nested = isRecord(value.error) ? value.error : value;
    return {
      code: nonEmpty(nested.code) ?? "UNCLASSIFIED_ERROR",
      message: nonEmpty(nested.message) ?? nonEmpty(nested.error) ?? FALLBACK_MESSAGE,
      retryable: nested.retryable === true,
      user_action: nonEmpty(nested.user_action),
      file_id: nonEmpty(nested.file_id),
      details: isRecord(nested.details) ? nested.details : safeDetails(nested),
    };
  }
  const message = nonEmpty(value);
  return {
    code: "UNCLASSIFIED_ERROR",
    message: !message || message === "[object Object]" ? FALLBACK_MESSAGE : message,
    retryable: false,
    user_action: null,
    file_id: null,
    details: null,
  };
}

export function errorMessage(value: unknown): string {
  const error = normalizeAppError(value);
  return error.user_action ? `${error.message} ${error.user_action}` : error.message;
}

function nonEmpty(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const normalized = value.trim();
  return normalized ? normalized : null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function safeDetails(value: Record<string, unknown>): Record<string, unknown> | null {
  const details = Object.fromEntries(Object.entries(value).filter(([key]) => !["message", "error", "stack"].includes(key)));
  return Object.keys(details).length ? details : null;
}

function parseStructuredError(value: string): Record<string, unknown> | null {
  const trimmed = value.trim();
  if (!trimmed.startsWith("{") || !trimmed.endsWith("}")) return null;
  try {
    const parsed: unknown = JSON.parse(trimmed);
    return isRecord(parsed) ? parsed : null;
  } catch { return null; }
}
