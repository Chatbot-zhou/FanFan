import { isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import type { AppError, JobRecord, SystemNotice } from "../bridge";
import { recordDiagnosticEvent } from "../bridge/observed-bridge";
import { RUNTIME_EVENTS } from "../bridge/runtime-events";
import { normalizeAppError } from "../utils/app-error";

const LIGHT_QUERY_KEYS = ["home-summary", "roots", "settings-roots", "maintenance", "app-status"];
const CATALOG_QUERY_KEYS = [...LIGHT_QUERY_KEYS, "files", "inbox", "collections", "collection-files", "collection-suggestions", "file-relations"];

export function useBackendEvents() {
  const queryClient = useQueryClient();
  const [notices, setNotices] = useState<SystemNotice[]>([]);

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let refreshTimer: ReturnType<typeof setTimeout> | null = null;
    const unlisteners: UnlistenFn[] = [];
    const upsertNotice = (notice: SystemNotice) => {
      setNotices((current) => [...current.filter((item) => item.notice_key !== notice.notice_key), notice]);
    };
    const refresh = (queryKeys: string[] = LIGHT_QUERY_KEYS) => {
      if (refreshTimer) clearTimeout(refreshTimer);
      refreshTimer = setTimeout(() => {
        for (const queryKey of queryKeys) void queryClient.invalidateQueries({ queryKey: [queryKey] });
      }, 1_000);
    };
    const register = async () => {
      const listeners = await Promise.all([
        listen(RUNTIME_EVENTS.startupState, () => {
          void queryClient.invalidateQueries({ queryKey: ["startup-state"] });
        }),
        listen(RUNTIME_EVENTS.runtimeState, () => {
          void queryClient.invalidateQueries({ queryKey: ["app-status"] });
        }),
        listen<JobRecord>(RUNTIME_EVENTS.jobProgress, (event) => {
          const terminal = ["succeeded", "partial", "failed", "cancelled"].includes(event.payload.status);
          refresh(terminal ? CATALOG_QUERY_KEYS : LIGHT_QUERY_KEYS);
        }),
        listen(RUNTIME_EVENTS.catalogChanged, () => refresh(CATALOG_QUERY_KEYS)),
        listen(RUNTIME_EVENTS.indexChanged, () => refresh(["home-summary", "maintenance", "inbox"])),
        listen(RUNTIME_EVENTS.indexRebuildStarted, () => refresh(["maintenance"])),
        listen(RUNTIME_EVENTS.collectionSuggestionsChanged, () => refresh(["collections", "collection-suggestions", "collection-files", "file-relations"])),
        listen(RUNTIME_EVENTS.modelDownloadStarted, () => {
          refresh();
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen(RUNTIME_EVENTS.modelDownloadState, () => {
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen<AppError>(RUNTIME_EVENTS.modelDownloadFailed, (event) => {
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen(RUNTIME_EVENTS.modelDownloadCompleted, () => {
          refresh();
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
          void queryClient.invalidateQueries({ queryKey: ["model-artifacts"] });
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
        }),
        listen(RUNTIME_EVENTS.modelDownloadRemoved, () => {
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen(RUNTIME_EVENTS.modelState, () => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          // 后台 GPU 探测完成时环境检测结果也随之刷新（GPU 名称/显存/后端）
          void queryClient.invalidateQueries({ queryKey: ["environment"] });
        }),
        listen(RUNTIME_EVENTS.embeddingIndexPhase, () => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen<AppError>(RUNTIME_EVENTS.embeddingFailed, (event) => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
          upsertNotice({
            notice_key: `embedding-failed-${event.payload?.code ?? "unknown"}`,
            level: "warning",
            message: "语义索引构建失败，已保留原索引",
            details: event.payload?.message ?? "可在模型配置中重试。",
            action_label: "查看模型",
            action_route: "model_setup",
          });
        }),
        listen<AppError>(RUNTIME_EVENTS.catalogWatchDegraded, (event) => {
          upsertNotice({
            notice_key: `catalog-watch-degraded-${event.payload?.code ?? "unknown"}`,
            level: "warning",
            message: "部分资料目录的实时监听暂时不可用",
            details: event.payload?.message ?? "翻翻会保留已有索引。",
            action_label: "查看设置",
            action_route: "settings",
          });
          refresh();
        }),
        listen<AppError>(RUNTIME_EVENTS.indexFailed, (event) => {
          upsertNotice({
            notice_key: `index-failed-${event.payload?.code ?? "unknown"}`,
            level: "warning",
            message: "部分资料索引失败",
            details: event.payload?.message ?? "请前往收件箱查看并重试。",
            action_label: "查看收件箱",
            action_route: "inbox",
          });
          refresh();
        }),
      ]);
      if (disposed) listeners.forEach((unlisten) => unlisten());
      else unlisteners.push(...listeners);
    };
    void register().catch((registrationError) => {
      const error = normalizeAppError(registrationError);
      recordDiagnosticEvent({
        level: "error",
        component: "frontend.events",
        event_name: "event_listener.registration_failed",
        fields: { error_code: error.code, retryable: error.retryable },
      });
      setNotices((current) => [...current.filter((item) => item.notice_key !== "event-listener-registration-failed"), {
        notice_key: "event-listener-registration-failed",
        level: "urgent",
        message: "后台状态监听未能启动",
        details: error.message,
        action_label: "打开日志",
        action_route: "settings",
      }]);
    });
    return () => {
      disposed = true;
      if (refreshTimer) clearTimeout(refreshTimer);
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [queryClient]);

  return notices;
}
