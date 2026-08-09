import { isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import type { AppError, JobRecord } from "../bridge";

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
        listen("startup.state", () => {
          void queryClient.invalidateQueries({ queryKey: ["startup-state"] });
        }),
        listen<JobRecord>("job.progress", (event) => {
          const terminal = ["succeeded", "partial", "failed", "cancelled"].includes(event.payload.status);
          refresh(terminal ? CATALOG_QUERY_KEYS : LIGHT_QUERY_KEYS);
        }),
        listen("catalog.changed", () => refresh(CATALOG_QUERY_KEYS)),
        listen("index.changed", () => refresh(["home-summary", "maintenance", "inbox"])),
        listen("collection.suggestions_changed", () => refresh(["collections", "collection-suggestions", "collection-files", "file-relations"])),
        listen("model.download_started", () => {
          refresh();
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen("model.download_state", () => {
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen<AppError>("model.download_failed", (event) => {
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
          setNotice(event.payload?.message || "模型下载失败，可在右上角打开详情后重试或切换来源。");
        }),
        listen("model.download_completed", () => {
          refresh();
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
          void queryClient.invalidateQueries({ queryKey: ["model-artifacts"] });
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
        }),
        listen("model.state", () => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-role-configs"] });
        }),
        listen("embedding.index_phase", () => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
        }),
        listen<AppError>("embedding.failed", (event) => {
          void queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
          void queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
          setNotice(event.payload?.message || "新语义索引构建失败，拾忆已保留原索引。可在模型配置中重试。");
        }),
        listen<AppError>("catalog.watch_degraded", (event) => {
          setNotice(event.payload?.message || "部分资料目录的实时监听暂时不可用，拾忆会保留已有索引。");
          refresh();
        }),
        listen<AppError>("index.failed", (event) => {
          setNotice(event.payload?.message || "部分资料索引失败，请前往收件箱查看并重试。");
          refresh();
        }),
      ]);
      if (disposed) listeners.forEach((unlisten) => unlisten());
      else unlisteners.push(...listeners);
    };
    void register().catch(() => setNotice("实时状态通道暂时不可用；你仍可手动刷新页面。"));
    return () => {
      disposed = true;
      if (refreshTimer) clearTimeout(refreshTimer);
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [queryClient]);

  return notice;
}
