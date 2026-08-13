import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ModelDownloadJob, SystemNotice } from "../../bridge";

vi.mock("../../components/BrandMark", () => ({ BrandMark: () => <span>FanFan</span> }));
vi.mock("../../components/WindowControls", () => ({ WindowControls: () => <span /> }));
vi.mock("../../bridge/observed-bridge", () => ({ recordDiagnosticEvent: vi.fn() }));

import { TitleBar } from "./TitleBar";

const job = (jobId: string, name: string, downloaded: number, total: number, createdAt: string): ModelDownloadJob => ({
  job_id: jobId,
  edition_id: `edition-${jobId}`,
  edition_name: name,
  source: "huggingface",
  status: "running",
  phase: "downloading",
  downloaded_bytes: downloaded,
  total_bytes: total,
  progress: total > 0 ? downloaded / total : 0,
  bytes_per_second: 10,
  eta_seconds: 5,
  retry_count: 0,
  current_file: `${jobId}.gguf`,
  files: [],
  installed_artifact_ids: [],
  profile_id: null,
  error: null,
  activation_status: null,
  activation_error: null,
  created_at: createdAt,
  updated_at: createdAt,
});

describe("TitleBar unified status center", () => {
  it("shows a stable weighted summary and keeps every task and notice in the open panel", async () => {
    const first = job("first", "模型一", 50, 100, "2026-08-13T01:00:00Z");
    const second = job("second", "模型二", 100, 900, "2026-08-13T01:05:00Z");
    const notices: SystemNotice[] = [{
      notice_key: "disk-warning",
      level: "warning",
      message: "磁盘空间不足",
      details: "后台任务已经暂停。",
      action_label: null,
      action_route: null,
    }];
    const { rerender } = render(<TitleBar model_state={null} model_downloads={[second, first]} notices={notices} />);

    expect(screen.getByRole("button", { name: "打开统一状态中心" })).toHaveTextContent("2 个模型任务 · 总体 15% · 1 项待处理");
    fireEvent.click(screen.getByRole("button", { name: "打开统一状态中心" }));
    expect(await screen.findByText("模型一")).toBeInTheDocument();
    expect(screen.getByText("模型二")).toBeInTheDocument();
    expect(screen.getByText("磁盘空间不足")).toBeInTheDocument();
    const firstTaskNode = screen.getByText("模型一").closest("article");

    rerender(<TitleBar model_state={null} model_downloads={[{ ...second, updated_at: "2026-08-13T03:00:00Z", downloaded_bytes: 200 }, { ...first, updated_at: "2026-08-13T02:00:00Z", downloaded_bytes: 60 }]} notices={notices} />);
    await waitFor(() => expect(screen.getByRole("button", { name: "打开统一状态中心" })).toHaveAttribute("aria-expanded", "true"));
    expect(screen.getByText("模型一").closest("article")).toBe(firstTaskNode);
    expect(screen.getByText("模型二")).toBeInTheDocument();
  });
});
