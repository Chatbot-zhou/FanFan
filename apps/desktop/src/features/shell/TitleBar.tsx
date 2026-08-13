import { DownOutlined } from "@ant-design/icons";
import { Popover } from "antd";
import { useMemo, useState } from "react";
import type { ModelDownloadJob, ModelRuntimeState, SystemNotice } from "../../bridge";
import { recordDiagnosticEvent } from "../../bridge/observed-bridge";
import { BrandMark } from "../../components/BrandMark";
import { ModelDownloadList } from "../model-downloads/ModelDownloadList";
import {
  summarizeModelDownloads,
  visibleModelDownloadJobs,
} from "../model-downloads/model-downloads";
import { WindowControls } from "../../components/WindowControls";
import { useAppStore } from "../../state/app-store";

interface TitleBarProps {
  model_state: ModelRuntimeState | null;
  model_downloads?: ModelDownloadJob[];
  notices?: SystemNotice[];
  welcome?: boolean;
}

const noticePriority = { urgent: 0, warning: 1, info: 2 } as const;

export function TitleBar({ model_state, model_downloads = [], notices = [], welcome = false }: TitleBarProps) {
  const navigate = useAppStore((state) => state.navigate);
  const dismissed = useAppStore((state) => state.model_prompt_dismissed);
  const dismiss = useAppStore((state) => state.dismiss_model_prompt);
  const [open, setOpen] = useState(false);

  const visibleDownloads = useMemo(() => visibleModelDownloadJobs(model_downloads), [model_downloads]);
  const downloadSummary = useMemo(() => summarizeModelDownloads(visibleDownloads), [visibleDownloads]);
  const resolved = useMemo(() => {
    const unique = new Map<string, SystemNotice>();
    notices.forEach((notice) => unique.set(notice.notice_key, notice));
    if (!dismissed && !welcome && model_state?.status === "unconfigured" && downloadSummary.active_count === 0) {
      unique.set("model-unconfigured", {
        notice_key: "model-unconfigured",
        level: "info",
        message: "未配置本地模型",
        details: "配置生成与 Embedding 模型后可使用完整本地 RAG。",
        action_label: "去配置",
        action_route: "model_setup",
      });
    }
    return [...unique.values()].sort((left, right) => (
      noticePriority[left.level] - noticePriority[right.level]
      || left.notice_key.localeCompare(right.notice_key)
    ));
  }, [dismissed, downloadSummary.active_count, model_state?.status, notices, welcome]);

  const noticeAttentionCount = resolved.filter((notice) => notice.level !== "info").length;
  const attentionCount = downloadSummary.attention_count + noticeAttentionCount;
  const summary = downloadSummary.active_count > 0
    ? `${downloadSummary.active_count} 个模型任务 · ${downloadSummary.progress == null ? "正在准备" : `总体 ${Math.round(downloadSummary.progress * 100)}%`}${attentionCount > 0 ? ` · ${attentionCount} 项待处理` : ""}`
    : attentionCount > 0
      ? `${attentionCount} 项需要处理`
      : resolved.length > 0
        ? `${resolved.length} 条状态提示`
        : null;

  const highestLevel = resolved[0]?.level
    ?? (downloadSummary.attention_count > 0 ? "warning" : "info");
  const dot = highestLevel === "urgent"
    ? "system-dot system-dot--urgent"
    : highestLevel === "warning" || downloadSummary.attention_count > 0
      ? "system-dot system-dot--warning"
      : "system-dot system-dot--info";

  const openChanged = (nextOpen: boolean) => {
    setOpen(nextOpen);
    if (nextOpen) {
      recordDiagnosticEvent({
        level: "info",
        component: "frontend.status_center",
        event_name: "status_center.opened",
        fields: {
          download_count: visibleDownloads.length,
          active_download_count: downloadSummary.active_count,
          notice_count: resolved.length,
        },
      });
    }
  };

  const manageDownload = (job: ModelDownloadJob) => {
    setOpen(false);
    recordDiagnosticEvent({
      level: "info",
      component: "frontend.status_center",
      event_name: "status_center.download_manage_clicked",
      fields: { job_id: job.job_id, status: job.status, phase: job.phase },
    });
    navigate("model_setup");
  };

  const content = (
    <div className="status-center" role="dialog" aria-modal="false" aria-label="统一状态中心">
      <header className="status-center__header">
        <div><strong>当前状态</strong><small>进度更新不会改变任务顺序</small></div>
        <span>{visibleDownloads.length + resolved.length} 项</span>
      </header>
      <div className="status-center__body">
        {visibleDownloads.length > 0 && (
          <section aria-labelledby="status-center-downloads">
            <div className="status-center__section-title"><h2 id="status-center-downloads">模型下载</h2><span>{downloadSummary.active_count} 个进行中</span></div>
            <ModelDownloadList jobs={visibleDownloads} compact on_manage={manageDownload} />
          </section>
        )}
        {resolved.length > 0 && (
          <section aria-labelledby="status-center-notices">
            <div className="status-center__section-title"><h2 id="status-center-notices">系统提示</h2><span>{resolved.length} 条</span></div>
            <div className="status-notice-list">
              {resolved.map((notice) => (
                <article key={notice.notice_key} className={`status-notice status-notice--${notice.level}`}>
                  <span className={`system-dot system-dot--${notice.level}`} aria-hidden="true" />
                  <div><strong>{notice.message}</strong>{notice.details && <small>{notice.details}</small>}</div>
                  {notice.action_label && notice.action_route && (
                    <button type="button" onClick={() => { setOpen(false); navigate(notice.action_route!); }}>{notice.action_label}</button>
                  )}
                  {notice.notice_key === "model-unconfigured" && <button type="button" onClick={dismiss}>稍后</button>}
                </article>
              ))}
            </div>
          </section>
        )}
      </div>
    </div>
  );

  return (
    <header className={`title-bar${welcome ? " title-bar--welcome" : ""}`} data-tauri-drag-region>
      <div className="title-bar__brand" data-tauri-drag-region><BrandMark compact /></div>
      <div className="title-bar__spacer" data-tauri-drag-region />
      {summary && !welcome && (
        <Popover
          content={content}
          open={open}
          onOpenChange={openChanged}
          trigger="click"
          placement="bottomRight"
          classNames={{ root: "status-center-popover" }}
        >
          <button type="button" className="system-status system-status--trigger" aria-label="打开统一状态中心" aria-expanded={open} aria-haspopup="dialog">
            <span className={dot} aria-hidden="true" />
            <span>{summary}</span>
            <DownOutlined className={open ? "system-status__arrow system-status__arrow--open" : "system-status__arrow"} />
          </button>
        </Popover>
      )}
      <WindowControls />
    </header>
  );
}
