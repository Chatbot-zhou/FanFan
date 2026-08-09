import { AppstoreOutlined, CalendarOutlined, CloseOutlined, FileDoneOutlined, FolderAddOutlined, PlusOutlined } from "@ant-design/icons";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { FormEvent, useState } from "react";
import { bridge, type CollectionKind, type CollectionRule, type CreateCollectionRequest } from "../bridge";
import { useAppStore } from "../state/app-store";
import { displayPath } from "../utils/display-path";

export function CollectionsPage() {
  const queryClient = useQueryClient();
  const initialCollectionId = useAppStore((state) => state.selected_collection_id);
  const clearCollectionSelection = useAppStore((state) => state.clear_collection_selection);
  const [creating, setCreating] = useState(false);
  const [editingId, setEditingId] = useState<string | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(initialCollectionId);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [kind, setKind] = useState<CollectionKind>("manual");
  const [extensions, setExtensions] = useState("");
  const [keywords, setKeywords] = useState("");
  const [recentDays, setRecentDays] = useState("");
  const [addFileId, setAddFileId] = useState("");
  const [previewCount, setPreviewCount] = useState<number | null>(null);
  const collections = useQuery({ queryKey: ["collections"], queryFn: () => bridge.collection_list() });
  const selectedCollection = collections.data?.find((item) => item.collection_id === selectedId) ?? null;
  const files = useInfiniteQuery({
    queryKey: ["collection-files", selectedId],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.collection_file_query(selectedId!, { cursor: pageParam, page_size: 100 }),
    getNextPageParam: (page) => page.next_cursor,
    enabled: selectedId !== null,
  });
  const collectionItems = files.data?.pages.flatMap((page) => page.items) ?? [];
  const allFiles = useQuery({ queryKey: ["collection-file-picker"], queryFn: () => bridge.file_query({ cursor: null, page_size: 200 }), enabled: selectedCollection?.kind === "manual" });
  const create = useMutation({
    mutationFn: (request: CreateCollectionRequest) => bridge.collection_create(request),
    onSuccess: async (collection) => {
      setCreating(false);
      setSelectedId(collection.collection_id);
      setName(""); setDescription(""); setExtensions(""); setKeywords(""); setRecentDays("");
      await queryClient.invalidateQueries({ queryKey: ["collections"] });
    },
  });
  const update = useMutation({
    mutationFn: (request: CreateCollectionRequest) => bridge.collection_update(editingId!, request),
    onSuccess: async (collection) => {
      setCreating(false); setEditingId(null); setSelectedId(collection.collection_id);
      setName(""); setDescription(""); setExtensions(""); setKeywords(""); setRecentDays("");
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["collections"] }),
        queryClient.invalidateQueries({ queryKey: ["collection-files", collection.collection_id] }),
      ]);
    },
  });
  const addFile = useMutation({
    mutationFn: () => bridge.collection_add_file(selectedId!, addFileId),
    onSuccess: async () => {
      setAddFileId("");
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["collection-files", selectedId] }),
        queryClient.invalidateQueries({ queryKey: ["collections"] }),
      ]);
    },
  });
  const removeFile = useMutation({
    mutationFn: (fileId: string) => bridge.collection_remove_file(selectedId!, fileId),
    onSuccess: async () => Promise.all([
      queryClient.invalidateQueries({ queryKey: ["collection-files", selectedId] }),
      queryClient.invalidateQueries({ queryKey: ["collections"] }),
    ]),
  });
  const deleteCollection = useMutation({
    mutationFn: () => bridge.collection_delete(selectedId!),
    onSuccess: async () => {
      setSelectedId(null);
      await queryClient.invalidateQueries({ queryKey: ["collections"] });
    },
  });

  const buildRule = (): CollectionRule => ({
    operator: "all",
    extensions: extensions.split(/[，,\s]+/).map((value) => value.trim().replace(/^\./, "")).filter(Boolean),
    filename_keywords: keywords.split(/[，,]+/).map((value) => value.trim()).filter(Boolean),
    path_keywords: [],
    text_keywords: [],
    parse_statuses: [],
    modified_within_days: recentDays ? Number(recentDays) : null,
  });
  const previewRule = useMutation({
    mutationFn: () => bridge.collection_rule_preview(buildRule(), 100),
    onSuccess: (matched) => setPreviewCount(matched.length),
  });

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const request: CreateCollectionRequest = {
      name,
      description: description || null,
      icon: kind === "manual" ? "folder" : "sparkles",
      color: kind === "manual" ? "#71a7ca" : "#8c7cf0",
      kind,
      rule: kind === "rule" ? buildRule() : null,
    };
    if (editingId) update.mutate(request); else create.mutate(request);
  };

  const beginEdit = () => {
    if (!selectedCollection || selectedCollection.built_in) return;
    setEditingId(selectedCollection.collection_id); setCreating(true);
    setName(selectedCollection.name); setDescription(selectedCollection.description ?? "");
    setKind(selectedCollection.kind);
    setExtensions(selectedCollection.rule?.extensions.join(", ") ?? "");
    setKeywords(selectedCollection.rule?.filename_keywords.join(", ") ?? "");
    setRecentDays(selectedCollection.rule?.modified_within_days?.toString() ?? "");
    setPreviewCount(null);
  };

  const closeEditor = () => {
    setCreating(false); setEditingId(null); setName(""); setDescription("");
    setExtensions(""); setKeywords(""); setRecentDays(""); setPreviewCount(null);
  };

  return (
    <section className="page">
      <header className="page-heading">
        <div><h1>智能集合</h1><p>集合是动态视图，不会移动电脑中的原文件。</p></div>
        <button type="button" className="primary-button" onClick={() => { if (creating) closeEditor(); else { setEditingId(null); setCreating(true); } }}>{creating ? <CloseOutlined /> : <PlusOutlined />} {creating ? "取消" : "新建集合"}</button>
      </header>
      {creating && <form className="collection-create" onSubmit={submit}>
        <h2>{editingId ? "编辑集合" : "新建集合"}</h2>
        <label>集合名称<input required maxLength={40} value={name} onChange={(event) => setName(event.target.value)} placeholder="例如：求职资料" /></label>
        <label>说明<input maxLength={120} value={description} onChange={(event) => setDescription(event.target.value)} placeholder="这个集合用来收集什么" /></label>
        <fieldset><legend>集合方式</legend><label><input type="radio" checked={kind === "manual"} onChange={() => setKind("manual")} /> 手动添加</label><label><input type="radio" checked={kind === "rule"} onChange={() => setKind("rule")} /> 按规则自动收集</label></fieldset>
        {kind === "rule" && <div className="collection-create__rules"><label>文件扩展名<input value={extensions} onChange={(event) => { setExtensions(event.target.value); setPreviewCount(null); }} placeholder="pdf, docx, xlsx" /></label><label>文件名关键词<input value={keywords} onChange={(event) => { setKeywords(event.target.value); setPreviewCount(null); }} placeholder="合同, 项目" /></label><label>最近修改天数<input type="number" min={1} max={3650} value={recentDays} onChange={(event) => { setRecentDays(event.target.value); setPreviewCount(null); }} placeholder="例如 30" /></label><button type="button" disabled={previewRule.isPending || (!extensions.trim() && !keywords.trim() && !recentDays)} onClick={() => previewRule.mutate()}>{previewRule.isPending ? "正在预览" : "预览匹配"}</button>{previewCount !== null && <small>当前规则至少匹配 {previewCount} 项{previewCount === 100 ? "（仅显示前100项）" : ""}</small>}</div>}
        {(create.isError || update.isError) && <p role="alert" className="inline-error">{(create.error ?? update.error) instanceof Error ? (create.error ?? update.error as Error).message : String(create.error ?? update.error)}</p>}
        <button type="submit" className="primary-button" disabled={create.isPending || update.isPending || !name.trim() || (kind === "rule" && !extensions.trim() && !keywords.trim() && !recentDays)}>{create.isPending || update.isPending ? "正在保存" : editingId ? "保存修改" : "创建集合"}</button>
      </form>}
      {collections.isLoading && <div className="page-empty"><p>正在读取智能集合…</p></div>}
      {collections.isError && <div className="page-empty"><h2>智能集合暂时无法读取</h2><button className="primary-button" type="button" onClick={() => void collections.refetch()}>重试</button></div>}
      <div className="collection-grid">
        {collections.data?.map((collection) => (
          <button type="button" className={`collection-card${selectedId === collection.collection_id ? " selected" : ""}`} key={collection.collection_id} onClick={() => setSelectedId(collection.collection_id)}>
            <span className="collection-card__icon" style={{ color: collection.color }}>{collection.icon === "calendar" ? <CalendarOutlined /> : collection.icon === "pending" ? <FileDoneOutlined /> : <AppstoreOutlined />}</span>
            <h2>{collection.name}{collection.built_in && <small>内置</small>}</h2><p>{collection.description ?? (collection.kind === "manual" ? "手动收集的资料" : "按规则自动更新")}</p><strong>{collection.file_count} 项</strong>
          </button>
        ))}
      </div>
      {selectedId && <section className="collection-detail">
        <header><h2>{selectedCollection?.name}</h2><div>{selectedCollection && !selectedCollection.built_in && <><button className="text-button" type="button" onClick={beginEdit}>编辑集合</button><button className="danger-button" type="button" disabled={deleteCollection.isPending} onClick={() => { if (window.confirm(`删除集合“${selectedCollection.name}”？原文件不会受到影响。`)) deleteCollection.mutate(); }}>删除集合</button></>}<button className="text-button" type="button" onClick={() => { setSelectedId(null); setAddFileId(""); clearCollectionSelection(); }}>关闭</button></div></header>
        {selectedCollection?.kind === "manual" && <div className="collection-add-file">
          <label><FolderAddOutlined /> 添加资料
            <select value={addFileId} onChange={(event) => setAddFileId(event.target.value)}>
              <option value="">选择已索引资料</option>
          {allFiles.data?.items.filter((file) => file.parse_status === "parsed" && !collectionItems.some((current) => current.file_id === file.file_id)).map((file) => <option key={file.file_id} value={file.file_id}>{file.display_name} · {displayPath(file.display_path)}</option>)}
            </select>
          </label>
          <button type="button" className="primary-button" disabled={!addFileId || addFile.isPending} onClick={() => addFile.mutate()}>{addFile.isPending ? "正在添加" : "添加到集合"}</button>
          {addFile.isError && <p role="alert" className="inline-error">{addFile.error instanceof Error ? addFile.error.message : String(addFile.error)}</p>}
        </div>}
        {files.isLoading && <p>正在计算集合内容…</p>}
        {collectionItems.length === 0 && <p>这个集合当前没有资料。</p>}
          {collectionItems.map((file) => <div key={file.file_id}><button type="button" onClick={() => void bridge.file_open(file.file_id)}><strong>{file.display_name}</strong><small>{displayPath(file.display_path)}</small></button>{selectedCollection?.kind === "manual" && <button type="button" className="text-button" disabled={removeFile.isPending} onClick={() => removeFile.mutate(file.file_id)}>移出集合</button>}</div>)}
        {files.hasNextPage && <button type="button" className="load-more-button" disabled={files.isFetchingNextPage} onClick={() => void files.fetchNextPage()}>{files.isFetchingNextPage ? "正在加载" : "加载更多"}</button>}
      </section>}
    </section>
  );
}
