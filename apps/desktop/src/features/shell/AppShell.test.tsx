import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const bridgeMock = vi.hoisted(() => ({
  startup_get_state: vi.fn(),
  model_state_get: vi.fn(),
  model_download_list: vi.fn(),
  home_get_summary: vi.fn(),
  root_list: vi.fn(),
  welcome_get_state: vi.fn(),
  environment_get_latest: vi.fn(),
  environment_detect: vi.fn(),
  app_status_get: vi.fn(),
  welcome_authorization_complete: vi.fn(),
}));

vi.mock("../../bridge", () => ({ bridge: bridgeMock }));
vi.mock("../../hooks/useBackendEvents", () => ({ useBackendEvents: () => null }));
vi.mock("./TitleBar", () => ({ TitleBar: () => <div data-testid="title-bar" /> }));
vi.mock("./Sidebar", () => ({ Sidebar: () => <div /> }));
vi.mock("./StatusBar", () => ({ StatusBar: () => <div /> }));
vi.mock("../onboarding/RootAuthorizationPage", () => ({
  RootAuthorizationPage: () => <div>首次目录授权</div>,
}));

import { AppShell } from "./AppShell";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((resolvePromise) => { resolve = resolvePromise; });
  return { promise, resolve };
}

describe("AppShell", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    bridgeMock.startup_get_state.mockResolvedValue({ phase: "ready", ready: true, progress: 1, pending_files: 0, blocker: null, recovery_actions: [] });
    bridgeMock.model_state_get.mockResolvedValue(null);
    bridgeMock.model_download_list.mockResolvedValue([]);
    bridgeMock.home_get_summary.mockResolvedValue({ local_date: "2026-08-11", metrics: [], scan_progress: null, recent_files: [], favorite_files: [], collections: [], candidate_roots: [] });
    bridgeMock.environment_get_latest.mockResolvedValue(null);
    bridgeMock.environment_detect.mockResolvedValue({ status: "ready" });
    bridgeMock.app_status_get.mockResolvedValue({ local_only: true, source_files_readonly: true, roots: [], scan_progress: null, maintenance: { active_jobs: 0, background_notice: null }, recovery_actions: [], checked_at: "2026-08-11T00:00:00Z" });
    bridgeMock.welcome_authorization_complete.mockResolvedValue(undefined);
  });

  it("keeps the hook order stable when first-use authorization becomes required", async () => {
    const roots = deferred<unknown[]>();
    const welcome = deferred<{ root_authorization_completed: boolean }>();
    bridgeMock.root_list.mockReturnValue(roots.promise);
    bridgeMock.welcome_get_state.mockReturnValue(welcome.promise);
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });

    render(<QueryClientProvider client={queryClient}><AppShell startup_notice={null} /></QueryClientProvider>);
    await waitFor(() => expect(bridgeMock.welcome_get_state).toHaveBeenCalled());
    await act(async () => {
      roots.resolve([]);
      welcome.resolve({ root_authorization_completed: false });
      await Promise.all([roots.promise, welcome.promise]);
    });

    expect(await screen.findByText("首次目录授权")).toBeInTheDocument();
    expect(consoleError.mock.calls.flat().join(" ")).not.toContain("Rendered fewer hooks than expected");
    consoleError.mockRestore();
  });
});
