import type { AskStreamEvent } from "../../bridge";

export type AskExecutionNodeStatus = "queued" | "running" | "completed" | "failed" | "skipped";
export type AskExecutionStatus = "idle" | "running" | "completed" | "failed" | "cancelled";

export interface AskExecutionNode {
  node_id: string;
  node_name: string;
  public_label: string;
  status: AskExecutionNodeStatus;
  public_summary: string | null;
  progress_lines: string[];
  duration_ms: number | null;
  auto_expanded: boolean;
  user_expanded: boolean;
}

export interface AskExecutionState {
  operation_id: string;
  status: AskExecutionStatus;
  nodes: AskExecutionNode[];
  active_node_id: string | null;
  streamed_answer: string;
  answer_started: boolean;
  answer_completed: boolean;
  step_count: number;
  total_duration_ms: number | null;
  last_sequence: number;
}

export function createAskExecutionState(operationId: string): AskExecutionState {
  return {
    operation_id: operationId,
    status: "running",
    nodes: [],
    active_node_id: null,
    streamed_answer: "",
    answer_started: false,
    answer_completed: false,
    step_count: 0,
    total_duration_ms: null,
    last_sequence: 0,
  };
}

const normalizeLines = (lines: string[] | null | undefined) => (lines ?? []).filter(Boolean).slice(-3);

const upsertNode = (state: AskExecutionState, event: AskStreamEvent, patch: Partial<AskExecutionNode>): AskExecutionNode[] => {
  if (!event.node_id) return state.nodes;
  const existing = state.nodes.find((node) => node.node_id === event.node_id);
  const next: AskExecutionNode = {
    node_id: event.node_id,
    node_name: event.node_name ?? existing?.node_name ?? event.node_id,
    public_label: event.public_label ?? existing?.public_label ?? event.node_id,
    status: existing?.status ?? "queued",
    public_summary: existing?.public_summary ?? null,
    progress_lines: existing?.progress_lines ?? [],
    duration_ms: existing?.duration_ms ?? null,
    auto_expanded: existing?.auto_expanded ?? false,
    user_expanded: existing?.user_expanded ?? false,
    ...patch,
  };
  return existing
    ? state.nodes.map((node) => node.node_id === next.node_id ? next : node)
    : [...state.nodes, next];
};

export function applyAskStreamEvent(current: AskExecutionState | null, event: AskStreamEvent): AskExecutionState | null {
  if (!event.operation_id) return current;
  if (current && current.operation_id !== event.operation_id && event.event_type !== "ask_started") return current;
  const base = current ?? createAskExecutionState(event.operation_id);
  if (event.sequence <= base.last_sequence) return base;
  let next: AskExecutionState = { ...base, last_sequence: event.sequence };

  if (event.event_type === "ask_started") {
    return { ...createAskExecutionState(event.operation_id), last_sequence: event.sequence };
  }

  if (event.event_type === "node_started") {
    next = {
      ...next,
      status: "running",
      active_node_id: event.node_id ?? next.active_node_id,
      nodes: next.nodes.map((node) => node.user_expanded ? node : { ...node, auto_expanded: false }),
    };
    next.nodes = upsertNode(next, event, {
      status: "running",
      public_summary: event.public_summary ?? null,
      progress_lines: normalizeLines(event.progress_lines),
      duration_ms: null,
      auto_expanded: true,
    });
    return next;
  }

  if (event.event_type === "node_progress") {
    next.nodes = upsertNode(next, event, {
      status: "running",
      public_summary: event.public_summary ?? null,
      progress_lines: normalizeLines(event.progress_lines),
      auto_expanded: true,
    });
    return next;
  }

  if (["node_completed", "node_failed", "node_skipped"].includes(event.event_type)) {
    const status: AskExecutionNode["status"] = event.event_type === "node_completed" ? "completed" : event.event_type === "node_failed" ? "failed" : "skipped";
    next.nodes = upsertNode(next, event, {
      status,
      public_summary: event.public_summary ?? null,
      progress_lines: normalizeLines(event.progress_lines),
      duration_ms: event.duration_ms ?? null,
      auto_expanded: false,
    });
    next.active_node_id = next.active_node_id === event.node_id ? null : next.active_node_id;
    next.step_count = next.nodes.filter((node) => node.status !== "skipped").length;
    return next;
  }

  if (event.event_type === "answer_started") {
    return { ...next, answer_started: true };
  }

  if (event.event_type === "answer_delta") {
    return { ...next, answer_started: true, streamed_answer: `${next.streamed_answer}${event.delta ?? ""}` };
  }

  if (event.event_type === "answer_completed") {
    return { ...next, answer_started: true, answer_completed: true };
  }

  if (event.event_type === "ask_completed") {
    const status = event.status === "failed" ? "failed" : event.status === "cancelled" ? "cancelled" : "completed";
    const nodes = next.nodes.map((node) => ({ ...node, auto_expanded: false, user_expanded: false }));
    return {
      ...next,
      status,
      nodes,
      active_node_id: null,
      answer_completed: next.answer_started ? true : next.answer_completed,
      step_count: event.step_count ?? nodes.filter((node) => node.status !== "skipped").length,
      total_duration_ms: event.total_duration_ms ?? next.total_duration_ms,
    };
  }

  return next;
}

export function toggleAskExecutionNode(state: AskExecutionState | null, nodeId: string): AskExecutionState | null {
  if (!state) return state;
  return {
    ...state,
    nodes: state.nodes.map((node) => node.node_id === nodeId
      ? { ...node, user_expanded: !node.user_expanded, auto_expanded: false }
      : node),
  };
}

export function finalizeAskExecutionState(state: AskExecutionState | null, totalDurationMs: number | null): AskExecutionState | null {
  if (!state) return state;
  const nodes = state.nodes.map((node) => ({ ...node, auto_expanded: false, user_expanded: false }));
  return {
    ...state,
    status: state.status === "failed" || state.status === "cancelled" ? state.status : "completed",
    nodes,
    active_node_id: null,
    answer_completed: state.answer_started || state.answer_completed,
    step_count: nodes.filter((node) => node.status !== "skipped").length,
    total_duration_ms: state.total_duration_ms ?? totalDurationMs,
  };
}
