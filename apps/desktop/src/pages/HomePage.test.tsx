import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { HomeSummary } from "../bridge";
import type { ModelSetupState } from "../features/model-management/model-setup-state";
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
  overview: {
    discovered_files: 12,
    searchable_files: 9,
    parsed_files: 8,
    embedded_files: 4,
    ocr_pages: 3,
  },
  index_initialized: true,
};

const readyModelSetup: ModelSetupState = {
  status: "ready",
  label: "问资料已准备好",
  description: "可以基于已授权资料进行本地问答、搜索和整理。",
  action_label: "开始问资料",
};

function renderHome(value = summary, modelSetup = readyModelSetup, authorizedRootCount: number | null = 1) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <HomePage summary={value} loading={false} model_setup={modelSetup} authorized_root_count={authorizedRootCount} />
    </QueryClientProvider>,
  );
}

describe("HomePage", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    useAppStore.setState({
      route: "home",
      inbox_initial_status: "new",
      inbox_today_only: false,
      selected_collection_id: null,
      settings_tab: "roots",
    });
  });

  it("shows hero, ready banner, overview, quickstart, and metrics when everything is ready", () => {
    renderHome();

    expect(screen.getByRole("heading", { name: /想不起来？翻翻就知道/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "开始问资料" })).toBeInTheDocument();
    // 就绪后引导折叠为完成横幅，仅保留「开始问资料」主按钮。
    expect(screen.getByText("全部就绪")).toBeInTheDocument();
    expect(screen.queryByText("待完成")).not.toBeInTheDocument();
    // 功能入口 + 底部指标卡。
    expect(screen.getByLabelText("开始使用")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /找资料/ })).toBeInTheDocument();
    expect(within(screen.getByLabelText("数据概览")).getByRole("button", { name: /今日新增/ })).toBeInTheDocument();
    expect(screen.getByText("准备完成")).toBeInTheDocument();
  });

  it("opens ask from the ready hero button", () => {
    renderHome();

    fireEvent.click(screen.getByRole("button", { name: "开始问资料" }));

    expect(useAppStore.getState()).toMatchObject({ route: "ask" });
  });

  it("shows the three-step wizard while the index is still being built", () => {
    renderHome({ ...summary, index_initialized: false });

    expect(screen.getByText("配置本地模型")).toBeInTheDocument();
    expect(screen.getByText("添加资料来源")).toBeInTheDocument();
    expect(screen.getByText("解析并建立索引")).toBeInTheDocument();
    expect(screen.getByText(/正在整理 8\/12 个文件/)).toBeInTheDocument();
    // 已完成的步骤显示「已就绪」，索引步骤仍待完成。
    expect(screen.getAllByText("已就绪")).toHaveLength(2);
    expect(screen.queryByText("全部就绪")).not.toBeInTheDocument();

    fireEvent.click(screen.getAllByRole("button", { name: "查看整理进度" })[0]!);
    expect(useAppStore.getState()).toMatchObject({ route: "inbox", inbox_initial_status: "all" });
  });

  it("routes to adding a root when资料 sources are missing", () => {
    renderHome(summary, readyModelSetup, 0);

    expect(screen.getByRole("button", { name: "添加资料" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "添加资料" }));

    expect(useAppStore.getState()).toMatchObject({ route: "settings", settings_tab: "roots" });
  });

  it("prioritizes the local model step when models are not ready", () => {
    renderHome(summary, { status: "no_model", label: "还没有可用的问资料模型", description: "请选择模型配置。", action_label: "选择模型配置" }, 0);

    // 引导中模型步骤与主按钮都指向模型配置。
    expect(screen.getByText("配置本地模型")).toBeInTheDocument();
    expect(screen.getByText("添加资料来源")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "选择模型配置" })).toHaveLength(2);
    expect(screen.queryByText("全部就绪")).not.toBeInTheDocument();
    fireEvent.click(screen.getAllByRole("button", { name: "选择模型配置" })[0]!);

    expect(useAppStore.getState()).toMatchObject({ route: "model_setup", settings_tab: "models" });
  });

  it("does not treat loading authorized roots as an empty library", () => {
    renderHome(summary, readyModelSetup, null);

    expect(screen.getByText("正在确认已授权目录。")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "添加资料" })).not.toBeInTheDocument();
    fireEvent.click(screen.getAllByRole("button", { name: "查看资料来源" })[0]!);

    expect(useAppStore.getState()).toMatchObject({ route: "settings", settings_tab: "roots" });
  });

  it("routes to model setup when Ollama or models are not ready", () => {
    renderHome(summary, { status: "not_installed", label: "需要先安装 Ollama", description: "需要本机 Ollama。", action_label: "配置 Ollama" }, 1);

    const [configureButton] = screen.getAllByRole("button", { name: /配置 Ollama/ });
    if (!configureButton) throw new Error("配置 Ollama button not found");
    fireEvent.click(configureButton);

    expect(useAppStore.getState()).toMatchObject({ route: "model_setup", settings_tab: "models" });
  });

  it("opens each metric destination with its concrete filter", () => {
    renderHome();
    const metrics = screen.getByLabelText("数据概览");

    fireEvent.click(within(metrics).getByRole("button", { name: /今日新增/ }));
    expect(useAppStore.getState()).toMatchObject({ route: "inbox", inbox_initial_status: "all", inbox_today_only: true });

    useAppStore.setState({ route: "home" });
    fireEvent.click(within(metrics).getByRole("button", { name: /待确认/ }));
    expect(useAppStore.getState()).toMatchObject({ route: "inbox", inbox_initial_status: "new", inbox_today_only: false });

    useAppStore.setState({ route: "home" });
    fireEvent.click(within(metrics).getByRole("button", { name: /处理失败/ }));
    expect(useAppStore.getState()).toMatchObject({ route: "inbox", inbox_initial_status: "error", inbox_today_only: false });

    useAppStore.setState({ route: "home" });
    fireEvent.click(within(metrics).getByRole("button", { name: /可能重复/ }));
    expect(useAppStore.getState().route).toBe("library");
  });
});