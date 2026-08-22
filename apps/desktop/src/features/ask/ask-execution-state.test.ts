import { describe, expect, it } from "vitest";
import type { AskStreamEvent } from "../../bridge";
import { applyAskStreamEvent, toggleAskExecutionNode } from "./ask-execution-state";

const event = (patch: Partial<AskStreamEvent>): AskStreamEvent => ({
  event_type: "node_started",
  operation_id: "op-1",
  sequence: 1,
  node_id: null,
  node_name: null,
  public_label: null,
  status: null,
  public_summary: null,
  progress_lines: null,
  duration_ms: null,
  delta: null,
  step_count: null,
  total_duration_ms: null,
  ...patch,
});

describe("ask execution state", () => {
  it("keeps only the running node auto-expanded", () => {
    let state = applyAskStreamEvent(null, event({ event_type: "ask_started" }));
    state = applyAskStreamEvent(state, event({ sequence: 2, node_id: "planning", node_name: "query_planning", public_label: "规划检索" }));
    state = applyAskStreamEvent(state, event({ sequence: 3, event_type: "node_completed", node_id: "planning", node_name: "query_planning", public_label: "规划检索", public_summary: "已完成", duration_ms: 82 }));
    state = applyAskStreamEvent(state, event({ sequence: 4, event_type: "node_started", node_id: "retrieval", node_name: "retrieval", public_label: "检索相关内容" }));

    expect(state?.nodes.find((node) => node.node_id === "planning")?.auto_expanded).toBe(false);
    expect(state?.nodes.find((node) => node.node_id === "retrieval")?.auto_expanded).toBe(true);
  });

  it("replaces progress with the latest three public lines", () => {
    let state = applyAskStreamEvent(null, event({ event_type: "ask_started" }));
    state = applyAskStreamEvent(state, event({ sequence: 2, node_id: "retrieval", node_name: "retrieval", public_label: "检索相关内容" }));
    state = applyAskStreamEvent(state, event({ sequence: 3, event_type: "node_progress", node_id: "retrieval", node_name: "retrieval", public_label: "检索相关内容", progress_lines: ["1", "2", "3", "4"] }));

    expect(state?.nodes[0]?.progress_lines).toEqual(["2", "3", "4"]);
  });

  it("preserves user-expanded completed nodes until ask completion", () => {
    let state = applyAskStreamEvent(null, event({ event_type: "ask_started" }));
    state = applyAskStreamEvent(state, event({ sequence: 2, node_id: "planning", node_name: "query_planning", public_label: "规划检索" }));
    state = applyAskStreamEvent(state, event({ sequence: 3, event_type: "node_completed", node_id: "planning", node_name: "query_planning", public_label: "规划检索" }));
    state = toggleAskExecutionNode(state, "planning");
    state = applyAskStreamEvent(state, event({ sequence: 4, event_type: "node_started", node_id: "retrieval", node_name: "retrieval", public_label: "检索相关内容" }));
    expect(state?.nodes.find((node) => node.node_id === "planning")?.user_expanded).toBe(true);

    state = applyAskStreamEvent(state, event({ sequence: 5, event_type: "ask_completed", status: "completed", step_count: 2, total_duration_ms: 1600 }));
    expect(state?.nodes.every((node) => !node.user_expanded && !node.auto_expanded)).toBe(true);
  });

  it("ignores stale events", () => {
    let state = applyAskStreamEvent(null, event({ event_type: "ask_started", sequence: 3 }));
    state = applyAskStreamEvent(state, event({ sequence: 2, event_type: "answer_delta", delta: "late" }));
    expect(state?.streamed_answer).toBe("");
  });
});
