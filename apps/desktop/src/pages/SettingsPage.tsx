import { DeleteOutlined, FolderOpenOutlined, ReloadOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { bridge } from "../bridge";
import { confirmAction } from "../components/AppConfirm";
import {
  formatModelDownloadBytes,
  summarizeModelDownloads,
  visibleModelDownloadJobs,
} from "../features/model-downloads/model-downloads";
import { useAppStore } from "../state/app-store";
import { useThemePreference } from "../features/theme/ThemeProvider";
import { errorMessage } from "../utils/app-error";

const bytes = (value: number) => value < 1024 * 1024 ? `${Math.round(value / 1024)} KB` : value < 1024 * 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MB` : `${(value / 1024 / 1024 / 1024).toFixed(2)} GB`;

const CAPABILITY_LABELS = [
  ["generation", "问答"],
  ["embedding", "Embedding"],
  ["vision", "多模态"],
  ["reranker", "Rerank"],
  ["tts", "语音合成"],
  ["asr", "语音识别"],
  ["ocr", "OCR"],
] as const;

export function SettingsPage() {
  const navigate = useAppStore((state) => state.navigate);
  const tab = useAppStore((state) => state.settings_tab);
  const setTab = useAppStore((state) => state.set_settings_tab);
  const theme = useThemePreference();
  const queryClient = useQueryClient();
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const roots = useQuery({ queryKey: ["settings-roots"], queryFn: () => bridge.root_list() });
  const models = useQuery({ queryKey: ["settings-models"], queryFn: async () => ({ state: await bridge.model_state_get(), artifacts: await bridge.model_artifact_list() }) });
  const modelStore = useQuery({ queryKey: ["model-store-status"], queryFn: () => bridge.model_store_status_get(), enabled: tab === "index" });
  const modelDownloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    enabled: tab === "models",
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
  });
  const maintenance = useQuery({ queryKey: ["maintenance"], queryFn: () => bridge.maintenance_get() });
  const storage = useQuery({ queryKey: ["storage-usage"], queryFn: () => bridge.storage_usage_get(), enabled: tab === "index" });
  const storageLocation = useQuery({ queryKey: ["storage-location"], queryFn: () => bridge.storage_location_get(), enabled: tab === "index" });
  const logs = useInfiniteQuery({
    queryKey: ["maintenance-logs"],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.maintenance_log_query({ cursor: pageParam, page_size: 100 }),
    getNextPageParam: (page) => page.next_cursor,
    enabled: tab === "logs",
    refetchInterval: tab === "logs" ? 3_000 : false,
  });
  const logItems = logs.data?.pages.flatMap((page) => page.items) ?? [];
  const [traceFlow, setTraceFlow] = useState<string | null>(null);
  const traces = useInfiniteQuery({
    queryKey: ["node-traces", traceFlow],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.node_trace_query({ flow: traceFlow, node: null, cursor: pageParam, page_size: 100 }),
    getNextPageParam: (page) => page.next_cursor,
    enabled: tab === "node-traces",
    refetchInterval: tab === "node-traces" ? 3_000 : false,
  });
  const traceItems = traces.data?.pages.flatMap((page) => page.items) ?? [];
  const [expandedTrace, setExpandedTrace] = useState<string | null>(null);
  const visibleDownloads = useMemo(() => visibleModelDownloadJobs(modelDownloads.data ?? []), [modelDownloads.data]);
  const downloadSummary = useMemo(() => summarizeModelDownloads(visibleDownloads), [visibleDownloads]);
  const activeCapabilities = useMemo(() => {
    const caps = models.data?.state.capabilities;
    if (!caps) return null;
    return CAPABILITY_LABELS.filter(([key]) => caps[key]).map(([, label]) => label);
  }, [models.data?.state.capabilities]);

  const addRoot = async (volumeOnly: boolean) => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不调用系统目录选择器，请在桌面程序中使用。"); return; }
    const selected = await open({ directory: true, multiple: false, title: volumeOnly ? "选择本地磁盘根目录" : "添加资料文件夹" });
    if (typeof selected !== "string") return;
    const fullVolume = /^[a-zA-Z]:\\?$/.test(selected);
    if (volumeOnly && !fullVolume) { setError("添加整个磁盘时请选择盘符根目录，例如 D:\\。"); return; }
    if (fullVolume && !await confirmAction({ actionKey: "settings_add_full_volume", title: "添加整个磁盘？", description: "扫描可能耗时较长，系统、程序、凭据、应用数据和翻翻自身目录仍会被强制排除。", confirmLabel: "确认添加" })) return;
    setBusy(true);
    try { await bridge.root_add({ path: selected, label: null, watch_mode: "realtime", authorization_source: "user_selected", full_volume_confirmed: fullVolume }); await roots.refetch(); setMessage("资料位置已添加，扫描会在后台继续。"); }
    catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const rebuild = async () => {
    if (!await confirmAction({ actionKey: "index_rebuild", title: "重建派生索引？", description: "翻翻会在后台建立新索引并保留当前可用索引，校验成功后才切换。不会修改任何源文件。", confirmLabel: "开始重建", danger: true })) return;
    setBusy(true); setError(null); setMessage(null);
    try { const operation = await bridge.index_rebuild("REBUILD_INDEX"); setMessage(`索引重建已进入后台队列（${operation.operation_id.slice(0, 8)}）。当前索引会继续服务，新结果按文件校验后逐步替换；源文件修改：否。`); await maintenance.refetch(); }
    catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const clearLogs = async () => {
    if (!await confirmAction({ actionKey: "diagnostic_logs_clear", title: "清除本地诊断日志？", description: "此操作不会影响资料和索引，但会删除当前用于排查体验问题的记录。", confirmLabel: "清除日志", danger: true })) return;
    setBusy(true); setError(null);
    try { const count = await bridge.maintenance_logs_clear(); setMessage(`已清除 ${count} 条本地诊断日志。`); await Promise.all([logs.refetch(), maintenance.refetch()]); }
    catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const clearNodeTraces = async () => {
    if (!await confirmAction({ actionKey: "node_traces_clear", title: "清空节点追踪记录？", description: "删除全部链路节点的输入输出快照（最多保留最近 2 万条），不影响资料、索引和问答记录。", confirmLabel: "清空追踪记录", danger: true })) return;
    setBusy(true); setError(null);
    try { const count = await bridge.node_trace_clear(); setMessage(`已清空 ${count} 条节点追踪记录。`); await traces.refetch(); }
    catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const toggleTrace = (traceId: string) => setExpandedTrace((current) => current === traceId ? null : traceId);
  const copyTraceJson = async (value: unknown, label: string) => {
    try { await navigator.clipboard.writeText(typeof value === "string" ? value : JSON.stringify(value, null, 2)); setMessage(`已复制${label}到剪贴板。`); }
    catch { setError("复制失败，请手动选择文本复制。"); }
  };

  const exportDiagnostics = async () => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不写入电脑文件，请在桌面程序中使用。"); return; }
    const target = await save({ title: "导出本地诊断包", defaultPath: `翻翻-诊断-${new Date().toISOString().slice(0, 10)}.json`, filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!target) return;
    if (!await confirmAction({ actionKey: "diagnostic_export", title: "导出本地诊断包？", description: "包含脱敏后的启动、环境、模型任务状态和运行日志，不包含文档正文或提问内容；目标已存在时会拒绝覆盖。", confirmLabel: "导出" })) return;
    setBusy(true);
    try {
      const result = await bridge.diagnostic_export(target, true);
      setMessage(`诊断包已新建：${result.size_bytes ? bytes(result.size_bytes) : "完成"}。翻翻没有修改源文件。`);
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const checkDatabase = async (level: "quick" | "full") => {
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.maintenance_check(level);
      if (result.database_result !== "ok") throw new Error(`数据库检查未通过：${result.database_result}`);
      setMessage(`${level === "full" ? "完整" : "快速"}数据库检查通过，用时 ${result.elapsed_ms} 毫秒；源文件修改：否。`);
      await maintenance.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const clearCache = async (category: "temporary_cache" | "failed_downloads", label: string) => {
    if (!await confirmAction({ actionKey: `cache_clear_${category}`, title: `清理“${label}”？`, description: "只删除翻翻可重建或已判定无效的缓存，不会修改源文件、索引数据库、已安装模型和可续传下载。", confirmLabel: "清理缓存", danger: true })) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.cache_clear(category, "CLEAR_CACHE");
      setMessage(`已清理“${label}”：释放 ${bytes(result.freed_bytes)}，移除 ${result.removed_entries} 个缓存项；源文件修改：否。`);
      await storage.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const resetApplicationData = async () => {
    if (!await confirmAction({ actionKey: "application_data_reset", title: "重置翻翻应用数据？", description: "这会关闭翻翻，并在下次启动前隔离数据库、索引、设置、日志和缓存。独立模型仓库及所有已下载模型会完整保留，源文件不会被修改。", confirmLabel: "重置并重新启动", danger: true, confirmPhrase: "RESET_APPLICATION_DATA" })) return;
    setBusy(true); setError(null); setMessage("正在安排安全重置，翻翻将重新启动……");
    try { await bridge.app_data_reset_schedule("RESET_APPLICATION_DATA"); }
    catch (actionError) { setError(errorMessage(actionError)); setBusy(false); }
  };

  const disableRoot = async (rootId: string, label: string) => {
    if (!await confirmAction({ actionKey: "settings_remove_root", title: `从翻翻移除“${label}”？`, description: "翻翻会立即停止读取并撤销授权，派生数据在后台清理；不会删除、移动或修改任何源文件。", confirmLabel: "从翻翻移除", danger: true })) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      await bridge.root_disable(rootId);
      setMessage(`已从翻翻移除“${label}”。原文件没有变化；以后仍可重新添加。`);
      await roots.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const scheduleStorageMigration = async () => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不迁移电脑文件，请在翻翻桌面程序中使用。"); return; }
    const selected = await open({ directory: true, multiple: false, title: "选择新的翻翻应用数据磁盘或文件夹" });
    if (typeof selected !== "string") return;
    if (!await confirmAction({ actionKey: "storage_migration", title: "迁移翻翻应用数据？", description: "数据库、索引和缓存会在下次启动前迁移并逐文件校验；独立模型仓库保持原位且不会被删除。全部成功后才切换，资料源文件不受影响。", confirmLabel: "安排迁移" })) return;
    setBusy(true);
    try {
      const status = await bridge.storage_migration_schedule(selected, "MIGRATE_APPLICATION_STORAGE");
      setMessage(`迁移计划已保存：${status.pending_target_directory ?? selected}。请关闭并重新打开翻翻；失败时会继续使用原位置。`);
      await storageLocation.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  return (
    <section className="page">
      <header className="page-heading"><div><h1>设置</h1><p>管理资料位置、本地模型、索引和异常恢复。</p></div></header>
      {error && <p role="alert" className="inline-error">{error}</p>}{message && <p className="inline-success">{message}</p>}
      <div className="settings-grid">
        <aside className="settings-nav">
          <button type="button" className={tab === "roots" ? "active" : ""} onClick={() => setTab("roots")}>资料目录</button><button type="button" className={tab === "models" ? "active" : ""} onClick={() => setTab("models")}>本地模型</button><button type="button" className={tab === "index" ? "active" : ""} onClick={() => setTab("index")}>索引与存储</button><button type="button" className={tab === "appearance" ? "active" : ""} onClick={() => setTab("appearance")}>外观与辅助功能</button><button type="button" className={tab === "logs" ? "active" : ""} onClick={() => setTab("logs")}>日志与恢复</button><button type="button" className={tab === "node-traces" ? "active" : ""} onClick={() => setTab("node-traces")}>节点追踪</button>
        </aside>
        <div className="settings-content">
          {tab === "index" && <section><h2>自适应存储空间</h2><p>翻翻根据当前磁盘容量自动计算软配额（容量的10%，最低10GB、最高50GB），无需手动设置。达到配额后会暂停可恢复的后台增强任务，搜索和预览仍可使用。</p><div className="metric-strip"><span><strong>{storage.data ? bytes(storage.data.total_bytes) : "—"}</strong>当前占用</span><span><strong>{storage.data ? bytes(storage.data.soft_quota_bytes) : "—"}</strong>自适应配额</span><span><strong>{storage.data?.disk_available_bytes != null ? bytes(storage.data.disk_available_bytes) : "—"}</strong>磁盘可用</span></div>{storage.data?.notice && <p role="status" className="inline-error">{storage.data.notice}</p>}<small>应用管理目录：{storage.data?.data_directory ?? "正在读取"}</small></section>}
          {tab === "index" && <section><h2>迁移应用数据</h2><p>可把数据库、USearch索引和缓存迁移到其他本地磁盘。已下载模型位于独立仓库，不随数据库重置或应用数据迁移而删除。</p><div className="readonly-note">当前位置：{storageLocation.data?.active_data_directory ?? storage.data?.data_directory ?? "正在读取"}{storageLocation.data?.pending_target_directory ? ` · 等待迁移到 ${storageLocation.data.pending_target_directory}` : ""}</div>{storageLocation.data?.last_error && <p role="alert" className="inline-error">{storageLocation.data.last_error}</p>}<div className="settings-actions"><button type="button" disabled={busy || storageLocation.data?.restart_required} onClick={() => void scheduleStorageMigration()}><FolderOpenOutlined /> {storageLocation.data?.restart_required ? "等待重启迁移" : "选择新位置"}</button></div></section>}
          {tab === "index" && <section><h2>迁移模型</h2><p>已下载模型位于独立模型仓库，与应用数据分开存储；迁移应用数据或重置应用数据时都不会移动模型文件。</p><div className="readonly-note">模型仓库位置：{modelStore.data?.store_path ?? "正在读取"}{modelStore.data ? ` · ${modelStore.data.installed_artifacts} 个组件 / ${bytes(modelStore.data.installed_bytes)}` : ""}</div></section>}
          {tab === "roots" && <>
            <section><h2>资料目录</h2><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void addRoot(false)}><FolderOpenOutlined /> 添加文件夹</button><button type="button" disabled={busy} onClick={() => void addRoot(true)}><FolderOpenOutlined /> 添加整个磁盘</button></div></section>
            <section><h2>已授权位置</h2><div className="settings-list">{roots.data?.map((root) => <div key={root.root_id}><span><strong>{root.label}</strong><small>{root.path}</small></span><em>{root.enabled ? `${root.file_count} 个文件` : "已停用"}</em><button type="button" className="text-button" disabled={busy} onClick={() => void disableRoot(root.root_id, root.label)}>从翻翻移除</button></div>)}{!roots.isLoading && roots.data?.length === 0 && <p>当前没有资料目录。添加前翻翻不会读取本地文件。</p>}</div></section>
          </>}
          {tab === "models" && <>
            {visibleDownloads.length > 0 && <section><h2>模型下载</h2><div className="setting-row model-download-overview"><div><strong>{downloadSummary.attention_count > 0 ? `${downloadSummary.attention_count} 项需要处理` : downloadSummary.active_count > 0 ? `${downloadSummary.active_count} 个任务正在进行` : `${downloadSummary.visible_count} 个任务已结束`}</strong><small>{downloadSummary.active_count > 0 ? `${downloadSummary.progress == null ? "正在准备下载" : `总体 ${Math.round(downloadSummary.progress * 100)}%`} · ${formatModelDownloadBytes(downloadSummary.downloaded_bytes)} / ${formatModelDownloadBytes(downloadSummary.total_bytes)}` : "完成和取消记录仅保留到本次应用会话结束"}</small></div><button type="button" onClick={() => navigate("model_setup")}>查看全部任务</button></div></section>}
            <section><h2>当前模型</h2><div className="setting-row"><div className="capability-list">{activeCapabilities === null ? "正在读取" : activeCapabilities.length > 0 ? activeCapabilities.join(" · ") : "尚未启用本地模型"}</div><button type="button" onClick={() => navigate("model_setup")}>管理模型</button></div></section>
            <section><h2>已导入组件</h2><div className="settings-list">{models.data?.artifacts.map((artifact) => <div key={artifact.artifact_id}><span><strong>{artifact.model_id}</strong><small>{artifact.role} · {artifact.format.toUpperCase()} · {bytes(artifact.size_bytes)}</small></span><em>{artifact.embedding_dimension ? `已自检 · ${artifact.embedding_dimension}维` : artifact.status}</em></div>)}{models.data?.artifacts.length === 0 && <p>还没有导入本地模型。基础搜索不受影响。</p>}</div></section>
          </>}
          {tab === "index" && <><section><h2>索引状态</h2><div className="metric-strip"><span><strong>{maintenance.data?.indexed_files ?? "—"}</strong>已索引文件</span><span><strong>{maintenance.data?.searchable_chunks ?? "—"}</strong>全文块</span><span><strong>{maintenance.data?.embedded_chunks ?? "—"}</strong>向量块</span><span><strong>{maintenance.data ? bytes(maintenance.data.database_size_bytes) : "—"}</strong>数据库</span></div></section><section><h2>存储分类</h2><p>只允许清理明确标记为缓存的内容；模型断点、索引与源文件不会作为缓存删除。</p><div className="settings-list">{storage.data?.categories.map((category) => <div key={category.key}><span><strong>{category.label}</strong><small>{category.detail}</small></span><em>{bytes(category.size_bytes)}</em>{category.clearable && <button type="button" className="text-button" disabled={busy || category.size_bytes === 0} onClick={() => void clearCache(category.key as "temporary_cache" | "failed_downloads", category.label)}>清理</button>}</div>)}</div></section><section><h2>自动检查站</h2><div className="health-list">{maintenance.data?.checks.map((check) => <div key={check.key} className={`health-${check.status}`}><i /><span><strong>{check.label}</strong><small>{check.detail}</small></span></div>)}</div><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void checkDatabase("quick")}><ReloadOutlined /> {busy ? "检查中" : "快速检查"}</button><button type="button" disabled={busy} onClick={() => void checkDatabase("full")}><ReloadOutlined /> {busy ? "检查中" : "完整检查"}</button><button type="button" className="danger-button" disabled={busy} onClick={() => void rebuild()}><DeleteOutlined /> 重建派生索引</button></div></section></>}
          {tab === "appearance" && <section><h2>显示与动效</h2><p>默认跟随Windows深浅色；你也可以固定使用白天渐变或夜晚暗黑。系统启用减少动态效果后，翻翻会关闭非必要动画。</p><div className="theme-options" role="radiogroup" aria-label="主题"><button type="button" role="radio" aria-checked={theme.preference === "system"} className={theme.preference === "system" ? "selected" : ""} onClick={() => void theme.setPreference("system")}><strong>跟随系统</strong><small>随Windows自动切换</small></button><button type="button" role="radio" aria-checked={theme.preference === "day_gradient"} className={theme.preference === "day_gradient" ? "selected" : ""} onClick={() => void theme.setPreference("day_gradient")}><strong>白天渐变</strong><small>雾蓝 · 浅紫 · 淡粉</small></button><button type="button" role="radio" aria-checked={theme.preference === "night_dark"} className={theme.preference === "night_dark" ? "selected" : ""} onClick={() => void theme.setPreference("night_dark")}><strong>夜晚暗黑</strong><small>黑底 · 白字</small></button></div><div className="readonly-note">当前显示：{theme.effective_theme === "night_dark" ? "夜晚暗黑" : "白天渐变"}</div></section>}
          {tab === "logs" && <><section><h2>体验日志与诊断包</h2><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void exportDiagnostics()}>导出诊断包</button></div><small>复现问题后请尽量不要清除日志，直接导出诊断包，并记下大致时间、操作步骤、预期结果和实际现象。</small></section><section><h2>本地诊断日志</h2><p>日志只保存在电脑中，按大小自动轮转。当前可查看 {maintenance.data?.log_events ?? 0} 条。</p><div className="log-list">{logItems.map((log) => <div key={log.log_id} className={`log-level--${log.level}`}><time>{new Date(log.created_at).toLocaleString("zh-CN")}</time><strong>{log.level.toUpperCase()} · {log.component} · {log.event_name}</strong><code>{JSON.stringify(log.fields)}</code></div>)}{logItems.length === 0 && <p>当前没有诊断日志。</p>}</div>{logs.hasNextPage && <button type="button" className="load-more-button" disabled={logs.isFetchingNextPage} onClick={() => void logs.fetchNextPage()}>{logs.isFetchingNextPage ? "正在加载" : "加载更多日志"}</button>}<button type="button" className="danger-button" disabled={busy || logItems.length === 0} onClick={() => void clearLogs()}><DeleteOutlined /> 清除日志</button></section><section><h2>重置应用数据</h2><p>重置会清理翻翻的数据库、索引、缓存、设置和日志；独立模型仓库及所有已下载模型永久保留。下次启动前会先移动到时间戳隔离目录，资料源文件始终不动。</p><button type="button" className="danger-button" disabled={busy} onClick={() => void resetApplicationData()}><DeleteOutlined /> 重置并重新启动</button></section></>}
          {tab === "node-traces" && <><section><h2>节点追踪</h2><p>记录问资料 / 找资料 / 资料关系分析 / 智能集合 AI 分析每一步节点的输入输出（明文保存，最多保留最近 2 万条，自动裁剪）。适合优化检索、生成与核验链路时复盘。</p><div className="settings-actions"><button type="button" className="danger-button" disabled={busy || traceItems.length === 0} onClick={() => void clearNodeTraces()}><DeleteOutlined /> 清空追踪记录</button></div></section><section><h2>按链路筛选</h2><div className="trace-filters">{["ask", "search", "relation", "collection"].map((flow) => <button key={flow} type="button" className={traceFlow === flow ? "selected" : ""} onClick={() => setTraceFlow((current) => current === flow ? null : flow)}>{flow === "ask" ? "问资料" : flow === "search" ? "找资料" : flow === "relation" ? "资料关系分析" : "智能集合 AI"}</button>)}<button type="button" className={traceFlow === null ? "selected" : ""} onClick={() => setTraceFlow(null)}>全部</button></div></section><section><h2>追踪明细（{traces.data?.pages[0]?.total ?? 0} 条）</h2><div className="log-list">{traceItems.map((trace) => <div key={trace.trace_id} className={`log-level--${trace.status === "error" ? "error" : "info"} trace-row`}><button type="button" className="trace-head" onClick={() => void toggleTrace(trace.trace_id)}><time>{new Date(trace.created_at).toLocaleString("zh-CN")}</time><strong>{trace.flow}.{trace.node}</strong><em>{trace.status === "error" ? "失败" : "正常"}</em>{trace.elapsed_ms != null && <small>{trace.elapsed_ms}ms</small>}<small className="trace-corr">{trace.correlation_id.slice(0, 8)}</small>{trace.entity_id && <small>#{trace.entity_id.slice(0, 8)}</small>}</button>{expandedTrace === trace.trace_id && <div className="trace-detail"><div><strong>输入</strong><button type="button" className="text-button" onClick={() => void copyTraceJson(trace.input_json, "输入")}>复制</button><pre>{JSON.stringify(trace.input_json, null, 2)}</pre></div><div><strong>输出</strong><button type="button" className="text-button" onClick={() => void copyTraceJson(trace.output_json, "输出")}>复制</button><pre>{JSON.stringify(trace.output_json, null, 2)}</pre></div></div>}</div>)}{traceItems.length === 0 && <p>还没有节点追踪记录。去问一次资料或找一次资料后回来查看。</p>}</div>{traces.hasNextPage && <button type="button" className="load-more-button" disabled={traces.isFetchingNextPage} onClick={() => void traces.fetchNextPage()}>{traces.isFetchingNextPage ? "正在加载" : "加载更多"}</button>}</section></>}
        </div>
      </div>
    </section>
  );
}
