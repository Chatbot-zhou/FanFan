import { AppstoreOutlined, CalendarOutlined, CloseOutlined, FileDoneOutlined, FolderAddOutlined, PlusOutlined, RobotOutlined } from "@ant-design/icons";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useVirtualizer } from "@tanstack/react-virtual";
import { FormEvent, useRef, useState } from "react";
import { bridge, type CollectionKind, type CollectionRule, type CreateCollectionRequest, type KnowledgeSpace } from "../bridge";
import { useAppStore } from "../state/app-store";
import { displayPath } from "../utils/display-path";

const splitRuleValues = (value: string) => value.split(/[，,]+/).map((item) => item.trim()).filter(Boolean);
const megabytesToBytes = (value: string) => value ? Math.round(Number(value) * 1024 * 1024) : null;

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
  const [ruleOperator, setRuleOperator] = useState<CollectionRule["operator"]>("all");
  const [extensions, setExtensions] = useState("");
  const [keywords, setKeywords] = useState("");
  const [pathKeywords, setPathKeywords] = useState("");
  const [textKeywords, setTextKeywords] = useState("");
  const [recentDays, setRecentDays] = useState("");
  const [minSizeMb, setMinSizeMb] = useState("");
  const [maxSizeMb, setMaxSizeMb] = useState("");
  const [excludeExtensions, setExcludeExtensions] = useState("");
  const [excludeFilenameKeywords, setExcludeFilenameKeywords] = useState("");
  const [excludePathKeywords, setExcludePathKeywords] = useState("");
  const [excludeTextKeywords, setExcludeTextKeywords] = useState("");
  const [addFileId, setAddFileId] = useState("");
  const [filePickerQuery, setFilePickerQuery] = useState("");
  const [previewCount, setPreviewCount] = useState<number | null>(null);
  const [editingSuggestionId, setEditingSuggestionId] = useState<string | null>(null);
  const [suggestionName, setSuggestionName] = useState("");
  const [suggestionMemberIds, setSuggestionMemberIds] = useState<string[]>([]);
  const [spaceEditorOpen, setSpaceEditorOpen] = useState(false);
  const [spaceEditingId, setSpaceEditingId] = useState<string | null>(null);
  const [spaceName, setSpaceName] = useState("");
  const [spaceDescription, setSpaceDescription] = useState("");
  const [spaceRootIds, setSpaceRootIds] = useState<string[]>([]);
  const [spaceCollectionIds, setSpaceCollectionIds] = useState<string[]>([]);
  const collections = useQuery({ queryKey: ["collections"], queryFn: () => bridge.collection_list() });
  const roots = useQuery({ queryKey: ["roots"], queryFn: () => bridge.root_list() });
  const knowledgeSpaces = useQuery({ queryKey: ["knowledge-spaces"], queryFn: () => bridge.knowledge_space_list() });
  const suggestions = useQuery({ queryKey: ["collection-suggestions"], queryFn: () => bridge.collection_suggestion_query(null, 50, "suggested") });
  const selectedCollection = collections.data?.find((item) => item.collection_id === selectedId) ?? null;
  const files = useInfiniteQuery({
    queryKey: ["collection-files", selectedId],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.collection_file_query(selectedId!, { cursor: pageParam, page_size: 100 }),
    getNextPageParam: (page) => page.next_cursor,
    enabled: selectedId !== null,
  });
  const collectionItems = files.data?.pages.flatMap((page) => page.items) ?? [];
  const allFiles = useQuery({ queryKey: ["collection-file-picker", filePickerQuery], queryFn: () => bridge.file_query({ cursor: null, page_size: 200, query: filePickerQuery || null, parse_statuses: ["parsed"], availability: "present" }), enabled: selectedCollection?.kind === "manual" || selectedCollection?.kind === "ai" });
  const collectionListRef = useRef<HTMLDivElement>(null);
  const collectionVirtualizer = useVirtualizer({ count: collectionItems.length, getScrollElement: () => collectionListRef.current, estimateSize: () => 58, overscan: 8 });
  const create = useMutation({
    mutationFn: (request: CreateCollectionRequest) => bridge.collection_create(request),
    onSuccess: async (collection) => {
      setCreating(false);
      setSelectedId(collection.collection_id);
      resetRuleEditor(); setName(""); setDescription("");
      await queryClient.invalidateQueries({ queryKey: ["collections"] });
    },
  });
  const update = useMutation({
    mutationFn: (request: CreateCollectionRequest) => bridge.collection_update(editingId!, request),
    onSuccess: async (collection) => {
      setCreating(false); setEditingId(null); setSelectedId(collection.collection_id);
      resetRuleEditor(); setName(""); setDescription("");
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
  const refreshSuggestions = useMutation({
    mutationFn: () => bridge.collection_suggestion_refresh(500),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["collection-suggestions"] }),
  });
  const confirmSuggestion = useMutation({
    mutationFn: (suggestionId: string) => bridge.collection_suggestion_confirm(suggestionId),
    onSuccess: async (collection) => {
      setSelectedId(collection.collection_id);
      await Promise.all([queryClient.invalidateQueries({ queryKey: ["collections"] }), queryClient.invalidateQueries({ queryKey: ["collection-suggestions"] })]);
    },
  });
  const rejectSuggestion = useMutation({ mutationFn: (suggestionId: string) => bridge.collection_suggestion_reject(suggestionId), onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["collection-suggestions"] }) });
  const updateSuggestion = useMutation({
    mutationFn: (suggestionId: string) => {
      const current = suggestions.data?.items.find((item) => item.suggestion_id === suggestionId);
      if (!current) throw new Error("AI集合建议不存在");
      return bridge.collection_suggestion_update(suggestionId, { suggested_name: suggestionName, description: current.description, member_file_ids: suggestionMemberIds });
    },
    onSuccess: async () => { setEditingSuggestionId(null); await queryClient.invalidateQueries({ queryKey: ["collection-suggestions"] }); },
  });
  const saveKnowledgeSpace = useMutation({
    mutationFn: () => {
      const request = { name: spaceName, description: spaceDescription || null, root_ids: spaceRootIds, collection_ids: spaceCollectionIds };
      return spaceEditingId ? bridge.knowledge_space_update(spaceEditingId, request) : bridge.knowledge_space_create(request);
    },
    onSuccess: async () => {
      setSpaceEditorOpen(false); setSpaceEditingId(null); setSpaceName(""); setSpaceDescription(""); setSpaceRootIds([]); setSpaceCollectionIds([]);
      await queryClient.invalidateQueries({ queryKey: ["knowledge-spaces"] });
    },
  });
  const deleteKnowledgeSpace = useMutation({
    mutationFn: (spaceId: string) => bridge.knowledge_space_delete(spaceId),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["knowledge-spaces"] }),
  });

  const resetRuleEditor = () => {
    setRuleOperator("all"); setExtensions(""); setKeywords(""); setPathKeywords(""); setTextKeywords("");
    setRecentDays(""); setMinSizeMb(""); setMaxSizeMb(""); setExcludeExtensions("");
    setExcludeFilenameKeywords(""); setExcludePathKeywords(""); setExcludeTextKeywords("");
  };
  const hasRuleCondition = [extensions, keywords, pathKeywords, textKeywords, recentDays, minSizeMb, maxSizeMb, excludeExtensions, excludeFilenameKeywords, excludePathKeywords, excludeTextKeywords].some((value) => value.trim());
  const buildRule = (): CollectionRule => ({
    operator: ruleOperator,
    extensions: extensions.split(/[，,\s]+/).map((value) => value.trim().replace(/^\./, "")).filter(Boolean),
    filename_keywords: splitRuleValues(keywords),
    path_keywords: splitRuleValues(pathKeywords),
    text_keywords: splitRuleValues(textKeywords),
    parse_statuses: [],
    modified_within_days: recentDays ? Number(recentDays) : null,
    min_size_bytes: megabytesToBytes(minSizeMb),
    max_size_bytes: megabytesToBytes(maxSizeMb),
    exclude_extensions: excludeExtensions.split(/[，,\s]+/).map((value) => value.trim().replace(/^\./, "")).filter(Boolean),
    exclude_filename_keywords: splitRuleValues(excludeFilenameKeywords),
    exclude_path_keywords: splitRuleValues(excludePathKeywords),
    exclude_text_keywords: splitRuleValues(excludeTextKeywords),
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
    setRuleOperator(selectedCollection.rule?.operator ?? "all");
    setExtensions(selectedCollection.rule?.extensions.join(", ") ?? "");
    setKeywords(selectedCollection.rule?.filename_keywords.join(", ") ?? "");
    setPathKeywords(selectedCollection.rule?.path_keywords.join(", ") ?? "");
    setTextKeywords(selectedCollection.rule?.text_keywords.join(", ") ?? "");
    setRecentDays(selectedCollection.rule?.modified_within_days?.toString() ?? "");
    setMinSizeMb(selectedCollection.rule?.min_size_bytes ? (selectedCollection.rule.min_size_bytes / 1024 / 1024).toString() : "");
    setMaxSizeMb(selectedCollection.rule?.max_size_bytes ? (selectedCollection.rule.max_size_bytes / 1024 / 1024).toString() : "");
    setExcludeExtensions(selectedCollection.rule?.exclude_extensions.join(", ") ?? "");
    setExcludeFilenameKeywords(selectedCollection.rule?.exclude_filename_keywords.join(", ") ?? "");
    setExcludePathKeywords(selectedCollection.rule?.exclude_path_keywords.join(", ") ?? "");
    setExcludeTextKeywords(selectedCollection.rule?.exclude_text_keywords.join(", ") ?? "");
    setPreviewCount(null);
  };

  const closeEditor = () => {
    setCreating(false); setEditingId(null); setName(""); setDescription("");
    resetRuleEditor(); setPreviewCount(null);
  };

  const editKnowledgeSpace = (space: KnowledgeSpace) => {
    setSpaceEditingId(space.space_id); setSpaceEditorOpen(true); setSpaceName(space.name);
    setSpaceDescription(space.description ?? ""); setSpaceRootIds(space.root_ids); setSpaceCollectionIds(space.collection_ids);
  };

  const closeSpaceEditor = () => {
    setSpaceEditorOpen(false); setSpaceEditingId(null); setSpaceName(""); setSpaceDescription(""); setSpaceRootIds([]); setSpaceCollectionIds([]);
  };

  return (
    <section className="page">
      <header className="page-heading">
        <div><h1>智能集合</h1><p>AI判断资料之间的内容联系，只在拾忆中形成虚拟分类，不改变原文件位置。</p></div>
        <div><button type="button" disabled={refreshSuggestions.isPending} onClick={() => refreshSuggestions.mutate()}><RobotOutlined /> {refreshSuggestions.isPending ? "正在分析" : "AI分析新建议"}</button><button type="button" className="primary-button" onClick={() => { if (creating) closeEditor(); else { setEditingId(null); setCreating(true); } }}>{creating ? <CloseOutlined /> : <PlusOutlined />} {creating ? "取消" : "新建集合"}</button></div>
      </header>
      <section className="knowledge-spaces">
        <header><div><h2>知识空间</h2><p>把多个已授权目录和虚拟集合组合成一个检索范围，不复制或移动任何文件。</p></div><button type="button" onClick={() => spaceEditorOpen ? closeSpaceEditor() : setSpaceEditorOpen(true)}>{spaceEditorOpen ? <CloseOutlined /> : <PlusOutlined />} {spaceEditorOpen ? "取消" : "新建知识空间"}</button></header>
        {spaceEditorOpen && <form className="collection-create" onSubmit={(event) => { event.preventDefault(); saveKnowledgeSpace.mutate(); }}>
          <label>空间名称<input required maxLength={40} value={spaceName} onChange={(event) => setSpaceName(event.target.value)} placeholder="例如：毕业论文资料" /></label>
          <label>说明<input maxLength={500} value={spaceDescription} onChange={(event) => setSpaceDescription(event.target.value)} placeholder="这个空间用于什么检索场景" /></label>
          <fieldset><legend>授权资料位置</legend>{roots.data?.length ? roots.data.map((root) => <label key={root.root_id}><input type="checkbox" checked={spaceRootIds.includes(root.root_id)} onChange={() => setSpaceRootIds((current) => current.includes(root.root_id) ? current.filter((id) => id !== root.root_id) : [...current, root.root_id])} /> {root.label}</label>) : <small>尚未添加资料位置</small>}</fieldset>
          <fieldset><legend>虚拟集合</legend>{collections.data?.length ? collections.data.map((collection) => <label key={collection.collection_id}><input type="checkbox" checked={spaceCollectionIds.includes(collection.collection_id)} onChange={() => setSpaceCollectionIds((current) => current.includes(collection.collection_id) ? current.filter((id) => id !== collection.collection_id) : [...current, collection.collection_id])} /> {collection.name}</label>) : <small>尚无可用集合</small>}</fieldset>
          {saveKnowledgeSpace.isError && <p role="alert" className="inline-error">{saveKnowledgeSpace.error instanceof Error ? saveKnowledgeSpace.error.message : String(saveKnowledgeSpace.error)}</p>}
          <button type="submit" className="primary-button" disabled={saveKnowledgeSpace.isPending || !spaceName.trim() || (spaceRootIds.length === 0 && spaceCollectionIds.length === 0)}>{saveKnowledgeSpace.isPending ? "正在保存" : spaceEditingId ? "保存知识空间" : "创建知识空间"}</button>
        </form>}
        <div className="collection-grid">{knowledgeSpaces.data?.map((space) => <article className="collection-card" key={space.space_id}><span className="collection-card__icon"><FolderAddOutlined /></span><h3>{space.name}</h3><p>{space.description ?? "自定义本地检索范围"}</p><strong>{space.file_count} 项</strong><small>{space.root_ids.length} 个位置 · {space.collection_ids.length} 个集合</small><div><button type="button" className="text-button" onClick={() => editKnowledgeSpace(space)}>编辑</button><button type="button" className="danger-button" disabled={deleteKnowledgeSpace.isPending} onClick={() => { if (window.confirm(`删除知识空间“${space.name}”？原文件和集合不会受到影响。`)) deleteKnowledgeSpace.mutate(space.space_id); }}>删除</button></div></article>)}</div>
      </section>
      {(refreshSuggestions.isError || suggestions.isError) && <p role="alert" className="inline-error">{refreshSuggestions.error instanceof Error ? refreshSuggestions.error.message : suggestions.error instanceof Error ? suggestions.error.message : "AI集合建议暂时不可用"}</p>}
      {(suggestions.data?.items.length ?? 0) > 0 && <section className="ai-suggestions">
        <header><div><h2>AI 集合建议</h2><p>建议需确认后才会成为正式虚拟集合；你可以先改名或移除误判成员。</p></div><strong>{suggestions.data?.total} 条待确认</strong></header>
        {suggestions.data?.items.map((suggestion) => <article key={suggestion.suggestion_id}>
          <div className="ai-suggestion__summary">
            {editingSuggestionId === suggestion.suggestion_id ? <input value={suggestionName} maxLength={40} onChange={(event) => setSuggestionName(event.target.value)} /> : <h3>{suggestion.suggested_name}</h3>}
            <p>{suggestion.description}</p><small>整体置信度 {Math.round(suggestion.confidence * 100)}% · {suggestion.members.length} 份资料 · {suggestion.algorithm_version}</small>
          </div>
          <div className="ai-suggestion__members">
            {suggestion.members.map((member) => {
              const selected = editingSuggestionId !== suggestion.suggestion_id || suggestionMemberIds.includes(member.file.file_id);
              return <label key={member.file.file_id} className={selected ? "" : "excluded"}>{editingSuggestionId === suggestion.suggestion_id && <input type="checkbox" checked={selected} onChange={() => setSuggestionMemberIds((current) => current.includes(member.file.file_id) ? current.filter((id) => id !== member.file.file_id) : [...current, member.file.file_id])} />}<span><strong>{member.file.display_name}</strong><small>{member.rationale} · {Math.round(member.confidence * 100)}%</small></span></label>;
            })}
          </div>
          <div className="ai-suggestion__actions">
            {editingSuggestionId === suggestion.suggestion_id ? <><button type="button" onClick={() => setEditingSuggestionId(null)}>取消编辑</button><button type="button" className="primary-button" disabled={!suggestionName.trim() || suggestionMemberIds.length < 2 || updateSuggestion.isPending} onClick={() => updateSuggestion.mutate(suggestion.suggestion_id)}>保存建议</button></> : <button type="button" onClick={() => { setEditingSuggestionId(suggestion.suggestion_id); setSuggestionName(suggestion.suggested_name); setSuggestionMemberIds(suggestion.members.map((member) => member.file.file_id)); }}>编辑成员</button>}
            <button type="button" className="danger-button" disabled={rejectSuggestion.isPending} onClick={() => rejectSuggestion.mutate(suggestion.suggestion_id)}>拒绝</button><button type="button" className="primary-button" disabled={confirmSuggestion.isPending} onClick={() => confirmSuggestion.mutate(suggestion.suggestion_id)}>确认虚拟集合</button>
          </div>
        </article>)}
      </section>}
      {creating && <form className="collection-create" onSubmit={submit}>
        <h2>{editingId ? "编辑集合" : "新建集合"}</h2>
        <label>集合名称<input required maxLength={40} value={name} onChange={(event) => setName(event.target.value)} placeholder="例如：求职资料" /></label>
        <label>说明<input maxLength={120} value={description} onChange={(event) => setDescription(event.target.value)} placeholder="这个集合用来收集什么" /></label>
        <fieldset><legend>集合方式</legend><label><input type="radio" checked={kind === "manual"} onChange={() => setKind("manual")} /> 手动添加</label><label><input type="radio" checked={kind === "rule"} onChange={() => setKind("rule")} /> 按规则自动收集</label>{kind === "ai" && <label><input type="radio" checked readOnly /> AI虚拟集合</label>}</fieldset>
        {kind === "rule" && <div className="collection-create__rules">
          <label>条件关系<select value={ruleOperator} onChange={(event) => { setRuleOperator(event.target.value as CollectionRule["operator"]); setPreviewCount(null); }}><option value="all">同时满足全部条件（AND）</option><option value="any">满足任一条件（OR）</option></select></label>
          <label>文件类型<input value={extensions} onChange={(event) => { setExtensions(event.target.value); setPreviewCount(null); }} placeholder="pdf, docx, xlsx" /></label>
          <label>文件名关键词<input value={keywords} onChange={(event) => { setKeywords(event.target.value); setPreviewCount(null); }} placeholder="合同, 项目" /></label>
          <label>路径关键词<input value={pathKeywords} onChange={(event) => { setPathKeywords(event.target.value); setPreviewCount(null); }} placeholder="客户A, 2026" /></label>
          <label>正文关键词<input value={textKeywords} onChange={(event) => { setTextKeywords(event.target.value); setPreviewCount(null); }} placeholder="付款条件, 交付日期" /></label>
          <label>最近修改天数<input type="number" min={1} max={3650} value={recentDays} onChange={(event) => { setRecentDays(event.target.value); setPreviewCount(null); }} placeholder="例如 30" /></label>
          <label>最小大小（MB）<input type="number" min={0} step="0.1" value={minSizeMb} onChange={(event) => { setMinSizeMb(event.target.value); setPreviewCount(null); }} placeholder="不限制" /></label>
          <label>最大大小（MB）<input type="number" min={0} step="0.1" value={maxSizeMb} onChange={(event) => { setMaxSizeMb(event.target.value); setPreviewCount(null); }} placeholder="不限制" /></label>
          <fieldset className="collection-rule-exclusions"><legend>排除条件（命中任一项即排除）</legend>
            <label>排除类型<input value={excludeExtensions} onChange={(event) => { setExcludeExtensions(event.target.value); setPreviewCount(null); }} placeholder="tmp, bak" /></label>
            <label>排除文件名<input value={excludeFilenameKeywords} onChange={(event) => { setExcludeFilenameKeywords(event.target.value); setPreviewCount(null); }} placeholder="草稿, 旧版" /></label>
            <label>排除路径<input value={excludePathKeywords} onChange={(event) => { setExcludePathKeywords(event.target.value); setPreviewCount(null); }} placeholder="归档, 临时" /></label>
            <label>排除正文<input value={excludeTextKeywords} onChange={(event) => { setExcludeTextKeywords(event.target.value); setPreviewCount(null); }} placeholder="作废, 测试数据" /></label>
          </fieldset>
          <button type="button" disabled={previewRule.isPending || !hasRuleCondition} onClick={() => previewRule.mutate()}>{previewRule.isPending ? "正在预览" : "预览匹配"}</button>
          {previewCount !== null && <small>当前规则至少匹配 {previewCount} 项{previewCount === 100 ? "（仅显示前100项）" : ""}</small>}
        </div>}
        {(create.isError || update.isError) && <p role="alert" className="inline-error">{(create.error ?? update.error) instanceof Error ? (create.error ?? update.error as Error).message : String(create.error ?? update.error)}</p>}
        <button type="submit" className="primary-button" disabled={create.isPending || update.isPending || !name.trim() || (kind === "rule" && !hasRuleCondition)}>{create.isPending || update.isPending ? "正在保存" : editingId ? "保存修改" : "创建集合"}</button>
      </form>}
      {collections.isLoading && <div className="page-empty"><p>正在读取智能集合…</p></div>}
      {collections.isError && <div className="page-empty"><h2>智能集合暂时无法读取</h2><button className="primary-button" type="button" onClick={() => void collections.refetch()}>重试</button></div>}
      <div className="collection-grid">
        {collections.data?.map((collection) => (
          <button type="button" className={`collection-card${selectedId === collection.collection_id ? " selected" : ""}`} key={collection.collection_id} onClick={() => setSelectedId(collection.collection_id)}>
            <span className="collection-card__icon" style={{ color: collection.color }}>{collection.icon === "calendar" ? <CalendarOutlined /> : collection.icon === "pending" ? <FileDoneOutlined /> : <AppstoreOutlined />}</span>
            <h2>{collection.name}{collection.built_in && <small>内置</small>}{collection.kind === "ai" && <small>AI</small>}</h2><p>{collection.description ?? (collection.kind === "manual" ? "手动收集的资料" : collection.kind === "ai" ? "经确认的AI虚拟分类" : "按规则自动更新")}</p><strong>{collection.file_count} 项</strong>
          </button>
        ))}
      </div>
      {selectedId && <section className="collection-detail">
        <header><h2>{selectedCollection?.name}</h2><div>{selectedCollection && !selectedCollection.built_in && <><button className="text-button" type="button" onClick={beginEdit}>编辑集合</button><button className="danger-button" type="button" disabled={deleteCollection.isPending} onClick={() => { if (window.confirm(`删除集合“${selectedCollection.name}”？原文件不会受到影响。`)) deleteCollection.mutate(); }}>删除集合</button></>}<button className="text-button" type="button" onClick={() => { setSelectedId(null); setAddFileId(""); clearCollectionSelection(); }}>关闭</button></div></header>
        {(selectedCollection?.kind === "manual" || selectedCollection?.kind === "ai") && <div className="collection-add-file">
          <label><FolderAddOutlined /> 添加资料
            <input aria-label="搜索资料" value={filePickerQuery} onChange={(event) => { setFilePickerQuery(event.target.value); setAddFileId(""); }} placeholder="按文件名查找（最多显示200项）" />
            <select aria-label="添加资料" value={addFileId} onChange={(event) => setAddFileId(event.target.value)}>
              <option value="">选择已索引资料</option>
          {allFiles.data?.items.filter((file) => file.parse_status === "parsed" && !collectionItems.some((current) => current.file_id === file.file_id)).map((file) => <option key={file.file_id} value={file.file_id}>{file.display_name} · {displayPath(file.display_path)}</option>)}
            </select>
          </label>
          <button type="button" className="primary-button" disabled={!addFileId || addFile.isPending} onClick={() => addFile.mutate()}>{addFile.isPending ? "正在添加" : "添加到集合"}</button>
          {addFile.isError && <p role="alert" className="inline-error">{addFile.error instanceof Error ? addFile.error.message : String(addFile.error)}</p>}
        </div>}
        {files.isLoading && <p>正在计算集合内容…</p>}
        {collectionItems.length === 0 && <p>这个集合当前没有资料。</p>}
        <div ref={collectionListRef} className="collection-members--virtual"><div style={{ height: `${collectionVirtualizer.getTotalSize()}px`, position: "relative" }}>
          {collectionVirtualizer.getVirtualItems().map((virtualRow) => { const file = collectionItems[virtualRow.index]!; return <div key={file.file_id} style={{ position: "absolute", transform: `translateY(${virtualRow.start}px)`, width: "100%", height: `${virtualRow.size}px` }}><button type="button" onClick={() => void bridge.file_open(file.file_id)}><strong>{file.display_name}</strong><small>{displayPath(file.display_path)}</small></button>{selectedCollection?.kind !== "rule" && <button type="button" className="text-button" disabled={removeFile.isPending} onClick={() => removeFile.mutate(file.file_id)}>{selectedCollection?.kind === "ai" ? "人工排除" : "移出集合"}</button>}</div>; })}
        </div></div>
        {files.hasNextPage && <button type="button" className="load-more-button" disabled={files.isFetchingNextPage} onClick={() => void files.fetchNextPage()}>{files.isFetchingNextPage ? "正在加载" : "加载更多"}</button>}
      </section>}
    </section>
  );
}
