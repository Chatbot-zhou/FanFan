import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { bridge, type CollectionRecord, type FileRecord } from "../bridge";
import { useAppStore } from "../state/app-store";
import { CollectionsPage } from "./CollectionsPage";

const collection: CollectionRecord = {
  collection_id: "collection-1",
  name: "项目资料",
  description: "手动整理的项目资料",
  icon: "folder",
  color: "#71a7ca",
  kind: "manual",
  rule: null,
  file_count: 0,
  built_in: false,
  created_at: "2026-08-08T00:00:00Z",
  updated_at: "2026-08-08T00:00:00Z",
};

const file: FileRecord = {
  file_id: "file-1",
  volume_id: "volume-1",
  display_path: "…\\Documents\\项目总结.pdf",
  display_name: "项目总结.pdf",
  extension: "pdf",
  mime_type: "application/pdf",
  size_bytes: 128,
  fs_created_at: null,
  fs_modified_at: "2026-08-08T00:00:00Z",
  windows_file_id: null,
  content_sha256: null,
  availability: "present",
  current_revision_id: "revision-1",
  parse_status: "parsed",
  first_seen_at: "2026-08-08T00:00:00Z",
  last_seen_at: "2026-08-08T00:00:00Z",
};

describe("CollectionsPage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAppStore.setState({ selected_collection_id: "collection-1" });
  });

  it("opens the collection selected from home and adds an indexed file", async () => {
    vi.spyOn(bridge, "collection_list").mockResolvedValue([collection]);
    vi.spyOn(bridge, "collection_file_query").mockResolvedValue({
      items: [],
      next_cursor: null,
      total: 0,
    });
    vi.spyOn(bridge, "file_query").mockResolvedValue({
      items: [file],
      next_cursor: null,
      total: 1,
    });
    const add = vi.spyOn(bridge, "collection_add_file").mockResolvedValue();
    const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });

    render(<QueryClientProvider client={client}><CollectionsPage /></QueryClientProvider>);

    expect((await screen.findAllByRole("heading", { name: "项目资料" })).length).toBe(2);
    fireEvent.change(await screen.findByRole("combobox", { name: /添加资料/ }), { target: { value: "file-1" } });
    fireEvent.click(screen.getByRole("button", { name: "添加到集合" }));

    await waitFor(() => expect(add).toHaveBeenCalledWith("collection-1", "file-1"));
  });
});
