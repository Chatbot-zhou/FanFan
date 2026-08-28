import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { bridge, type InboxItem } from "../../bridge";
import { useAppStore } from "../../state/app-store";
import { Sidebar } from "./Sidebar";

const failedItem: InboxItem = {
  inbox_id: "failure-1",
  file_id: "file-1",
  display_name: "处理失败.pdf",
  display_path: "…\\Documents\\处理失败.pdf",
  event_type: "parse_failed",
  observed_at: "2026-08-25T00:00:00Z",
  previous_display_path: null,
  triage_status: "new",
  resolution_status: "pending_retry",
  attempt_count: 0,
  last_attempt_at: null,
  retry_action: "retry_parse",
  suggested_collection_ids: [],
  duplicate_group_id: null,
  summary: "处理失败",
  error_code: "PARSE_FAILED",
};

describe("Sidebar", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAppStore.setState({ route: "home" });
  });

  it("uses only active processing failures for the inbox badge", async () => {
    const query = vi.spyOn(bridge, "inbox_query").mockResolvedValue({
      items: [failedItem],
      next_cursor: null,
      has_more: false,
    });
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });

    render(<QueryClientProvider client={client}><Sidebar /></QueryClientProvider>);

    expect(await screen.findByText("1")).toHaveClass("sidebar-item__badge");
    await waitFor(() => expect(query).toHaveBeenCalledWith(expect.objectContaining({ status: "error" })));
  });
});
