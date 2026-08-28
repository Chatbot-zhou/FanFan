import { afterEach, describe, expect, it } from "vitest";
import type { AskStreamEvent } from "../bridge";
import { useAppStore } from "./app-store";

const askEvent = (patch: Partial<AskStreamEvent>): AskStreamEvent => ({
  event_type: "ask_started",
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

describe("app store ask stream gate", () => {
  afterEach(() => {
    useAppStore.getState().reset_ask_state();
    useAppStore.setState({ ask_execution_history: {} });
  });

  it("ignores ask stream events when no ask is active", () => {
    useAppStore.getState().apply_ask_stream_event(askEvent({ operation_id: "stale-op" }));

    expect(useAppStore.getState().ask_execution).toBeNull();
    expect(useAppStore.getState().ask_streamed_answer).toBe("");
  });

  it("attaches the first ask_started event only while a question is pending", () => {
    useAppStore.setState({ ask_loading: true, ask_pending_question: "我的简历里写了哪些项目？" });

    useAppStore.getState().apply_ask_stream_event(askEvent({ operation_id: "current-op" }));
    useAppStore.getState().apply_ask_stream_event(askEvent({ event_type: "answer_delta", operation_id: "old-op", sequence: 2, delta: "不该出现" }));
    useAppStore.getState().apply_ask_stream_event(askEvent({ event_type: "answer_delta", operation_id: "current-op", sequence: 2, delta: "已验证片段" }));

    expect(useAppStore.getState().ask_execution?.operation_id).toBe("current-op");
    expect(useAppStore.getState().ask_streamed_answer).toBe("已验证片段");
  });

  it("remembers completed execution by answer message id", () => {
    useAppStore.setState({ ask_loading: true, ask_pending_question: "问题" });
    useAppStore.getState().apply_ask_stream_event(askEvent({ operation_id: "current-op" }));
    useAppStore.getState().apply_ask_stream_event(askEvent({ event_type: "node_started", operation_id: "current-op", sequence: 2, node_id: "retrieval", node_name: "retrieval", public_label: "检索相关内容" }));
    useAppStore.getState().apply_ask_stream_event(askEvent({ event_type: "ask_completed", operation_id: "current-op", sequence: 3, status: "completed", step_count: 1, total_duration_ms: 1200 }));

    const execution = useAppStore.getState().ask_execution;
    useAppStore.getState().remember_ask_execution("message-1", execution);
    useAppStore.getState().reset_ask_state();

    expect(useAppStore.getState().ask_execution).toBeNull();
    expect(useAppStore.getState().ask_execution_history["message-1"]?.nodes[0]?.public_label).toBe("检索相关内容");
  });
});
