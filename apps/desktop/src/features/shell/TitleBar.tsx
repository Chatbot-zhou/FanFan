import type { ModelDownloadJob, ModelRuntimeState } from "../../bridge";
import { BrandMark } from "../../components/BrandMark";
import { WindowControls } from "../../components/WindowControls";
import { useAppStore } from "../../state/app-store";

interface TitleBarProps {
  model_state: ModelRuntimeState | null;
  model_download?: ModelDownloadJob | null;
  welcome?: boolean;
}

const PHASE_LABELS: Record<ModelDownloadJob["phase"], string> = {
  queued: "等待下载",
  downloading: "下载中",
  verifying: "校验中",
  installing: "安装中",
  self_testing: "自检中",
  activating: "启用中",
  indexing: "正在建立语义索引",
  paused: "下载已暂停",
  completed: "完整 RAG 已就绪",
  failed: "模型下载失败",
  cancelled: "模型下载已取消",
};

export function TitleBar({ model_state, model_download = null, welcome = false }: TitleBarProps) {
  const navigate = useAppStore((state) => state.navigate);
  const dismissed = useAppStore((state) => state.model_prompt_dismissed);
  const dismiss = useAppStore((state) => state.dismiss_model_prompt);
  const incompleteDownload = !welcome && model_download && model_download.status !== "completed";
  const showPrompt = !incompleteDownload && !welcome && !dismissed && model_state?.status === "unconfigured";
  const fullRagReady = model_state?.status === "ready" && model_state.rag_complete;

  return (
    <header className={`title-bar${welcome ? " title-bar--welcome" : ""}`} data-tauri-drag-region>
      <div className="title-bar__brand" data-tauri-drag-region>
        <BrandMark compact />
      </div>
      <div className="title-bar__spacer" data-tauri-drag-region />
      {incompleteDownload && (
        <button type="button" className={`model-download-pill model-download-pill--${model_download.status}`} onClick={() => navigate("model_setup")} aria-label="查看模型下载详情">
          <span className="model-prompt__dot" aria-hidden="true" />
          <span>{PHASE_LABELS[model_download.phase]}</span>
          <strong>{Math.round(model_download.progress * 100)}%</strong>
          <progress value={model_download.progress} max={1} />
        </button>
      )}
      {showPrompt && (
        <div className="model-prompt" aria-label="模型配置提示">
          <span className="model-prompt__dot" aria-hidden="true" />
          <span>未配置本地模型</span>
          <button type="button" onClick={() => navigate("model_setup")}>去配置</button>
          <button type="button" onClick={dismiss}>稍后</button>
        </div>
      )}
      {!incompleteDownload && model_state?.status === "ready" && <button type="button" className="model-ready" onClick={() => navigate("model_setup")}>{fullRagReady ? "完整 RAG 已就绪" : model_state.message}</button>}
      <WindowControls />
    </header>
  );
}
