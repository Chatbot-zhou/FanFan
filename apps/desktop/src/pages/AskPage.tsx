import { FileSearchOutlined, SendOutlined } from "@ant-design/icons";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useMemo, useRef, useState } from "react";
import { bridge, type AnswerResult, type CollectionRecord, type FilePreview, type ModelRuntimeState } from "../bridge";
import { displayPath } from "../utils/display-path";

const locatorLabel = (locator: AnswerResult["claims"][number]["citations"][number]["locator"]) => {
  if (locator.page_no) return `第 ${locator.page_no} 页`;
  if (locator.slide_no) return `第 ${locator.slide_no} 张幻灯片`;
  if (locator.sheet_name) return `${locator.sheet_name}${locator.cell_range ? ` · ${locator.cell_range}` : ""}`;
  if (locator.paragraph_no) return `第 ${locator.paragraph_no} 段`;
  if (locator.line_start) return `第 ${locator.line_start} 行`;
  return "正文位置";
};

export function AskPage({ model_state }: { model_state: ModelRuntimeState | null }) {
  const [question, setQuestion] = useState("");
  const [answer, setAnswer] = useState<AnswerResult | null>(null);
  const [turns, setTurns] = useState<Array<{ question: string; answer: AnswerResult }>>([]);
  const [loading, setLoading] = useState(false);
  const [activeOperationId, setActiveOperationId] = useState<string | null>(null);
  const [streamedAnswer, setStreamedAnswer] = useState("");
  const activeOperationRef = useRef<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [collections, setCollections] = useState<CollectionRecord[]>([]);
  const [collectionId, setCollectionId] = useState("");
  const [preview, setPreview] = useState<FilePreview | null>(null);
  const [previewLoading, setPreviewLoading] = useState<string | null>(null);
  const hasGeneration = model_state?.capabilities.generation === true;
  const hasEmbedding = model_state?.capabilities.embedding === true;

  useEffect(() => {
    void bridge.collection_list().then(setCollections).catch(() => setCollections([]));
  }, []);

  useEffect(() => {
    if (!window.__TAURI_INTERNALS__) return undefined;
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    void listen<{ operation_id: string; token: string }>("ask.token", (event) => {
      if (event.payload.operation_id === activeOperationRef.current) {
        setStreamedAnswer((current) => current + event.payload.token);
      }
    }).then((unlisten) => disposed ? unlisten() : unlisteners.push(unlisten));
    return () => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, []);

  const sourceNames = useMemo(
    () => new Map(answer?.source_files.map((source) => [source.file_id, source.display_name]) ?? []),
    [answer],
  );

  const submit = async () => {
    const trimmed = question.trim();
    if (!trimmed || loading) return;
    setLoading(true);
    setError(null);
    setPreview(null);
    try {
      const result = await bridge.ask_start({
        question: trimmed,
        session_id: answer?.session_id ?? null,
        scope: { root_ids: [], collection_ids: collectionId ? [collectionId] : [], file_ids: [], extensions: [], modified_from: null, modified_to: null, availability: "present" },
        answer_style: "concise",
        retrieval_limit: 12,
        max_source_files: 8,
        strict_evidence: true,
      });
      activeOperationRef.current = result.operation_id;
      setActiveOperationId(result.operation_id);
      setStreamedAnswer("");
      while (activeOperationRef.current === result.operation_id) {
        const snapshot = await bridge.ask_operation_get(result.operation_id);
        if (snapshot.handle.status === "completed") {
          if (!snapshot.result) throw new Error("问答已完成，但结果不完整");
          setAnswer(snapshot.result);
          setTurns((current) => [...current, { question: trimmed, answer: snapshot.result! }]);
          setQuestion("");
          activeOperationRef.current = null;
          setActiveOperationId(null);
          break;
        }
        if (snapshot.handle.status === "failed" || snapshot.handle.status === "cancelled") {
          throw new Error(snapshot.error?.message ?? (snapshot.handle.status === "cancelled" ? "问答已取消" : "问答失败"));
        }
        await new Promise((resolve) => window.setTimeout(resolve, 120));
      }
    } catch (askError) {
      setError(askError instanceof Error ? askError.message : String(askError));
    } finally {
      activeOperationRef.current = null;
      setActiveOperationId(null);
      setLoading(false);
    }
  };

  const cancelAsk = async () => {
    const operationId = activeOperationRef.current;
    if (!operationId) return;
    try {
      const snapshot = await bridge.ask_cancel(operationId);
      if (snapshot.handle.status === "completed" && snapshot.result) {
        setAnswer(snapshot.result);
      } else {
        setError(snapshot.error?.message ?? "问答已取消");
      }
    } catch (cancelError) {
      setError(cancelError instanceof Error ? cancelError.message : String(cancelError));
    } finally {
      activeOperationRef.current = null;
      setActiveOperationId(null);
      setLoading(false);
    }
  };

  const showPreview = async (fileId: string, anchorNodeId: string | null = null, offset = 0) => {
    setPreviewLoading(fileId);
    setError(null);
    try {
      const page = await bridge.preview_get(fileId, offset, 80, anchorNodeId);
      setPreview((current) => current?.file.file_id === fileId && offset > 0
        ? { ...page, nodes: [...current.nodes, ...page.nodes], offset: current.offset, anchor_node_id: current.anchor_node_id }
        : page);
    } catch (previewError) {
      setError(previewError instanceof Error ? previewError.message : String(previewError));
    } finally {
      setPreviewLoading(null);
    }
  };

  return (
    <section className="page page--ask">
      <header className="page-heading"><div><h1>问资料</h1><p>回答只使用你的本地资料，并在回答中标出来源。</p></div>{turns.length > 0 && !loading && <button type="button" className="text-button" onClick={() => { setTurns([]); setAnswer(null); setPreview(null); }}>新建会话</button>}</header>
      <div className="scope-bar">
        <FileSearchOutlined /> 检索范围：
        <select aria-label="问答范围" value={collectionId} onChange={(event) => setCollectionId(event.target.value)}>
          <option value="">全部资料</option>{collections.map((item) => <option key={item.collection_id} value={item.collection_id}>{item.name}</option>)}
        </select>
        <span>{hasGeneration ? "本地生成回答" : hasEmbedding ? "语义检索 · 严格摘录" : "全文检索 · 严格摘录"}</span>
      </div>
      <div className={`conversation-area${answer ? " conversation-area--answered" : ""}`}>
        {!answer && <div className="conversation-empty">
          <div className="conversation-empty__mark">拾</div>
          <h2>从你的资料中寻找答案</h2>
          <p>{hasGeneration ? "回答会附带原文引用；证据不足时，拾忆会明确说明。" : "未配置生成模型时，拾忆仍会给出可核对的原文依据，不会编造答案。"}</p>
        </div>}
        {turns.slice(0, -1).map((turn, index) => <article className={`answer-card answer-card--${turn.answer.grounding_status}`} key={`${turn.answer.session_id}-${index}`}><header><span>你：{turn.question}</span><small>{turn.answer.elapsed_ms} ms</small></header><p className="answer-card__text">{turn.answer.answer}</p></article>)}
        {answer && <article className={`answer-card answer-card--${answer.grounding_status}`}>
          <header><span>{answer.answer_mode === "extractive" ? "严格证据摘录" : "本地模型回答"}</span><small>{answer.elapsed_ms} ms · {answer.used_file_ids.length} 个来源文件</small></header>
          <p className="answer-card__text">{answer.answer}</p>
          {answer.claims.length > 0 && <div className="answer-claims">
            <h2>引用依据</h2>
            {answer.claims.map((claim, index) => <section key={claim.claim_id}>
              <p>{claim.text}</p>
              <div>{claim.citations.map((citation) => <button type="button" key={citation.evidence_id} onClick={() => void showPreview(citation.file_id, citation.node_id)}>
                [{index + 1}] {sourceNames.get(citation.file_id) ?? "本地资料"} · {locatorLabel(citation.locator)}{previewLoading === citation.file_id ? " · 载入中" : ""}
              </button>)}</div>
            </section>)}
          </div>}
          {preview && <div className="answer-preview" aria-label={`${preview.file.display_name}原文预览`}>
      <header><strong>{preview.file.display_name}</strong><small>{displayPath(preview.file.display_path)}</small></header>
            {preview.nodes.map((node) => <p key={node.node_id} className={node.node_id === preview.anchor_node_id ? "preview-node--anchor" : undefined}><small>{locatorLabel(node.locator)}</small>{node.text ?? (node.table_data ? JSON.stringify(node.table_data) : "")}</p>)}
            {preview.next_offset !== null && <button type="button" className="text-button" disabled={previewLoading === preview.file.file_id} onClick={() => void showPreview(preview.file.file_id, null, preview.next_offset ?? 0)}>继续载入</button>}
          </div>}
        </article>}
        {loading && streamedAnswer && <article className="answer-card answer-card--grounded" aria-live="polite"><header><span>正在生成已验证回答</span><small>本地处理</small></header><p className="answer-card__text">{streamedAnswer}</p></article>}
      </div>
      {error && <p role="alert" className="inline-error">{error}</p>}
      <form className="ask-composer" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
        <textarea value={question} onChange={(event) => setQuestion(event.target.value)} placeholder="基于我的资料提问…" />
        {activeOperationId && <button type="button" className="text-button" onClick={() => void cancelAsk()}>取消</button>}
        <button type="submit" aria-label="发送" disabled={loading || !question.trim()}>{loading ? "…" : <SendOutlined />}</button>
      </form>
    </section>
  );
}
