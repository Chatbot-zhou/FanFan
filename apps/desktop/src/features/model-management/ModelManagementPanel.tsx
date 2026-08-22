import { useMemo, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { bridge, type ModelDownloadJob } from "../../bridge";
import { recordDiagnosticEvent } from "../../bridge/observed-bridge";
import { ModelDownloadList, type ModelDownloadAction } from "../model-downloads/ModelDownloadList";
import {
  modelDownloadIsActive,
  summarizeModelDownloads,
  visibleModelDownloadJobs,
} from "../model-downloads/model-downloads";
import { errorMessage, normalizeAppError } from "../../utils/app-error";

/**
 * 官方档位页面底部的收敛区：保留「官方 4 档对应的模型下载任务」。
 * 本地模型导入入口已随 Ollama 迁移移除：generation/embedding/vision 改走本机
 * Ollama（`qwen3.5:*`、`qwen3-embedding:0.6b`），不再支持本地 GGUF/ONNX 导入。
 */
export function ModelManagementPanel() {
  const queryClient = useQueryClient();
  const [pendingDownloadActions, setPendingDownloadActions] = useState<Record<string, ModelDownloadAction>>({});
  const [downloadActionErrors, setDownloadActionErrors] = useState<Record<string, string>>({});
  const downloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
  });
  const visibleDownloads = useMemo(() => visibleModelDownloadJobs(downloads.data ?? []), [downloads.data]);
  const downloadSummary = useMemo(() => summarizeModelDownloads(visibleDownloads), [visibleDownloads]);

  const runDownloadAction = async (job: ModelDownloadJob, action: ModelDownloadAction) => {
    if (pendingDownloadActions[job.job_id]) return;
    const startedAt = performance.now();
    setPendingDownloadActions((current) => ({ ...current, [job.job_id]: action }));
    setDownloadActionErrors((current) => {
      const next = { ...current };
      delete next[job.job_id];
      return next;
    });
    recordDiagnosticEvent({
      level: "info",
      component: "frontend.model_downloads",
      event_name: "model_download.action_started",
      fields: { job_id: job.job_id, action, phase: job.phase },
    });
    try {
      let updated: ModelDownloadJob | null = null;
      if (action === "pause") updated = await bridge.model_download_pause(job.job_id);
      else if (action === "cancel") await bridge.model_download_cancel(job.job_id);
      else if (action === "resume") updated = await bridge.model_download_resume(job.job_id);
      else if (action === "remove") await bridge.model_download_remove(job.job_id);
      else updated = await bridge.model_download_retry(job.job_id);
      queryClient.setQueryData<ModelDownloadJob[]>(["model-downloads"], (current) => {
        if (!current) return updated ? [updated] : [];
        if (action === "cancel" || action === "remove") return current.filter((item) => item.job_id !== job.job_id);
        return current.map((item) => item.job_id === job.job_id && updated ? updated : item);
      });
      await queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
      await queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
      recordDiagnosticEvent({
        level: "info",
        component: "frontend.model_downloads",
        event_name: "model_download.action_completed",
        fields: { job_id: job.job_id, action, elapsed_ms: Math.round(performance.now() - startedAt) },
      });
    } catch (cause) {
      const error = normalizeAppError(cause);
      setDownloadActionErrors((current) => ({ ...current, [job.job_id]: error.message }));
      recordDiagnosticEvent({
        level: "error",
        component: "frontend.model_downloads",
        event_name: "model_download.action_failed",
        fields: {
          job_id: job.job_id,
          action,
          elapsed_ms: Math.round(performance.now() - startedAt),
          error_code: error.code,
          retryable: error.retryable,
        },
      });
    } finally {
      setPendingDownloadActions((current) => {
        const next = { ...current };
        delete next[job.job_id];
        return next;
      });
    }
  };

  return (
    <div className="model-management">
      {(visibleDownloads.some(modelDownloadIsActive) || downloadSummary.attention_count > 0) && (
        <section className="model-download-section" aria-label="模型下载任务">
          <header>
            <div><h2>模型下载</h2></div>
          </header>
          <ModelDownloadList
            jobs={visibleDownloads}
            pending_actions={pendingDownloadActions}
            action_errors={downloadActionErrors}
            on_action={(job, action) => void runDownloadAction(job, action)}
          />
        </section>
      )}
    </div>
  );
}