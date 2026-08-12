import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { bridge, type InboxItem } from "../bridge";
import { useAppStore } from "../state/app-store";
import { InboxPage } from "./InboxPage";

function item(inboxId: string, name: string): InboxItem {
  return {
    inbox_id: inboxId,
    file_id: `file-${inboxId}`,
    display_name: name,
  display_path: `…\\Documents\\${name}`,
    event_type: "discovered",
    observed_at: "2026-08-08T00:00:00Z",
  previous_display_path: null,
    triage_status: "new",
    resolution_status: "normal",
    attempt_count: 0,
    last_attempt_at: null,
    retry_action: null,
    suggested_collection_ids: [],
    duplicate_group_id: null,
    summary: null,
    error_code: null,
  };
}

describe("InboxPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAppStore.setState({ inbox_initial_status: "all", inbox_today_only: true });
  });

  it("keeps the home date filter and loads the next cursor page", async () => {
    const query = vi.spyOn(bridge, "inbox_query")
      .mockResolvedValueOnce({ items: [item("1", "今天新增.pdf")], next_cursor: "cursor-2", has_more: true })
      .mockResolvedValueOnce({ items: [item("2", "第二页.docx")], next_cursor: null, has_more: false });
    const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });

    render(<QueryClientProvider client={client}><InboxPage /></QueryClientProvider>);

    expect(await screen.findByText("今天新增.pdf")).toBeInTheDocument();
    expect(query.mock.calls[0]?.[0]).toMatchObject({ status: "all", cursor: null });
    expect(query.mock.calls[0]?.[0].date_from).not.toBeNull();
    expect(query.mock.calls[0]?.[0].date_to).not.toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "加载更多" }));
    expect(await screen.findByText("第二页.docx")).toBeInTheDocument();
    await waitFor(() => expect(query.mock.calls[1]?.[0].cursor).toBe("cursor-2"));
  });

  it("updates reviewed state and renders a structured failure as readable text", async () => {
    vi.spyOn(bridge, "inbox_query").mockResolvedValue({ items: [item("1", "待查看.pdf")], next_cursor: null, has_more: false });
    const update = vi.spyOn(bridge, "inbox_update")
      .mockResolvedValueOnce({ ...item("1", "待查看.pdf"), triage_status: "reviewed" })
      .mockRejectedValueOnce({ code: "INBOX_UPDATE_FAILED", message: "资料库暂时繁忙", retryable: true });
    const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
    render(<QueryClientProvider client={client}><InboxPage /></QueryClientProvider>);

    expect(await screen.findByText("待查看.pdf")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /已查看/ }));
    await waitFor(() => expect(update).toHaveBeenCalledWith("1", "reviewed"));
    fireEvent.click(screen.getByRole("button", { name: "忽略" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("资料库暂时繁忙");
    expect(screen.getByRole("alert")).not.toHaveTextContent("[object Object]");
  });
});
