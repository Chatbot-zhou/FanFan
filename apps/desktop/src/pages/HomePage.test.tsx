import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { bridge, type HomeSummary, type JobRecord, type MaintenanceSnapshot } from "../bridge";
import { useAppStore } from "../state/app-store";
import { HomePage } from "./HomePage";

const summary: HomeSummary = {
  local_date: "2026-08-08",
  metrics: [
    { key: "today_added", label: "今日新增", value: 2 },
    { key: "awaiting_confirmation", label: "待确认", value: 1 },
    { key: "possible_duplicates", label: "可能重复", value: 3 },
    { key: "processing_failed", label: "处理失败", value: 1 },
  ],
  scan_progress: {
    scan_job_id: "0198f7ac-0000-7000-8000-000000000001",
    status: "running",
    discovered_files: 12,
    searchable_files: 9,
    parsed_files: 8,
    embedded_files: 4,
    ocr_pages: 3,
    progress: 0.75,
  },
  index_initialized: true,
  recent_files: [],
  favorite_files: [],
  collections: [{ collection_id: "collection-1", name: "项目资料", item_count: 4, tone: "purple" }],
  candidate_roots: [],
};

const maintenance: MaintenanceSnapshot = {
  schema_version: 34,
  database_size_bytes: 1024,
  indexed_files: 1,
  indexable_files: 2,
  parsed_files: 2,
  embedded_files: 2,
  active_index_files: 1,
  searchable_chunks: 20_534,
  embedded_chunks: 20_534,
  active_vector_keys: 19_054,
  pending_files: 0,
  failed_files: 0,
  active_jobs: 0,
  log_events: 0,
  background_notice: null,
  checks: [],
  checked_at: "2026-08-22T00:00:00Z",
};
function renderHome(value = summary, maintenance?: MaintenanceSnapshot) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(<QueryClientProvider client={client}><HomePage summary={value} loading={false} maintenance={maintenance ?? null} /></QueryClientProvider>);
}

describe("HomePage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAppStore.setState({
      route: "home",
      inbox_initial_status: "new",
      inbox_today_only: false,
      selected_collection_id: null,
    });
  });

  it("opens each summary destination with its concrete filter", () => {
    renderHome();

    fireEvent.click(screen.getByRole("button", { name: /今日新增/ }));
    expect(useAppStore.getState()).toMatchObject({ route: "inbox", inbox_initial_status: "all", inbox_today_only: true });

    useAppStore.setState({ route: "home" });
    fireEvent.click(screen.getByRole("button", { name: /处理失败/ }));
    expect(useAppStore.getState()).toMatchObject({ route: "inbox", inbox_initial_status: "error", inbox_today_only: false });

    useAppStore.setState({ route: "home" });
    fireEvent.click(screen.getByRole("button", { name: /可能重复/ }));
    expect(useAppStore.getState().route).toBe("library");

    useAppStore.setState({ route: "home" });
    fireEvent.click(screen.getByRole("button", { name: /项目资料/ }));
    expect(useAppStore.getState()).toMatchObject({ route: "collections", selected_collection_id: "collection-1" });
  });

  it("uses active USearch keys for retrieval coverage", () => {
    renderHome({ ...summary, scan_progress: null }, maintenance);

    expect(screen.getByText("检索覆盖 93%")).toBeInTheDocument();
    expect(screen.queryByText("检索覆盖 100%")).not.toBeInTheDocument();
  });
  it("pauses the active backend scan job", async () => {
    const pausedJob: JobRecord = {
      job_id: summary.scan_progress!.scan_job_id,
      job_type: "initial_scan",
      status: "paused",
      stage: "enumerating",
      progress: 0.75,
      processed_items: 9,
      total_items: 12,
      error: null,
      created_at: "2026-08-08T00:00:00Z",
      started_at: "2026-08-08T00:00:01Z",
      finished_at: null,
    };
    const pause = vi.spyOn(bridge, "scan_pause").mockResolvedValue(pausedJob);
    renderHome();

    fireEvent.click(screen.getByRole("button", { name: /暂停/ }));

    await waitFor(() => expect(pause).toHaveBeenCalledWith(summary.scan_progress!.scan_job_id));
  });

  it("dismisses a discovered source immediately after the backend confirms it", async () => {
    const action = vi.spyOn(bridge, "candidate_root_action").mockResolvedValue({ candidate_id: "candidate-1", candidate_type: "wechat", label: "微信资料", display_path: "…\\WeChat Files", status: "ignored" });
    renderHome({ ...summary, candidate_roots: [{ candidate_id: "candidate-1", candidate_type: "wechat", label: "微信资料", display_path: "…\\WeChat Files", status: "suggested" }] });

    fireEvent.click(screen.getByRole("button", { name: "暂不添加微信资料" }));

    await waitFor(() => expect(action).toHaveBeenCalledWith("candidate-1", "ignore"));
    await waitFor(() => expect(screen.queryByText("微信资料")).not.toBeInTheDocument());
  });
});
