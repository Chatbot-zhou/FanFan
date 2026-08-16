import { CloseOutlined, CloudServerOutlined, DatabaseOutlined, FolderOpenOutlined, ReloadOutlined, ThunderboltOutlined } from "@ant-design/icons";
import { useQueryClient } from "@tanstack/react-query";
import { Drawer, Progress } from "antd";
import { useEffect, useRef, useState } from "react";
import { bridge, type AppStatusSnapshot } from "../../bridge";
import { formatModelDownloadEta } from "../../features/model-downloads/model-downloads";

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
  const [refreshing, setRefreshing] = useState(false);
  const queryClient = useQueryClient();
  const roots = snapshot?.roots ?? null;
  const maintenance = snapshot?.maintenance ?? null;
  const inference = snapshot?.inference_runtime ?? null;
  const runtime = snapshot?.ai_runtime ?? null;
  const enabledRoots = roots?.filter((root) => root.enabled).length;
  const searchableChunks = maintenance?.searchable_chunks ?? 0;
  const embeddedChunks = maintenance?.embedded_chunks ?? 0;
  const embeddingCoverage = searchableChunks > 0
    ? Math.min(100, Math.round((embeddedChunks / searchableChunks) * 100))
    : 0;
  // 嵌入进行时以约 1.5s 间隔轮询快照，这里用最近 60s 的窗口估算嵌入速率，给出预计剩余时间
  const embeddedHistory = useRef<Array<{ t: number; embedded: number }>>([]);
  useEffect(() => {
    const data = snapshot?.maintenance;
    if (!data) return;
    const now = Date.now();
    embeddedHistory.current = [
      ...embeddedHistory.current.filter((point) => now - point.t <= 60_000),
      { t: now, embedded: data.embedded_chunks },
    ];
  }, [snapshot]);
  const firstPoint = embeddedHistory.current[0];
  const lastPoint = embeddedHistory.current[embeddedHistory.current.length - 1];
  let embeddingRate = 0;
  if (firstPoint && lastPoint && lastPoint !== firstPoint) {
    const elapsedSeconds = (lastPoint.t - firstPoint.t) / 1000;
    const gained = lastPoint.embedded - firstPoint.embedded;
    if (elapsedSeconds >= 1 && gained > 0) embeddingRate = gained / elapsedSeconds;
  }
  const embeddingEta = embeddingCoverage < 100 && searchableChunks > embeddedChunks && embeddingRate > 0
    ? formatModelDownloadEta(Math.ceil((searchableChunks - embeddedChunks) / embeddingRate))
    : null;
  const backendLabel = inference ? (BACKEND_LABELS[inference.backend] ?? inference.backend.toUpperCase()) : "检测中";
  const loadedModels = runtime?.instances.map((instance) => instance.model_id).filter((model): model is string => Boolean(model)) ?? [];
  const refreshInference = async () => {
    if (refreshing) return;
    setRefreshing(true);
    try {
      await bridge.inference_runtime_refresh();
      // 后端探测完成会 emit model:state / runtime:state，这里再主动失效一次兜底
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["app-status"] }),
        queryClient.invalidateQueries({ queryKey: ["model-runtime"] }),
        queryClient.invalidateQueries({ queryKey: ["environment"] }),
      ]);
    } finally {
      setRefreshing(false);
    }
  };

  return (<>
    <footer className="status-bar" aria-label="应用状态">
      <button type="button" onClick={() => setOpen(true)} title="查看本地处理与隐私状态"><CloudServerOutlined /> 完全本地</button>
      <i />
      <button type="button" onClick={() => setOpen(true)}><FolderOpenOutlined /> {enabledRoots == null ? "资料位置读取中" : `${enabledRoots}个资料位置`}</button>
      <i />
      <button type="button" onClick={() => setOpen(true)}><DatabaseOutlined /> 检索覆盖 {embeddingCoverage}%</button>
      <i />
      <button type="button" onClick={() => setOpen(true)}><ThunderboltOutlined /> {backendLabel}</button>
    </footer>
    <Drawer placement="bottom" height="min(340px, 52vh)" open={open} onClose={() => setOpen(false)} closable={false} className="app-status-drawer" styles={{
      wrapper: { width: "min(920px, calc(100% - 48px))", left: 24, right: 24, bottom: 14, margin: "0 auto", boxShadow: "0 18px 50px rgba(30,27,55,.28)" },
      section: { borderRadius: 22, overflow: "hidden" },
    }}>
      <div className="app-status-panel__title"><h2>当前状态</h2><button type="button" aria-label="关闭状态面板" onClick={() => setOpen(false)}><CloseOutlined /></button></div>
      <div className="app-status-panel">
        <section className="app-status-panel__coverage"><header><h3>检索覆盖</h3><span>{embeddingCoverage}%</span></header><Progress percent={embeddingCoverage} size="small" showInfo={false} strokeColor="#7468cf" trailColor="rgba(116,104,207,.13)" />{embeddingEta && <small className="status-eta">预计剩余 {embeddingEta}</small>}</section>
        <div className="app-status-panel__compact-grid">
          <section><header><h3>授权资料位置</h3><span>{enabledRoots ?? 0} 个</span></header><div className="status-root-list">{roots?.filter((root) => root.enabled).map((root) => <div key={root.root_id}><span className={`status-dot status-dot--${root.status}`} /><strong>{root.label}</strong><small>{root.status === "ready" ? "在线" : root.status === "scanning" ? "扫描中" : root.status === "offline" ? "离线" : "需检查"}</small></div>)}{(enabledRoots ?? 0) === 0 && <p>尚未授权资料位置</p>}</div></section>
          <section><header><h3>索引</h3><span>{maintenance?.failed_files ?? 0} 个失败</span></header><div className="status-metrics"><span><strong>{maintenance?.indexed_files ?? 0}</strong>文件</span><span><strong>{searchableChunks}</strong>文本块</span></div></section>
          <section><header><h3>推理资源</h3><span>{runtime?.running_count || inference?.active ? "运行中" : "空闲"}</span></header><div className="inference-status"><ThunderboltOutlined /><div><strong>{backendLabel}</strong><p>{inference?.device_names.join("、") || "当前运行时未识别 GPU"}</p></div></div><div className="inference-status-card__footer"><small className="status-model-list">{loadedModels.length ? `已加载：${loadedModels.join("、")}` : "当前没有已加载模型"}</small><button type="button" className="inference-refresh" onClick={refreshInference} disabled={refreshing} aria-label="重新探测推理资源" title="重新探测推理资源（CPU/GPU）"><ReloadOutlined spin={refreshing} />{refreshing ? "探测中" : "刷新"}</button></div></section>
        </div>
      </div>
    </Drawer>
  </>);
}
