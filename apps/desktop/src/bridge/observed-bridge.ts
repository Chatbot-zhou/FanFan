import type { AppError, DiagnosticEventInput, ReminBridge } from "./contracts";
import { tauriBridge } from "./tauri-bridge";
import { normalizeAppError } from "../utils/app-error";

const IMPORTANT_ACTIONS = new Set<keyof ReminBridge>([
  "welcome_complete",
  "welcome_authorization_complete",
  "theme_set_preference",
  "environment_detect",
  "model_import_scan",
  "model_import_confirm",
  "model_download_start",
  "model_download_pause",
  "model_download_cancel",
  "model_download_retry",
  "model_artifact_activate",
  "candidate_root_action",
  "search_start",
  "ask_start",
  "ask_session_rename",
  "ask_session_delete",
  "ask_cancel",
  "preview_get",
  "file_open",
  "file_reveal",
  "inbox_update",
  "inbox_retry",
  "ocr_retry",
  "image_understanding_retry",
  "image_deep_analyze",
  "collection_create",
  "collection_update",
  "collection_delete",
  "collection_rule_preview",
  "collection_add_file",
  "collection_remove_file",
  "collection_suggestion_refresh",
  "collection_suggestion_update",
  "collection_suggestion_confirm",
  "collection_suggestion_reject",
  "relation_refresh",
  "relation_review",
  "relation_batch_review",
  "answer_export",
  "exclusion_rule_upsert",
  "exclusion_rule_delete",
  "maintenance_check",
  "storage_migration_schedule",
  "cache_clear",
  "app_data_reset_schedule",
  "diagnostic_export",
  "index_rebuild",
  "root_add",
  "root_disable",
  "scan_start",
  "scan_pause",
  "scan_resume",
  "scan_cancel",
]);

const SLOW_OPERATION_MS = 750;

const newCorrelationId = (): string => globalThis.crypto?.randomUUID?.()
  ?? `${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;

export const recordDiagnosticEvent = (request: DiagnosticEventInput): void => {
  if (!window.__TAURI_INTERNALS__) return;
  void tauriBridge.diagnostic_event_append(request).catch(() => undefined);
};

const errorSummary = (error: unknown): Pick<AppError, "code" | "retryable"> & { error_type: string; message?: string } => {
  const normalized = normalizeAppError(error);
  return {
    code: normalized.code,
    retryable: normalized.retryable,
    error_type: error instanceof Error ? error.name : typeof error,
    message: normalized.message,
  };
};

const responseSummary = (response: unknown): Record<string, unknown> => {
  if (Array.isArray(response)) return { result_count: response.length };
  if (!response || typeof response !== "object") return {};
  const record = response as Record<string, unknown>;
  const summary: Record<string, unknown> = {};
  if (Array.isArray(record.items)) summary.returned_count = record.items.length;
  if (typeof record.has_more === "boolean") summary.has_more = record.has_more;
  if ("next_cursor" in record) summary.next_cursor_present = typeof record.next_cursor === "string" && record.next_cursor.length > 0;
  if (typeof record.total === "number") summary.total = record.total;
  if (typeof record.status === "string") summary.status = record.status;
  if (typeof record.progress === "number") summary.progress = record.progress;
  if (typeof record.operation_id === "string") summary.operation_id = record.operation_id;
  if (typeof record.job_id === "string") summary.job_id = record.job_id;
  return summary;
};

export const observedTauriBridge: ReminBridge = new Proxy(tauriBridge, {
  get(target, property, receiver) {
    const value = Reflect.get(target, property, receiver);
    if (typeof value !== "function" || property === "diagnostic_event_append") return value;
    const command = String(property) as keyof ReminBridge;
    return (...args: unknown[]) => {
      const correlationId = newCorrelationId();
      const startedAt = performance.now();
      const important = IMPORTANT_ACTIONS.has(command);
      if (important) {
        recordDiagnosticEvent({
          level: "info",
          component: "frontend.bridge",
          event_name: "feature.action_started",
          correlation_id: correlationId,
          fields: { command },
        });
      }
      try {
        const result = Reflect.apply(value, target, args) as Promise<unknown>;
        return Promise.resolve(result).then(
          (response) => {
            const durationMs = Math.round(performance.now() - startedAt);
            if (important || durationMs >= SLOW_OPERATION_MS) {
              recordDiagnosticEvent({
                level: durationMs >= SLOW_OPERATION_MS ? "warning" : "info",
                component: "frontend.bridge",
                event_name: durationMs >= SLOW_OPERATION_MS ? "feature.action_slow" : "feature.action_completed",
                correlation_id: correlationId,
                fields: { command, duration_ms: durationMs, ...responseSummary(response) },
              });
            }
            return response;
          },
          (error: unknown) => {
            recordDiagnosticEvent({
              level: "error",
              component: "frontend.bridge",
              event_name: "feature.action_failed",
              correlation_id: correlationId,
              fields: {
                command,
                duration_ms: Math.round(performance.now() - startedAt),
                ...errorSummary(error),
              },
            });
            throw error;
          },
        );
      } catch (error) {
        recordDiagnosticEvent({
          level: "error",
          component: "frontend.bridge",
          event_name: "feature.action_failed",
          correlation_id: correlationId,
          fields: {
            command,
            duration_ms: Math.round(performance.now() - startedAt),
            ...errorSummary(error),
          },
        });
        throw error;
      }
    };
  },
});
