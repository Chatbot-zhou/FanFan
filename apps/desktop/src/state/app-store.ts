import { create } from "zustand";
import type { AnswerResult, AppRoute, InboxQuery, SearchRequest, SearchSession } from "../bridge";

type InboxStatus = InboxQuery["status"];
export type SettingsTab = "roots" | "models" | "index" | "appearance" | "logs";
export type SearchModifiedWindow = "all" | "7" | "30" | "365";
export type AskTurn = { question: string; answer: AnswerResult };
/** 后台分析任务种类：资料关系分析、AI 集合建议分析 */
export type AnalysisTaskKind = "relation" | "collection";
/** 分析任务跨页面保留的运行状态：切页不中断也不丢失，页面重挂载后仍显示「正在分析」。 */
export interface AnalysisTaskState {
  status: "idle" | "running" | "done" | "error";
  started_at: number | null;
  finished_at: number | null;
  /** 成功时的反馈文案（如各关系类型的数量汇总），跨页保留展示。 */
  summary: string | null;
  error: string | null;
}

interface SearchPrefs {
  mode: SearchRequest["mode"];
  sort: SearchRequest["sort"];
  extension: string;
  modified_window: SearchModifiedWindow;
  scope_collection_ids: string[];
}

interface AppState {
  route: AppRoute;
  search_query: string;
  search_session: SearchSession | null;
  search_session_query: string;
  search_prefs: SearchPrefs;
  ask_turns: AskTurn[];
  ask_pending_question: string | null;
  ask_loading: boolean;
  ask_active_session_id: string | null;
  ask_operation_id: string | null;
  ask_streamed_answer: string;
  ask_active_phase: string;
  ask_scope_collection_ids: string[];
  inbox_initial_status: InboxStatus;
  inbox_today_only: boolean;
  selected_collection_id: string | null;
  settings_tab: SettingsTab;
  model_prompt_dismissed: boolean;
  analysis_tasks: Record<AnalysisTaskKind, AnalysisTaskState>;
  set_analysis_task: (kind: AnalysisTaskKind, patch: Partial<AnalysisTaskState>) => void;
  navigate: (route: AppRoute) => void;
  start_search: (query: string) => void;
  set_search_query: (query: string) => void;
  set_search_session: (session: SearchSession | null, query?: string) => void;
  set_search_prefs: (prefs: Partial<SearchPrefs>) => void;
  set_ask_turns: (turns: AskTurn[]) => void;
  set_ask_pending_question: (question: string | null) => void;
  set_ask_loading: (loading: boolean) => void;
  set_ask_active_session_id: (sessionId: string | null) => void;
  set_ask_operation_id: (operationId: string | null) => void;
  set_ask_streamed_answer: (answer: string) => void;
  set_ask_active_phase: (phase: string) => void;
  set_ask_scope_collection_ids: (ids: string[]) => void;
  reset_ask_state: () => void;
  open_inbox: (status: InboxStatus, todayOnly?: boolean) => void;
  open_collection: (collectionId: string) => void;
  set_settings_tab: (tab: SettingsTab) => void;
  clear_collection_selection: () => void;
  dismiss_model_prompt: () => void;
}

export const useAppStore = create<AppState>((set) => ({
  route: "home",
  search_query: "",
  search_session: null,
  search_session_query: "",
  search_prefs: {
    mode: "hybrid",
    sort: "relevance",
    extension: "",
    modified_window: "all",
    scope_collection_ids: [],
  },
  ask_turns: [],
  ask_pending_question: null,
  ask_loading: false,
  ask_active_session_id: null,
  ask_operation_id: null,
  ask_streamed_answer: "",
  ask_active_phase: "queued",
  ask_scope_collection_ids: [],
  inbox_initial_status: "new",
  inbox_today_only: false,
  selected_collection_id: null,
  settings_tab: "roots",
  model_prompt_dismissed: false,
  analysis_tasks: {
    relation: { status: "idle", started_at: null, finished_at: null, summary: null, error: null },
    collection: { status: "idle", started_at: null, finished_at: null, summary: null, error: null },
  },
  set_analysis_task: (kind, patch) => set((state) => ({
    analysis_tasks: { ...state.analysis_tasks, [kind]: { ...state.analysis_tasks[kind], ...patch } },
  })),
  navigate: (route) => set((state) => route === state.route ? state : (route === "inbox"
    ? { route, inbox_initial_status: "new", inbox_today_only: false }
    : route === "collections"
      ? { route, selected_collection_id: null }
      : { route })),
  start_search: (search_query) => set({ route: "search", search_query, search_session: null, search_session_query: "" }),
  set_search_query: (search_query) => set({ search_query }),
  set_search_session: (search_session, query) => set(search_session
    ? { search_session, search_session_query: query ?? "" }
    : { search_session: null }),
  set_search_prefs: (prefs) => set((state) => ({ search_prefs: { ...state.search_prefs, ...prefs } })),
  set_ask_turns: (ask_turns) => set({ ask_turns }),
  set_ask_pending_question: (ask_pending_question) => set({ ask_pending_question }),
  set_ask_loading: (ask_loading) => set({ ask_loading }),
  set_ask_active_session_id: (ask_active_session_id) => set({ ask_active_session_id }),
  set_ask_operation_id: (ask_operation_id) => set({ ask_operation_id }),
  set_ask_streamed_answer: (ask_streamed_answer) => set({ ask_streamed_answer }),
  set_ask_active_phase: (ask_active_phase) => set({ ask_active_phase }),
  set_ask_scope_collection_ids: (ask_scope_collection_ids) => set({ ask_scope_collection_ids }),
  reset_ask_state: () => set({
    ask_turns: [],
    ask_pending_question: null,
    ask_loading: false,
    ask_active_session_id: null,
    ask_operation_id: null,
    ask_streamed_answer: "",
    ask_active_phase: "queued",
  }),
  open_inbox: (inbox_initial_status, inbox_today_only = false) => set({
    route: "inbox",
    inbox_initial_status,
    inbox_today_only,
  }),
  open_collection: (selected_collection_id) => set({
    route: "collections",
    selected_collection_id,
  }),
  set_settings_tab: (settings_tab) => set({ settings_tab }),
  clear_collection_selection: () => set({ selected_collection_id: null }),
  dismiss_model_prompt: () => set({ model_prompt_dismissed: true }),
}));
