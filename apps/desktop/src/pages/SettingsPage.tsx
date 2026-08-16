import { DeleteOutlined, FolderOpenOutlined, ReloadOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { bridge } from "../bridge";
import { confirmAction } from "../components/AppConfirm";
import { ModelManagementPanel } from "../features/model-management/ModelManagementPanel";
import { AskDebugPanel } from "../features/debug/AskDebugPanel";
import { useAppStore, type SettingsTab } from "../state/app-store";
import { useThemePreference } from "../features/theme/ThemeProvider";
import { errorMessage } from "../utils/app-error";

const bytes = (value: number) => value < 1024 * 1024 ? `${Math.round(value / 1024)} KB` : value < 1024 * 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MB` : `${(value / 1024 / 1024 / 1024).toFixed(2)} GB`;

/** initialTab：外部（如原 model_setup 入口）希望直接定位到的设置 tab。 */
export function SettingsPage({ initialTab }: { initialTab?: SettingsTab }) {
  const tab = useAppStore((state) => state.settings_tab);
  const setTab = useAppStore((state) => state.set_settings_tab);
  useEffect(() => {
    if (initialTab) setTab(initialTab);
    // 仅在挂载时定位一次，后续用户在设置页内的切换不受影响。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const theme = useThemePreference();
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const roots = useQuery({ queryKey: ["settings-roots"], queryFn: () => bridge.root_list() });
  const modelStore = useQuery({ queryKey: ["model-store-status"], queryFn: () => bridge.model_store_status_get(), enabled: tab === "index" });
  const maintenance = useQuery({ queryKey: ["maintenance"], queryFn: () => bridge.maintenance_get() });
  const storage = useQuery({ queryKey: ["storage-usage"], queryFn: () => bridge.storage_usage_get(), enabled: tab === "index" });
  const storageLocation = useQuery({ queryKey: ["storage-location"], queryFn: () => bridge.storage_location_get(), enabled: tab === "index" });
  const addRoot = async () => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不调用系统目录选择器，请在桌面程序中使用。"); return; }
    const selected = await open({ directory: true, multiple: false, title: "添加资料文件夹" });
    if (typeof selected !== "string") return;
    const fullVolume = /^[a-zA-Z]:\\?$/.test(selected);
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
    try { const count = await bridge.maintenance_logs_clear(); setMessage(`已清除 ${count} 条本地诊断日志。`); await maintenance.refetch(); }
    catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
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

  const scheduleModelStoreMigration = async () => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不迁移电脑文件，请在翻翻桌面程序中使用。"); return; }
    const selected = await open({ directory: true, multiple: false, title: "选择新的模型仓库磁盘或文件夹" });
    if (typeof selected !== "string") return;
    if (!await confirmAction({ actionKey: "model_store_migration", title: "迁移模型仓库？", description: "已下载模型会在下次启动前复制到新位置并逐文件校验；原位置完整保留，全部成功后才切换。资料源文件不受影响。", confirmLabel: "安排迁移" })) return;
    setBusy(true);
    try {
      const status = await bridge.model_store_migration_schedule(selected, "MIGRATE_MODEL_STORE");
      setMessage(`迁移计划已保存：${status.pending_target_directory ?? selected}。请关闭并重新打开翻翻；失败时会继续使用原位置。`);
      await modelStore.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const cleanupStorageMigration = async () => {
    const previous = storageLocation.data?.previous_data_directory;
    if (!previous) return;
    if (!await confirmAction({ actionKey: "storage_migration_cleanup", title: "清理迁移前的旧数据？", description: `将删除迁移前位于 ${previous} 的全部旧数据（当前数据不受影响）。清理前会校验当前数据完整，校验通过才执行；删除后旧数据无法恢复。`, confirmLabel: "确认清理", danger: true, confirmPhrase: "CLEANUP_MIGRATED_STORAGE" })) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.storage_migration_cleanup("CLEANUP_MIGRATED_STORAGE");
      setMessage(`已释放 ${bytes(result.freed_bytes)}，移除 ${result.removed_entries} 项旧数据。`);
      await storageLocation.refetch();
      await storage.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const cleanupModelStoreMigration = async () => {
    const previous = modelStore.data?.previous_model_store;
    if (!previous) return;
    if (!await confirmAction({ actionKey: "model_store_migration_cleanup", title: "清理迁移前的旧模型仓库？", description: `将删除迁移前位于 ${previous} 的全部旧模型（当前仓库不受影响）。清理前会校验当前仓库完整，校验通过才执行；删除后旧仓库无法恢复。`, confirmLabel: "确认清理", danger: true, confirmPhrase: "CLEANUP_MIGRATED_MODEL_STORE" })) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.model_store_migration_cleanup("CLEANUP_MIGRATED_MODEL_STORE");
      setMessage(`已释放 ${bytes(result.freed_bytes)}，移除 ${result.removed_entries} 项旧模型数据。`);
      await modelStore.refetch();
      await storage.refetch();
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  return (
    <section className="page">
      {error && <p role="alert" className="inline-error">{error}</p>}{message && <p className="inline-success">{message}</p>}
      <div className="settings-grid">
        <aside className="settings-side">
          <header className="page-heading settings-side__heading"><h1>设置</h1></header>
          <nav className="settings-nav">
            <button type="button" className={tab === "roots" ? "active" : ""} onClick={() => setTab("roots")}>资料目录</button><button type="button" className={tab === "models" ? "active" : ""} onClick={() => setTab("models")}>模型管理</button><button type="button" className={tab === "index" ? "active" : ""} onClick={() => setTab("index")}>存储状态</button><button type="button" className={tab === "appearance" ? "active" : ""} onClick={() => setTab("appearance")}>外观显示</button><button type="button" className={tab === "logs" ? "active" : ""} onClick={() => setTab("logs")}>日志与恢复</button>
          </nav>
        </aside>
        <div className="settings-content">
          {tab === "index" && <section><h2>自适应存储空间</h2><p>翻翻根据当前磁盘容量自动计算软配额（容量的10%，最低10GB、最高50GB），无需手动设置。达到配额后会暂停可恢复的后台增强任务，搜索和预览仍可使用。</p><div className="metric-strip"><span><strong>{storage.data ? bytes(storage.data.total_bytes) : "—"}</strong>当前占用</span><span><strong>{storage.data ? bytes(storage.data.soft_quota_bytes) : "—"}</strong>自适应配额</span><span><strong>{storage.data?.disk_available_bytes != null ? bytes(storage.data.disk_available_bytes) : "—"}</strong>磁盘可用</span></div>{storage.data?.notice && <p role="status" className="inline-error">{storage.data.notice}</p>}<small>应用管理目录：{storage.data?.data_directory ?? "正在读取"}</small></section>}
          {tab === "index" && <section><h2>迁移应用数据</h2><p>可把数据库、USearch索引和缓存迁移到其他本地磁盘。已下载模型位于独立仓库，不随数据库重置或应用数据迁移而删除。</p><div className="readonly-note">{storageLocation.data?.pending_target_directory ? <><span>迁移前：{storageLocation.data?.active_data_directory ?? storage.data?.data_directory ?? "正在读取"}</span><span>迁移后：{storageLocation.data.pending_target_directory}</span></> : <>当前位置：{storageLocation.data?.active_data_directory ?? storage.data?.data_directory ?? "正在读取"}</>}</div>{storageLocation.data?.previous_data_directory && !storageLocation.data?.pending_target_directory && <p className="cleanup-hint">迁移前的旧数据仍保留于 {storageLocation.data.previous_data_directory}，确认新位置正常后可清理以释放磁盘空间；清理后即可安排新的迁移。</p>}{storageLocation.data?.last_error && <p role="alert" className="inline-error">{storageLocation.data.last_error}</p>}<div className="settings-actions"><button type="button" disabled={busy || (storageLocation.data?.restart_required && !storageLocation.data?.last_error) || !!storageLocation.data?.previous_data_directory} onClick={() => void scheduleStorageMigration()}><FolderOpenOutlined /> {storageLocation.data?.restart_required && !storageLocation.data?.last_error ? "等待重启迁移" : storageLocation.data?.restart_required ? "重新选择迁移位置" : "选择新位置"}</button>{storageLocation.data?.previous_data_directory && !storageLocation.data?.pending_target_directory && <button type="button" className="danger-button" disabled={busy} onClick={() => void cleanupStorageMigration()}><DeleteOutlined /> 确认清理旧数据</button>}</div></section>}
          {tab === "index" && <section><h2>迁移模型</h2><p>可把已下载模型仓库迁移到其他本地磁盘，与应用数据分开存储。迁移是复制并逐文件校验，成功后原位置完整保留；重启后生效。</p><div className="readonly-note">{modelStore.data?.pending_target_directory ? <><span>迁移前：{modelStore.data?.store_path ?? "正在读取"}</span><span>迁移后：{modelStore.data.pending_target_directory}</span></> : <>当前位置：{modelStore.data?.store_path ?? "正在读取"}{modelStore.data ? ` · ${modelStore.data.installed_artifacts} 个组件 / ${bytes(modelStore.data.installed_bytes)}` : ""}</>}</div>{modelStore.data?.previous_model_store && !modelStore.data?.pending_target_directory && <p className="cleanup-hint">迁移前的旧模型仓库仍保留于 {modelStore.data.previous_model_store}，确认新位置正常后可清理以释放磁盘空间；清理后即可安排新的迁移。</p>}{modelStore.data?.last_error && <p role="alert" className="inline-error">{modelStore.data.last_error}</p>}<div className="settings-actions"><button type="button" disabled={busy || (modelStore.data?.restart_required && !modelStore.data?.last_error) || !!modelStore.data?.previous_model_store} onClick={() => void scheduleModelStoreMigration()}><FolderOpenOutlined /> {modelStore.data?.restart_required && !modelStore.data?.last_error ? "等待重启迁移" : modelStore.data?.restart_required ? "重新选择迁移位置" : "选择新位置"}</button>{modelStore.data?.previous_model_store && !modelStore.data?.pending_target_directory && <button type="button" className="danger-button" disabled={busy} onClick={() => void cleanupModelStoreMigration()}><DeleteOutlined /> 确认清理旧模型仓库</button>}</div></section>}
          {tab === "roots" && <>
            <section><h2>资料目录</h2><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void addRoot()}><FolderOpenOutlined /> 添加文件夹</button></div></section>
            <section><h2>已授权位置</h2><div className="settings-list">{roots.data?.map((root) => <div key={root.root_id}><span><strong>{root.label}</strong><small>{root.path}</small></span><em>{root.enabled ? `${root.file_count} 个文件` : "已停用"}</em><button type="button" className="text-button" disabled={busy} onClick={() => void disableRoot(root.root_id, root.label)}>从翻翻移除</button></div>)}{!roots.isLoading && roots.data?.length === 0 && <p>当前没有资料目录。添加前翻翻不会读取本地文件。</p>}</div></section>
          </>}
          {tab === "models" && <ModelManagementPanel />}
          {tab === "index" && <><section><h2>索引状态</h2><div className="metric-strip"><span><strong>{maintenance.data?.indexed_files ?? "—"}</strong>已索引文件</span><span><strong>{maintenance.data?.searchable_chunks ?? "—"}</strong>全文块</span><span><strong>{maintenance.data?.embedded_chunks ?? "—"}</strong>向量块</span><span><strong>{maintenance.data ? bytes(maintenance.data.database_size_bytes) : "—"}</strong>数据库</span></div></section><section><h2>存储分类</h2><p>只允许清理明确标记为缓存的内容；模型断点、索引与源文件不会作为缓存删除。</p><div className="settings-list">{storage.data?.categories.map((category) => <div key={category.key}><span><strong>{category.label}</strong><small>{category.detail}</small></span><em>{bytes(category.size_bytes)}</em>{category.clearable && <button type="button" className="text-button" disabled={busy || category.size_bytes === 0} onClick={() => void clearCache(category.key as "temporary_cache" | "failed_downloads", category.label)}>清理</button>}</div>)}</div></section><section><h2>自动检查站</h2><div className="health-list">{maintenance.data?.checks.map((check) => <div key={check.key} className={`health-${check.status}`}><i /><span><strong>{check.label}</strong><small>{check.detail}</small></span></div>)}</div><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void checkDatabase("quick")}><ReloadOutlined /> {busy ? "检查中" : "快速检查"}</button><button type="button" disabled={busy} onClick={() => void checkDatabase("full")}><ReloadOutlined /> {busy ? "检查中" : "完整检查"}</button><button type="button" className="danger-button" disabled={busy} onClick={() => void rebuild()}><DeleteOutlined /> 重建派生索引</button></div></section></>}
          {tab === "appearance" && <section><h2>显示与动效</h2><p>默认跟随Windows深浅色；你也可以固定使用白天渐变或夜晚暗黑。</p><div className="theme-options" role="radiogroup" aria-label="主题"><button type="button" role="radio" aria-checked={theme.preference === "system"} className={theme.preference === "system" ? "selected" : ""} onClick={() => void theme.setPreference("system")}><strong>跟随系统</strong><small>随Windows自动切换</small></button><button type="button" role="radio" aria-checked={theme.preference === "day_gradient"} className={theme.preference === "day_gradient" ? "selected" : ""} onClick={() => void theme.setPreference("day_gradient")}><strong>白天渐变</strong><small>雾蓝 · 浅紫 · 淡粉</small></button><button type="button" role="radio" aria-checked={theme.preference === "night_dark"} className={theme.preference === "night_dark" ? "selected" : ""} onClick={() => void theme.setPreference("night_dark")}><strong>夜晚暗黑</strong><small>黑底 · 白字</small></button></div><div className="readonly-note">当前显示：{theme.effective_theme === "night_dark" ? "夜晚暗黑" : "白天渐变"}</div></section>}
          {tab === "logs" && <><section><h2>日志与诊断</h2><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void exportDiagnostics()}>导出诊断包</button></div><small>出现问题后，请记录下大致时间、操作步骤、预期结果和实际现象。</small></section><section><h2>本地诊断日志</h2><p>日志只保存在电脑中，按大小自动轮转。当前保留 {maintenance.data?.log_events ?? 0} 条。</p><button type="button" className="danger-button" disabled={busy || (maintenance.data?.log_events ?? 0) === 0} onClick={() => void clearLogs()}><DeleteOutlined /> 清除日志</button></section><section><h2>Developer / 问答调试</h2><p>仅用于诊断与真实用户测试：Trace Viewer 查看单次问答的 12+ 阶段追踪并导出脱敏 JSON；Evaluation Runner 用 JSONL 测试集批量跑问答并统计指标（不写 Memory、不改任何参数）。</p><AskDebugPanel /></section><section><h2>重置应用数据</h2><p>重置会清理翻翻的数据库、索引、缓存、设置和日志；独立模型仓库及所有已下载模型永久保留。</p><button type="button" className="danger-button" disabled={busy} onClick={() => void resetApplicationData()}><DeleteOutlined /> 重置并重新启动</button></section></>}
        </div>
      </div>
    </section>
  );
}
