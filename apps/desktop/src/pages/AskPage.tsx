import { AudioOutlined, CaretRightOutlined, CloseOutlined, DownloadOutlined, EllipsisOutlined, FileSearchOutlined, PauseOutlined, QuestionCircleOutlined, SendOutlined, SoundOutlined, StopOutlined, UserOutlined, WarningOutlined } from "@ant-design/icons";
import { Dropdown, Input, Modal } from "antd";
import { isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { save } from "@tauri-apps/plugin-dialog";
import { Fragment, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { bridge, type AnswerResult, type AskSessionSummary, type CollectionRecord, type FilePreview, type ImageDeepAnalysis, type ModelRuntimeState, type RagReadiness } from "../bridge";
import { RUNTIME_EVENTS } from "../bridge/runtime-events";
import { displayPath } from "../utils/display-path";
import { extractQuestionTerms, highlightPlainTerms } from "../utils/query-terms";
import { PdfVisualPreview } from "../components/PdfVisualPreview";
import { OcrAttemptChain } from "../components/OcrAttemptChain";
import { ImageAssetGallery, imageAssetUrl } from "../components/ImageAssetGallery";
import { confirmAction } from "../components/AppConfirm";
import { AppSelect } from "../components/AppSelect";
import { errorMessage } from "../utils/app-error";
import fanfanLogo from "../assets/fanfan-logo.png";

const locatorLabel = (locator: AnswerResult["claims"][number]["citations"][number]["locator"]) => {
  if (locator.page_no) return `第 ${locator.page_no} 页`;
  if (locator.slide_no) return `第 ${locator.slide_no} 张幻灯片`;
  if (locator.sheet_name) return `${locator.sheet_name}${locator.cell_range ? ` · ${locator.cell_range}` : ""}`;
  if (locator.paragraph_no) return `第 ${locator.paragraph_no} 段`;
  if (locator.line_start) return `第 ${locator.line_start} 行`;
  return "正文位置";
};

// 模型输出区（回答/引用/分析）：把问题关键词补成 markdown **加粗**，交给渲染器显示；
// 模型已输出的 **加粗** 片段先保护起来，避免重复包裹。
const highlightQuestionTerms = (text: string, question: string): string => {
  const unique = extractQuestionTerms(question);
  if (!unique.length) return text;
  const protectedParts: string[] = [];
  const masked = text.replace(/\*\*([^*\n]+)\*\*/g, (match) => {
    protectedParts.push(match);
    return `\uE000${protectedParts.length - 1}\uE001`;
  });
  const expression = new RegExp(`(${unique.map((value) => value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "giu");
  const highlighted = masked.replace(expression, (_, term: string) => `**${term}**`);
  return highlighted.replace(/\uE000(\d+)\uE001/g, (_, index: string) => protectedParts[Number(index)] ?? "");
};

const MarkdownAnswer = ({ text, question }: { text: string; question: string }) => (
  <ReactMarkdown remarkPlugins={[remarkGfm]}>{highlightQuestionTerms(text, question)}</ReactMarkdown>
);

const ASK_PHASE_LABELS: Record<string, string> = {
  queued: "正在进入本地问答队列",
  understanding: "正在理解问题",
  evidence_retrieval: "正在查找原文证据",
  hybrid_retrieval: "正在执行混合检索",
  reranking: "正在重排候选证据",
  evidence_selection: "正在筛选证据",
  image_reanalysis: "正在按当前问题复核候选原图",
  generating: "正在依据证据组织回答",
  citation_validation: "正在逐句核验引用",
  citation_structure_repair: "正在修复引用格式",
  completed: "回答已完成",
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
      <div className="chat-bubble chat-bubble--user">{highlightPlainTerms(text, text)}</div>
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
  const [question, setQuestion] = useState("");
  const [turns, setTurns] = useState<Array<{ question: string; answer: AnswerResult }>>([]);
  const [pendingQuestion, setPendingQuestion] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [streamedAnswer, setStreamedAnswer] = useState("");
  const [activePhase, setActivePhase] = useState("queued");
  const [sessions, setSessions] = useState<AskSessionSummary[]>([]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [renameTarget, setRenameTarget] = useState<AskSessionSummary | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [lastFailedQuestion, setLastFailedQuestion] = useState<string | null>(null);
  const activeOperationRef = useRef<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [collections, setCollections] = useState<CollectionRecord[]>([]);
  const [scopeCollectionIds, setScopeCollectionIds] = useState<string[]>([]);
  const [preview, setPreview] = useState<FilePreview | null>(null);
  const [previewLoading, setPreviewLoading] = useState<string | null>(null);
  const [readiness, setReadiness] = useState<RagReadiness | null>(null);
  const [deepAnalyses, setDeepAnalyses] = useState<Record<string, ImageDeepAnalysis>>({});
  const [deepAnalysisLoading, setDeepAnalysisLoading] = useState<string | null>(null);
  const [exportingMessageId, setExportingMessageId] = useState<string | null>(null);
  const [exportMessage, setExportMessage] = useState<string | null>(null);
  const [recording, setRecording] = useState(false);
  const [recognizing, setRecognizing] = useState(false);
  const [speakingMessageId, setSpeakingMessageId] = useState<string | null>(null);
  const [speechLoadingMessageId, setSpeechLoadingMessageId] = useState<string | null>(null);
  const [speechPaused, setSpeechPaused] = useState(false);
  const audioContextRef = useRef<AudioContext | null>(null);
  const recordingStreamRef = useRef<MediaStream | null>(null);
  const recordingProcessorRef = useRef<ScriptProcessorNode | null>(null);
  const recordingSourceRef = useRef<MediaStreamAudioSourceNode | null>(null);
  const recordingChunksRef = useRef<Float32Array[]>([]);
  const recordingTimerRef = useRef<number | null>(null);
  const playbackRef = useRef<HTMLAudioElement | null>(null);
  const scope = useMemo(() => ({ root_ids: [], collection_ids: scopeCollectionIds, file_ids: [], extensions: [], modified_from: null, modified_to: null, availability: "present" as const }), [scopeCollectionIds]);
  const addScopeCollection = (value: string) => {
    if (!value || scopeCollectionIds.includes(value)) return;
    setScopeCollectionIds((current) => [...current, value]);
  };
  const removeScopeCollection = (collectionId: string) => {
    setScopeCollectionIds((current) => current.filter((id) => id !== collectionId));
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
  }, [turns, pendingQuestion, streamedAnswer, loading, error, preview, deepAnalyses]);
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
      const loadedTurns: Array<{ question: string; answer: AnswerResult }> = [];
      let pendingUser = "";
      let failedQuestion: string | null = null;
      for (const message of page.items) {
        if (message.role === "user") {
          pendingUser = message.content;
        } else if (message.answer && pendingUser) {
          loadedTurns.push({ question: pendingUser, answer: message.answer });
          pendingUser = "";
        } else if (message.error) {
          failedQuestion = pendingUser || failedQuestion;
          setError(`${message.error.message}（${message.error.code}）`);
          pendingUser = "";
        }
      }
      setTurns(loadedTurns);
      setLastFailedQuestion(failedQuestion);
      setActiveSessionId(sessionId);
      setPendingQuestion(null);
      setPreview(null);
      setDeepAnalyses({});
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
      if (latest) void loadSession(latest.session_id, latest);
    }).catch((actionError) => {
      if (!disposed) setError(errorMessage(actionError));
    });
    return () => { disposed = true; };
    // Only restore the newest persisted session when this page is mounted.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const startNewSession = () => {
    setActiveSessionId(null);
    setTurns([]);
    setPendingQuestion(null);
    setLastFailedQuestion(null);
    setPreview(null);
    setDeepAnalyses({});
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

  useEffect(() => {
    if (!window.__TAURI_INTERNALS__) return undefined;
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    void listen<{ operation_id: string; token: string }>(RUNTIME_EVENTS.askToken, (event) => {
      if (event.payload.operation_id === activeOperationRef.current) {
        setStreamedAnswer((current) => current + event.payload.token);
      }
    }).then((unlisten) => disposed ? unlisten() : unlisteners.push(unlisten));
    void listen<{ operation_id: string; phase: string; progress: number }>(RUNTIME_EVENTS.askPhase, (event) => {
      if (event.payload.operation_id === activeOperationRef.current) {
        setActivePhase(event.payload.phase);
      }
    }).then((unlisten) => disposed ? unlisten() : unlisteners.push(unlisten));
    return () => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, []);

  const lastAnswer = turns.at(-1)?.answer;
  const lastQuestion = turns.at(-1)?.question ?? pendingQuestion ?? "";
  const sourceNames = useMemo(
    () => new Map(lastAnswer?.source_files.map((source) => [source.file_id, source.display_name]) ?? []),
    [lastAnswer],
  );

  const submit = async (questionOverride?: string) => {
    const trimmed = (questionOverride ?? question).trim();
    if (!trimmed || loading) return;
    if (readiness && !readiness.ready) {
      setError(readiness.blockers.map((blocker) => blocker.message).join("；") || "完整 RAG 尚未就绪，请先配置生成模型、Embedding 并完成当前范围的语义索引。");
      setLastFailedQuestion(trimmed);
      return;
    }
    setLoading(true);
    setError(null);
    setLastFailedQuestion(null);
    setPreview(null);
    setPendingQuestion(trimmed);
    setQuestion("");
    setActivePhase("queued");
    try {
      const result = await bridge.ask_start({
        question: trimmed,
        session_id: activeSessionId ?? lastAnswer?.session_id ?? null,
        scope,
        answer_style: "concise",
        retrieval_limit: 12,
        max_source_files: 8,
        strict_evidence: true,
      });
      activeOperationRef.current = result.operation_id;
      setActivePhase("understanding");
      setStreamedAnswer("");
      while (activeOperationRef.current === result.operation_id) {
        const snapshot = await bridge.ask_operation_get(result.operation_id);
        if (snapshot.handle.status === "completed") {
          if (!snapshot.result) throw new Error("问答已完成，但结果不完整");
          setTurns((current) => [...current, { question: trimmed, answer: snapshot.result! }]);
          setActiveSessionId(snapshot.result.session_id);
          setPendingQuestion(null);
          void refreshSessions().catch(() => undefined);
          activeOperationRef.current = null;
          break;
        }
        if (snapshot.handle.status === "failed" || snapshot.handle.status === "cancelled") {
          throw new Error(snapshot.error?.message ?? (snapshot.handle.status === "cancelled" ? "问答已取消" : "问答失败"));
        }
        await new Promise((resolve) => window.setTimeout(resolve, 120));
      }
    } catch (askError) {
      setError(errorMessage(askError));
      setLastFailedQuestion(trimmed);
      void refreshSessions().catch(() => undefined);
    } finally {
      activeOperationRef.current = null;
      setActivePhase("queued");
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
      setError(errorMessage(previewError));
    } finally {
      setPreviewLoading(null);
    }
  };

  const analyzeOriginalImage = async (assetId: string) => {
    if (!lastQuestion || deepAnalysisLoading) return;
    setDeepAnalysisLoading(assetId);
    setError(null);
    try {
      const result = await bridge.image_deep_analyze(assetId, lastQuestion);
      setDeepAnalyses((current) => ({ ...current, [assetId]: result }));
    } catch (analysisError) {
      setError(errorMessage(analysisError));
    } finally {
      setDeepAnalysisLoading(null);
    }
  };

  const exportAnswer = async (answer: AnswerResult) => {
    setError(null); setExportMessage(null);
    if (!isTauri()) { setError("浏览器预览不会写入电脑文件，请在翻翻桌面程序中导出。"); return; }
    const target = await save({ title: "导出翻翻问答结果（只新建，不覆盖）", defaultPath: "翻翻问答结果.md", filters: [{ name: "Markdown", extensions: ["md"] }, { name: "纯文本", extensions: ["txt"] }] });
    if (typeof target !== "string") return;
    if (!await confirmAction({ actionKey: "answer_export", title: "导出当前回答？", description: "只会新建一个包含已验证回答与引用的文件；目标已存在时会拒绝覆盖，源文件不会发生任何变化。", confirmLabel: "新建导出文件" })) return;
    const format = target.toLocaleLowerCase().endsWith(".txt") ? "txt" : "md";
    setExportingMessageId(answer.message_id);
    try {
      const result = await bridge.answer_export(answer.message_id, target, format, "EXPORT_NEW_FILE");
      setExportMessage(`已新建导出文件 · ${(result.size_bytes / 1024).toFixed(1)} KB · SHA-256 ${result.sha256.slice(0, 12)}…`);
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setExportingMessageId(null); }
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

  const stopPlayback = () => {
    const playback = playbackRef.current;
    if (playback) {
      playback.pause();
      playback.currentTime = 0;
    }
    playbackRef.current = null;
    setSpeakingMessageId(null);
    setSpeechPaused(false);
    setSpeechLoadingMessageId(null);
  };

  const readAnswer = async (answer: AnswerResult) => {
    if (speakingMessageId === answer.message_id && playbackRef.current) {
      if (playbackRef.current.paused) {
        await playbackRef.current.play();
        setSpeechPaused(false);
      } else {
        playbackRef.current.pause();
        setSpeechPaused(true);
      }
      return;
    }
    if (!model_state?.capabilities.tts) {
      setError("尚未配置可用的语音合成模型，请先到本地模型中配置 TTS。");
      return;
    }
    stopPlayback();
    setError(null);
    setSpeakingMessageId(answer.message_id);
    setSpeechLoadingMessageId(answer.message_id);
    try {
      const session = await bridge.speech_synthesize_answer(answer.message_id, 1.0, 0);
      const playback = new Audio(`data:audio/wav;base64,${session.result.audio_base64}`);
      playback.onended = stopPlayback;
      playback.onerror = () => {
        stopPlayback();
        setError("生成的语音无法播放，请重试。");
      };
      playbackRef.current = playback;
      await playback.play();
      setSpeechLoadingMessageId(null);
    } catch (actionError) {
      stopPlayback();
      setSpeechLoadingMessageId(null);
      setError(errorMessage(actionError));
    }
  };

  useEffect(() => () => {
    if (recordingTimerRef.current !== null) window.clearTimeout(recordingTimerRef.current);
    recordingProcessorRef.current?.disconnect();
    recordingSourceRef.current?.disconnect();
    recordingStreamRef.current?.getTracks().forEach((track) => track.stop());
    void audioContextRef.current?.close();
    playbackRef.current?.pause();
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
        <AppSelect ariaLabel="最近问答会话" value={activeSessionId ?? ""} onChange={(value) => value ? void loadSession(value) : startNewSession()} options={[
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
          <p>回答只依据你的本地资料，每句话都标明来源，确保可追溯可复核。</p>
        </div>}
        {turns.map((turn, index) => {
          const isLast = index === turns.length - 1;
          return <Fragment key={`${turn.answer.session_id}-${index}`}>
            <UserMessage text={turn.question} />
            <AssistantMessage>
              <div className="chat-bubble chat-bubble--assistant">
                <div className="markdown-body"><MarkdownAnswer text={turn.answer.answer} question={turn.question} /></div>
                <div className="answer-actions">
                  <button type="button" disabled={exportingMessageId !== null} onClick={() => void exportAnswer(turn.answer)}><DownloadOutlined /> {exportingMessageId === turn.answer.message_id ? "正在导出" : "导出当前回答"}</button>
                  <button type="button" disabled={speechLoadingMessageId === turn.answer.message_id} onClick={() => void readAnswer(turn.answer)}>
                    {speechLoadingMessageId === turn.answer.message_id ? <><SoundOutlined /> 正在生成语音</> : speakingMessageId === turn.answer.message_id ? speechPaused ? <><CaretRightOutlined /> 继续朗读</> : <><PauseOutlined /> 暂停朗读</> : <><SoundOutlined /> 朗读</>}
                  </button>
                  {speakingMessageId === turn.answer.message_id && <button type="button" onClick={stopPlayback}><StopOutlined /> 停止</button>}
                </div>
                {isLast && turn.answer.claims.length > 0 && <div className="answer-claims">
                  <h2>引用依据</h2>
                  {turn.answer.claims.map((claim) => <section key={claim.claim_id}>
                    <div className="markdown-body"><MarkdownAnswer text={claim.text} question={turn.question} /></div>
                    <div>{claim.citations.map((citation, claimIndex) => {
                      const imageAssetId = citation.image_asset_id;
                      const deepAnalysis = imageAssetId ? deepAnalyses[imageAssetId] : undefined;
                      return <div className="answer-citation-group" key={citation.evidence_id}>
                        <button type="button" className={imageAssetId ? "answer-citation answer-citation--image" : "answer-citation"} onClick={() => void showPreview(citation.file_id, citation.node_id)}>
                          {imageAssetId && <img src={imageAssetUrl(imageAssetId)} alt="图片证据缩略图" loading="lazy" />}
                          [{claimIndex + 1}] {sourceNames.get(citation.file_id) ?? "本地资料"} · {locatorLabel(citation.locator)}{previewLoading === citation.file_id ? " · 载入中" : ""}
                        </button>
                        {imageAssetId && <button type="button" className="image-deep-analysis-button" disabled={deepAnalysisLoading !== null} onClick={() => void analyzeOriginalImage(imageAssetId)}>{deepAnalysisLoading === imageAssetId ? "正在分析原图…" : "深度分析原图"}</button>}
                        {deepAnalysis && <aside className="image-deep-analysis" aria-live="polite">
                          <strong>针对当前问题的原图分析</strong>
                          <div className="markdown-body"><MarkdownAnswer text={deepAnalysis.answer} question={turn.question} /></div>
                          {deepAnalysis.observations.length > 0 && <ul>{deepAnalysis.observations.map((observation) => <li key={observation}>{observation}</li>)}</ul>}
                          {deepAnalysis.uncertainties.length > 0 && <small>无法确认：{deepAnalysis.uncertainties.join("；")}</small>}
                        </aside>}
                      </div>;
                    })}</div>
                  </section>)}
                </div>}
                {isLast && preview && <div className="answer-preview" aria-label={`${preview.file.display_name}原文预览`}>
                  <header><strong>{preview.file.display_name}</strong><small>{displayPath(preview.file.display_path)}</small></header>
                  {preview.file.extension.toLowerCase() === "pdf" && <PdfVisualPreview preview={preview} />}
                  <OcrAttemptChain attempts={preview.ocr_attempts} />
                  <ImageAssetGallery assets={preview.image_assets} />
                  {preview.nodes.map((node) => <p key={node.node_id} className={node.node_id === preview.anchor_node_id ? "preview-node--anchor" : undefined}><small>{locatorLabel(node.locator)}</small>{highlightPlainTerms(node.text ?? (node.table_data ? JSON.stringify(node.table_data) : ""), turn.question)}</p>)}
                  {preview.next_offset !== null && <button type="button" className="text-button" disabled={previewLoading === preview.file.file_id} onClick={() => void showPreview(preview.file.file_id, null, preview.next_offset ?? 0)}>继续载入</button>}
                </div>}
              </div>
            </AssistantMessage>
          </Fragment>;
        })}
        {pendingQuestion && <UserMessage text={pendingQuestion} />}
        {loading && <AssistantMessage>
          <div className="chat-bubble chat-bubble--assistant" aria-live="polite">
            <small className="ask-phase-label">{ASK_PHASE_LABELS[activePhase] ?? "正在处理本地资料"}</small>
            {streamedAnswer ? <div className="markdown-body"><MarkdownAnswer text={streamedAnswer} question={pendingQuestion ?? ""} /></div> : <span className="chat-typing"><i /><i /><i /></span>}
          </div>
        </AssistantMessage>}
        {error && <AssistantMessage>
          <div className="chat-bubble chat-bubble--error"><WarningOutlined /> <span>{error}</span>{lastFailedQuestion && !loading && <button type="button" className="text-button" onClick={() => void submit(lastFailedQuestion)}>重试本次提问</button>}</div>
        </AssistantMessage>}
      </div>
      {exportMessage && <p className="inline-success" role="status">{exportMessage}</p>}
      <form className="ask-composer" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
        <textarea ref={composerRef} value={question} onChange={(event) => setQuestion(event.target.value)} onInput={resizeComposer} onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
            event.preventDefault();
            void submit();
          }
        }} placeholder={recording ? "正在录音，再次点击麦克风结束…" : recognizing ? "正在本地识别语音…" : "基于我的资料提问…"} disabled={recording || recognizing} />
        {recording && <button type="button" className="ask-composer__cancel-recording" onClick={() => void stopRecording(true)}>取消</button>}
        <button
          type="button"
          className={recording ? "ask-composer__voice ask-composer__voice--recording" : "ask-composer__voice"}
          aria-label={recording ? "结束录音" : "开始录音"}
          disabled={recognizing || loading}
          onClick={() => recording ? void stopRecording(false) : void startRecording()}
        >{recording ? <StopOutlined /> : <AudioOutlined />}</button>
        <button type="submit" aria-label="发送" disabled={loading || !question.trim() || readiness?.ready === false}><SendOutlined /></button>
      </form>
    </section>
  );
}
