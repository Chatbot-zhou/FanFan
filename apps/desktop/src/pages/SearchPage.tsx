import { FileExcelOutlined, FilePdfOutlined, FileWordOutlined, SearchOutlined } from "@ant-design/icons";
import { useEffect, useMemo, useRef, useState } from "react";
import { bridge, type CollectionRecord, type FilePreview, type SearchRequest, type SearchSession } from "../bridge";
import { useAppStore } from "../state/app-store";

const emptyScope = {
  root_ids: [],
  collection_ids: [],
  file_ids: [],
  extensions: [],
  modified_from: null,
  modified_to: null,
  availability: "present" as const,
};

function mergeSearchSessions(
  sessions: SearchSession[],
  sort: SearchRequest["sort"],
  running: boolean,
  semanticExpected: boolean,
): SearchSession {
  const merged = new Map<string, SearchSession["results"][number]>();
  for (const session of sessions) {
    for (const result of session.results) {
      const current = merged.get(result.file_id);
      if (!current) {
        merged.set(result.file_id, result);
        continue;
      }
      merged.set(result.file_id, {
        ...current,
        snippet: result.scores.fulltext || result.scores.semantic ? result.snippet : current.snippet,
        locator: result.locator ?? current.locator,
        revision_id: result.revision_id ?? current.revision_id,
        match_reasons: [...new Set([...current.match_reasons, ...result.match_reasons])],
        scores: {
          filename: Math.max(current.scores.filename ?? 0, result.scores.filename ?? 0) || null,
          fulltext: Math.max(current.scores.fulltext ?? 0, result.scores.fulltext ?? 0) || null,
          semantic: Math.max(current.scores.semantic ?? 0, result.scores.semantic ?? 0) || null,
          fused: Math.max(current.scores.fused, result.scores.fused),
        },
      });
    }
  }
  const results = [...merged.values()];
  if (sort === "modified_desc") results.sort((left, right) => Date.parse(right.modified_at) - Date.parse(left.modified_at));
  else if (sort === "name_asc") results.sort((left, right) => left.name.localeCompare(right.name, "zh-CN"));
  else results.sort((left, right) => right.scores.fused - left.scores.fused);
  const filenameCompleted = sessions.some((session) => session.channels.filename === "completed");
  const fulltextCompleted = sessions.some((session) => session.channels.fulltext === "completed");
  const semanticCompleted = sessions.some((session) => session.channels.semantic === "completed");
  return {
    search_id: sessions.at(-1)?.search_id ?? "pending",
    status: running ? "running" : "completed",
    channels: {
      filename: filenameCompleted ? "completed" : running ? "pending" : "unavailable",
      fulltext: fulltextCompleted ? "completed" : running ? "pending" : "unavailable",
      semantic: semanticCompleted ? "completed" : semanticExpected && running ? "pending" : "unavailable",
    },
    results,
    next_cursor: null,
    elapsed_ms: Math.max(0, ...sessions.map((session) => session.elapsed_ms)),
  };
}

