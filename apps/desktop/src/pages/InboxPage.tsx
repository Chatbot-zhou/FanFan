import { CheckOutlined, InboxOutlined, ReloadOutlined } from "@ant-design/icons";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type InboxItem, type InboxQuery } from "../bridge";
import { recordDiagnosticEvent } from "../bridge/observed-bridge";
import { OcrAttemptChain } from "../components/OcrAttemptChain";
import { useAppStore } from "../state/app-store";
import { errorMessage } from "../utils/app-error";
import { displayPath } from "../utils/display-path";

type InboxTab = InboxQuery["status"];

const tabs: Array<{ value: InboxTab; label: string }> = [
  { value: "new", label: "待处理" },
  { value: "all", label: "全部" },
  { value: "reviewed", label: "已查看" },
  { value: "error", label: "失败" },
  { value: "ignored", label: "已忽略" },
];

const eventLabels: Record<InboxItem["event_type"], string> = {
  discovered: "发现新资料",
  modified: "资料有新版本",
  renamed: "资料已重命名",
  missing: "资料已离开原位置",
  restored: "资料已恢复",
  ocr_required: "等待 OCR",
  parse_failed: "处理失败",
  relation_suggested: "发现资料关系",
  collection_suggested: "发现智能集合建议",
};

export function InboxPage() {
  const initialStatus = useAppStore((state) => state.inbox_initial_status);
  const initialTodayOnly = useAppStore((state) => state.inbox_today_only);
  const navigate = useAppStore((state) => state.navigate);
  const [status, setStatus] = useState<InboxTab>(initialStatus);
  const [todayOnly, setTodayOnly] = useState(initialTodayOnly);
  const queryClient = useQueryClient();
  const todayRange = todayOnly ? localDayRange() : { date_from: null, date_to: null };
  const inbox = useInfiniteQuery({
    queryKey: ["inbox", status, todayOnly],
    queryFn: async ({ pageParam }) => {
      recordDiagnosticEvent({ level: "info", component: "frontend.pagination", event_name: "inbox_page.requested", fields: { cursor_present: Boolean(pageParam), page_size: 100, status, today_only: todayOnly } });
      const page = await bridge.inbox_query({ status, event_types: [], root_ids: [], ...todayRange, cursor: pageParam, page_size: 100 });
      const advanced = !page.has_more || Boolean(page.next_cursor && page.next_cursor !== pageParam);
      recordDiagnosticEvent({ level: advanced ? "info" : "error", component: "frontend.pagination", event_name: "inbox_page.completed", fields: { returned_count: page.items.length, has_more: page.has_more, cursor_advanced: advanced } });
      if (!advanced) throw { code: "INBOX_CURSOR_INVALID", message: "收件箱分页游标没有推进，请刷新后重试。", retryable: false };
      return page;
    },
    initialPageParam: null as string | null,
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
  });
  const items = inbox.data?.pages.flatMap((page) => page.items) ?? [];
  const update = useMutation({
    mutationFn: ({ inboxId, nextStatus }: { inboxId: string; nextStatus: "reviewed" | "ignored" }) => bridge.inbox_update(inboxId, nextStatus),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["inbox"] }),
  });
  const retryProcessing = useMutation({
    mutationFn: (inboxId: string) => bridge.inbox_retry(inboxId),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["inbox"] }),
  });

  return (
    <section className="page">
      <header className="page-heading page-heading--inbox">
        <div className="inbox-tabs" role="tablist" aria-label="收件箱筛选">
          {tabs.map((tab) => <button type="button" role="tab" aria-selected={status === tab.value} className={status === tab.value ? "active" : ""} key={tab.value} onClick={() => setStatus(tab.value)}>{tab.label}</button>)}
        </div>
        <div className="page-heading__actions">
          {todayOnly && <button type="button" className="text-button" onClick={() => setTodayOnly(false)}>显示全部日期</button>}
          <button type="button" className="text-button" onClick={() => void inbox.refetch()} disabled={inbox.isFetching}><ReloadOutlined /> 刷新</button>
        </div>
      </header>
      {inbox.isLoading && <div className="page-empty"><p>正在读取本地收件箱…</p></div>}
      {inbox.isError && <div className="page-empty"><h2>收件箱暂时无法读取</h2><p>{errorMessage(inbox.error)}</p><button className="primary-button" type="button" onClick={() => void inbox.refetch()}>重试</button></div>}
      {(update.isError || retryProcessing.isError) && <p role="alert" className="inline-error">{errorMessage(update.error ?? retryProcessing.error)}</p>}
      {!inbox.isLoading && !inbox.isError && items.length === 0 && <div className="page-empty"><InboxOutlined /><h2>{status === "new" ? "没有待处理资料" : "这里还没有记录"}</h2><p>翻翻会把扫描中发现的变化和异常自动放到这里。</p></div>}
      <div className="inbox-list">
        {items.map((item) => (
          <article className="inbox-item" key={item.inbox_id}>
            <span className={`inbox-item__icon inbox-item__icon--${item.event_type}`}><InboxOutlined /></span>
            <div className="inbox-item__body">
              <h2>{eventLabels[item.event_type]} <strong>{item.display_name}</strong></h2>
              <p>{item.summary ?? "没有补充说明"}</p>
            <small>{displayPath(item.display_path)} · {new Date(item.observed_at).toLocaleString("zh-CN")}</small>
              <div className="inbox-item__tags">
                {item.duplicate_group_id && <span>发现完全重复项</span>}
                {item.suggested_collection_ids.length > 0 && <span>匹配 {item.suggested_collection_ids.length} 个集合</span>}
                {item.error_code && <span>{item.error_code}</span>}
                {item.resolution_status === "resolved" && <span>故障已解决</span>}
                {item.attempt_count > 0 && <span>已尝试 {item.attempt_count} 次</span>}
              </div>
              <InboxProcessingDetails item={item} />
            </div>
            <div className="inbox-item__actions">
              {item.retry_action && <button type="button" disabled={retryProcessing.isPending || item.resolution_status === "retrying"} onClick={() => retryProcessing.mutate(item.inbox_id)}><ReloadOutlined /> {item.resolution_status === "retrying" || (retryProcessing.isPending && retryProcessing.variables === item.inbox_id) ? "正在重试" : item.retry_action === "retry_ocr" ? "重试 OCR" : "重新处理"}</button>}
              {item.event_type === "collection_suggested" && <button type="button" onClick={() => navigate("collections")}>审核集合建议</button>}
              {item.event_type === "relation_suggested" && <button type="button" onClick={() => navigate("library")}>复核资料关系</button>}
              {item.triage_status === "new" ? <>
                <button type="button" disabled={update.isPending} onClick={() => update.mutate({ inboxId: item.inbox_id, nextStatus: "reviewed" })}><CheckOutlined /> {update.isPending && update.variables?.inboxId === item.inbox_id && update.variables.nextStatus === "reviewed" ? "正在更新" : "已查看"}</button>
                <button type="button" disabled={update.isPending} onClick={() => update.mutate({ inboxId: item.inbox_id, nextStatus: "ignored" })}>{update.isPending && update.variables?.inboxId === item.inbox_id && update.variables.nextStatus === "ignored" ? "正在忽略" : "忽略"}</button>
              </> : <span className="inbox-item__state">{item.triage_status === "reviewed" ? "已查看" : "已忽略"}</span>}
            </div>
          </article>
        ))}
      </div>
      {inbox.hasNextPage && <button type="button" className="load-more-button" disabled={inbox.isFetchingNextPage} onClick={() => void inbox.fetchNextPage()}>{inbox.isFetchingNextPage ? "正在加载" : "加载更多"}</button>}
    </section>
  );
}

