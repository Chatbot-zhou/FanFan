import {
  AppstoreOutlined,
  CheckCircleFilled,
  ClockCircleOutlined,
  CopyOutlined,
  DatabaseOutlined,
  ExclamationCircleOutlined,
  FileAddOutlined,
  FolderOpenOutlined,
  InboxOutlined,
  QuestionCircleOutlined,
  SearchOutlined,
  ToolOutlined,
} from "@ant-design/icons";
import type { HomeSummary } from "../bridge";
import type { ModelSetupState } from "../features/model-management/model-setup-state";
import { useAppStore } from "../state/app-store";

interface HomePageProps {
  summary: HomeSummary | null;
  loading: boolean;
  model_setup: ModelSetupState;
  /** null 表示授权目录仍在读取中。 */
  authorized_root_count: number | null;
}

interface SetupStep {
  key: "model" | "roots" | "index";
  title: string;
  detail: string;
  done: boolean;
  notice?: string;
  cta: string | null;
  action: (() => void) | null;
  icon: React.ReactNode;
}

interface QuickEntry {
  label: string;
  desc: string;
  route: "ask" | "search" | "collections" | "inbox";
  icon: React.ReactNode;
}

const metricIcons: Record<string, React.ReactNode> = {
  today_added: <FileAddOutlined aria-hidden="true" />,
  awaiting_confirmation: <ClockCircleOutlined aria-hidden="true" />,
  possible_duplicates: <CopyOutlined aria-hidden="true" />,
  processing_failed: <ExclamationCircleOutlined aria-hidden="true" />,
};

