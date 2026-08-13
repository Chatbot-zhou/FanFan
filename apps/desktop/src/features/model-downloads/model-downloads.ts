import type { ModelDownloadJob } from "../../bridge";

export const MODEL_DOWNLOAD_SESSION_STARTED_AT = typeof performance !== "undefined"
  && Number.isFinite(performance.timeOrigin)
  ? performance.timeOrigin
  : Date.now();

export const MODEL_DOWNLOAD_PHASE_LABELS: Record<ModelDownloadJob["phase"], string> = {
  queued: "等待下载",
  downloading: "正在下载",
  verifying: "正在校验",
  installing: "正在安装",
  self_testing: "正在自检",
  activating: "正在启用",
  indexing: "正在建立语义索引",
  paused: "已暂停",
  completed: "已完成",
  failed: "下载失败",
  cancelled: "已取消",
};

const ACTIVE_STATUSES = new Set<ModelDownloadJob["status"]>(["queued", "running", "paused"]);

export interface ModelDownloadSummary {
  visible_count: number;
  active_count: number;
  attention_count: number;
  completed_count: number;
  downloaded_bytes: number;
  total_bytes: number;
  progress: number | null;
}

export function modelDownloadNeedsAttention(job: ModelDownloadJob) {
  return job.status === "failed" || job.activation_status === "failed";
}

export function modelDownloadIsActive(job: ModelDownloadJob) {
  return ACTIVE_STATUSES.has(job.status) && !modelDownloadNeedsAttention(job);
}

export function visibleModelDownloadJobs(
  jobs: ModelDownloadJob[],
  sessionStartedAt = MODEL_DOWNLOAD_SESSION_STARTED_AT,
) {
  return jobs
    .filter((job) => {
      if (modelDownloadNeedsAttention(job) || modelDownloadIsActive(job)) return true;
      if (job.status === "cancelled") return false;
      if (job.status !== "completed") return true;
      const updatedAt = Date.parse(job.updated_at);
      return Number.isFinite(updatedAt) && updatedAt >= sessionStartedAt;
    })
    .sort((left, right) => {
      const createdDelta = Date.parse(left.created_at) - Date.parse(right.created_at);
      return createdDelta || left.job_id.localeCompare(right.job_id);
    });
}

export function summarizeModelDownloads(jobs: ModelDownloadJob[]): ModelDownloadSummary {
  const active = jobs.filter(modelDownloadIsActive);
  const downloadedBytes = active.reduce((total, job) => total + Math.max(0, job.downloaded_bytes), 0);
  const totalBytes = active.reduce((total, job) => total + Math.max(0, job.total_bytes), 0);
  return {
    visible_count: jobs.length,
    active_count: active.length,
    attention_count: jobs.filter(modelDownloadNeedsAttention).length,
    completed_count: jobs.filter((job) => job.status === "completed" && !modelDownloadNeedsAttention(job)).length,
    downloaded_bytes: downloadedBytes,
    total_bytes: totalBytes,
    progress: totalBytes > 0 ? Math.min(1, Math.max(0, downloadedBytes / totalBytes)) : null,
  };
}

export function formatModelDownloadBytes(value: number) {
  if (value < 1024) return `${Math.max(0, Math.round(value))} B`;
  if (value < 1024 ** 2) return `${(value / 1024).toFixed(value < 10 * 1024 ? 1 : 0)} KB`;
  if (value < 1024 ** 3) return `${(value / 1024 ** 2).toFixed(value < 100 * 1024 ** 2 ? 1 : 0)} MB`;
  return `${(value / 1024 ** 3).toFixed(2)} GB`;
}

export function formatModelDownloadEta(seconds: number | null) {
  if (seconds == null || seconds < 0) return null;
  if (seconds < 60) return "不到 1 分钟";
  if (seconds < 3600) return `约 ${Math.ceil(seconds / 60)} 分钟`;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.ceil((seconds % 3600) / 60);
  return minutes > 0 ? `约 ${hours} 小时 ${minutes} 分钟` : `约 ${hours} 小时`;
}
