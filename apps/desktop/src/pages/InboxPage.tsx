import { CheckOutlined, InboxOutlined, ReloadOutlined } from "@ant-design/icons";
import { useInfiniteQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type InboxItem, type InboxQuery } from "../bridge";
import { useAppStore } from "../state/app-store";
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
};

export function InboxPage() {
  const initialStatus = useAppStore((state) => state.inbox_initial_status);
  const initialTodayOnly = useAppStore((state) => state.inbox_today_only);
  const [status, setStatus] = useState<InboxTab>(initialStatus);
  const [todayOnly, setTodayOnly] = useState(initialTodayOnly);
  const queryClient = useQueryClient();
  const todayRange = todayOnly ? localDayRange() : { date_from: null, date_to: null };
  const inbox = useInfiniteQuery({
    queryKey: ["inbox", status, todayOnly],
    queryFn: ({ pageParam }) => bridge.inbox_query({ status, event_types: [], root_ids: [], ...todayRange, cursor: pageParam, page_size: 100 }),
    initialPageParam: null as string | null,
    getNextPageParam: (lastPage) => lastPage.next_cursor ?? undefined,
  });
  const items = inbox.data?.pages.flatMap((page) => page.items) ?? [];
  const update = useMutation({
    mutationFn: ({ inboxId, nextStatus }: { inboxId: string; nextStatus: "reviewed" | "ignored" }) => bridge.inbox_update(inboxId, nextStatus),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["inbox"] }),
  });
  const retryOcr = useMutation({
    mutationFn: (fileId: string) => bridge.ocr_retry(fileId),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["inbox"] }),
  });

  return (
    <section className="page">
      <header className="page-heading">
        <div><h1>收件箱</h1><p>{todayOnly ? "正在显示今天发现的事项；源文件不会被改动。" : "新增、修改、OCR 和处理异常集中在这里，源文件不会被改动。"}</p></div>
        <div className="page-heading__actions">
          {todayOnly && <button type="button" className="text-button" onClick={() => setTodayOnly(false)}>显示全部日期</button>}
          <button type="button" className="text-button" onClick={() => void inbox.refetch()} disabled={inbox.isFetching}><ReloadOutlined /> 刷新</button>
        </div>
      </header>
      <div className="inbox-tabs" role="tablist" aria-label="收件箱筛选">
        {tabs.map((tab) => <button type="button" role="tab" aria-selected={status === tab.value} className={status === tab.value ? "active" : ""} key={tab.value} onClick={() => setStatus(tab.value)}>{tab.label}</button>)}
      </div>
      {inbox.isLoading && <div className="page-empty"><p>正在读取本地收件箱…</p></div>}
      {inbox.isError && <div className="page-empty"><h2>收件箱暂时无法读取</h2><p>{inbox.error instanceof Error ? inbox.error.message : String(inbox.error)}</p><button className="primary-button" type="button" onClick={() => void inbox.refetch()}>重试</button></div>}
      {retryOcr.isError && <p role="alert" className="inline-error">{retryOcr.error instanceof Error ? retryOcr.error.message : String(retryOcr.error)}</p>}
      {!inbox.isLoading && !inbox.isError && items.length === 0 && <div className="page-empty"><InboxOutlined /><h2>{status === "new" ? "没有待处理资料" : "这里还没有记录"}</h2><p>拾忆会把扫描中发现的变化和异常自动放到这里。</p></div>}
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
              </div>
            </div>
            {item.triage_status === "new" || item.triage_status === "error" ? <div className="inbox-item__actions">
              {item.event_type === "ocr_required" && <button type="button" disabled={retryOcr.isPending} onClick={() => retryOcr.mutate(item.file_id)}><ReloadOutlined /> {retryOcr.isPending && retryOcr.variables === item.file_id ? "正在重试" : "重试 OCR"}</button>}
              <button type="button" disabled={update.isPending} onClick={() => update.mutate({ inboxId: item.inbox_id, nextStatus: "reviewed" })}><CheckOutlined /> 已查看</button>
              <button type="button" disabled={update.isPending} onClick={() => update.mutate({ inboxId: item.inbox_id, nextStatus: "ignored" })}>忽略</button>
            </div> : <span className="inbox-item__state">{item.triage_status === "reviewed" ? "已查看" : "已忽略"}</span>}
          </article>
        ))}
      </div>
      {inbox.hasNextPage && <button type="button" className="load-more-button" disabled={inbox.isFetchingNextPage} onClick={() => void inbox.fetchNextPage()}>{inbox.isFetchingNextPage ? "正在加载" : "加载更多"}</button>}
    </section>
  );
}

function localDayRange() {
  const start = new Date();
  start.setHours(0, 0, 0, 0);
  const end = new Date(start);
  end.setDate(end.getDate() + 1);
  return { date_from: start.toISOString(), date_to: end.toISOString() };
}
