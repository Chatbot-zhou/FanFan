import { useEffect, useMemo } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { bridge, type SystemNotice } from "../../bridge";
import { recordDiagnosticEvent } from "../../bridge/observed-bridge";
import { useAppStore } from "../../state/app-store";
import { AskPage } from "../../pages/AskPage";
import { CollectionsPage } from "../../pages/CollectionsPage";
import { HomePage } from "../../pages/HomePage";
import { InboxPage } from "../../pages/InboxPage";
import { LibraryPage } from "../../pages/LibraryPage";
import { SearchPage } from "../../pages/SearchPage";
import { SettingsPage } from "../../pages/SettingsPage";
import { Sidebar } from "./Sidebar";
import { StatusBar } from "./StatusBar";
import { TitleBar } from "./TitleBar";
import { useBackendEvents } from "../../hooks/useBackendEvents";
import { RootAuthorizationPage } from "../onboarding/RootAuthorizationPage";

interface AppShellProps {
  startup_notice: string | null;
}

export function AppShell({ startup_notice }: AppShellProps) {
  const eventNotices = useBackendEvents();
  const queryClient = useQueryClient();
  const route = useAppStore((state) => state.route);
  useEffect(() => {
    let timer: number | undefined;
    const showScrollbars = () => {
      document.documentElement.classList.add("is-scrolling");
      if (timer !== undefined) window.clearTimeout(timer);
      timer = window.setTimeout(() => document.documentElement.classList.remove("is-scrolling"), 800);
    };
    document.addEventListener("scroll", showScrollbars, true);
    window.addEventListener("wheel", showScrollbars, { passive: true });
    window.addEventListener("keydown", showScrollbars);
    return () => {
      document.removeEventListener("scroll", showScrollbars, true);
      window.removeEventListener("wheel", showScrollbars);
      window.removeEventListener("keydown", showScrollbars);
      if (timer !== undefined) window.clearTimeout(timer);
      document.documentElement.classList.remove("is-scrolling");
    };
  }, []);
  const startup = useQuery({
    queryKey: ["startup-state"],
    queryFn: () => bridge.startup_get_state(),
    refetchInterval: (query) => query.state.data?.ready ? false : 500,
  });
  const backendReady = startup.data?.ready === true;
  const model = useQuery({
    queryKey: ["model-runtime"],
    queryFn: () => bridge.model_state_get(),
    enabled: backendReady,
  });
  const currentModelState = model.data ?? null;
  const modelDownloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
    enabled: backendReady,
  });
  const home = useQuery({
    queryKey: ["home-summary", new Date().toLocaleDateString("sv-SE")],
    queryFn: () => bridge.home_get_summary(new Date().toLocaleDateString("sv-SE")),
    refetchInterval: (query) => query.state.data?.scan_progress ? 1500 : 30_000,
    enabled: backendReady,
  });
  const roots = useQuery({
    queryKey: ["roots"],
    queryFn: () => bridge.root_list(),
    enabled: backendReady,
  });
  const welcome = useQuery({
    queryKey: ["welcome-state"],
    queryFn: () => bridge.welcome_get_state(),
    enabled: backendReady,
  });
  const environment = useQuery({
    queryKey: ["environment"],
    queryFn: async () => (await bridge.environment_get_latest()) ?? bridge.environment_detect(),
    enabled: backendReady,
  });
  const appStatus = useQuery({
    queryKey: ["app-status"],
    queryFn: () => bridge.app_status_get(),
    refetchInterval: (query) => query.state.data?.maintenance.active_jobs ? 1500 : 15_000,
    enabled: backendReady,
  });
  const indexRebuild = useQuery({
    queryKey: ["index-rebuild-progress"],
    queryFn: () => bridge.index_rebuild_progress(),
    refetchInterval: (query) => query.state.data?.running ? 1000 : false,
    enabled: backendReady,
  });

  useEffect(() => {
    recordDiagnosticEvent({
      level: "info",
      component: "frontend.navigation",
      event_name: "route.changed",
      fields: { route },
    });
  }, [route]);

  useEffect(() => {
    if (!backendReady) return;
    // 启动就绪前 mount 的页面（如 AskPage）可能在 STARTUP_NOT_READY 失败后
    // 不再重试；backendReady 翻转时全局失效一次，让所有查询自动重新加载，
    // 无需用户手动切页。
    void queryClient.invalidateQueries();
  }, [backendReady, queryClient]);

  useEffect(() => {
    if (!backendReady) return;
    void bridge.environment_detect().then(() => {
      void queryClient.invalidateQueries({ queryKey: ["environment"] });
    }).catch(() => undefined);
  }, [backendReady, queryClient]);

  useEffect(() => {
    if (!backendReady || welcome.data?.root_authorization_completed || !roots.data?.length) return;
    void bridge.welcome_authorization_complete().then(() => {
      void queryClient.invalidateQueries({ queryKey: ["welcome-state"] });
    }).catch(() => undefined);
  }, [backendReady, queryClient, roots.data?.length, welcome.data?.root_authorization_completed]);

  const completeAuthorization = async () => {
    await bridge.welcome_authorization_complete();
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["welcome-state"] }),
      queryClient.invalidateQueries({ queryKey: ["roots"] }),
      queryClient.invalidateQueries({ queryKey: ["home-summary"] }),
      queryClient.invalidateQueries({ queryKey: ["app-status"] }),
    ]);
  };

  const authorizationRequired = backendReady
    && welcome.data?.root_authorization_completed === false
    && roots.data?.length === 0;

  // 汇集系统通知给 TitleBar
  const notices = useMemo<SystemNotice[]>(() => {
    const items: SystemNotice[] = [];
    // 启动阻塞（最高优先级）
    if (startup.data?.blocker) {
      items.push({
        notice_key: "startup-blocker",
        level: "urgent",
        message: startup.data.blocker.message,
        details: startup.data.blocker.message,
        action_label: null,
        action_route: null,
      });
    }
    // 后台事件通知（模型下载失败等）
    items.push(...eventNotices);
    // 欢迎页持久化失败
    if (startup_notice) {
      items.push({
        notice_key: "startup-notice",
        level: "warning",
        message: startup_notice,
        details: startup_notice,
        action_label: null,
        action_route: null,
      });
    }
    // 后台维护通知（磁盘不足、降级等）
    if (appStatus.data?.maintenance.background_notice) {
      items.push({
        notice_key: "background-notice",
        level: "warning",
        message: appStatus.data.maintenance.background_notice,
        details: appStatus.data.maintenance.background_notice,
        action_label: null,
        action_route: null,
      });
    }
    if (route === "collections" && currentModelState && (!currentModelState.capabilities.embedding || !currentModelState.capabilities.generation)) {
      const missing = [
        !currentModelState.capabilities.embedding ? "Embedding" : null,
        !currentModelState.capabilities.generation ? "生成模型" : null,
      ].filter(Boolean).join("、");
      items.push({
        notice_key: "collection-ai-unavailable",
        level: "warning",
        message: `AI集合分析暂不可用 · 缺少${missing}`,
        details: "手动集合、规则集合和已有虚拟集合仍可使用。",
        action_label: "配置模型",
        action_route: "model_setup",
      });
    }
    return items;
  }, [startup.data?.blocker, eventNotices, startup_notice, appStatus.data?.maintenance.background_notice, route, currentModelState]);

  if (authorizationRequired) {
    return (
      <div className="app-window">
        <TitleBar model_state={currentModelState} model_downloads={modelDownloads.data ?? []} />
        <RootAuthorizationPage onCompleted={completeAuthorization} />
      </div>
    );
  }

  const page = {
    home: <HomePage summary={home.data ?? null} loading={home.isLoading} maintenance={appStatus.data?.maintenance ?? null} />,
    search: <SearchPage />,
    ask: <AskPage model_state={currentModelState} />,
    library: <LibraryPage />,
    collections: <CollectionsPage />,
    inbox: <InboxPage />,
    settings: <SettingsPage />,
    // model_setup 兼容入口：直接落在设置页「本地模型」tab（下载通知、状态中心等沿用该路由名）。
    model_setup: <SettingsPage initialTab="models" />,
  }[route];

  return (
    <div className="app-window">
      <TitleBar model_state={currentModelState} model_downloads={modelDownloads.data ?? []} notices={notices} />
      <div className="app-window__body">
        <Sidebar />
        <main className="workspace">
          {page}
        </main>
      </div>
      <StatusBar snapshot={appStatus.data ?? null} rebuildProgress={indexRebuild.data} />
    </div>
  );
}
