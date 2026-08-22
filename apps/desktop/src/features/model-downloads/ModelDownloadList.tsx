import type { ModelDownloadJob } from "../../bridge";
import {
  cleanModelDisplayName,
  formatModelDownloadBytes,
  formatModelDownloadEta,
  MODEL_DOWNLOAD_PHASE_LABELS,
  modelDownloadNeedsAttention,
} from "./model-downloads";

export type ModelDownloadAction = "pause" | "cancel" | "resume" | "retry" | "remove";

interface ModelDownloadListProps {
  jobs: ModelDownloadJob[];
  compact?: boolean;
  pending_actions?: Record<string, ModelDownloadAction>;
  action_errors?: Record<string, string>;
  on_manage?: (job: ModelDownloadJob) => void;
  on_action?: (job: ModelDownloadJob, action: ModelDownloadAction) => void;
}

const sourceLabel = (job: ModelDownloadJob) => job.source === "modelscope" ? "魔搭社区" : job.source === "ollama" ? "本机 Ollama" : "Hugging Face";

export function ModelDownloadList({
  jobs,
  compact = false,
  pending_actions = {},
  action_errors = {},
  on_manage,
  on_action,
}: ModelDownloadListProps) {
  return (
    <div className={`model-download-list${compact ? " model-download-list--compact" : ""}`}>
      {jobs.map((job) => {
        const pendingAction = pending_actions[job.job_id];
        const progress = Math.round(Math.min(1, Math.max(0, job.progress)) * 100);
        const eta = formatModelDownloadEta(job.eta_seconds);
        const failed = modelDownloadNeedsAttention(job);
        const error = job.activation_status === "failed" ? job.activation_error : job.error;
        const errorText = action_errors[job.job_id] ?? (error ? `${error.message} · ${error.code}` : null);
        return (
          <article
            key={job.job_id}
            className={`model-download-row model-download-row--${failed ? "failed" : job.status}`}
            data-job-id={job.job_id}
          >
            <div className="model-download-row__primary">
              <div className="model-download-row__identity">
                <strong>{cleanModelDisplayName(job.edition_name)}</strong>
                <small>{failed && job.activation_status === "failed" ? "模型已下载，启用失败" : MODEL_DOWNLOAD_PHASE_LABELS[job.phase]} · <b>{sourceLabel(job)}</b></small>
              </div>
              <span title={errorText ?? undefined} role={errorText ? "alert" : undefined} className="model-download-row__error">{errorText ?? ""}</span>
              <em>{progress}%</em>
              <div className="model-download-row__actions">
                {on_manage && <button type="button" onClick={() => on_manage(job)}>管理</button>}
                {!compact && on_action && (job.status === "queued" || job.status === "running") && <>
                  {job.source !== "ollama" && (
                    <button type="button" disabled={Boolean(pendingAction)} onClick={() => on_action(job, "pause")}>{pendingAction === "pause" ? "暂停中" : "暂停"}</button>
                  )}
                  <button type="button" disabled={Boolean(pendingAction)} onClick={() => on_action(job, "cancel")}>{pendingAction === "cancel" ? "取消中" : "取消"}</button>
                </>}
                {!compact && on_action && job.status === "paused" && <>
                  <button type="button" className="primary-button" disabled={Boolean(pendingAction)} onClick={() => on_action(job, "resume")}>{pendingAction === "resume" ? "继续中" : "继续"}</button>
                  <button type="button" disabled={Boolean(pendingAction)} onClick={() => on_action(job, "cancel")}>{pendingAction === "cancel" ? "取消中" : "取消"}</button>
                </>}
                {!compact && on_action && failed && <>
                  <button type="button" className="primary-button" disabled={Boolean(pendingAction)} onClick={() => on_action(job, "retry")}>{pendingAction === "retry" ? "重试中" : "重试"}</button>
                  <button type="button" disabled={Boolean(pendingAction)} onClick={() => on_action(job, "remove")}>{pendingAction === "remove" ? "移除中" : "移除任务"}</button>
                </>}
              </div>
            </div>
            <div className="model-download-row__progress">
              <progress aria-label={`${cleanModelDisplayName(job.edition_name)}下载进度`} value={job.progress} max={1} />
              <span>{formatModelDownloadBytes(job.downloaded_bytes)} / {formatModelDownloadBytes(job.total_bytes)}</span>
              {job.bytes_per_second > 0 && <span>{formatModelDownloadBytes(job.bytes_per_second)}/s</span>}
              {eta && <span>{eta}</span>}
              {job.current_file && !compact && <span title={job.current_file}>当前：{job.current_file}</span>}
            </div>
          </article>
        );
      })}
    </div>
  );
}
