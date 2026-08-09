import { create } from "zustand";
import type { AppRoute, InboxQuery } from "../bridge";

type InboxStatus = InboxQuery["status"];

interface AppState {
  route: AppRoute;
  search_query: string;
  inbox_initial_status: InboxStatus;
  inbox_today_only: boolean;
  selected_collection_id: string | null;
  model_prompt_dismissed: boolean;
  navigate: (route: AppRoute) => void;
  start_search: (query: string) => void;
  open_inbox: (status: InboxStatus, todayOnly?: boolean) => void;
  open_collection: (collectionId: string) => void;
  clear_collection_selection: () => void;
  dismiss_model_prompt: () => void;
}

export const useAppStore = create<AppState>((set) => ({
  route: "home",
  search_query: "",
  inbox_initial_status: "new",
  inbox_today_only: false,
  selected_collection_id: null,
  model_prompt_dismissed: false,
  navigate: (route) => set(route === "inbox"
    ? { route, inbox_initial_status: "new", inbox_today_only: false }
    : route === "collections"
      ? { route, selected_collection_id: null }
      : { route }),
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
  clear_collection_selection: () => set({ selected_collection_id: null }),
  dismiss_model_prompt: () => set({ model_prompt_dismissed: true }),
}));
