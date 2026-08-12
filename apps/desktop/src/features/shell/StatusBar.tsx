import { CloseOutlined, CloudServerOutlined, DatabaseOutlined, FolderOpenOutlined, SafetyCertificateOutlined, SyncOutlined, ThunderboltOutlined } from "@ant-design/icons";
import { Drawer, Progress } from "antd";
import { useState } from "react";
import type { AppStatusSnapshot } from "../../bridge";
import { useAppStore } from "../../state/app-store";

interface StatusBarProps {
  snapshot: AppStatusSnapshot | null;
}

const BACKEND_LABELS: Record<string, string> = {
  cuda: "CUDA GPU",
  vulkan: "Vulkan GPU",
  metal: "Metal GPU",
  cpu: "CPU 回退",
  unavailable: "运行时不可用",
};

export function StatusBar({ snapshot }: StatusBarProps) {
  const [open, setOpen] = useState(false);
  const navigate = useAppStore((state) => state.navigate);
  const openInbox = useAppStore((state) => state.open_inbox);
  const openSettings = useAppStore((state) => state.open_settings);
  const roots = snapshot?.roots ?? null;
  const maintenance = snapshot?.maintenance ?? null;
  const inference = snapshot?.inference_runtime ?? null;
  const scan = snapshot?.scan_progress;
  const progress = scan ? Math.round(scan.progress * 100) : 0;
  const enabledRoots = roots?.filter((root) => root.enabled).length;
  const activeJobs = maintenance?.active_jobs;
  const indexLabel = scan
    ? scan.status === "running"
      ? `扫描 ${progress}% · 已解析 ${scan.parsed_files} 个`
      : scan.status === "paused"
        ? `扫描已暂停 · ${progress}%`
        : `索引 ${progress}%`
    : maintenance
      ? `${maintenance.indexed_files} 份资料已索引`
      : "索引状态读取中";
  const searchableChunks = maintenance?.searchable_chunks ?? 0;
  const embeddingCoverage = searchableChunks > 0
    ? Math.min(100, Math.round(((maintenance?.embedded_chunks ?? 0) / searchableChunks) * 100))
    : 0;
  const backendLabel = inference ? (BACKEND_LABELS[inference.backend] ?? inference.backend.toUpperCase()) : "检测中";

  return (<>
    <footer className="status-bar" aria-label="应用状态">
      <button type="button" onClick={() => setOpen(true)} title="查看本地处理与隐私状态"><CloudServerOutlined /> 完全本地</button>
      <i />
      <button type="button" onClick={() => setOpen(true)}><FolderOpenOutlined /> {enabledRoots == null ? "资料位置读取中" : `${enabledRoots}个资料位置`}</button>
      <i />
      <button type="button" onClick={() => setOpen(true)}><DatabaseOutlined /> {indexLabel}</button>
      <i />
      <button type="button" onClick={() => setOpen(true)} title={maintenance?.background_notice ?? undefined}><SyncOutlined spin={scan?.status === "running" || Boolean(activeJobs)} /> {maintenance?.background_notice ?? (activeJobs == null ? "任务状态读取中" : `${activeJobs}个后台任务`)}</button>
    </footer>
    <Drawer placement="bottom" height="min(620px, 84vh)" open={open} onClose={() => setOpen(false)} closable={false} className="app-status-drawer">
      <div className="app-status-panel__title"><div><h2>拾忆当前状态</h2><p>这里展示真实的资料、索引和推理资源状态。</p></div><button type="button" aria-label="关闭状态面板" onClick={() => setOpen(false)}><CloseOutlined /></button></div>
      <div className="app-status-panel">
        <section className="app-status-panel__privacy"><SafetyCertificateOutlined /><div><strong>完全本地，源文件只读</strong><p>资料解析、索引和模型推理只在本机运行。拾忆不会移动、重命名、删除或覆盖源文件。</p></div></section>
        <section><header><h3>授权资料位置</h3><span>{enabledRoots ?? 0} 个已授权</span></header><div className="status-root-list">{roots?.filter((root) => root.enabled).map((root) => <div key={root.root_id}><span className={`status-dot status-dot--${root.status}`} /><strong>{root.label}</strong><small>{root.status === "ready" ? "在线" : root.status === "scanning" ? "正在扫描" : root.status === "offline" ? "暂时离线" : root.status === "removing" ? "正在撤销授权" : "需要检查"} · {root.file_count} 个文件</small></div>)}{roots?.filter((root) => root.enabled).length === 0 && <p>尚未授权任何资料位置。</p>}</div></section>
        <section><header><h3>索引覆盖</h3><span>{embeddingCoverage}% 语义覆盖</span></header><div className="status-metrics"><span><strong>{maintenance?.indexed_files ?? 0}</strong>已发现资料</span><span><strong>{maintenance?.searchable_chunks ?? 0}</strong>可搜索文本块</span><span><strong>{maintenance?.embedded_chunks ?? 0}</strong>语义向量块</span></div><label>Embedding 覆盖率 <Progress percent={embeddingCoverage} size="small" showInfo={false} strokeColor="#7468cf" trailColor="rgba(116,104,207,.13)" /></label>{(maintenance?.failed_files ?? 0) > 0 && <div className="status-recovery-note"><strong>{maintenance?.failed_files} 项处理失败</strong><span>失败不会把覆盖率标红，可前往收件箱查看原因并重试。</span></div>}</section>
        <section><header><h3>推理资源</h3><span>{inference?.active ? "正在运行" : "当前空闲"}</span></header><div className="inference-status"><ThunderboltOutlined /><div><strong>{backendLabel}</strong><p>{inference?.device_names.length ? inference.device_names.join("、") : "没有被当前运行时识别的 GPU 设备"}</p></div></div><div className="status-metrics status-metrics--runtime"><span><strong>{inference?.gpu_offload_mode === "automatic" ? "自动适配" : inference?.gpu_offload_layers ?? "未启用"}</strong>GPU卸载</span><span><strong>{inference?.thread_budget ?? 0}</strong>生成线程</span><span><strong>{inference?.batch_thread_budget ?? 0}</strong>批处理线程</span></div>{inference?.pressure_reason && <p>{inference.pressure_reason}</p>}</section>
        <section><header><h3>后台任务</h3><span>{activeJobs ?? 0} 个活动任务</span></header>{scan ? <><p>{scan.status === "running" ? `正在扫描 ${scan.discovered_files} 个文件，已解析 ${scan.parsed_files} 个。` : indexLabel}</p><Progress percent={progress} size="small" status={scan.status === "paused" ? "normal" : "active"} strokeColor="#7468cf" /></> : <p>{maintenance?.background_notice ?? "当前没有需要前台等待的任务。"}</p>}<small>搜索、预览和问答开始时，可恢复的后台分析会主动让出资源。</small></section>
        <div className="app-status-panel__actions"><button type="button" onClick={() => { setOpen(false); openInbox("error"); }}>查看失败与待处理</button><button type="button" onClick={() => { setOpen(false); navigate("model_setup"); }}>查看模型与下载</button><button type="button" className="primary-button" onClick={() => { setOpen(false); openSettings("logs"); }}>打开日志与维护</button></div>
      </div>
    </Drawer>
  </>);
}
