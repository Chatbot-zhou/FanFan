import { create } from "zustand";
import type { AppRoute, InboxQuery } from "../bridge";

type InboxStatus = InboxQuery["status"];
export type SettingsTab = "roots" | "models" | "index" | "appearance" | "logs";

interface AppState {
  route: AppRoute;
  previous_route: AppRoute | null;
  search_query: string;
  inbox_initial_status: InboxStatus;
  inbox_today_only: boolean;
  selected_collection_id: string | null;
  settings_tab: SettingsTab;
  model_prompt_dismissed: boolean;
  navigate: (route: AppRoute) => void;
  go_back: () => void;
  start_search: (query: string) => void;
  open_inbox: (status: InboxStatus, todayOnly?: boolean) => void;
  open_collection: (collectionId: string) => void;
  open_settings: (tab: SettingsTab) => void;
  set_settings_tab: (tab: SettingsTab) => void;
  clear_collection_selection: () => void;
  dismiss_model_prompt: () => void;
}

export const useAppStore = create<AppState>((set) => ({
  route: "home",
  previous_route: null,
  search_query: "",
  inbox_initial_status: "new",
  inbox_today_only: false,
  selected_collection_id: null,
  settings_tab: "roots",
  model_prompt_dismissed: false,
  navigate: (route) => set((state) => route === state.route ? state : (route === "inbox"
    ? { route, previous_route: state.route, inbox_initial_status: "new", inbox_today_only: false }
    : route === "collections"
      ? { route, previous_route: state.route, selected_collection_id: null }
      : { route, previous_route: state.route })),
  go_back: () => set((state) => state.previous_route
    ? { route: state.previous_route, previous_route: state.route }
    : state),
  start_search: (search_query) => set({ route: "search", search_query }),
  open_inbox: (inbox_initial_status, inbox_today_only = false) => set({
    route: "inbox",
    inbox_initial_status,
    inbox_today_only,
  }),
  open_collection: (selected_collection_id) => set({
    route: "collections",
    selected_collection_id,
  }),
  open_settings: (settings_tab) => set((state) => ({
    route: "settings",
    previous_route: state.route,
    settings_tab,
  })),
  set_settings_tab: (settings_tab) => set({ settings_tab }),
  clear_collection_selection: () => set({ selected_collection_id: null }),
  dismiss_model_prompt: () => set({ model_prompt_dismissed: true }),
}));
