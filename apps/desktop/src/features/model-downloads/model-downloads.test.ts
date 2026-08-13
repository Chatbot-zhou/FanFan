import { describe, expect, it } from "vitest";
import type { ModelDownloadJob } from "../../bridge";
import { summarizeModelDownloads, visibleModelDownloadJobs } from "./model-downloads";

const job = (overrides: Partial<ModelDownloadJob> = {}): ModelDownloadJob => ({
  job_id: "job-a",
  edition_id: "edition-a",
  edition_name: "模型 A",
  source: "huggingface",
  status: "running",
  phase: "downloading",
  downloaded_bytes: 25,
  total_bytes: 100,
  progress: 0.25,
  bytes_per_second: 10,
  eta_seconds: 8,
  retry_count: 0,
  current_file: "model.gguf",
  files: [],
  installed_artifact_ids: [],
  profile_id: null,
  error: null,
  activation_status: null,
  activation_error: null,
  created_at: "2026-08-13T01:00:00Z",
  updated_at: "2026-08-13T01:01:00Z",
  ...overrides,
});

describe("model download view model", () => {
  it("keeps creation order when updated_at alternates", () => {
    const first = job({ job_id: "first", created_at: "2026-08-13T01:00:00Z", updated_at: "2026-08-13T02:00:00Z" });
    const second = job({ job_id: "second", created_at: "2026-08-13T01:05:00Z", updated_at: "2026-08-13T01:06:00Z" });
    expect(visibleModelDownloadJobs([second, first], 0).map((item) => item.job_id)).toEqual(["first", "second"]);

    const nextFirst = { ...first, updated_at: "2026-08-13T02:01:00Z", progress: 0.4 };
    const nextSecond = { ...second, updated_at: "2026-08-13T03:00:00Z", progress: 0.8 };
    expect(visibleModelDownloadJobs([nextSecond, nextFirst], 0).map((item) => item.job_id)).toEqual(["first", "second"]);
  });

  it("uses byte-weighted aggregate progress and avoids NaN for unknown totals", () => {
    const summary = summarizeModelDownloads([
      job({ job_id: "small", downloaded_bytes: 50, total_bytes: 100 }),
      job({ job_id: "large", downloaded_bytes: 100, total_bytes: 900 }),
    ]);
    expect(summary.progress).toBe(0.15);
    expect(summarizeModelDownloads([job({ total_bytes: 0, downloaded_bytes: 0 })]).progress).toBeNull();
  });

  it("retains failures across sessions but only keeps terminal success in its current session", () => {
    const sessionStart = Date.parse("2026-08-13T02:00:00Z");
    const previousCompletion = job({ status: "completed", phase: "completed", updated_at: "2026-08-13T01:30:00Z" });
    const currentCancellation = job({ job_id: "cancelled", status: "cancelled", phase: "cancelled", updated_at: "2026-08-13T02:30:00Z" });
    const previousFailure = job({ job_id: "failed", status: "failed", phase: "failed", updated_at: "2026-08-12T20:00:00Z" });

    expect(visibleModelDownloadJobs([previousCompletion, currentCancellation, previousFailure], sessionStart).map((item) => item.job_id)).toEqual(["failed"]);
  });
});