function InboxProcessingDetails({ item }: { item: InboxItem }) {
  const [open, setOpen] = useState(false);
  const canInspect = Boolean(item.error_code || item.retry_action || item.event_type === "ocr_required" || item.event_type === "parse_failed");
  const preview = useQuery({
    queryKey: ["file-preview", item.file_id, "inbox-processing-chain"],
    queryFn: () => bridge.preview_get(item.file_id),
    enabled: open && canInspect,
    staleTime: 15_000,
  });
  if (!canInspect) return null;
  return (
    <div className="inbox-processing-details">
      <button type="button" className="text-button" aria-expanded={open} onClick={() => setOpen((value) => !value)}>
        {open ? "收起处理链" : "查看处理链"}
      </button>
      {open && (
        <div className="inbox-processing-details__content">
          {preview.isLoading && <small>正在读取本地处理记录…</small>}
          {preview.isError && <small role="alert">处理记录暂时无法读取：{errorMessage(preview.error)}</small>}
          {preview.data && preview.data.ocr_attempts.length > 0 && <OcrAttemptChain attempts={preview.data.ocr_attempts} />}
          {preview.data && preview.data.ocr_attempts.length === 0 && <small>暂无 OCR 尝试记录{item.error_code ? `，最近错误：${item.error_code}` : ""}</small>}
        </div>
      )}
    </div>
  );
}

function localDayRange() {
  const start = new Date();
  start.setHours(0, 0, 0, 0);
  const end = new Date(start);
  end.setDate(end.getDate() + 1);
  return { date_from: start.toISOString(), date_to: end.toISOString() };
}
