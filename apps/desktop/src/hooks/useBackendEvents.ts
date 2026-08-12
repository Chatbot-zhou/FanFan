import { isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import type { AppError, JobRecord } from "../bridge";
import { recordDiagnosticEvent } from "../bridge/observed-bridge";
import { RUNTIME_EVENTS } from "../bridge/runtime-events";
import { normalizeAppError } from "../utils/app-error";

const LIGHT_QUERY_KEYS = ["home-summary", "roots", "settings-roots", "maintenance"];
const CATALOG_QUERY_KEYS = [...LIGHT_QUERY_KEYS, "files", "inbox", "collections", "collection-files", "collection-suggestions", "file-relations"];

export function useBackendEvents() {
  const queryClient = useQueryClient();
  const [notice, setNotice] = useState<string | null>(null);

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let refreshTimer: ReturnType<typeof setTimeout> | null = null;
    const unlisteners: UnlistenFn[] = [];
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
          setNotice(event.payload?.message || "模型下载失败，可在右上角打开详情后重试或切换来源。");
        }),
        listen(RUNTIME_EVENTS.modelDownloadCompleted, () => {
          refresh();
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
          void queryClient.invalidateQueries({ queryKey: ["model-artifacts"] });
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
        }),
        listen(RUNTIME_EVENTS.modelState, () => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-role-configs"] });
        }),
        listen(RUNTIME_EVENTS.embeddingIndexPhase, () => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen<AppError>(RUNTIME_EVENTS.embeddingFailed, (event) => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
          setNotice(event.payload?.message || "新语义索引构建失败，拾忆已保留原索引。可在模型配置中重试。");
        }),
        listen<AppError>(RUNTIME_EVENTS.catalogWatchDegraded, (event) => {
          setNotice(event.payload?.message || "部分资料目录的实时监听暂时不可用，拾忆会保留已有索引。");
          refresh();
        }),
        listen<AppError>(RUNTIME_EVENTS.indexFailed, (event) => {
          setNotice(event.payload?.message || "部分资料索引失败，请前往收件箱查看并重试。");
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
      setNotice(`后台状态监听未能启动：${error.message}`);
    });
    return () => {
      disposed = true;
      if (refreshTimer) clearTimeout(refreshTimer);
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [queryClient]);

  return notice;
}
