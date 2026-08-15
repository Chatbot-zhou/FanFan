import {
  AppstoreOutlined,
  DatabaseOutlined,
  FolderOpenOutlined,
  HomeOutlined,
  InboxOutlined,
  QuestionCircleOutlined,
  SearchOutlined,
  SettingOutlined,
} from "@ant-design/icons";
import { useQuery } from "@tanstack/react-query";
import { bridge, type AppRoute } from "../../bridge";
import { useAppStore } from "../../state/app-store";

const topItems: Array<{ route: AppRoute; label: string; icon: React.ReactNode }> = [
  { route: "home", label: "首页", icon: <HomeOutlined /> },
  { route: "search", label: "找资料", icon: <SearchOutlined /> },
  { route: "ask", label: "问资料", icon: <QuestionCircleOutlined /> },
];

const libraryItems: Array<{ route: AppRoute; label: string; icon: React.ReactNode; badge?: number }> = [
  { route: "library", label: "全部资料", icon: <FolderOpenOutlined /> },
  { route: "collections", label: "智能集合", icon: <AppstoreOutlined /> },
  { route: "inbox", label: "收件箱", icon: <InboxOutlined /> },
];

function NavigationItem({ item }: { item: (typeof topItems)[number] & { badge?: number } }) {
  const route = useAppStore((state) => state.route);
  const navigate = useAppStore((state) => state.navigate);
  return (
    <button
      className={`sidebar-item${route === item.route ? " sidebar-item--active" : ""}`}
      type="button"
      onClick={() => navigate(item.route)}
      aria-current={route === item.route ? "page" : undefined}
    >
      <span className="sidebar-item__icon">{item.icon}</span>
      <span>{item.label}</span>
      {item.badge !== undefined && <span className="sidebar-item__badge">{item.badge >= 100 ? "99+" : item.badge}</span>}
    </button>
  );
}

export function Sidebar() {
  const route = useAppStore((state) => state.route);
  const navigate = useAppStore((state) => state.navigate);
  const inbox = useQuery({
    queryKey: ["inbox", "new", "sidebar"],
    queryFn: () => bridge.inbox_query({ status: "new", event_types: [], root_ids: [], date_from: null, date_to: null, cursor: null, page_size: 100 }),
    refetchInterval: 30_000,
  });
  return (
    <aside className="sidebar" aria-label="主导航">
      <nav className="sidebar__navigation">
        <div className="sidebar__group">
          {topItems.map((item) => <NavigationItem key={item.route} item={item} />)}
        </div>
        <div className="sidebar__divider" />
        <div className="sidebar__group">
          {libraryItems.map((item) => <NavigationItem key={item.route} item={item.route === "inbox" && inbox.data?.items.length ? { ...item, badge: inbox.data.items.length } : item} />)}
        </div>
      </nav>
      <button
        className={`sidebar-item sidebar__settings${route === "settings" ? " sidebar-item--active" : ""}`}
        type="button"
        onClick={() => navigate("settings")}
      >
        <span className="sidebar-item__icon"><SettingOutlined /></span>
        <span>设置</span>
      </button>
    </aside>
  );
}
