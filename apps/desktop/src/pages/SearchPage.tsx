import { CloseOutlined, FileExcelOutlined, FilePdfOutlined, FileWordOutlined, SearchOutlined } from "@ant-design/icons";
import { useEffect, useMemo, useRef, useState } from "react";
import { bridge, type CollectionRecord, type FilePreview, type SearchRequest } from "../bridge";
import { PdfVisualPreview } from "../components/PdfVisualPreview";
import { ImageAssetGallery } from "../components/ImageAssetGallery";
import { OcrAttemptChain } from "../components/OcrAttemptChain";
import { AppSelect } from "../components/AppSelect";
import { errorMessage } from "../utils/app-error";
import { highlightPlainTerms } from "../utils/query-terms";
import { useAppStore, type SearchModifiedWindow } from "../state/app-store";

const emptyScope = {
  root_ids: [],
  collection_ids: [],
  file_ids: [],
  extensions: [],
  modified_from: null,
  modified_to: null,
  availability: "present" as const,
};

export function SearchPage() {
  // 搜索会话（查询词、结果、筛选偏好）放在全局 store，切换页面后回来仍保留
  const query = useAppStore((state) => state.search_query);
  const setQuery = useAppStore((state) => state.set_search_query);
  const session = useAppStore((state) => state.search_session);
  const sessionQuery = useAppStore((state) => state.search_session_query);
  const setSession = useAppStore((state) => state.set_search_session);
  const prefs = useAppStore((state) => state.search_prefs);
  const setPrefs = useAppStore((state) => state.set_search_prefs);
  const { mode, sort, extension, modified_window: modifiedWindow, scope_collection_ids: scopeCollectionIds } = prefs;
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [preview, setPreview] = useState<FilePreview | null>(null);
  const [previewLoadingId, setPreviewLoadingId] = useState<string | null>(null);
  const [collections, setCollections] = useState<CollectionRecord[]>([]);
  const searchSerial = useRef(0);
  const initialQuery = useRef(query);

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
      collection_ids: scopeCollectionIds,
      extensions: extension ? [extension] : [],
      modified_from: modifiedFrom,
    };
  }, [extension, modifiedWindow, scopeCollectionIds]);
  const addScopeCollection = (value: string) => {
    if (!value || scopeCollectionIds.includes(value)) return;
    setPrefs({ scope_collection_ids: [...scopeCollectionIds, value] });
  };
  const removeScopeCollection = (collectionId: string) => {
    setPrefs({ scope_collection_ids: scopeCollectionIds.filter((id) => id !== collectionId) });
  };

  const showPreview = async (fileId: string, offset = 0) => {
    setPreviewLoadingId(fileId);
    setError(null);
    try {
      const page = await bridge.preview_get(fileId, offset);
      setPreview((current) => current?.file.file_id === fileId && offset > 0
        ? { ...page, nodes: [...current.nodes, ...page.nodes], offset: current.offset }
        : page);
    } catch (previewError) {
      setError(errorMessage(previewError));
    } finally {
      setPreviewLoadingId(null);
    }
  };

  const runFileAction = async (action: "open" | "reveal", fileId: string) => {
    setError(null);
    try {
      await (action === "open" ? bridge.file_open(fileId) : bridge.file_reveal(fileId));
    } catch (actionError) {
      setError(errorMessage(actionError));
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
    try {
      setSession(null);
      const result = await bridge.search_start({ ...baseRequest, cursor: null });
      if (serial === searchSerial.current) setSession(result, trimmed);
    } catch (searchError) {
      if (serial !== searchSerial.current) return;
      setSession(null);
      setError(errorMessage(searchError));
    } finally {
      if (serial === searchSerial.current) setLoading(false);
    }
  };

  const loadMore = async () => {
    if (!session?.next_cursor) return;
    setLoadingMore(true);
    setError(null);
    try {
      const baseRequest: Omit<SearchRequest, "cursor"> = { query: sessionQuery, scope, mode, sort, page_size: 30 };
      const next = await bridge.search_start({ ...baseRequest, cursor: session.next_cursor });
      setSession(next, sessionQuery);
    } catch (searchError) {
      setError(errorMessage(searchError));
    } finally {
      setLoadingMore(false);
    }
  };

  // 挂载时：若 store 里已有同一查询词的搜索结果则直接恢复，否则（从首页发起等）自动搜索
  useEffect(() => {
    const initialValue = initialQuery.current;
    if (!initialValue || sessionQuery === initialValue) return;
    void search(initialValue);
    // 只在挂载时发起一次；之后的搜索由表单提交驱动
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <section className="page page--search">
      <form className="search-page__form" onSubmit={(event) => { event.preventDefault(); void search(query); }}>
        <SearchOutlined />
        <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="例如：去年那个关于RAG召回率优化的文档" autoFocus />
        <button type="submit" disabled={loading} aria-label="搜索"><SearchOutlined /></button>
      </form>
      {loading && <div className="search-progress" role="progressbar"><div className="search-progress__bar" /></div>}
      <div className="search-filters">
        <label>方式
          <AppSelect ariaLabel="搜索方式" value={mode} onChange={(value) => setPrefs({ mode: value as SearchRequest["mode"] })} options={[{ value: "hybrid", label: "混合搜索" }, { value: "filename", label: "文件名" }, { value: "fulltext", label: "全文" }, { value: "semantic", label: "语义" }]} />
        </label>
        <label>范围
          <AppSelect ariaLabel="选择检索范围" value="" showSearch onChange={addScopeCollection} labelRender={() => (
            scopeCollectionIds.length === 0
              ? <span>全部资料</span>
              : <span className="scope-select-trigger">选择范围</span>
          )} options={[
            ...collections.filter((item) => !scopeCollectionIds.includes(item.collection_id)).map((item) => ({ value: item.collection_id, label: item.name })),
          ]} />
        </label>
        <label>类型
          <AppSelect ariaLabel="资料类型" value={extension} onChange={(value) => setPrefs({ extension: value })} options={[{ value: "", label: "全部" }, { value: "pdf", label: "PDF" }, { value: "docx", label: "Word" }, { value: "xlsx", label: "Excel" }, { value: "pptx", label: "PPT" }, { value: "txt", label: "文本" }, { value: "md", label: "Markdown" }, { value: "png", label: "PNG 图片" }, { value: "jpg", label: "JPG 图片" }, { value: "zip", label: "ZIP" }]} />
        </label>
        <label>时间
          <AppSelect ariaLabel="修改时间" value={modifiedWindow} onChange={(value) => setPrefs({ modified_window: value as SearchModifiedWindow })} options={[{ value: "all", label: "不限" }, { value: "7", label: "最近7天" }, { value: "30", label: "最近30天" }, { value: "365", label: "最近一年" }]} />
        </label>
        <label>排序
          <AppSelect ariaLabel="结果排序" value={sort} onChange={(value) => setPrefs({ sort: value as SearchRequest["sort"] })} options={[{ value: "relevance", label: "相关性" }, { value: "modified_desc", label: "最近修改" }, { value: "name_asc", label: "文件名" }]} />
        </label>
        {scopeCollectionIds.length > 0 && <div className="search-filters__tags">
          {scopeCollectionIds.map((collectionId) => {
            const collection = collections.find((item) => item.collection_id === collectionId);
            return <span className="scope-tag" key={collectionId}>
              {collection?.name ?? "已删除的集合"}
              <button type="button" aria-label={`移除集合“${collection?.name ?? collectionId}”`} onClick={() => removeScopeCollection(collectionId)}><CloseOutlined /></button>
            </span>;
          })}
        </div>}
      </div>
      {session && (
        <div className="search-status">
          <span>找到 {session.results.length} 个结果</span>
          <span>
            文件名 {session.channels.filename === "completed" ? "✓" : "—"}
            {"　"}全文 {session.channels.fulltext === "completed" ? "✓" : "—"}
            {"　"}{session.channels.semantic === "unavailable" ? "语义搜索未启用，已自动使用名称与全文" : "语义搜索 ✓"}
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
              <div><h2>{highlightPlainTerms(result.name, query)}</h2><time>{new Date(result.modified_at).toLocaleDateString("zh-CN")}</time></div>
                    <small>{result.display_path}</small>
              <p>{highlightPlainTerms(result.snippet, query)}</p>
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
              {preview.file.extension.toLowerCase() === "pdf" && <PdfVisualPreview preview={preview} />}
              <OcrAttemptChain attempts={preview.ocr_attempts} />
              <ImageAssetGallery assets={preview.image_assets} />
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
