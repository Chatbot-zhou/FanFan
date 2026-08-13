import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ModelDownloadJob } from "../../bridge";
import { ModelDownloadList } from "./ModelDownloadList";

const job = (jobId: string, name: string, status: ModelDownloadJob["status"] = "running"): ModelDownloadJob => ({
  job_id: jobId,
  edition_id: `edition-${jobId}`,
  edition_name: name,
  source: "huggingface",
  status,
  phase: status === "paused" ? "paused" : "downloading",
  downloaded_bytes: 50,
  total_bytes: 100,
  progress: 0.5,
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
  created_at: "2026-08-13T01:00:00Z",
  updated_at: "2026-08-13T01:01:00Z",
});

describe("ModelDownloadList", () => {
  it("routes an action to the correct job without disabling another row", () => {
    const onAction = vi.fn();
    const jobs = [job("first", "模型一"), job("second", "模型二")];
    const { rerender } = render(<ModelDownloadList jobs={jobs} on_action={onAction} />);

    const pauseButtons = screen.getAllByRole("button", { name: "暂停" });
    expect(pauseButtons).toHaveLength(2);
    fireEvent.click(pauseButtons[1]!);
    expect(onAction).toHaveBeenCalledWith(jobs[1], "pause");

    rerender(<ModelDownloadList jobs={jobs} pending_actions={{ second: "pause" }} on_action={onAction} />);
    expect(screen.getByRole("button", { name: "暂停中" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "暂停" })).toBeEnabled();
  });

  it("renders a readable per-task operation error", () => {
    render(<ModelDownloadList jobs={[job("first", "模型一", "paused")]} action_errors={{ first: "网络连接已中断" }} />);
    expect(screen.getByRole("alert")).toHaveTextContent("网络连接已中断");
  });

  it("separates resume, retry, source switch and removal semantics", () => {
    const onAction = vi.fn();
    const failed = {
      ...job("failed", "失败模型", "failed"),
      phase: "failed" as const,
      error: { code: "MODEL_DOWNLOAD_FAILED", message: "连接失败", retryable: true, user_action: null, file_id: null, details: null },
    };
    render(<ModelDownloadList jobs={[job("paused", "暂停模型", "paused"), failed]} on_action={onAction} />);

    expect(screen.getByRole("button", { name: "继续" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "重试" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "切换来源" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "移除任务" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "继续/重试" })).not.toBeInTheDocument();
  });
});