export function SearchPage() {
  const initial = useAppStore((state) => state.search_query);
  const [query, setQuery] = useState(initial);
  const [session, setSession] = useState<SearchSession | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [preview, setPreview] = useState<FilePreview | null>(null);
  const [previewLoadingId, setPreviewLoadingId] = useState<string | null>(null);
  const [mode, setMode] = useState<SearchRequest["mode"]>("hybrid");
  const [extension, setExtension] = useState("");
  const [modifiedWindow, setModifiedWindow] = useState<"all" | "7" | "30" | "365">("all");
  const [sort, setSort] = useState<SearchRequest["sort"]>("relevance");
  const [collectionId, setCollectionId] = useState("");
  const [collections, setCollections] = useState<CollectionRecord[]>([]);
  const searchSerial = useRef(0);
  const lastSearchRequest = useRef<Omit<SearchRequest, "cursor"> | null>(null);

  useEffect(() => {
    let active = true;
    void bridge.collection_list().then((items) => {
      if (active) setCollections(items);
    }).catch(() => {
      if (active) setCollections([]);
    });
    return () => { active = false; };
  }, []);

  const scope = useMemo(() => {
    const modifiedFrom = modifiedWindow === "all"
      ? null
      : new Date(Date.now() - Number(modifiedWindow) * 24 * 60 * 60 * 1000).toISOString();
    return {
      ...emptyScope,
      collection_ids: collectionId ? [collectionId] : [],
      extensions: extension ? [extension] : [],
      modified_from: modifiedFrom,
    };
  }, [collectionId, extension, modifiedWindow]);

  const showPreview = async (fileId: string, offset = 0) => {
    setPreviewLoadingId(fileId);
    setError(null);
    try {
      const page = await bridge.preview_get(fileId, offset);
      setPreview((current) => current?.file.file_id === fileId && offset > 0
        ? { ...page, nodes: [...current.nodes, ...page.nodes], offset: current.offset }
        : page);
    } catch (previewError) {
      setError(previewError instanceof Error ? previewError.message : String(previewError));
    } finally {
      setPreviewLoadingId(null);
    }
  };

  const runFileAction = async (action: "open" | "reveal", fileId: string) => {
    setError(null);
    try {
      await (action === "open" ? bridge.file_open(fileId) : bridge.file_reveal(fileId));
    } catch (actionError) {
      setError(actionError instanceof Error ? actionError.message : String(actionError));
    }
  };

  const search = async (value: string) => {
    const trimmed = value.trim();
    if (!trimmed) return;
    setLoading(true);
    setError(null);
    setPreview(null);
    const serial = ++searchSerial.current;
    const baseRequest: Omit<SearchRequest, "cursor"> = { query: trimmed, scope, mode, sort, page_size: 30 };
    lastSearchRequest.current = baseRequest;
    try {
      if (mode !== "hybrid") {
        setSession(null);
        const result = await bridge.search_start({ ...baseRequest, cursor: null });
        if (serial === searchSerial.current) setSession(result);
        return;
      }
      const modelState = await bridge.model_state_get().catch(() => null);
      if (serial !== searchSerial.current) return;
      const semanticExpected = modelState?.capabilities.embedding === true;
      const phases: SearchRequest["mode"][] = ["filename", "fulltext"];
      if (semanticExpected) phases.push("semantic");
      const phaseResults: SearchSession[] = [];
      const phaseErrors: string[] = [];
      setSession(mergeSearchSessions([], sort, true, semanticExpected));
      await Promise.all(phases.map(async (phase) => {
        try {
          const result = await bridge.search_start({ ...baseRequest, mode: phase, cursor: null });
          if (serial !== searchSerial.current) return;
          phaseResults.push(result);
          setSession(mergeSearchSessions(phaseResults, sort, true, semanticExpected));
        } catch (phaseError) {
          phaseErrors.push(phaseError instanceof Error ? phaseError.message : String(phaseError));
        }
      }));
      if (serial === searchSerial.current) {
        const canonical = await bridge.search_start({ ...baseRequest, mode: "hybrid", cursor: null });
        if (serial !== searchSerial.current) return;
        lastSearchRequest.current = { ...baseRequest, mode: "hybrid" };
        setSession(canonical);
        if (phaseErrors.length > 0) setError(`部分搜索通道已降级：${phaseErrors[0]}`);
      }
    } catch (searchError) {
      if (serial !== searchSerial.current) return;
      setSession(null);
      setError(searchError instanceof Error ? searchError.message : String(searchError));
    } finally {
      if (serial === searchSerial.current) setLoading(false);
    }
  };

  const cancelSearch = () => {
    searchSerial.current += 1;
    setLoading(false);
    setSession((current) => current ? { ...current, status: "cancelled" } : current);
  };

  const loadMore = async () => {
    if (!session?.next_cursor || !lastSearchRequest.current) return;
    setLoadingMore(true);
    setError(null);
    try {
      const next = await bridge.search_start({ ...lastSearchRequest.current, cursor: session.next_cursor });
      setSession((current) => {
        if (!current) return next;
        const results = new Map(current.results.map((result) => [result.file_id, result]));
        next.results.forEach((result) => results.set(result.file_id, result));
        return { ...next, results: [...results.values()] };
      });
    } catch (searchError) {
      setError(searchError instanceof Error ? searchError.message : String(searchError));
    } finally {
      setLoadingMore(false);
    }
  };

  useEffect(() => {
    if (initial) void search(initial);
  }, [initial]);

  return (
    <section className="page page--search">
      <header className="page-heading"><div><h1>找资料</h1><p>按文件名、正文和内容含义找到过去的资料。</p></div></header>
      <form className="search-page__form" onSubmit={(event) => { event.preventDefault(); void search(query); }}>
        <SearchOutlined />
        <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="例如：去年那个关于RAG召回率优化的文档" autoFocus />
        <button type="submit" disabled={loading}>{loading ? "搜索中" : "搜索"}</button>
        {loading && <button type="button" onClick={cancelSearch}>停止</button>}
      </form>
      <div className="search-filters">
        <label>方式
          <select aria-label="搜索方式" value={mode} onChange={(event) => setMode(event.target.value as SearchRequest["mode"])}>
            <option value="hybrid">混合搜索</option><option value="filename">文件名</option><option value="fulltext">全文</option><option value="semantic">语义</option>
          </select>
        </label>
        <label>范围
          <select aria-label="搜索范围" value={collectionId} onChange={(event) => setCollectionId(event.target.value)}>
            <option value="">全部资料</option>{collections.map((item) => <option key={item.collection_id} value={item.collection_id}>{item.name}</option>)}
          </select>
        </label>
        <label>类型
          <select aria-label="资料类型" value={extension} onChange={(event) => setExtension(event.target.value)}>
            <option value="">全部</option><option value="pdf">PDF</option><option value="docx">Word</option><option value="xlsx">Excel</option><option value="pptx">PPT</option><option value="md">Markdown</option><option value="txt">文本</option>
          </select>
        </label>
        <label>时间
          <select aria-label="修改时间" value={modifiedWindow} onChange={(event) => setModifiedWindow(event.target.value as typeof modifiedWindow)}>
            <option value="all">不限</option><option value="7">最近7天</option><option value="30">最近30天</option><option value="365">最近一年</option>
          </select>
        </label>
        <label>排序
          <select aria-label="结果排序" value={sort} onChange={(event) => setSort(event.target.value as SearchRequest["sort"])}>
            <option value="relevance">相关性</option><option value="modified_desc">最近修改</option><option value="name_asc">文件名</option>
          </select>
        </label>
      </div>
      {session && (
        <div className="search-status">
          <span>找到 {session.results.length} 个结果</span>
          <span>
            文件名 {session.channels.filename === "completed" ? "✓" : "—"}　
            全文 {session.channels.fulltext === "completed" ? "✓" : "—"}　
            {session.channels.semantic === "unavailable" ? "语义搜索未启用，已自动使用名称与全文" : "语义搜索 ✓"}
          </span>
        </div>
      )}
      {session?.results.length === 0 && <div className="page-empty page-empty--compact"><SearchOutlined /><h2>没有找到匹配资料</h2><p>可以缩短关键词、放宽筛选条件，或确认资料已完成扫描。</p></div>}
      {error && <p role="alert" className="inline-error">{error}</p>}
      <div className="search-results">
        {session?.results.map((result) => (
          <article className="search-result" key={result.file_id}>
            <div className={`search-result__icon search-result__icon--${result.extension}`}>
              {result.extension === "pdf" ? <FilePdfOutlined /> : result.extension === "xlsx" ? <FileExcelOutlined /> : <FileWordOutlined />}
            </div>
            <div className="search-result__body">
              <div><h2>{result.name}</h2><time>{new Date(result.modified_at).toLocaleDateString("zh-CN")}</time></div>
                    <small>{result.display_path}</small>
              <p>{result.snippet}</p>
              {result.locator && (
                <small className="source-locator">
                  {result.locator.page_no ? `第 ${result.locator.page_no} 页` :
                    result.locator.slide_no ? `第 ${result.locator.slide_no} 张幻灯片` :
                    result.locator.sheet_name ? `${result.locator.sheet_name}${result.locator.cell_range ? ` · ${result.locator.cell_range}` : ""}` :
                    result.locator.paragraph_no ? `第 ${result.locator.paragraph_no} 段` :
                    result.locator.line_start ? `第 ${result.locator.line_start} 行` : "正文命中"}
                </small>
              )}
              <div className="match-reasons">匹配：{result.match_reasons.map((reason) => ({ filename: "文件名", fulltext: "正文", semantic: "语义", path: "路径", time_filter: "时间" })[reason]).join(" · ")}</div>
              <div className="search-result__actions">
                <button type="button" onClick={() => void showPreview(result.file_id)}>{previewLoadingId === result.file_id ? "载入中" : "查看内容"}</button>
                <button type="button" onClick={() => void runFileAction("open", result.file_id)}>打开原文件</button>
                <button type="button" onClick={() => void runFileAction("reveal", result.file_id)}>所在文件夹</button>
              </div>
              {preview?.file.file_id === result.file_id && (
                <div className="search-result__preview" aria-label={`${result.name}内容预览`}>
                  {preview.nodes.length === 0 ? <p>此文件当前只有名称和元数据，正文尚未就绪。</p> : preview.nodes.map((node) => (
                    <p key={node.node_id}>{node.text ?? (node.table_data ? JSON.stringify(node.table_data) : "")}</p>
                  ))}
                  {preview.next_offset !== null && <button type="button" className="text-button" disabled={previewLoadingId === result.file_id} onClick={() => void showPreview(result.file_id, preview.next_offset ?? 0)}>继续载入</button>}
                </div>
              )}
            </div>
          </article>
        ))}
      </div>
      {session?.next_cursor && <button type="button" className="load-more-button" disabled={loadingMore} onClick={() => void loadMore()}>{loadingMore ? "正在载入" : "加载更多结果"}</button>}
      {!session && !loading && <div className="page-empty"><SearchOutlined /><h2>描述你记得的内容</h2><p>即使忘记文件名，也可以说出主题、时间或大概内容。</p></div>}
    </section>
  );
}
