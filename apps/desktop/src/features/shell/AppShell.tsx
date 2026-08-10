import { useEffect, useMemo } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { bridge, type SystemNotice } from "../../bridge";
import { useAppStore } from "../../state/app-store";
import { AskPage } from "../../pages/AskPage";
import { CollectionsPage } from "../../pages/CollectionsPage";
import { HomePage } from "../../pages/HomePage";
import { InboxPage } from "../../pages/InboxPage";
import { LibraryPage } from "../../pages/LibraryPage";
import { ModelSetupPage } from "../../pages/ModelSetupPage";
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

const STARTUP_PHASE_LABELS: Record<string, string> = {
  opening_catalog: "正在打开本地资料库",
  recovering_jobs: "正在恢复上次未完成的任务",
  scheduling_background_work: "正在安排后台索引任务",
  ready: "后台服务已就绪",
  degraded: "部分后台服务暂时不可用",
};

export function AppShell({ startup_notice }: AppShellProps) {
  const eventNotice = useBackendEvents();
  const queryClient = useQueryClient();
  const route = useAppStore((state) => state.route);
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
  const maintenance = useQuery({
    queryKey: ["maintenance"],
    queryFn: () => bridge.maintenance_get(),
    refetchInterval: 15_000,
    enabled: backendReady,
  });

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
    ]);
  };

  const authorizationRequired = backendReady
    && welcome.data?.root_authorization_completed === false
    && roots.data?.length === 0;

  if (authorizationRequired) {
    return (
      <div className="app-window">
        <TitleBar model_state={currentModelState} model_download={modelDownloads.data?.[0] ?? null} />
        <RootAuthorizationPage onCompleted={completeAuthorization} />
      </div>
    );
  }

  // 汇集系统通知给 TitleBar
  const notices = useMemo<SystemNotice[]>(() => {
    const items: SystemNotice[] = [];
    // 启动阻塞（最高优先级）
    if (startup.data?.blocker) {
      items.push({
        level: "urgent",
        message: startup.data.blocker.message,
        action_label: null,
        action_route: null,
      });
    }
    // 后台事件通知（模型下载失败等）
    if (eventNotice) {
      items.push({
        level: "warning",
        message: eventNotice,
        action_label: null,
        action_route: null,
      });
    }
    // 欢迎页持久化失败
    if (startup_notice) {
      items.push({
        level: "warning",
        message: startup_notice,
        action_label: null,
        action_route: null,
      });
    }
    // 后台维护通知（磁盘不足、降级等）
    if (maintenance.data?.background_notice) {
      items.push({
        level: "warning",
        message: maintenance.data.background_notice,
        action_label: null,
        action_route: null,
      });
    }
    return items;
  }, [startup.data?.blocker, eventNotice, startup_notice, maintenance.data?.background_notice]);

  const page = {
    home: <HomePage summary={home.data ?? null} loading={home.isLoading} />,
    search: <SearchPage />,
    ask: <AskPage model_state={currentModelState} />,
    library: <LibraryPage />,
    collections: <CollectionsPage />,
    inbox: <InboxPage />,
    settings: <SettingsPage environment={environment.data ?? null} />,
    model_setup: <ModelSetupPage />,
  }[route];

  return (
    <div className="app-window">
      <TitleBar model_state={currentModelState} model_download={modelDownloads.data?.[0] ?? null} notices={notices} />
      <div className="app-window__body">
        <Sidebar />
        <main className="workspace">
          {!backendReady && <div className="startup-notice" role="status">{STARTUP_PHASE_LABELS[startup.data?.phase ?? "opening_catalog"]} · {Math.round((startup.data?.progress ?? 0.1) * 100)}%</div>}
          {page}
        </main>
      </div>
      <StatusBar summary={home.data ?? null} roots={roots.data ?? null} maintenance={maintenance.data ?? null} />
    </div>
  );
}
