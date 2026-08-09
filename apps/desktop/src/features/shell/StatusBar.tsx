import { CloudServerOutlined, DatabaseOutlined, FolderOpenOutlined, SyncOutlined } from "@ant-design/icons";
import type { HomeSummary, MaintenanceSnapshot, RootRecord } from "../../bridge";

interface StatusBarProps {
  summary: HomeSummary | null;
  roots: RootRecord[] | null;
  maintenance: MaintenanceSnapshot | null;
}

export function StatusBar({ summary, roots, maintenance }: StatusBarProps) {
  const progress = summary?.scan_progress ? Math.round(summary.scan_progress.progress * 100) : 0;
  const enabledRoots = roots?.filter((root) => root.enabled).length;
  const activeJobs = maintenance?.active_jobs;
  const runtimeLabel = maintenance?.degradation_level === "core"
    ? "核心模式"
    : maintenance?.degradation_level === "balanced"
      ? "均衡模式"
      : "完整模式";
  const indexLabel = summary?.scan_progress
    ? `索引${progress}%`
    : maintenance
      ? `${maintenance.indexed_files}份资料已索引`
      : "索引状态读取中";
  return (
    <footer className="status-bar" aria-label="应用状态">
      <span title={maintenance?.degradation_reasons.join("；") || "所有本地能力按完整模式运行"}><CloudServerOutlined /> 完全本地 · {runtimeLabel}</span>
      <i />
      <span><FolderOpenOutlined /> {enabledRoots == null ? "资料位置读取中" : `${enabledRoots}个资料位置`}</span>
      <i />
      <span><DatabaseOutlined /> {indexLabel}</span>
      <i />
      <span><SyncOutlined spin={summary?.scan_progress?.status === "running" || Boolean(activeJobs)} /> {activeJobs == null ? "任务状态读取中" : `${activeJobs}个后台任务`}</span>
    </footer>
  );
}
