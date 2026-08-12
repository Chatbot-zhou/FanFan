import { DeleteOutlined, FolderOpenOutlined, ReloadOutlined, SafetyCertificateOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type EnvironmentCheck } from "../bridge";
import { confirmAction } from "../components/AppConfirm";
import { useAppStore } from "../state/app-store";
import { useThemePreference } from "../features/theme/ThemeProvider";
import { errorMessage } from "../utils/app-error";

const bytes = (value: number) => value < 1024 * 1024 ? `${Math.round(value / 1024)} KB` : value < 1024 * 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MB` : `${(value / 1024 / 1024 / 1024).toFixed(2)} GB`;

export function SettingsPage({ environment }: { environment: EnvironmentCheck | null }) {
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

  const addRoot = async (volumeOnly: boolean) => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不调用系统目录选择器，请在桌面程序中使用。"); return; }
    const selected = await open({ directory: true, multiple: false, title: volumeOnly ? "选择本地磁盘根目录" : "添加资料文件夹" });
    if (typeof selected !== "string") return;
    const fullVolume = /^[a-zA-Z]:\\?$/.test(selected);
    if (volumeOnly && !fullVolume) { setError("添加整个磁盘时请选择盘符根目录，例如 D:\\。"); return; }
    if (fullVolume && !await confirmAction({ actionKey: "settings_add_full_volume", title: "添加整个磁盘？", description: "扫描可能耗时较长，系统、程序、凭据、应用数据和拾忆自身目录仍会被强制排除。", confirmLabel: "确认添加" })) return;
    setBusy(true);
    try { await bridge.root_add({ path: selected, label: null, watch_mode: "realtime", authorization_source: "user_selected", full_volume_confirmed: fullVolume }); await roots.refetch(); setMessage("资料位置已添加，扫描会在后台继续。"); }
    catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const rebuild = async () => {
    if (!await confirmAction({ actionKey: "index_rebuild", title: "重建派生索引？", description: "拾忆会在后台建立新索引并保留当前可用索引，校验成功后才切换。不会修改任何源文件。", confirmLabel: "开始重建", danger: true })) return;
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

  const exportDiagnostics = async () => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不写入电脑文件，请在桌面程序中使用。"); return; }
    const target = await save({ title: "导出本地诊断包", defaultPath: `拾忆-诊断-${new Date().toISOString().slice(0, 10)}.json`, filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!target) return;
    if (!await confirmAction({ actionKey: "diagnostic_export", title: "导出本地诊断包？", description: "包含脱敏后的启动、环境、模型任务状态和运行日志，不包含文档正文或提问内容；目标已存在时会拒绝覆盖。", confirmLabel: "导出" })) return;
    setBusy(true);
    try {
      const result = await bridge.diagnostic_export(target, true);
      setMessage(`诊断包已新建：${result.size_bytes ? bytes(result.size_bytes) : "完成"}。拾忆没有修改源文件。`);
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
    if (!await confirmAction({ actionKey: `cache_clear_${category}`, title: `清理“${label}”？`, description: "只删除拾忆可重建或已判定无效的缓存，不会修改源文件、索引数据库、已安装模型和可续传下载。", confirmLabel: "清理缓存", danger: true })) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.cache_clear(category, "CLEAR_CACHE");
      setMessage(`已清理“${label}”：释放 ${bytes(result.freed_bytes)}，移除 ${result.removed_entries} 个缓存项；源文件修改：否。`);
      await storage.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const resetApplicationData = async () => {
    if (!await confirmAction({ actionKey: "application_data_reset", title: "重置拾忆应用数据？", description: "这会关闭拾忆，并在下次启动前隔离数据库、索引、模型、设置、日志和缓存；源文件不会被修改。", confirmLabel: "重置并重新启动", danger: true, confirmPhrase: "RESET_APPLICATION_DATA" })) return;
    setBusy(true); setError(null); setMessage("正在安排安全重置，拾忆将重新启动……");
    try { await bridge.app_data_reset_schedule("RESET_APPLICATION_DATA"); }
    catch (actionError) { setError(errorMessage(actionError)); setBusy(false); }
  };

  const disableRoot = async (rootId: string, label: string) => {
    if (!await confirmAction({ actionKey: "settings_remove_root", title: `从拾忆移除“${label}”？`, description: "拾忆会立即停止读取并撤销授权，派生数据在后台清理；不会删除、移动或修改任何源文件。", confirmLabel: "从拾忆移除", danger: true })) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      await bridge.root_disable(rootId);
      setMessage(`已从拾忆移除“${label}”。原文件没有变化；以后仍可重新添加。`);
      await roots.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const scheduleStorageMigration = async () => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不迁移电脑文件，请在拾忆桌面程序中使用。"); return; }
    const selected = await open({ directory: true, multiple: false, title: "选择新的拾忆应用数据磁盘或文件夹" });
    if (typeof selected !== "string") return;
    if (!await confirmAction({ actionKey: "storage_migration", title: "迁移拾忆应用数据？", description: "数据库、索引、缓存和本地模型会在下次启动前迁移并逐文件校验；全部成功后才切换，原位置会保留。资料源文件不受影响。", confirmLabel: "安排迁移" })) return;
    setBusy(true);
    try {
      const status = await bridge.storage_migration_schedule(selected, "MIGRATE_APPLICATION_STORAGE");
      setMessage(`迁移计划已保存：${status.pending_target_directory ?? selected}。请关闭并重新打开拾忆；失败时会继续使用原位置。`);
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
          <button type="button" className={tab === "roots" ? "active" : ""} onClick={() => setTab("roots")}>资料目录</button><button type="button" className={tab === "models" ? "active" : ""} onClick={() => setTab("models")}>本地模型</button><button type="button" className={tab === "index" ? "active" : ""} onClick={() => setTab("index")}>索引与存储</button><button type="button" className={tab === "appearance" ? "active" : ""} onClick={() => setTab("appearance")}>外观与辅助功能</button><button type="button" className={tab === "logs" ? "active" : ""} onClick={() => setTab("logs")}>日志与恢复</button>
        </aside>
        <div className="settings-content">
          {tab === "index" && <section><h2>自适应存储空间</h2><p>拾忆根据当前磁盘容量自动计算软配额（容量的10%，最低10GB、最高50GB），无需手动设置。达到配额后会暂停可恢复的后台增强任务，搜索和预览仍可使用。</p><div className="metric-strip"><span><strong>{storage.data ? bytes(storage.data.total_bytes) : "—"}</strong>当前占用</span><span><strong>{storage.data ? bytes(storage.data.soft_quota_bytes) : "—"}</strong>自适应配额</span><span><strong>{storage.data?.disk_available_bytes != null ? bytes(storage.data.disk_available_bytes) : "—"}</strong>磁盘可用</span></div>{storage.data?.notice && <p role="status" className="inline-error">{storage.data.notice}</p>}<small>应用管理目录：{storage.data?.data_directory ?? "正在读取"}</small></section>}
          {tab === "index" && <section><h2>迁移应用数据</h2><p>可把数据库、USearch索引、缓存和本地模型迁移到其他本地磁盘。迁移在下次启动时执行并逐文件校验，成功前始终继续使用原位置。</p><div className="readonly-note">当前位置：{storageLocation.data?.active_data_directory ?? storage.data?.data_directory ?? "正在读取"}{storageLocation.data?.pending_target_directory ? ` · 等待迁移到 ${storageLocation.data.pending_target_directory}` : ""}</div>{storageLocation.data?.last_error && <p role="alert" className="inline-error">{storageLocation.data.last_error}</p>}<div className="settings-actions"><button type="button" disabled={busy || storageLocation.data?.restart_required} onClick={() => void scheduleStorageMigration()}><FolderOpenOutlined /> {storageLocation.data?.restart_required ? "等待重启迁移" : "选择新位置"}</button></div></section>}
          {tab === "roots" && <>
            <section><h2>资料目录</h2><p>拾忆只扫描你明确授权的位置，不会自动添加桌面、文档、下载或图片。</p><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void addRoot(false)}><FolderOpenOutlined /> 添加文件夹</button><button type="button" disabled={busy} onClick={() => void addRoot(true)}><FolderOpenOutlined /> 添加整个磁盘</button></div></section>
            <section><h2>已授权位置</h2><div className="settings-list">{roots.data?.map((root) => <div key={root.root_id}><span><strong>{root.label}</strong><small>{root.path}</small></span><em>{root.enabled ? `${root.file_count} 个文件` : "已停用"}</em><button type="button" className="text-button" disabled={busy} onClick={() => void disableRoot(root.root_id, root.label)}>从拾忆移除</button></div>)}{!roots.isLoading && roots.data?.length === 0 && <p>当前没有资料目录。添加前拾忆不会读取本地文件。</p>}</div></section>
            <section><h2>扫描保护</h2><p>系统目录、程序目录、凭据、应用数据、重解析点和拾忆自身数据会自动排除，无需手动配置。</p></section>
            <section><h2>源文件保护</h2><div className="readonly-note"><SafetyCertificateOutlined /> 源文件始终只读。整理功能只创建虚拟集合和操作建议。</div></section>
          </>}
          {tab === "models" && <><section><h2>当前能力</h2><div className="setting-row"><div><strong>{models.data?.state.message ?? "正在读取模型状态"}</strong><small>运行后端：{models.data?.state.runtime_backend ?? "尚未启用"} · 内存 {environment?.memory_total_gb ?? "—"} GB</small></div><button type="button" onClick={() => navigate("model_setup")}>管理模型</button></div></section><section><h2>已导入组件</h2><div className="settings-list">{models.data?.artifacts.map((artifact) => <div key={artifact.artifact_id}><span><strong>{artifact.model_id}</strong><small>{artifact.role} · {artifact.format.toUpperCase()} · {bytes(artifact.size_bytes)}</small></span><em>{artifact.embedding_dimension ? `已自检 · ${artifact.embedding_dimension}维` : artifact.status}</em></div>)}{models.data?.artifacts.length === 0 && <p>还没有导入本地模型。基础搜索不受影响。</p>}</div></section></>}
          {tab === "index" && <><section><h2>索引状态</h2><div className="metric-strip"><span><strong>{maintenance.data?.indexed_files ?? "—"}</strong>已索引文件</span><span><strong>{maintenance.data?.searchable_chunks ?? "—"}</strong>全文块</span><span><strong>{maintenance.data?.embedded_chunks ?? "—"}</strong>向量块</span><span><strong>{maintenance.data ? bytes(maintenance.data.database_size_bytes) : "—"}</strong>数据库</span></div></section><section><h2>存储分类</h2><p>只允许清理明确标记为缓存的内容；模型断点、索引与源文件不会作为缓存删除。</p><div className="settings-list">{storage.data?.categories.map((category) => <div key={category.key}><span><strong>{category.label}</strong><small>{category.detail}</small></span><em>{bytes(category.size_bytes)}</em>{category.clearable && <button type="button" className="text-button" disabled={busy || category.size_bytes === 0} onClick={() => void clearCache(category.key as "temporary_cache" | "failed_downloads", category.label)}>清理</button>}</div>)}</div></section><section><h2>自动检查站</h2><div className="health-list">{maintenance.data?.checks.map((check) => <div key={check.key} className={`health-${check.status}`}><i /><span><strong>{check.label}</strong><small>{check.detail}</small></span></div>)}</div><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void checkDatabase("quick")}><ReloadOutlined /> {busy ? "检查中" : "快速检查"}</button><button type="button" disabled={busy} onClick={() => void checkDatabase("full")}><ReloadOutlined /> {busy ? "检查中" : "完整检查"}</button><button type="button" className="danger-button" disabled={busy} onClick={() => void rebuild()}><DeleteOutlined /> 重建派生索引</button></div></section></>}
          {tab === "appearance" && <section><h2>显示与动效</h2><p>默认跟随Windows深浅色；你也可以固定使用白天渐变或夜晚暗黑。系统启用减少动态效果后，拾忆会关闭非必要动画。</p><div className="theme-options" role="radiogroup" aria-label="主题"><button type="button" role="radio" aria-checked={theme.preference === "system"} className={theme.preference === "system" ? "selected" : ""} onClick={() => void theme.setPreference("system")}><strong>跟随系统</strong><small>随Windows自动切换</small></button><button type="button" role="radio" aria-checked={theme.preference === "day_gradient"} className={theme.preference === "day_gradient" ? "selected" : ""} onClick={() => void theme.setPreference("day_gradient")}><strong>白天渐变</strong><small>雾蓝 · 浅紫 · 淡粉</small></button><button type="button" role="radio" aria-checked={theme.preference === "night_dark"} className={theme.preference === "night_dark" ? "selected" : ""} onClick={() => void theme.setPreference("night_dark")}><strong>夜晚暗黑</strong><small>黑底 · 白字</small></button></div><div className="readonly-note">当前显示：{theme.effective_theme === "night_dark" ? "夜晚暗黑" : "白天渐变"}</div></section>}
          {tab === "logs" && <><section><h2>体验日志与诊断包</h2><p>{maintenance.data?.background_notice ?? "后台任务会自动让出资源，优先保证搜索和预览。"}</p><p>体验期间会记录启动、页面切换、功能调用、扫描解析、图片理解、Embedding、模型下载、RAG、智能集合、异常和耗时节点。只记录任务标识、阶段、数量、耗时与错误码，不记录文档正文、提问内容或敏感绝对路径。</p><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void exportDiagnostics()}>导出诊断包</button></div><small>复现问题后请尽量不要清除日志，直接导出诊断包，并记下大致时间、操作步骤、预期结果和实际现象。</small></section><section><h2>本地诊断日志</h2><p>日志只保存在电脑中，按大小自动轮转。当前可查看 {maintenance.data?.log_events ?? 0} 条。</p><div className="log-list">{logItems.map((log) => <div key={log.log_id} className={`log-level--${log.level}`}><time>{new Date(log.created_at).toLocaleString("zh-CN")}</time><strong>{log.level.toUpperCase()} · {log.component} · {log.event_name}</strong><code>{JSON.stringify(log.fields)}</code></div>)}{logItems.length === 0 && <p>当前没有诊断日志。</p>}</div>{logs.hasNextPage && <button type="button" className="load-more-button" disabled={logs.isFetchingNextPage} onClick={() => void logs.fetchNextPage()}>{logs.isFetchingNextPage ? "正在加载" : "加载更多日志"}</button>}<button type="button" className="danger-button" disabled={busy || logItems.length === 0} onClick={() => void clearLogs()}><DeleteOutlined /> 清除日志</button></section><section><h2>重置应用数据</h2><p>重置只处理拾忆自己的数据库、模型、缓存和配置。下次启动前会先移动到时间戳隔离目录，便于出现异常时恢复；资料源文件始终不动。</p><button type="button" className="danger-button" disabled={busy} onClick={() => void resetApplicationData()}><DeleteOutlined /> 重置并重新启动</button></section></>}
        </div>
      </div>
    </section>
  );
}