export function HomePage({ summary, loading, model_setup, authorized_root_count }: HomePageProps) {
  const navigate = useAppStore((state) => state.navigate);
  const openInbox = useAppStore((state) => state.open_inbox);
  const setSettingsTab = useAppStore((state) => state.set_settings_tab);
  const openRootSettings = () => { setSettingsTab("roots"); navigate("settings"); };
  const openModelSettings = () => { setSettingsTab("models"); navigate("model_setup"); };

  // 就绪判定：模型、资料来源、索引。
  const modelReady = model_setup.status === "ready";
  const rootStatus = authorized_root_count === null ? "checking" : authorized_root_count > 0 ? "ready" : "empty";
  const rootReady = rootStatus === "ready";
  const indexReady = summary?.index_initialized === true;

  // 主行动按钮随引导状态动态切换；全部就绪后只剩「开始问资料」。
  const primary = !modelReady
    ? { label: model_setup.action_label, action: openModelSettings }
    : rootStatus === "checking"
      ? { label: "查看资料来源", action: openRootSettings }
      : rootStatus === "empty"
        ? { label: "添加资料", action: openRootSettings }
        : !indexReady
          ? { label: "查看整理进度", action: () => openInbox("all") }
          : { label: "开始问资料", action: () => navigate("ask") };

  const readyAll = modelReady && rootReady && indexReady;
  const doneStepCount = [modelReady, rootReady, indexReady].filter(Boolean).length;

  const scanProgressText = summary?.scan_progress
    ? `正在整理 ${summary.scan_progress.parsed_files}/${summary.scan_progress.discovered_files} 个文件`
    : "资料需要完成解析和索引后，才能稳定搜索和问资料。";

  // 四个配置步骤组成的引导向导；全部就绪后折叠为完成横幅。
  const steps: SetupStep[] = [
    {
      key: "model",
      title: "配置本地模型",
      detail: modelReady ? "问答与语义检索模型已就绪。" : model_setup.description,
      done: modelReady,
      cta: modelReady ? null : model_setup.action_label,
      action: modelReady ? null : openModelSettings,
      icon: <ToolOutlined aria-hidden="true" />,
    },
    {
      key: "roots",
      title: "添加资料来源",
      detail: rootReady
        ? `已授权 ${authorized_root_count} 个资料目录。`
        : rootStatus === "checking"
          ? "正在确认已授权目录。"
          : "还没有添加资料目录，配置后才会开始读取。",
      done: rootReady,
      cta: rootReady ? null : (rootStatus === "checking" ? "查看资料来源" : "添加资料目录"),
      action: rootReady ? null : openRootSettings,
      icon: <FolderOpenOutlined aria-hidden="true" />,
    },
    {
      key: "index",
      title: "解析并建立索引",
      detail: indexReady ? "资料已完成解析，可以稳定搜索和问答。" : scanProgressText,
      done: indexReady,
      cta: indexReady ? null : "查看整理进度",
      action: indexReady ? null : () => openInbox("all"),
      icon: <DatabaseOutlined aria-hidden="true" />,
    },
  ];

  const quickEntries: QuickEntry[] = [
    { label: "问资料", desc: "基于已授权资料提问", route: "ask", icon: <QuestionCircleOutlined aria-hidden="true" /> },
    { label: "找资料", desc: "关键词搜索本地资料", route: "search", icon: <SearchOutlined aria-hidden="true" /> },
    { label: "智能集合", desc: "按主题自动归类整理", route: "collections", icon: <AppstoreOutlined aria-hidden="true" /> },
    { label: "收件箱", desc: "查看今日新增与待办", route: "inbox", icon: <InboxOutlined aria-hidden="true" /> },
  ];

  return (
    <section className="page page--home" aria-label="翻翻首页">
      {/* 顶部横幅：左侧标语，右侧主行动按钮 + 准备进度提示 */}
      <header className="home-hero">
        <div className="home-hero__copy">
          <h1>想不起来？翻翻就知道。</h1>
          <p className="home-hero__sub">
            {readyAll
              ? "全部准备就绪，可以直接基于你的资料提问了。"
              : `已完成 ${doneStepCount}/3 项准备，跟着下方指引完成设置即可开始。`}
          </p>
        </div>
        <div className="home-hero__actions">
          <span className="home-hero__progress">准备进度 {doneStepCount}/3</span>
          <button type="button" className="primary-button primary-button--large" onClick={primary.action}>
            {primary.label}
          </button>
        </div>
      </header>

      {/* 引导向导：全部就绪后折叠为完成横幅 */}
      {readyAll ? (
        <div className="home-setup home-setup--done" role="status">
          <CheckCircleFilled aria-hidden="true" />
          <div>
            <strong>全部就绪</strong>
            <span>模型、资料来源与索引都已准备好，可以进行本地问答和搜索。</span>
          </div>
          <span className="home-setup__badge">准备完成</span>
        </div>
      ) : (
        <section className="home-setup" aria-label="开始配置，共 3 步">
          {steps.map((step) => (
            <div className={`home-setup__step home-setup__step--${step.done ? "done" : "todo"}`} key={step.key}>
              <div className="home-setup__icon">{step.done ? <CheckCircleFilled aria-hidden="true" /> : step.icon}</div>
              <div className="home-setup__body">
                <strong>{step.title}</strong>
                <span>{step.detail}</span>
              </div>
              {step.done
                ? <em className="home-setup__state">已就绪</em>
                : <>
                    <em className="home-setup__state">{step.notice ?? "待完成"}</em>
                    {step.cta && <button type="button" onClick={step.action ?? undefined}>{step.cta}</button>}
                  </>}
            </div>
          ))}
        </section>
      )}

      {/* 两个分区上下排列，各区内部卡片横向铺满整行 */}
      <section className="home-sections">
        <section className="home-panel home-quickstart" aria-label="开始使用">
          <h2 className="home-zone__heading">开始使用</h2>
          <div className="home-quickstart__grid">
            {quickEntries.map((entry) => (
              <button className="home-quickstart__item" type="button" key={entry.route} onClick={() => navigate(entry.route)}>
                <span className="home-quickstart__icon">{entry.icon}</span>
                <span className="home-quickstart__label">{entry.label}</span>
                <small>{entry.desc}</small>
              </button>
            ))}
          </div>
        </section>

        <section className="home-metrics" aria-label="数据概览">
          <h2 className="home-zone__heading">数据概览</h2>
          <div className="metric-grid" aria-busy={loading}>
            {loading && Array.from({ length: 4 }, (_, index) => <div className="metric-card metric-card--loading" key={index}>正在读取…</div>)}
            {(summary?.metrics ?? []).map((metric) => (
              <button className={`metric-card metric-card--${metric.key}`} type="button" key={metric.key} onClick={() => {
                if (metric.key === "today_added") openInbox("all", true);
                else if (metric.key === "processing_failed") openInbox("error");
                else if (metric.key === "possible_duplicates") navigate("library");
                else openInbox("new");
              }}>
                <span className="metric-card__icon">{metricIcons[metric.key]}</span>
                <span><small>{metric.label}</small><strong>{metric.value}</strong></span>
              </button>
            ))}
          </div>
        </section>
      </section>
    </section>
  );
}