import { useEffect, useRef } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { bridge } from "../../bridge";
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

interface AppShellProps {
  startup_notice: string | null;
}

const STARTUP_PHASE_LABELS: Record<string, string> = {
  opening_catalog: "正在打开本地资料库",
  recovering_jobs: "正在恢复上次未完成的任务",
  scheduling_background_work: "正在安排后台索引任务",
  ready: "后台服务已就绪",
  degraded: "后台服务已进入核心模式",
};

export function AppShell({ startup_notice }: AppShellProps) {
  const eventNotice = useBackendEvents();
  const queryClient = useQueryClient();
  const discoveryStarted = useRef(false);
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
    if (!backendReady || discoveryStarted.current) return;
    discoveryStarted.current = true;
    void bridge.root_discover_defaults().then(() => {
      void queryClient.invalidateQueries({ queryKey: ["roots"] });
      void queryClient.invalidateQueries({ queryKey: ["home-summary"] });
    }).catch(() => undefined);
    void bridge.environment_detect().then(() => {
      void queryClient.invalidateQueries({ queryKey: ["environment"] });
    }).catch(() => undefined);
  }, [backendReady, queryClient]);

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
      <TitleBar model_state={currentModelState} />
      <div className="app-window__body">
        <Sidebar />
        <main className="workspace">
          {!backendReady && <div className="startup-notice" role="status">{STARTUP_PHASE_LABELS[startup.data?.phase ?? "opening_catalog"]} · {Math.round((startup.data?.progress ?? 0.1) * 100)}%</div>}
          {(startup_notice || eventNotice || startup.data?.blocker) && <div className="startup-notice" role="status">{startup_notice || eventNotice || startup.data?.blocker?.message}</div>}
          {page}
        </main>
      </div>
      <StatusBar summary={home.data ?? null} roots={roots.data ?? null} maintenance={maintenance.data ?? null} />
    </div>
  );
}
