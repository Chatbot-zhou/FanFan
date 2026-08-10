import { FileSearchOutlined, SendOutlined } from "@ant-design/icons";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useMemo, useRef, useState } from "react";
import { bridge, type AnswerResult, type CollectionRecord, type FilePreview, type ImageDeepAnalysis, type KnowledgeSpace, type ModelRuntimeState, type RagReadiness } from "../bridge";
import { displayPath } from "../utils/display-path";
import { PdfVisualPreview } from "../components/PdfVisualPreview";
import { ImageAssetGallery, imageAssetUrl } from "../components/ImageAssetGallery";

const locatorLabel = (locator: AnswerResult["claims"][number]["citations"][number]["locator"]) => {
  if (locator.page_no) return `第 ${locator.page_no} 页`;
  if (locator.slide_no) return `第 ${locator.slide_no} 张幻灯片`;
  if (locator.sheet_name) return `${locator.sheet_name}${locator.cell_range ? ` · ${locator.cell_range}` : ""}`;
  if (locator.paragraph_no) return `第 ${locator.paragraph_no} 段`;
  if (locator.line_start) return `第 ${locator.line_start} 行`;
  return "正文位置";
};

const highlightQuestionTerms = (text: string, question: string) => {
  const terms = (question.match(/[\p{L}\p{N}]{2,}/gu) ?? [])
    .flatMap((value) => value.split(/(?:关于|有关|哪些|什么|如何|是否|请问|请|的|了|是|在|中|和|与)+/u))
    .map((value) => value.trim())
    .filter((value) => value.length >= 2)
    .sort((left, right) => right.length - left.length);
  const unique = [...new Set(terms)];
  if (!unique.length) return text;
  const expression = new RegExp(`(${unique.map((value) => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "giu");
  const normalized = new Set(unique.map((value) => value.toLocaleLowerCase("zh-CN")));
  return text.split(expression).map((part, index) => normalized.has(part.toLocaleLowerCase("zh-CN"))
    ? <strong className="answer-keyword" key={`${part}-${index}`}>{part}</strong>
    : part);
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
  const [knowledgeSpaces, setKnowledgeSpaces] = useState<KnowledgeSpace[]>([]);
  const [collectionId, setCollectionId] = useState("");
  const [knowledgeSpaceId, setKnowledgeSpaceId] = useState("");
  const [preview, setPreview] = useState<FilePreview | null>(null);
  const [previewLoading, setPreviewLoading] = useState<string | null>(null);
  const [readiness, setReadiness] = useState<RagReadiness | null>(null);
  const [allowEvidenceExtracts, setAllowEvidenceExtracts] = useState(false);
  const [phase, setPhase] = useState<string | null>(null);
  const [deepAnalyses, setDeepAnalyses] = useState<Record<string, ImageDeepAnalysis>>({});
  const [deepAnalysisLoading, setDeepAnalysisLoading] = useState<string | null>(null);
  const hasGeneration = readiness?.generation_ready ?? model_state?.capabilities.generation === true;
  const hasEmbedding = readiness?.embedding_ready ?? model_state?.capabilities.embedding === true;
  const scope = useMemo(() => ({ knowledge_space_ids: knowledgeSpaceId ? [knowledgeSpaceId] : [], root_ids: [], collection_ids: collectionId ? [collectionId] : [], file_ids: [], extensions: [], modified_from: null, modified_to: null, availability: "present" as const }), [collectionId, knowledgeSpaceId]);

  useEffect(() => {
    void bridge.collection_list().then(setCollections).catch(() => setCollections([]));
    void bridge.knowledge_space_list().then(setKnowledgeSpaces).catch(() => setKnowledgeSpaces([]));
  }, []);

  useEffect(() => {
    let disposed = false;
    void bridge.rag_readiness_get(scope).then((result) => {
      if (!disposed) setReadiness(result);
    }).catch(() => {
      if (!disposed) setReadiness(null);
    });
    return () => { disposed = true; };
  }, [scope]);

  useEffect(() => {
    if (!window.__TAURI_INTERNALS__) return undefined;
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    void listen<{ operation_id: string; token: string }>("ask.token", (event) => {
      if (event.payload.operation_id === activeOperationRef.current) {
        setStreamedAnswer((current) => current + event.payload.token);
      }
    }).then((unlisten) => disposed ? unlisten() : unlisteners.push(unlisten));
    void listen<{ operation_id: string; phase: string }>("ask.phase", (event) => {
      if (event.payload.operation_id === activeOperationRef.current) setPhase(event.payload.phase);
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
    if (readiness && !readiness.ready && !allowEvidenceExtracts) {
      setError("完整 RAG 当前不可用。请先配置/完成索引，或明确选择下方的“证据摘录模式”。");
      return;
    }
    setLoading(true);
    setError(null);
    setPreview(null);
    try {
      const result = await bridge.ask_start({
        question: trimmed,
        session_id: answer?.session_id ?? null,
        scope,
        answer_style: "concise",
        retrieval_limit: 12,
        max_source_files: 8,
        strict_evidence: true,
        mode: readiness?.ready ? "rag" : "evidence_extracts",
        allow_degraded_extractive: !readiness?.ready && allowEvidenceExtracts,
      });
      activeOperationRef.current = result.operation_id;
      setActiveOperationId(result.operation_id);
      setStreamedAnswer("");
      setPhase("queued");
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
      setPhase(null);
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

  const analyzeOriginalImage = async (assetId: string) => {
    const currentQuestion = turns.at(-1)?.question ?? question.trim();
    if (!currentQuestion || deepAnalysisLoading) return;
    setDeepAnalysisLoading(assetId);
    setError(null);
    try {
      const result = await bridge.image_deep_analyze(assetId, currentQuestion);
      setDeepAnalyses((current) => ({ ...current, [assetId]: result }));
    } catch (analysisError) {
      setError(analysisError instanceof Error ? analysisError.message : String(analysisError));
    } finally {
      setDeepAnalysisLoading(null);
    }
  };

  return (
    <section className="page page--ask">
      <header className="page-heading"><div><h1>问资料</h1><p>回答只使用你的本地资料</p></div>{turns.length > 0 && !loading && <button type="button" className="text-button" onClick={() => { setTurns([]); setAnswer(null); setPreview(null); setDeepAnalyses({}); }}>新建会话</button>}</header>
      <div className="scope-bar">
        <FileSearchOutlined /> 检索范围：
        <select aria-label="问答范围" value={knowledgeSpaceId ? `space:${knowledgeSpaceId}` : collectionId ? `collection:${collectionId}` : ""} onChange={(event) => {
          const [kind, id = ""] = event.target.value.split(":", 2);
          setKnowledgeSpaceId(kind === "space" ? id : "");
          setCollectionId(kind === "collection" ? id : "");
        }}>
          <option value="">全部资料</option>
          {knowledgeSpaces.length > 0 && <optgroup label="知识空间">{knowledgeSpaces.map((item) => <option key={item.space_id} value={`space:${item.space_id}`}>{item.name}</option>)}</optgroup>}
          <optgroup label="集合">{collections.map((item) => <option key={item.collection_id} value={`collection:${item.collection_id}`}>{item.name}</option>)}</optgroup>
        </select>
        <span>{readiness?.ready ? `完整 RAG · 语义覆盖 ${Math.round(readiness.scope_index_coverage * 100)}%${readiness.pending_image_assets > 0 ? ` · 图片理解 ${Math.round(readiness.image_index_coverage * 100)}%` : ""}` : hasGeneration || hasEmbedding ? "完整 RAG 未就绪" : "仅证据摘录可用"}</span>
      </div>
      {readiness && !readiness.ready && <div className="rag-readiness" role="status">
        <div><strong>完整 RAG 暂不可用</strong><small>{readiness.blockers.map((blocker) => blocker.message).join("；")}</small></div>
        <button type="button" className={allowEvidenceExtracts ? "active" : ""} onClick={() => setAllowEvidenceExtracts((value) => !value)}>{allowEvidenceExtracts ? "已选择证据摘录模式" : "明确使用证据摘录模式"}</button>
      </div>}
      <div className={`conversation-area${answer ? " conversation-area--answered" : ""}`}>
        {!answer && <div className="conversation-empty">
          <h2>从你的资料中寻找答案</h2>
        </div>}
        {turns.slice(0, -1).map((turn, index) => <article className={`answer-card answer-card--${turn.answer.grounding_status}`} key={`${turn.answer.session_id}-${index}`}><header><span>你：{turn.question}</span><small>{turn.answer.elapsed_ms} ms</small></header><p className="answer-card__text">{highlightQuestionTerms(turn.answer.answer, turn.question)}</p></article>)}
        {answer && <article className={`answer-card answer-card--${answer.grounding_status}`}>
          <header><span>{answer.answer_mode === "extractive" ? "严格证据摘录" : "本地模型回答"}</span><small>{answer.elapsed_ms} ms · {answer.used_file_ids.length} 个来源文件</small></header>
          <p className="answer-card__text">{highlightQuestionTerms(answer.answer, turns.at(-1)?.question ?? question)}</p>
          {answer.claims.length > 0 && <div className="answer-claims">
            <h2>引用依据</h2>
            {answer.claims.map((claim, index) => <section key={claim.claim_id}>
              <p>{highlightQuestionTerms(claim.text, turns.at(-1)?.question ?? question)}</p>
              <div>{claim.citations.map((citation) => {
                const imageAssetId = citation.image_asset_id;
                const deepAnalysis = imageAssetId ? deepAnalyses[imageAssetId] : undefined;
                return <div className="answer-citation-group" key={citation.evidence_id}>
                  <button type="button" className={imageAssetId ? "answer-citation answer-citation--image" : "answer-citation"} onClick={() => void showPreview(citation.file_id, citation.node_id)}>
                    {imageAssetId && <img src={imageAssetUrl(imageAssetId)} alt="图片证据缩略图" loading="lazy" />}
                    [{index + 1}] {sourceNames.get(citation.file_id) ?? "本地资料"} · {locatorLabel(citation.locator)}{previewLoading === citation.file_id ? " · 载入中" : ""}
                  </button>
                  {imageAssetId && <button type="button" className="image-deep-analysis-button" disabled={deepAnalysisLoading !== null} onClick={() => void analyzeOriginalImage(imageAssetId)}>{deepAnalysisLoading === imageAssetId ? "正在分析原图…" : "深度分析原图"}</button>}
                  {deepAnalysis && <aside className="image-deep-analysis" aria-live="polite">
                    <strong>针对当前问题的原图分析</strong>
                    <p>{highlightQuestionTerms(deepAnalysis.answer, turns.at(-1)?.question ?? question)}</p>
                    {deepAnalysis.observations.length > 0 && <ul>{deepAnalysis.observations.map((observation) => <li key={observation}>{observation}</li>)}</ul>}
                    {deepAnalysis.uncertainties.length > 0 && <small>无法确认：{deepAnalysis.uncertainties.join("；")}</small>}
                  </aside>}
                </div>;
              })}</div>
            </section>)}
          </div>}
          {preview && <div className="answer-preview" aria-label={`${preview.file.display_name}原文预览`}>
      <header><strong>{preview.file.display_name}</strong><small>{displayPath(preview.file.display_path)}</small></header>
            {preview.file.extension.toLowerCase() === "pdf" && <PdfVisualPreview preview={preview} />}
            <ImageAssetGallery assets={preview.image_assets} />
            {preview.nodes.map((node) => <p key={node.node_id} className={node.node_id === preview.anchor_node_id ? "preview-node--anchor" : undefined}><small>{locatorLabel(node.locator)}</small>{highlightQuestionTerms(node.text ?? (node.table_data ? JSON.stringify(node.table_data) : ""), turns.at(-1)?.question ?? question)}</p>)}
            {preview.next_offset !== null && <button type="button" className="text-button" disabled={previewLoading === preview.file.file_id} onClick={() => void showPreview(preview.file.file_id, null, preview.next_offset ?? 0)}>继续载入</button>}
          </div>}
        </article>}
        {loading && <article className="answer-card answer-card--grounded" aria-live="polite"><header><span>{({ queued: "正在排队", understanding: "正在理解问题", hybrid_retrieval: "正在执行混合检索", evidence_retrieval: "正在检索证据摘录", evidence_selection: "正在筛选证据", generating: "正在本地生成", citation_validation: "正在逐句校验引用", completed: "已完成" } as Record<string,string>)[phase ?? "queued"] ?? "正在处理"}</span><small>本地处理</small></header>{streamedAnswer && <p className="answer-card__text">{highlightQuestionTerms(streamedAnswer, question)}</p>}</article>}
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
