import { ApartmentOutlined, FolderAddOutlined, MoreOutlined, ReloadOutlined, SafetyCertificateOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type RelationGroupRole, type RelationGroupType, type RootRecord } from "../bridge";
import { confirmAction } from "../components/AppConfirm";
import { AppSelect } from "../components/AppSelect";
import { useAppStore } from "../state/app-store";
import { errorMessage } from "../utils/app-error";
import { displayPath } from "../utils/display-path";

const ROOT_STATUS_LABELS: Record<RootRecord["status"], string> = {
  discovering: "正在发现", ready: "就绪", scanning: "扫描中", partial_denied: "部分受限",
  permission_denied: "无权限", paused: "已暂停", offline: "离线", failed: "异常", removing: "正在移除",
};

const GROUP_TYPE_LABELS: Record<RelationGroupType, string> = {
  duplicate: "完全重复",
  version_family: "版本族",
  summary_group: "摘要与来源",
  topic_group: "同主题或同用途",
  mixed: "混合关系",
};

const MEMBER_ROLE_LABELS: Record<RelationGroupRole, string | null> = {
  latest: "最新版本", copy: "复制件", summary: "摘要", source: "来源", member: null,
};

export function LibraryPage() {
  const navigate = useAppStore((state) => state.navigate);
  const relationTask = useAppStore((state) => state.analysis_tasks.relation);
  const setAnalysisTask = useAppStore((state) => state.set_analysis_task);
  const queryClient = useQueryClient();
  const roots = useQuery({ queryKey: ["roots"], queryFn: () => bridge.root_list() });
  const summary = useQuery({
    queryKey: ["home-summary", new Date().toLocaleDateString("sv-SE")],
    queryFn: () => bridge.home_get_summary(new Date().toLocaleDateString("sv-SE")),
    refetchInterval: (query) => query.state.data?.scan_progress ? 1500 : 10_000,
  });
  const [groupType, setGroupType] = useState<RelationGroupType | "">("");
  const [groupReview, setGroupReview] = useState<"suggested" | "accepted" | "rejected" | "">("");
  const groups = useInfiniteQuery({
    queryKey: ["relation-groups", groupType, groupReview],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.relation_group_query({ cursor: pageParam, page_size: 50, group_type: groupType || null, review_status: groupReview || null }),
    getNextPageParam: (page) => page.next_cursor,
  });
  const groupItems = groups.data?.pages.flatMap((page) => page.items) ?? [];
  const groupTotal = groups.data?.pages[0]?.total ?? 0;
  const refreshRelations = useMutation({
    mutationFn: () => bridge.relation_refresh(5000),
    // 任务状态写入全局 store：切页后「正在分析」/完成反馈/错误仍能跨页保留
    onMutate: () => setAnalysisTask("relation", { status: "running", started_at: Date.now(), summary: null, error: null }),
    onSuccess: async (data) => {
      setAnalysisTask("relation", { status: "done", finished_at: Date.now(), summary: `本次发现 ${data.exact_duplicate_pairs} 组完全重复、${data.version_candidate_pairs} 组版本候选、${data.semantic_related_pairs} 组同主题/同用途关系、${data.contains_or_summarizes_pairs} 组包含/摘要关系，聚合出 ${data.groups_created} 组关系分组。` });
      await queryClient.invalidateQueries({ queryKey: ["relation-groups"] });
    },
    onError: (actionError) => setAnalysisTask("relation", { status: "error", finished_at: Date.now(), error: errorMessage(actionError) }),
  });
  const reviewGroup = useMutation({
    mutationFn: ({ groupId, action }: { groupId: string; action: "accepted" | "rejected" }) => bridge.relation_group_review(groupId, action),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["relation-groups"] }),
  });
  const [selectedGroups, setSelectedGroups] = useState<Set<string>>(new Set());
  const toggleGroup = (groupId: string) => setSelectedGroups((current) => { const next = new Set(current); if (next.has(groupId)) next.delete(groupId); else next.add(groupId); return next; });
  const batchReviewGroups = useMutation({
    mutationFn: (action: "accepted" | "rejected") => bridge.relation_group_batch_review([...selectedGroups], action),
    onSuccess: async (count, action) => {
      setMessage(`已${action === "accepted" ? "确认" : "排除"} ${count} 组文件关系。`);
      setSelectedGroups(new Set());
      await queryClient.invalidateQueries({ queryKey: ["relation-groups"] });
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

      <section className="relation-panel"><header><div><h2><ApartmentOutlined /> 资料关系分析</h2><p>同时分析完全重复、版本、同主题/同用途和包含或摘要关系；不会修改源文件。</p></div><button type="button" className="text-button" disabled={refreshRelations.isPending || relationTask.status === "running"} onClick={() => refreshRelations.mutate()}><ReloadOutlined /> {refreshRelations.isPending || relationTask.status === "running" ? "正在分析" : "重新分析"}</button></header>
        <div className="relation-filters">
          <AppSelect ariaLabel="分组类型" value={groupType} onChange={(value) => { setGroupType(value as RelationGroupType | ""); setSelectedGroups(new Set()); }} options={[{ value: "", label: "全部分组类型" }, ...(Object.keys(GROUP_TYPE_LABELS) as RelationGroupType[]).map((type) => ({ value: type, label: GROUP_TYPE_LABELS[type] }))]} />
          <AppSelect ariaLabel="分组复核状态" value={groupReview} onChange={(value) => { setGroupReview(value as typeof groupReview); setSelectedGroups(new Set()); }} options={[{ value: "", label: "待处理与已确认" }, { value: "suggested", label: "仅待处理" }, { value: "accepted", label: "仅已确认" }, { value: "rejected", label: "已排除" }]} />
          <button type="button" disabled={groupItems.length === 0} onClick={() => setSelectedGroups(new Set(groupItems.map((group) => group.group_id)))}>选择当前页</button>
          <button type="button" disabled={selectedGroups.size === 0 || batchReviewGroups.isPending} onClick={() => batchReviewGroups.mutate("accepted")}>批量确认</button>
          <button type="button" disabled={selectedGroups.size === 0 || batchReviewGroups.isPending} onClick={() => batchReviewGroups.mutate("rejected")}>批量排除</button>
        </div>
        {relationTask.status === "running" && <p className="relation-summary">AI 分析进行中…</p>}
        {relationTask.status === "done" && relationTask.finished_at !== null && relationTask.finished_at > Date.now() - 2 * 60_000 && relationTask.summary && <p className="relation-summary">{relationTask.summary}</p>}
        {relationTask.status === "error" && relationTask.finished_at !== null && relationTask.finished_at > Date.now() - 2 * 60_000 && relationTask.error && <p role="alert" className="inline-error">{relationTask.error}</p>}
        {groups.isLoading && <p>正在读取关系分组…</p>}
        {!groups.isLoading && groupItems.length === 0 && <div className="relation-empty"><p>还没有分析结果。配置 Embedding 后重新分析可发现语义关系；未配置时仍会检查重复和版本候选。</p></div>}
        <div className="relation-list">{groupItems.map((group) => <section className={`relation-card${group.review_status === "accepted" ? " relation-card--accepted" : group.review_status === "rejected" ? " relation-card--rejected" : ""}`} key={group.group_id}><header><label className="relation-select"><input type="checkbox" aria-label={`选择分组“${group.title}”`} checked={selectedGroups.has(group.group_id)} onChange={() => toggleGroup(group.group_id)} /></label><div className="relation-card__title"><strong>{group.title}</strong><span className="relation-card__type">{GROUP_TYPE_LABELS[group.group_type]}</span>{group.review_status === "accepted" ? <em className="relation-card__status relation-card__status--accepted">已确认</em> : group.review_status === "rejected" ? <em className="relation-card__status relation-card__status--rejected">已排除</em> : <em className="relation-card__status relation-card__status--pending">待处理</em>}</div><small className="relation-card__confidence">置信度 {Math.round(group.confidence * 100)}% · {group.member_count} 个文件</small><div className="relation-actions"><button type="button" disabled={reviewGroup.isPending || group.review_status === "accepted"} onClick={() => reviewGroup.mutate({ groupId: group.group_id, action: "accepted" })}>确认</button><button type="button" disabled={reviewGroup.isPending || group.review_status === "rejected"} onClick={() => reviewGroup.mutate({ groupId: group.group_id, action: "rejected" })}>排除</button></div></header><ul className="relation-card__members">{group.members.map((member) => <li key={member.file_id} className="relation-member"><div><strong>{member.file.display_name}</strong><small>{displayPath(member.file.display_path)}</small></div>{MEMBER_ROLE_LABELS[member.role] && <em className={`relation-member__role relation-member__role--${member.role}`}>{MEMBER_ROLE_LABELS[member.role]}</em>}</li>)}</ul></section>)}</div>
        {groups.hasNextPage && <button type="button" className="load-more-button" disabled={groups.isFetchingNextPage} onClick={() => void groups.fetchNextPage()}>{groups.isFetchingNextPage ? "正在加载" : `加载更多分组（还剩 ${Math.max(0, groupTotal - groupItems.length)} 组）`}</button>}
      </section>
    </section>
  );
}
