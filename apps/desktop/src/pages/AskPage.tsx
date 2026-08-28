import { AudioOutlined, CloseOutlined, EllipsisOutlined, FileSearchOutlined, FileTextOutlined, QuestionCircleOutlined, SendOutlined, StopOutlined, UserOutlined, WarningOutlined } from "@ant-design/icons";
import { Dropdown, Input, Modal } from "antd";
import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { bridge, type AnswerResult, type AskSessionSummary, type CollectionRecord, type FilePreview, type ModelRuntimeState, type RagReadiness } from "../bridge";
import { displayPath } from "../utils/display-path";
import { extractQuestionTerms, highlightPlainTerms } from "../utils/query-terms";
import { PdfVisualPreview } from "../components/PdfVisualPreview";
import { OcrAttemptChain } from "../components/OcrAttemptChain";
import { ImageAssetGallery } from "../components/ImageAssetGallery";
import { confirmAction } from "../components/AppConfirm";
import { AppSelect } from "../components/AppSelect";
import { AskExecutionPanel } from "../features/ask/AskExecutionPanel";
import type { AskExecutionState } from "../features/ask/ask-execution-state";
import { errorMessage } from "../utils/app-error";
import { useAppStore, type AskTurn } from "../state/app-store";
import fanfanLogo from "../assets/fanfan-logo.png";

// 发送后的"即时占位"执行状态：后端需先获取模型租约（冷启动可能耗时数秒）才发首个
// node_started 事件，此段期间用该运行中的"理解问题"节点顶替显示，事件到达后无缝替换。
const placeholderExecution: AskExecutionState = {
  operation_id: "__placeholder",
  status: "running",
  nodes: [{
    node_id: "seed-understanding",
    node_name: "understanding",
    public_label: "理解问题",
    status: "running",
    public_summary: null,
    progress_lines: [],
    duration_ms: null,
    auto_expanded: true,
    user_expanded: false,
  }],
  active_node_id: "seed-understanding",
  streamed_answer: "",
  answer_started: false,
  answer_completed: false,
  step_count: 1,
  total_duration_ms: null,
  last_sequence: 0,
};

const locatorLabel = (locator: AnswerResult["claims"][number]["citations"][number]["locator"]) => {
  if (locator.page_no) return `第 ${locator.page_no} 页`;
  if (locator.slide_no) return `第 ${locator.slide_no} 张幻灯片`;
  if (locator.sheet_name) return `${locator.sheet_name}${locator.cell_range ? ` · ${locator.cell_range}` : ""}`;
  if (locator.paragraph_no) return `第 ${locator.paragraph_no} 段`;
  if (locator.line_start) return `第 ${locator.line_start} 行`;
  return "正文位置";
};

