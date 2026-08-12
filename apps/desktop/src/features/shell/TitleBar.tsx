import type { ModelDownloadJob, ModelRuntimeState, SystemNotice } from "../../bridge";
import { BrandMark } from "../../components/BrandMark";
import { WindowControls } from "../../components/WindowControls";
import { useAppStore } from "../../state/app-store";

interface TitleBarProps {
  model_state: ModelRuntimeState | null;
  model_download?: ModelDownloadJob | null;
  notices?: SystemNotice[];
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

export function TitleBar({ model_state, model_download = null, notices = [], welcome = false }: TitleBarProps) {
  const navigate = useAppStore((state) => state.navigate);
  const dismissed = useAppStore((state) => state.model_prompt_dismissed);
  const dismiss = useAppStore((state) => state.dismiss_model_prompt);

  // 汇集所有系统通知
  const resolved: SystemNotice[] = [...notices];

  // 模型下载进度
  if (!welcome && model_download && (model_download.status !== "completed" || model_download.activation_status === "failed")) {
    resolved.push({
      notice_key: `model-download-${model_download.job_id}`,
      level: model_download.status === "failed" || model_download.activation_status === "failed" ? "warning" : "info",
      message: model_download.activation_status === "failed"
        ? `模型已下载，语义索引启用失败 · ${model_download.activation_error?.code ?? "可重试"}`
        : `${PHASE_LABELS[model_download.phase]} · ${Math.round(model_download.progress * 100)}%`,
      details: model_download.activation_error?.message ?? null,
      action_label: "查看",
      action_route: "model_setup",
    });
  }

  // 模型未配置
  if (!dismissed && !welcome && model_state?.status === "unconfigured" && !model_download) {
    resolved.push({
      notice_key: "model-unconfigured",
      level: "info",
      message: "未配置本地模型",
      details: "配置生成与 Embedding 模型后可使用完整本地 RAG。",
      action_label: "去配置",
      action_route: "model_setup",
    });
  }

  // 取优先级最高的一条：urgent > warning > info
  const priority = { urgent: 0, warning: 1, info: 2 } as const;
  resolved.sort((a, b) => priority[a.level] - priority[b.level]);
  const top = resolved[0];

  const dot = top?.level === "urgent"
    ? "system-dot system-dot--urgent"
    : top?.level === "warning"
      ? "system-dot system-dot--warning"
      : "system-dot system-dot--info";

  return (
    <header className={`title-bar${welcome ? " title-bar--welcome" : ""}`} data-tauri-drag-region>
      <div className="title-bar__brand" data-tauri-drag-region>
        <BrandMark compact />
      </div>
      <div className="title-bar__spacer" data-tauri-drag-region />
      {top && (
        <div className="system-status" aria-label="系统状态">
          <span className={dot} aria-hidden="true" />
          <span>{top.message}</span>
          {top.action_label && top.action_route && (
            <button type="button" onClick={() => navigate(top.action_route!)}>
              {top.action_label}
            </button>
          )}
          {top.level === "info" && top.message.includes("未配置本地模型") && (
            <button type="button" onClick={dismiss}>稍后</button>
          )}
        </div>
      )}
      <WindowControls />
    </header>
  );
}
