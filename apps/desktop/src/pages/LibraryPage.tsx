import { ApartmentOutlined, FileOutlined, FolderAddOutlined, MoreOutlined, ReloadOutlined, SafetyCertificateOutlined, SearchOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { useMemo, useRef, useState } from "react";
import { bridge, type RelationType, type RootRecord } from "../bridge";
import { recordDiagnosticEvent } from "../bridge/observed-bridge";
import { confirmAction } from "../components/AppConfirm";
import { AppSelect } from "../components/AppSelect";
import { useAppStore } from "../state/app-store";
import { errorMessage } from "../utils/app-error";
import { displayPath } from "../utils/display-path";

const ROOT_STATUS_LABELS: Record<RootRecord["status"], string> = {
  discovering: "正在发现", ready: "就绪", scanning: "扫描中", partial_denied: "部分受限",
  permission_denied: "无权限", paused: "已暂停", offline: "离线", failed: "异常", removing: "正在移除",
};

export function LibraryPage() {
  const navigate = useAppStore((state) => state.navigate);
  const queryClient = useQueryClient();
  const roots = useQuery({ queryKey: ["roots"], queryFn: () => bridge.root_list() });
  const summary = useQuery({
    queryKey: ["home-summary", new Date().toLocaleDateString("sv-SE")],
    queryFn: () => bridge.home_get_summary(new Date().toLocaleDateString("sv-SE")),
    refetchInterval: (query) => query.state.data?.scan_progress ? 1500 : 10_000,
  });
  const [fileQuery, setFileQuery] = useState("");
  const [fileExtension, setFileExtension] = useState("");
  const [fileStatus, setFileStatus] = useState("");
  const files = useInfiniteQuery({
    queryKey: ["files", fileQuery.trim(), fileExtension, fileStatus],
    initialPageParam: null as string | null,
    queryFn: async ({ pageParam }) => {
      recordDiagnosticEvent({ level: "info", component: "frontend.pagination", event_name: "file_page.requested", fields: { cursor_present: Boolean(pageParam), page_size: 100, filtered: Boolean(fileQuery.trim() || fileExtension || fileStatus) } });
      const page = await bridge.file_query({ cursor: pageParam, page_size: 100, query: fileQuery.trim() || null, extensions: fileExtension ? [fileExtension] : [], parse_statuses: fileStatus ? [fileStatus] : [], availability: "present" });
      const advanced = !page.has_more || Boolean(page.next_cursor && page.next_cursor !== pageParam);
      recordDiagnosticEvent({ level: advanced ? "info" : "error", component: "frontend.pagination", event_name: "file_page.completed", fields: { returned_count: page.items.length, has_more: page.has_more, cursor_advanced: advanced } });
      if (!advanced) throw { code: "FILE_CURSOR_INVALID", message: "资料分页游标没有推进，请刷新后重试。", retryable: false };
      return page;
    },
    getNextPageParam: (page) => page.next_cursor,
  });
  const fileItems = useMemo(() => files.data?.pages.flatMap((page) => page.items) ?? [], [files.data]);
  const fileTotal = files.data?.pages[0]?.total ?? null;
  const fileListRef = useRef<HTMLDivElement>(null);
  const fileVirtualizer = useVirtualizer({
    count: fileItems.length,
    getScrollElement: () => fileListRef.current,
    estimateSize: () => 64,
    overscan: 10,
  });
  const [relationType, setRelationType] = useState<RelationType | "">("");
  const [relationReview, setRelationReview] = useState<"suggested" | "accepted" | "rejected" | "">("");
  const relations = useInfiniteQuery({
    queryKey: ["file-relations", relationType, relationReview],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.relation_query({ cursor: pageParam, page_size: 100, relation_type: relationType || null, review_status: relationReview || null }),
    getNextPageParam: (page) => page.next_cursor,
  });
  const relationItems = relations.data?.pages.flatMap((page) => page.items) ?? [];
  const relationTotal = relations.data?.pages[0]?.total ?? 0;
  const relationGroups = useMemo(() => (["exact_duplicate", "version_candidate", "semantic_related", "contains_or_summarizes", "related"] as RelationType[]).map((type) => ({ type, items: relationItems.filter((relation) => relation.relation_type === type) })).filter((group) => group.items.length > 0), [relationItems]);
  const refreshRelations = useMutation({ mutationFn: () => bridge.relation_refresh(5000), onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["file-relations"] }) });
  const reviewRelation = useMutation({
    mutationFn: ({ relationId, action }: { relationId: string; action: "accepted" | "rejected" }) => bridge.relation_review(relationId, action),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["file-relations"] }),
  });
  const [selectedRelations, setSelectedRelations] = useState<Set<string>>(new Set());
  const batchReviewRelations = useMutation({
    mutationFn: (action: "accepted" | "rejected") => bridge.relation_batch_review([...selectedRelations], action),
    onSuccess: async (count, action) => {
      setMessage(`已${action === "accepted" ? "确认" : "排除"} ${count} 条文件关系。`);
      setSelectedRelations(new Set());
      await queryClient.invalidateQueries({ queryKey: ["file-relations"] });
    },
    onError: (actionError) => setError(errorMessage(actionError)),
  });
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [menuRootId, setMenuRootId] = useState<string | null>(null);
  const [actionRootId, setActionRootId] = useState<string | null>(null);

  const addRoot = async () => {
    setError(null);
    if (!isTauri()) { setError("浏览器预览不调用系统目录选择器，请在翻翻桌面程序中添加资料位置。"); return; }
    setAdding(true);
    try {
      const selectedPath = await open({ directory: true, multiple: false, title: "添加资料位置" });
      if (typeof selectedPath !== "string") return;
      const fullVolume = /^[a-zA-Z]:\\?$/.test(selectedPath);
      if (fullVolume && !await confirmAction({ actionKey: "library_add_full_volume", title: "添加整个磁盘？", description: "扫描可能耗时较长，并会自动排除系统、程序、凭据和翻翻自身目录。", confirmLabel: "确认添加" })) return;
      await bridge.root_add({ path: selectedPath, label: null, watch_mode: "realtime", authorization_source: "user_selected", full_volume_confirmed: fullVolume });
      await roots.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setAdding(false); }
  };

  const rescanRoot = async (root: RootRecord) => {
    setMenuRootId(null); setError(null); setMessage(null);
    setActionRootId(root.root_id);
    try {
      await bridge.scan_start(root.root_id, "user_requested");
      setMessage(`已开始重新扫描“${root.label}”，进度会在状态列实时更新。`);
      await Promise.all([roots.refetch(), summary.refetch()]);
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setActionRootId(null); }
  };

  const removeRoot = async (root: RootRecord) => {
    if (!await confirmAction({ actionKey: "library_remove_root", title: `从翻翻移除“${root.label}”？`, description: "翻翻会立即停止读取并撤销该位置的授权，派生索引在后台清理；不会删除、移动或修改任何源文件。", confirmLabel: "从翻翻移除", danger: true })) return;
    setMenuRootId(null); setError(null); setMessage(null);
    setActionRootId(root.root_id);
    try {
      await bridge.root_disable(root.root_id);
      setMessage(`已从翻翻移除“${root.label}”。原文件没有变化；以后仍可重新添加。`);
      await roots.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setActionRootId(null); }
  };

  const openFile = async (fileId: string) => {
    setError(null);
    try { await bridge.file_open(fileId); }
    catch (actionError) { setError(errorMessage(actionError)); }
  };

  const loadMoreFiles = async () => {
    const previousCount = fileItems.length;
    setError(null);
    try {
      const result = await files.fetchNextPage();
      const loadedCount = (result.data?.pages.flatMap((page) => page.items).length ?? previousCount) - previousCount;
      setMessage(loadedCount > 0 ? `已加载 ${loadedCount} 份资料。` : "没有更多资料可加载。");
      if (loadedCount > 0) requestAnimationFrame(() => fileVirtualizer.scrollToIndex(previousCount, { align: "start" }));
    } catch (actionError) {
      setError(errorMessage(actionError));
    }
  };

  return (
    <section className="page">
      <header className="page-heading page-heading--inline-note page-heading--divider">
        <div className="readonly-note"><SafetyCertificateOutlined /> 翻翻只读取资料，不移动、重命名、删除或覆盖源文件</div>
        <button type="button" className="primary-button" disabled={adding} onClick={() => void addRoot()}><FolderAddOutlined /> {adding ? "正在添加" : "添加资料位置"}</button>
      </header>
      {error && <p role="alert" className="inline-error">{error}</p>}{message && <p className="inline-success">{message}</p>}
      <div className="root-table">
        <div className="root-table__head"><span>资料位置</span><span>状态</span><span>文件</span><span>最近扫描</span><span /></div>
        {roots.data?.map((root) => { const scan = summary.data?.scan_progress; const scanning = root.status === "scanning" && scan; return <div className="root-table__row" key={root.root_id}><span><strong>{root.label}</strong><small>{root.path}</small></span><span className="root-status-cell"><span><i className={`status-dot status-dot--${root.status}`} />{root.status === "scanning" ? "扫描中" : ROOT_STATUS_LABELS[root.status] ?? root.status}</span>{scanning && <><small>{Math.round(scan.progress * 100)}% · 已解析 {scan.parsed_files} 个</small><span className="root-progress"><i style={{ width: `${Math.round(scan.progress * 100)}%` }} /></span></>}</span><span>{root.file_count}</span><span>{root.last_scan_at ? new Date(root.last_scan_at).toLocaleString("zh-CN") : "—"}</span><span className="root-menu-wrap"><button type="button" aria-label={`操作${root.label}`} title="操作" onClick={() => setMenuRootId((current) => current === root.root_id ? null : root.root_id)}><MoreOutlined /></button>{menuRootId === root.root_id && <div className="root-menu" role="menu"><button type="button" role="menuitem" disabled={actionRootId !== null} onClick={() => void rescanRoot(root)}>{actionRootId === root.root_id ? "正在扫描…" : "重新扫描"}</button><button type="button" role="menuitem" disabled={actionRootId !== null} onClick={() => void removeRoot(root)}>{actionRootId === root.root_id ? "正在撤销授权…" : "从翻翻移除"}</button><button type="button" role="menuitem" onClick={() => { setMenuRootId(null); navigate("settings"); }}>前往设置管理</button></div>}</span></div>; })}
        {menuRootId && <div className="menu-backdrop" onClick={() => setMenuRootId(null)} />}
      </div>

      <section className="library-files">
        <header><div><h2><FileOutlined /> 资料文件</h2><p>这里用于定位和管理已入库文件，不搜索正文；正文和内容含义请到“找资料”。</p></div><span>{fileTotal == null ? `已加载 ${fileItems.length} 项` : `已显示 ${fileItems.length} / ${fileTotal}`}</span></header>
        <div className="library-file-filters">
          <label><SearchOutlined /><input aria-label="筛选当前资料列表" value={fileQuery} onChange={(event) => setFileQuery(event.target.value)} placeholder="筛选当前资料列表：文件名、显示路径" /></label>
          <AppSelect ariaLabel="按文件类型筛选" value={fileExtension} onChange={setFileExtension} options={[{ value: "", label: "全部类型" }, { value: "pdf", label: "PDF" }, { value: "docx", label: "Word" }, { value: "xlsx", label: "Excel" }, { value: "pptx", label: "PPT" }, { value: "txt", label: "文本" }, { value: "md", label: "Markdown" }, { value: "png", label: "PNG 图片" }, { value: "jpg", label: "JPG 图片" }, { value: "zip", label: "ZIP" }]} />
          <AppSelect ariaLabel="按索引状态筛选" value={fileStatus} onChange={setFileStatus} options={[{ value: "", label: "全部状态" }, { value: "parsed", label: "已索引" }, { value: "pending", label: "等待处理" }, { value: "parsing", label: "处理中" }, { value: "ocr_pending", label: "等待图片识别" }, { value: "unsupported", label: "仅元数据" }, { value: "encrypted", label: "已加密" }, { value: "failed", label: "处理失败" }]} />
        </div>
        {files.isLoading && <p>正在读取资料文件…</p>}
        {files.isError && <p role="alert" className="inline-error">{errorMessage(files.error)}</p>}
        {!files.isLoading && fileItems.length === 0 && <div className="relation-empty"><p>当前筛选条件下没有资料文件。</p></div>}
        {fileItems.length > 0 && <div ref={fileListRef} className="file-browser--virtual"><div style={{ height: `${fileVirtualizer.getTotalSize()}px`, position: "relative" }}>
          {fileVirtualizer.getVirtualItems().map((virtualRow) => { const file = fileItems[virtualRow.index]!; const stateLabel = file.parse_status === "parsed" ? "已索引" : file.parse_status === "parsing" ? "处理中" : file.parse_status === "ocr_pending" ? "等待图片识别" : file.parse_status === "unsupported" ? "仅元数据" : file.parse_status === "encrypted" ? "已加密" : file.parse_status === "failed" ? "处理失败" : "等待处理"; return <article key={file.file_id} style={{ position: "absolute", transform: `translateY(${virtualRow.start}px)`, width: "100%", height: `${virtualRow.size}px` }}><FileOutlined /><div><strong>{file.display_name}</strong><small>{displayPath(file.display_path)}</small></div><span>{file.extension ? file.extension.toUpperCase() : "文件"}</span><em>{stateLabel}</em><button type="button" onClick={() => void openFile(file.file_id)}>打开原文件</button></article>; })}
        </div></div>}
        {files.hasNextPage && <button type="button" className="load-more-button" disabled={files.isFetchingNextPage} onClick={() => void loadMoreFiles()}>{files.isFetchingNextPage ? "正在加载" : fileTotal == null ? "加载更多资料" : `加载更多（还剩 ${Math.max(0, fileTotal - fileItems.length)} 项）`}</button>}
      </section>

      <section className="relation-panel"><header><div><h2><ApartmentOutlined /> 资料关系分析</h2><p>同时分析完全重复、版本、同主题/同用途和包含或摘要关系；不会修改源文件。</p></div><button type="button" className="text-button" disabled={refreshRelations.isPending} onClick={() => refreshRelations.mutate()}><ReloadOutlined /> {refreshRelations.isPending ? "正在分析" : "重新分析"}</button></header>
        <div className="relation-filters">
          <AppSelect ariaLabel="关系类型" value={relationType} onChange={(value) => { setRelationType(value as RelationType | ""); setSelectedRelations(new Set()); }} options={[{ value: "", label: "全部关系类型" }, { value: "exact_duplicate", label: "完全重复" }, { value: "version_candidate", label: "版本候选" }, { value: "semantic_related", label: "同主题或同用途" }, { value: "contains_or_summarizes", label: "包含、摘要或派生" }, { value: "related", label: "历史内容关系" }]} />
          <AppSelect ariaLabel="关系复核状态" value={relationReview} onChange={(value) => { setRelationReview(value as typeof relationReview); setSelectedRelations(new Set()); }} options={[{ value: "", label: "待处理与已确认" }, { value: "suggested", label: "仅待处理" }, { value: "accepted", label: "仅已确认" }, { value: "rejected", label: "已排除" }]} />
          <button type="button" disabled={relationItems.length === 0} onClick={() => setSelectedRelations(new Set(relationItems.map((relation) => relation.relation_id)))}>选择当前页</button>
          <button type="button" disabled={selectedRelations.size === 0 || batchReviewRelations.isPending} onClick={() => batchReviewRelations.mutate("accepted")}>批量确认</button>
          <button type="button" disabled={selectedRelations.size === 0 || batchReviewRelations.isPending} onClick={() => batchReviewRelations.mutate("rejected")}>批量排除</button>
        </div>
        {refreshRelations.data && <p className="relation-summary">本次发现 {refreshRelations.data.exact_duplicate_pairs} 组完全重复、{refreshRelations.data.version_candidate_pairs} 组版本候选、{refreshRelations.data.semantic_related_pairs} 组同主题/同用途关系、{refreshRelations.data.contains_or_summarizes_pairs} 组包含/摘要关系。</p>}
        {refreshRelations.isError && <p role="alert" className="inline-error">{errorMessage(refreshRelations.error)}</p>}
        {relations.isLoading && <p>正在读取文件关系…</p>}
        {!relations.isLoading && relationItems.length === 0 && <div className="relation-empty"><p>还没有分析结果。配置 Embedding 后重新分析可发现语义关系；未配置时仍会检查重复和版本候选。</p></div>}
        <div className="relation-list">{relationGroups.map((group) => <section className="relation-group" key={group.type}><h3>{group.type === "exact_duplicate" ? "完全重复" : group.type === "version_candidate" ? "版本候选" : group.type === "contains_or_summarizes" ? "包含、摘要或派生" : group.type === "semantic_related" ? "同主题或同用途" : "历史内容关系"}<small>{group.items.length} 条已加载</small></h3>{group.items.map((relation) => <article key={relation.relation_id}><label className="relation-select"><input type="checkbox" aria-label={`选择${relation.left_file.display_name}与${relation.right_file.display_name}的关系`} checked={selectedRelations.has(relation.relation_id)} onChange={() => setSelectedRelations((current) => { const next = new Set(current); if (next.has(relation.relation_id)) next.delete(relation.relation_id); else next.add(relation.relation_id); return next; })} /></label><div><strong>{relation.left_file.display_name}</strong><small>{displayPath(relation.left_file.display_path)}</small></div><i>{relation.relation_type === "contains_or_summarizes" ? "→" : "↔"}</i><div><strong>{relation.right_file.display_name}</strong><small>{displayPath(relation.right_file.display_path)}</small></div><em>{relation.review_status === "accepted" ? "已确认" : relation.review_status === "rejected" ? "已排除" : `${Math.round(relation.confidence * 100)}%`}</em><p className="relation-reasons">{relation.reasons.join("；")}</p><div className="relation-actions"><button type="button" disabled={reviewRelation.isPending || relation.review_status === "accepted"} onClick={() => reviewRelation.mutate({ relationId: relation.relation_id, action: "accepted" })}>确认</button><button type="button" disabled={reviewRelation.isPending || relation.review_status === "rejected"} onClick={() => reviewRelation.mutate({ relationId: relation.relation_id, action: "rejected" })}>排除</button></div></article>)}</section>)}</div>
        {relations.hasNextPage && <button type="button" className="load-more-button" disabled={relations.isFetchingNextPage} onClick={() => void relations.fetchNextPage()}>{relations.isFetchingNextPage ? "正在加载" : `加载更多关系（还剩 ${Math.max(0, relationTotal - relationItems.length)} 项）`}</button>}
      </section>
    </section>
  );
}