// 模型输出区（回答/引用/分析）：把问题关键词补成 markdown **加粗**，交给渲染器显示。
// 高亮前三层保护，确保不破坏模型输出的原样内容：
//   1) markdown 代码块 ``` ... ```；2) 行内代码 `...`；3) 模型已输出的 **加粗** 片段。
// 这样目录树符号、连接服务标识、代码段里的中文词不会被硬塞进 `**词**` 而破坏原样。
const highlightQuestionTerms = (text: string, question: string): string => {
  const unique = extractQuestionTerms(question);
  if (!unique.length) return text;
  const protectedParts: string[] = [];
  const mask = (match: string) => {
    protectedParts.push(match);
    return `\uE000${protectedParts.length - 1}\uE001`;
  };
  const masked = text
    .replace(/```[\s\S]*?```/g, mask)
    .replace(/`[^`\n]+`/g, mask)
    .replace(/\*\*([^*\n]+)\*\*/g, mask);
  const expression = new RegExp(`(${unique.map((value) => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "giu");
  const highlighted = masked.replace(expression, (_, term: string) => `**${term}**`);
  return highlighted.replace(/\uE000(\d+)\uE001/g, (_, index: string) => protectedParts[Number(index)] ?? "");
};

const MarkdownAnswer = ({ text, question }: { text: string; question: string }) => (
  <ReactMarkdown remarkPlugins={[remarkGfm]}>{highlightQuestionTerms(text, question)}</ReactMarkdown>
);

// 引用文件过多（超过阈值）时折叠为单个"引用了 N 个文件"胶囊，点击展开
const REFS_COLLAPSE_THRESHOLD = 3;

// 回答下方的引用文件标签：胶囊排列在操作栏（导出）最左侧；
// 点击文件胶囊展开该文件被引用的具体事实（润色句）与原文（quote），
// 每条可"查看原文"跳转整页预览。
function AnswerReferences({ answer, question, expanded, activeFile, onToggleRefs, onSelectFile, onOpenPreview }: {
  answer: AnswerResult;
  question: string;
  expanded: boolean;
  activeFile: string | null;
  onToggleRefs: () => void;
  onSelectFile: (fileId: string | null) => void;
  onOpenPreview: (fileId: string, nodeId: string) => void;
}) {
  const files = answer.source_files;
  if (files.length === 0) return null;
  const collapsed = files.length > REFS_COLLAPSE_THRESHOLD && !expanded;
  const citationsOf = (fileId: string) => answer.claims
    .flatMap((claim) => claim.citations.map((citation) => ({ claimText: claim.text, citation })))
    .filter((item) => item.citation.file_id === fileId);
  // 胶囊行与引用详情分离：详情作为 answer-actions 的独立换行子元素，
  // 展开时不再撑满/挤压操作行，操作按钮保持原尺寸。
  return (
    <>
      <div className="answer-refs">
        {collapsed ? (
          <button type="button" className="source-chip source-chip--collapse" onClick={onToggleRefs} title="展开全部引用文件">
            <FileTextOutlined /> 引用了 {files.length} 个文件
          </button>
        ) : (
          <>
            {files.length > REFS_COLLAPSE_THRESHOLD && (
              <button type="button" className="source-chip source-chip--collapse" onClick={onToggleRefs} title="折叠引用文件">
                <FileTextOutlined /> 引用了 {files.length} 个文件
              </button>
            )}
            {files.map((source) => (
              <button
                key={source.file_id}
                type="button"
                className={`source-chip${activeFile === source.file_id ? " source-chip--active" : ""}`}
                title={source.display_path}
                onClick={() => onSelectFile(activeFile === source.file_id ? null : source.file_id)}
              >
                <FileTextOutlined /> {source.display_name}
              </button>
            ))}
          </>
        )}
      </div>
      {activeFile !== null && (
        <div className="answer-ref-detail">
          <h2>{files.find((file) => file.file_id === activeFile)?.display_name ?? "引用详情"}</h2>
          {citationsOf(activeFile).map((item, index) => (
            <section key={`${item.citation.evidence_id}-${index}`}>
              <div className="markdown-body"><MarkdownAnswer text={item.claimText} question={question} /></div>
              <blockquote className="answer-ref-quote">{item.citation.quote}</blockquote>
              <div className="answer-ref-detail__meta">
                <small>{locatorLabel(item.citation.locator)}</small>
                <button type="button" className="text-button" onClick={() => onOpenPreview(item.citation.file_id, item.citation.node_id)}>查看原文</button>
              </div>
            </section>
          ))}
        </div>
      )}
    </>
  );
}


// 回答模式徽标：clarification/extract/find 等非普通回答显示模式标签，
// 让用户知道当前回答属于哪种处理结果（普通 RAG 回答与闲聊不显示）。
const ANSWER_MODE_LABELS: Record<string, string> = {
  clarification: "需要澄清",
  summary: "文档摘要",
  compare: "文档对比",
  extract: "结构化抽取",
  find: "定位文件",
  rag_refusal: "未找到资料",
  unverified: "未验证回答",
};

const resampleMono = (chunks: Float32Array[], sourceRate: number, targetRate = 16_000): number[] => {
  const sourceLength = chunks.reduce((total, chunk) => total + chunk.length, 0);
  const source = new Float32Array(sourceLength);
  let offset = 0;
  for (const chunk of chunks) {
    source.set(chunk, offset);
    offset += chunk.length;
  }
  if (sourceRate === targetRate) return Array.from(source);
  const ratio = sourceRate / targetRate;
  const output = new Array<number>(Math.max(1, Math.floor(source.length / ratio)));
  for (let index = 0; index < output.length; index += 1) {
    const position = index * ratio;
    const left = Math.floor(position);
    const right = Math.min(source.length - 1, left + 1);
    const fraction = position - left;
    output[index] = (source[left] ?? 0) * (1 - fraction) + (source[right] ?? 0) * fraction;
  }
  return output;
};

/** 用户消息气泡：虚拟小人头像 + 名字"我" */
const UserMessage = ({ text }: { text: string }) => (
  <div className="chat-message chat-message--user">
    <div className="chat-avatar chat-avatar--user"><UserOutlined /></div>
    <div className="chat-message__main">
      <span className="chat-message__name">我</span>
      <div className="chat-bubble chat-bubble--user">{text}</div>
    </div>
  </div>
);

/** 翻翻消息气泡容器：应用 logo 头像 + 名字"翻翻" */
const AssistantMessage = ({ children }: { children: ReactNode }) => (
  <div className="chat-message chat-message--assistant">
    <div className="chat-avatar chat-avatar--assistant"><img src={fanfanLogo} alt="翻翻" /></div>
    <div className="chat-message__main">
      <span className="chat-message__name">翻翻</span>
      {children}
    </div>
  </div>
);

export function AskPage({ model_state }: { model_state: ModelRuntimeState | null }) {
  // 问答会话（对话轮次、进行中状态、检索范围）放在全局 store，切换页面后回来仍保留
  const [question, setQuestion] = useState("");
  const turns = useAppStore((state) => state.ask_turns);
  const setTurns = useAppStore((state) => state.set_ask_turns);
  const pendingQuestion = useAppStore((state) => state.ask_pending_question);
  const setPendingQuestion = useAppStore((state) => state.set_ask_pending_question);
  const loading = useAppStore((state) => state.ask_loading);
  const setLoading = useAppStore((state) => state.set_ask_loading);
  const streamedAnswer = useAppStore((state) => state.ask_streamed_answer);
  const setStreamedAnswer = useAppStore((state) => state.set_ask_streamed_answer);
  const askExecution = useAppStore((state) => state.ask_execution);
  const toggleAskExecutionNode = useAppStore((state) => state.toggle_ask_execution_node);
  const finalizeAskExecution = useAppStore((state) => state.finalize_ask_execution);
  const rememberAskExecution = useAppStore((state) => state.remember_ask_execution);
  const activeSessionId = useAppStore((state) => state.ask_active_session_id);
  const setActiveSessionId = useAppStore((state) => state.set_ask_active_session_id);
  const scopeCollectionIds = useAppStore((state) => state.ask_scope_collection_ids);
  const setScopeCollectionIds = useAppStore((state) => state.set_ask_scope_collection_ids);
  const setAskOperationId = useAppStore((state) => state.set_ask_operation_id);
  const resetAskState = useAppStore((state) => state.reset_ask_state);
  const [sessions, setSessions] = useState<AskSessionSummary[]>([]);
  const [renameTarget, setRenameTarget] = useState<AskSessionSummary | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [lastFailedQuestion, setLastFailedQuestion] = useState<string | null>(null);
  const activeOperationRef = useRef<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [collections, setCollections] = useState<CollectionRecord[]>([]);
  const [preview, setPreview] = useState<FilePreview | null>(null);
  const [previewTargetMessageId, setPreviewTargetMessageId] = useState<string | null>(null);
  const [previewLoading, setPreviewLoading] = useState<string | null>(null);
  const [readiness, setReadiness] = useState<RagReadiness | null>(null);
  // 每条回答的引用文件标签状态：折叠区是否展开、正在查看引文的文件
  const [refsExpanded, setRefsExpanded] = useState<Record<string, boolean>>({});
  const [activeRefFile, setActiveRefFile] = useState<Record<string, string | null>>({});
  // 澄清场景下用户自定义输入：当系统给出的澄清选项都不符合预期时，
  // 用户可在选项下方输入自己想表达的问题，重新走完整链路。
  const [clarifCustom, setClarifCustom] = useState<Record<string, string>>({});
  const [recording, setRecording] = useState(false);
  const [recognizing, setRecognizing] = useState(false);
  const audioContextRef = useRef<AudioContext | null>(null);
  const recordingStreamRef = useRef<MediaStream | null>(null);
  const recordingProcessorRef = useRef<ScriptProcessorNode | null>(null);
  const recordingSourceRef = useRef<MediaStreamAudioSourceNode | null>(null);
  const recordingChunksRef = useRef<Float32Array[]>([]);
  const recordingTimerRef = useRef<number | null>(null);
  const scope = useMemo(() => ({ root_ids: [], collection_ids: scopeCollectionIds, file_ids: [], extensions: [], modified_from: null, modified_to: null, availability: "present" as const }), [scopeCollectionIds]);
  const addScopeCollection = (value: string) => {
    if (!value || scopeCollectionIds.includes(value)) return;
    setScopeCollectionIds([...scopeCollectionIds, value]);
  };
  const removeScopeCollection = (collectionId: string) => {
    setScopeCollectionIds(scopeCollectionIds.filter((id) => id !== collectionId));
  };

  const conversationRef = useRef<HTMLDivElement>(null);
  const composerRef = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    const element = conversationRef.current;
    if (!element) return;
    element.scrollTop = element.scrollHeight;
    // 内容溢出时才显示滚动条，否则隐藏
    const overflow = element.scrollHeight > element.clientHeight + 2;
    element.classList.toggle("conversation-area--scrollable", overflow);
  }, [turns, pendingQuestion, streamedAnswer, askExecution, loading, error, preview]);
  useEffect(() => {
    const element = conversationRef.current;
    if (!element) return;
    const check = () => {
      element.classList.toggle("conversation-area--scrollable", element.scrollHeight > element.clientHeight + 2);
    };
    const observer = new ResizeObserver(check);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  // 输入框自适应高度：内容增多时变长，最多 134px（与 CSS max-height 一致）
  const resizeComposer = () => {
    const element = composerRef.current;
    if (!element) return;
    element.style.height = "auto";
    element.style.height = `${Math.min(element.scrollHeight, 134)}px`;
  };
  useEffect(() => {
    resizeComposer();
  }, [question]);

  useEffect(() => {
    void bridge.collection_list().then(setCollections).catch(() => setCollections([]));
  }, []);

  const refreshSessions = async () => {
    const page = await bridge.ask_session_query(null, 30);
    setSessions(page.items);
  };

  const loadSession = async (sessionId: string, knownSession?: AskSessionSummary) => {
    if (loading) return;
    setError(null);
    try {
      const page = await bridge.ask_message_query(sessionId, null, 200);
      const loadedTurns: AskTurn[] = [];
      let pendingUser = "";
      let failedQuestion: string | null = null;
      const executionHistory = useAppStore.getState().ask_execution_history;
      for (const message of page.items) {
        if (message.role === "user") {
          pendingUser = message.content;
        } else if (message.answer && pendingUser) {
          loadedTurns.push({ question: pendingUser, answer: message.answer, execution: executionHistory[message.answer.message_id] ?? null });
          pendingUser = "";
        } else if (message.error) {
          failedQuestion = pendingUser || failedQuestion;
          pendingUser = "";
        }
      }
      // 只有最近一次消息是失败时才恢复红色警告与重试记录；
      // 历史失败（其后已有成功问答）不复活弹窗，避免"成功问答后弹窗仍在"。
      const lastMessage = page.items.at(-1);
      const lastIsFailure = lastMessage !== undefined && lastMessage.error !== null;
      if (lastIsFailure) {
        setError(`${lastMessage.error!.message}（${lastMessage.error!.code}）`);
      }
      setAskOperationId(null);
      setLoading(false);
      setStreamedAnswer("");
      useAppStore.setState({ ask_execution: null, ask_streamed_thinking: "", ask_think_mode: false, ask_active_phase: "queued" });
      setTurns(loadedTurns);
      setLastFailedQuestion(lastIsFailure ? failedQuestion : null);
      setActiveSessionId(sessionId);
      setPendingQuestion(null);
      setPreview(null);
      setPreviewTargetMessageId(null);
      const selected = knownSession ?? sessions.find((session) => session.session_id === sessionId);
      if (selected) {
        setScopeCollectionIds(selected.scope.collection_ids ?? []);
      }
    } catch (actionError) {
      setError(errorMessage(actionError));
    }
  };

  useEffect(() => {
    let disposed = false;
    void bridge.ask_session_query(null, 30).then((page) => {
      if (disposed) return;
      setSessions(page.items);
      const latest = page.items[0];
      // 若问答操作仍在进行（状态在全局 store 中，由提交循环继续驱动），不覆盖恢复中的会话
      const state = useAppStore.getState();
      const busy = state.ask_loading || state.ask_pending_question !== null;
      if (latest && !busy) void loadSession(latest.session_id, latest);
    }).catch((actionError) => {
      if (!disposed) setError(errorMessage(actionError));
    });
    return () => { disposed = true; };
    // Only restore the newest persisted session when this page is mounted.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startNewSession = () => {
    resetAskState();
    setPreview(null);
    setPreviewTargetMessageId(null);
    setError(null);
  };

  useEffect(() => {
    let disposed = false;
    void bridge.rag_readiness_get(scope).then((result) => {
      if (!disposed) setReadiness(result);
    }).catch(() => {
      if (!disposed) setReadiness(null);
    });
    return () => { disposed = true; };
  }, [scope]);


  const lastAnswer = turns.at(-1)?.answer;
  // 严格 RAG：生成、Embedding 与当前范围索引均就绪后才允许发送。
  const chatUnavailable = readiness !== null && !readiness.ready;

  // 澄清收拢：本次已选澄清（选文件/自定义输入）时携带被澄清回答的 message_id。
  // 完成后不再追加新轮次，而是用最终回答**原位替换**那条被澄清的轮次，
  // 使「提问→澄清→最终回答」在历史里始终是一个 turn。
  const submit = async (questionOverride?: string, clarificationSelection: string | null = null, clarificationMessageId: string | null = null) => {
    const trimmed = (questionOverride ?? question).trim();
    if (!trimmed || loading) return;
    if (chatUnavailable) {
      setError((readiness?.blockers ?? []).map((blocker) => blocker.message).join("；") || "完整 RAG 尚未就绪，请先配置生成模型、Embedding 并完成当前范围的语义索引。");
      setLastFailedQuestion(trimmed);
      return;
    }
    setLoading(true);
    setError(null);
    setLastFailedQuestion(null);
    setPreview(null);
    setPreviewTargetMessageId(null);
    setPendingQuestion(trimmed);
    setQuestion("");
    setStreamedAnswer("");
    useAppStore.setState({ ask_execution: null, ask_streamed_thinking: "", ask_think_mode: false, ask_active_phase: "queued" });
    try {
      const result = await bridge.ask_start({
        question: trimmed,
        session_id: activeSessionId ?? lastAnswer?.session_id ?? null,
        scope,
        answer_style: "concise",
        retrieval_limit: 12,
        max_source_files: 8,
        strict_evidence: true,
        clarification_selection: clarificationSelection,
        clarification_message_id: clarificationMessageId,
        think_mode: false,
      });
      activeOperationRef.current = result.operation_id;
      setAskOperationId(result.operation_id);
      while (activeOperationRef.current === result.operation_id) {
        const snapshot = await bridge.ask_operation_get(result.operation_id);
        if (snapshot.handle.status === "completed") {
          if (!snapshot.result) throw new Error("问答已完成，但结果不完整");
          // 问答成功即消除红色警告弹窗与重试记录（即使错误来自历史恢复或先前失败）
          setError(null);
          setLastFailedQuestion(null);
          // 函数式更新：避免闭包捕获的旧 turns 在异步等待期间覆盖其他路径新增的轮次。
          // 用局部 const 固定已窄化的 result（TS 回调内会丢失非空窄化）。
          const completedResult = snapshot.result;
          finalizeAskExecution(completedResult.elapsed_ms);
          const completedExecution = useAppStore.getState().ask_execution;
          rememberAskExecution(completedResult.message_id, completedExecution);
          // 澄清收拢：携带被澄清 message_id 时，用最终回答**原位替换**该轮
          // （不新增第二条用户提问，实现「提问→澄清→最终回答」为一个 turn）；
          // 否则照常追加新轮次。
          setTurns((current) => {
            if (!clarificationMessageId) return [...current, { question: trimmed, answer: completedResult, execution: completedExecution }];
            const index = current.findIndex((turn) => turn.answer.message_id === clarificationMessageId);
            if (index === -1) return [...current, { question: trimmed, answer: completedResult, execution: completedExecution }];
            // 原位覆写：后端已把最终回答 UPDATE 到原澄清消息上，这里对齐 message_id，
            // 保证会话内 / 历史加载 / 执行记忆三者指向同一消息 id。
            const next = [...current];
            next[index] = { question: trimmed, answer: { ...completedResult, message_id: clarificationMessageId }, execution: completedExecution };
            return next;
          });
          setActiveSessionId(completedResult.session_id);
          setPendingQuestion(null);
          activeOperationRef.current = null;
          setAskOperationId(null);
          setStreamedAnswer("");
          useAppStore.setState({ ask_execution: null, ask_streamed_thinking: "", ask_think_mode: false, ask_active_phase: "queued" });
          void refreshSessions().catch(() => undefined);
          break;
        }
        if (snapshot.handle.status === "failed" || snapshot.handle.status === "cancelled") {
          throw new Error(snapshot.error?.message ?? (snapshot.handle.status === "cancelled" ? "问答已取消" : "问答失败"));
        }
        await new Promise((resolve) => window.setTimeout(resolve, 120));
      }
    } catch (askError) {
      activeOperationRef.current = null;
      setAskOperationId(null);
      setStreamedAnswer("");
      useAppStore.setState({ ask_execution: null, ask_streamed_thinking: "", ask_think_mode: false, ask_active_phase: "queued" });
      setError(errorMessage(askError));
      setLastFailedQuestion(trimmed);
      void refreshSessions().catch(() => undefined);
    } finally {
      activeOperationRef.current = null;
      setAskOperationId(null);
      setLoading(false);
    }
  };

  // 澄清场景：系统给出的选项都不符合用户预期时，用用户自行输入的问题重新
  // 走完整链路（不带澄清选择，等同重新提问），并携带被澄清消息 id 原地收拢为同一轮。
  const submitCustomClarification = (messageId: string) => {
    const text = (clarifCustom[messageId] ?? "").trim();
    if (!text) return;
    void submit(text, null, messageId);
  };

  const showPreview = async (fileId: string, anchorNodeId: string | null = null, offset = 0, messageId: string | null = null) => {
    setPreviewLoading(fileId);
    setError(null);
    try {
      const page = await bridge.preview_get(fileId, offset, 80, anchorNodeId);
      setPreviewTargetMessageId((current) => messageId ?? current);
      setPreview((current) => current?.file.file_id === fileId && offset > 0
        ? { ...page, nodes: [...current.nodes, ...page.nodes], offset: current.offset, anchor_node_id: current.anchor_node_id }
        : page);
    } catch (previewError) {
      setError(errorMessage(previewError));
    } finally {
      setPreviewLoading(null);
    }
  };

  const releaseRecordingResources = async () => {
    if (recordingTimerRef.current !== null) window.clearTimeout(recordingTimerRef.current);
    recordingTimerRef.current = null;
    recordingProcessorRef.current?.disconnect();
    recordingSourceRef.current?.disconnect();
    recordingProcessorRef.current = null;
    recordingSourceRef.current = null;
    recordingStreamRef.current?.getTracks().forEach((track) => track.stop());
    recordingStreamRef.current = null;
    const context = audioContextRef.current;
    audioContextRef.current = null;
    if (context && context.state !== "closed") await context.close().catch(() => undefined);
  };

  const stopRecording = async (cancelled = false) => {
    const context = audioContextRef.current;
    const chunks = recordingChunksRef.current;
    recordingChunksRef.current = [];
    setRecording(false);
    await releaseRecordingResources();
    if (cancelled) return;
    if (!context || chunks.reduce((total, chunk) => total + chunk.length, 0) < context.sampleRate / 4) {
      setError("录音时间太短，请至少说话约一秒后再结束。");
      return;
    }
    setRecognizing(true);
    setError(null);
    try {
      const result = await bridge.speech_recognize({
        samples: resampleMono(chunks, context.sampleRate),
        sample_rate: 16_000,
      });
      if (!result.result.text.trim()) {
        setError("没有识别到清晰语音，你可以重新录音或直接输入文字。");
      } else {
        setQuestion(result.result.text);
        composerRef.current?.focus();
      }
    } catch (actionError) {
      setError(errorMessage(actionError));
    } finally {
      setRecognizing(false);
    }
  };

  const startRecording = async () => {
    if (recording || recognizing) return;
    if (!model_state?.capabilities.asr) {
      setError("尚未配置可用的语音识别模型，请先到本地模型中配置 ASR。");
      return;
    }
    if (!navigator.mediaDevices?.getUserMedia) {
      setError("当前系统没有提供麦克风录音接口。");
      return;
    }
    setError(null);
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: { channelCount: 1, echoCancellation: true, noiseSuppression: true }, video: false });
      const context = new AudioContext({ latencyHint: "interactive" });
      const source = context.createMediaStreamSource(stream);
      const processor = context.createScriptProcessor(4096, 1, 1);
      const chunks: Float32Array[] = [];
      processor.onaudioprocess = (event) => chunks.push(new Float32Array(event.inputBuffer.getChannelData(0)));
      source.connect(processor);
      processor.connect(context.destination);
      audioContextRef.current = context;
      recordingStreamRef.current = stream;
      recordingSourceRef.current = source;
      recordingProcessorRef.current = processor;
      recordingChunksRef.current = chunks;
      setRecording(true);
      recordingTimerRef.current = window.setTimeout(() => void stopRecording(false), 60_000);
    } catch (actionError) {
      await releaseRecordingResources();
      setError(`无法开始录音：${errorMessage(actionError)}`);
    }
  };

  useEffect(() => () => {
    if (recordingTimerRef.current !== null) window.clearTimeout(recordingTimerRef.current);
    recordingProcessorRef.current?.disconnect();
    recordingSourceRef.current?.disconnect();
    recordingStreamRef.current?.getTracks().forEach((track) => track.stop());
    void audioContextRef.current?.close();
  }, []);

  const openRename = (session: AskSessionSummary) => {
    setRenameValue(session.title);
    setRenameTarget(session);
  };

  const renameSession = async () => {
    if (!renameTarget) return;
    const title = renameValue.trim();
    if (title.length < 1 || title.length > 80) return;
    try {
      await bridge.ask_session_rename(renameTarget.session_id, title);
      setRenameTarget(null);
      await refreshSessions();
    } catch (actionError) {
      setError(errorMessage(actionError));
    }
  };

  const deleteSession = async (session: AskSessionSummary) => {
    const confirmed = await confirmAction({
      actionKey: `ask_session_delete_${session.session_id.slice(0, 8)}`,
      title: "删除这段问答记录？",
      description: "只删除翻翻数据库中的会话与回答记录，不会修改任何源文件或知识库内容。",
      confirmLabel: "删除会话",
      danger: true,
    });
    if (!confirmed) return;
    try {
      await bridge.ask_session_delete(session.session_id);
      if (activeSessionId === session.session_id) startNewSession();
      await refreshSessions();
    } catch (actionError) {
      setError(errorMessage(actionError));
    }
  };

  return (
    <section className="page page--ask">
      <div className="ask-session-toolbar">
        <AppSelect ariaLabel="最近问答会话" value={activeSessionId ?? ""} onChange={(value) => value ? void loadSession(value) : startNewSession()} labelRender={({ value }) => {
          // 选中项标题兜底：活动会话不在已加载列表（刷新失败或分页截断）时，
          // 防止 antd 直接把 session_id 显示出来；标题与后端一致取首问文本
          if (value === "") return "新对话";
          const session = sessions.find((item) => item.session_id === value);
          if (session) return <span className="session-option__title">{session.title}</span>;
          const firstTurn = turns[0];
          if (firstTurn?.answer.session_id === value) return <span className="session-option__title">{firstTurn.question}</span>;
          return <span className="session-option__title">当前会话</span>;
        }} options={[
          { value: "", label: "新对话" },
          ...sessions.map((session) => ({
            value: session.session_id,
            label: (
              <div className="session-option">
                <span className="session-option__title">{session.title}</span>
                <span className="session-option__actions" onMouseDown={(event) => { event.preventDefault(); event.stopPropagation(); }} onClick={(event) => event.stopPropagation()}>
                  <Dropdown trigger={["hover"]} placement="bottomRight" styles={{ root: { zIndex: 1100 } }} menu={{ items: [
                    { key: "rename", label: "重命名", onClick: () => openRename(session) },
                    { key: "delete", label: "删除", danger: true, onClick: () => void deleteSession(session) },
                  ] }}>
                    <button type="button" aria-label={`管理会话“${session.title}”`}><EllipsisOutlined /></button>
                  </Dropdown>
                </span>
              </div>
            ),
          })),
        ]} />
        <div className="scope-tags">
          {scopeCollectionIds.map((collectionId) => {
            const collection = collections.find((item) => item.collection_id === collectionId);
            return <span className="scope-tag" key={collectionId}>
              {collection?.name ?? "已删除的集合"}
              <button type="button" aria-label={`移除集合“${collection?.name ?? collectionId}”`} onClick={() => removeScopeCollection(collectionId)}><CloseOutlined /></button>
            </span>;
          })}
        </div>
        {readiness && !readiness.ready && <small className="rag-status-inline">完整 RAG 尚未就绪 · 语义覆盖 {Math.round(readiness.scope_index_coverage * 100)}% · 配置完成后才能发送</small>}
        <AppSelect className="ask-scope-select" ariaLabel="选择检索范围" value="" showSearch onChange={addScopeCollection} labelRender={() => (
          scopeCollectionIds.length === 0
            ? <span>全部资料</span>
            : <span className="scope-select-trigger"><FileSearchOutlined /> 检索范围</span>
        )} options={[
          ...collections.filter((item) => !scopeCollectionIds.includes(item.collection_id)).map((item) => ({ value: item.collection_id, label: item.name })),
        ]} />
      </div>
      <Modal
        open={renameTarget !== null}
        title="重命名会话"
        className="app-confirm"
        centered
        okText="保存名称"
        cancelText="取消"
        onOk={() => void renameSession()}
        onCancel={() => setRenameTarget(null)}
      >
        <div className="app-confirm__content">
          <Input aria-label="会话名称" maxLength={80} value={renameValue} onChange={(event) => setRenameValue(event.target.value)} onPressEnter={() => void renameSession()} placeholder="输入新的会话名称（最多80字）" />
        </div>
      </Modal>
      <div className="conversation-area" ref={conversationRef}>
        {turns.length === 0 && !pendingQuestion && <div className="page-empty">
          <QuestionCircleOutlined />
          <h2>从你的资料中寻找答案</h2>
        </div>}
        {turns.map((turn, index) => {
          return <Fragment key={`${turn.answer.session_id}-${index}`}>
            <UserMessage text={turn.question} />
            <AssistantMessage>
              <div className="chat-bubble chat-bubble--assistant">
                {turn.execution && <AskExecutionPanel execution={turn.execution} />}
                <div className="markdown-body"><MarkdownAnswer text={turn.answer.answer} question={turn.question} /></div>
                {turn.answer.answer_mode === "clarification" && turn.answer.clarification && (
                  <div className="clarification-options">
                    <p className="clarification-options__reason">{turn.answer.clarification.reason}</p>
                    <div className="clarification-options__list">
                      {turn.answer.clarification.options.map((option) => (
                        <button
                          key={option.file_id}
                          type="button"
                          disabled={loading}
                          onClick={() => void submit(turn.question, option.file_id, turn.answer.message_id)}
                        >
                          <strong>{option.display_name}</strong>
                          {option.document_type && <small>{option.document_type}</small>}
                          {option.signals.length > 0 && <span className="clarification-option__signals">{option.signals.join(" · ")}</span>}
                        </button>
                      ))}
                    </div>
                    {!loading && (
                      <div className="clarification-custom">
                        <label htmlFor={`clarif-custom-${turn.answer.message_id}`}>
                          以上都不符合，输入你想表达的问题：
                        </label>
                        <div className="clarification-custom__row">
                          <input
                            id={`clarif-custom-${turn.answer.message_id}`}
                            type="text"
                            value={clarifCustom[turn.answer.message_id] ?? ""}
                            autoComplete="off"
                            placeholder="直接输入你的问题，重新帮我看……"
                            onChange={(event) => setClarifCustom((prev) => ({ ...prev, [turn.answer.message_id]: event.target.value }))}
                            onKeyDown={(event) => {
                              if (event.key === "Enter") submitCustomClarification(turn.answer.message_id);
                            }}
                          />
                          <button
                            type="button"
                            disabled={!(clarifCustom[turn.answer.message_id] ?? "").trim()}
                            onClick={() => submitCustomClarification(turn.answer.message_id)}
                          >
                            重新提问
                          </button>
                        </div>
                      </div>
                    )}
                  </div>
                )}
                <div className="answer-actions">
                  {turn.answer.source_files.length === 0 && turn.answer.answer_mode !== "generated" && turn.answer.answer_mode !== "chat" && (
                    <span
                      className={`answer-mode-chip answer-mode-chip--inline answer-mode-chip--${turn.answer.answer_mode}`}
                      title={turn.answer.answer_mode === "rag_refusal" ? "当前资料中未找到足够依据" : undefined}
                    >
                      {ANSWER_MODE_LABELS[turn.answer.answer_mode] ?? turn.answer.answer_mode}
                    </span>
                  )}
                  <AnswerReferences
                    answer={turn.answer}
                    question={turn.question}
                    expanded={refsExpanded[turn.answer.message_id] ?? false}
                    activeFile={activeRefFile[turn.answer.message_id] ?? null}
                    onToggleRefs={() => setRefsExpanded((prev) => ({ ...prev, [turn.answer.message_id]: !(prev[turn.answer.message_id] ?? false) }))}
                    onSelectFile={(fileId) => setActiveRefFile((prev) => ({ ...prev, [turn.answer.message_id]: fileId }))}
                    onOpenPreview={(fileId, nodeId) => void showPreview(fileId, nodeId, 0, turn.answer.message_id)}
                  />
                </div>
                {preview && previewTargetMessageId === turn.answer.message_id && <div className="answer-preview" aria-label={`${preview.file.display_name}原文预览`}>
                  <header><strong>{preview.file.display_name}</strong><small>{displayPath(preview.file.display_path)}</small></header>
                  {preview.file.extension.toLowerCase() === "pdf" && <PdfVisualPreview preview={preview} />}
                  <OcrAttemptChain attempts={preview.ocr_attempts} />
                  <ImageAssetGallery assets={preview.image_assets} />
                  {preview.nodes.map((node) => <p key={node.node_id} className={node.node_id === preview.anchor_node_id ? "preview-node--anchor" : undefined}><small>{locatorLabel(node.locator)}</small>{highlightPlainTerms(node.text ?? (node.table_data ? JSON.stringify(node.table_data) : ""), turn.question)}</p>)}
                  {preview.next_offset !== null && <button type="button" className="text-button" disabled={previewLoading === preview.file.file_id} onClick={() => void showPreview(preview.file.file_id, null, preview.next_offset ?? 0, turn.answer.message_id)}>继续载入</button>}
                </div>}
              </div>
            </AssistantMessage>
          </Fragment>;
        })}
        {pendingQuestion && <UserMessage text={pendingQuestion} />}
        {loading && <AssistantMessage>
          <div className="chat-bubble chat-bubble--assistant" aria-live="polite">
            {askExecution && askExecution.nodes.length > 0
              ? <AskExecutionPanel execution={askExecution} onToggleNode={toggleAskExecutionNode} />
              : <AskExecutionPanel execution={placeholderExecution} />}
            {streamedAnswer ? <div className="markdown-body"><MarkdownAnswer text={streamedAnswer} question={pendingQuestion ?? ""} /></div> : <span className="chat-typing"><i /><i /><i /></span>}
          </div>
        </AssistantMessage>}
        {error && <AssistantMessage>
          <div className="chat-bubble chat-bubble--error"><WarningOutlined /> <span>{error}</span>{lastFailedQuestion && !loading && <button type="button" className="text-button" onClick={() => void submit(lastFailedQuestion)}>重试本次提问</button>}</div>
        </AssistantMessage>}
      </div>
      <form className="ask-composer" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
        <div className="ask-composer__controls">
          <textarea ref={composerRef} value={question} onChange={(event) => setQuestion(event.target.value)} onInput={resizeComposer} onKeyDown={(event) => {
            if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
              event.preventDefault();
              void submit();
            }
          }} placeholder={recording ? "正在录音，再次点击麦克风结束…" : recognizing ? "正在本地识别语音…" : "基于我的资料提问…"} disabled={recording || recognizing} />
        </div>
        {recording && <button type="button" className="ask-composer__cancel-recording" onClick={() => void stopRecording(true)}>取消</button>}
        <button
          type="button"
          className={recording ? "ask-composer__voice ask-composer__voice--recording" : "ask-composer__voice"}
          aria-label={recording ? "结束录音" : "开始录音"}
          disabled={recognizing || loading}
          onClick={() => recording ? void stopRecording(false) : void startRecording()}
        >{recording ? <StopOutlined /> : <AudioOutlined />}</button>
        <button type="submit" aria-label="发送" disabled={loading || !question.trim() || chatUnavailable}><SendOutlined /></button>
      </form>
    </section>
  );
}
