import { DeleteOutlined, FolderOpenOutlined, ReloadOutlined, SafetyCertificateOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type EnvironmentCheck, type ExclusionRuleInput } from "../bridge";
import { useAppStore } from "../state/app-store";
import { useThemePreference } from "../features/theme/ThemeProvider";

type SettingsTab = "roots" | "models" | "index" | "appearance" | "logs";
const bytes = (value: number) => value < 1024 * 1024 ? `${Math.round(value / 1024)} KB` : value < 1024 * 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MB` : `${(value / 1024 / 1024 / 1024).toFixed(2)} GB`;

export function SettingsPage({ environment }: { environment: EnvironmentCheck | null }) {
  const navigate = useAppStore((state) => state.navigate);
  const theme = useThemePreference();
  const queryClient = useQueryClient();
  const [tab, setTab] = useState<SettingsTab>("roots");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ruleRootId, setRuleRootId] = useState("");
  const [ruleType, setRuleType] = useState<ExclusionRuleInput["rule_type"]>("path_name");
  const [ruleValue, setRuleValue] = useState("");
  const [quotaGb, setQuotaGb] = useState("");
  const roots = useQuery({ queryKey: ["settings-roots"], queryFn: () => bridge.root_list() });
  const models = useQuery({ queryKey: ["settings-models"], queryFn: async () => ({ state: await bridge.model_state_get(), artifacts: await bridge.model_artifact_list() }) });
  const maintenance = useQuery({ queryKey: ["maintenance"], queryFn: () => bridge.maintenance_get() });
  const storage = useQuery({ queryKey: ["storage-usage"], queryFn: () => bridge.storage_usage_get(), enabled: tab === "index" });
  const exclusionRules = useQuery({ queryKey: ["exclusion-rules"], queryFn: () => bridge.exclusion_rule_list(), enabled: tab === "roots" });
  const upsertRule = useMutation({ mutationFn: (request: ExclusionRuleInput) => bridge.exclusion_rule_upsert(request), onSuccess: async () => { setRuleValue(""); await queryClient.invalidateQueries({ queryKey: ["exclusion-rules"] }); } });
  const deleteRule = useMutation({ mutationFn: (ruleId: string) => bridge.exclusion_rule_delete(ruleId), onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["exclusion-rules"] }) });
  const logs = useInfiniteQuery({
    queryKey: ["maintenance-logs"],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.maintenance_log_query({ cursor: pageParam, page_size: 100 }),
    getNextPageParam: (page) => page.next_cursor,
    enabled: tab === "logs",
  });
  const logItems = logs.data?.pages.flatMap((page) => page.items) ?? [];

  const addRoot = async (volumeOnly: boolean) => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不调用系统目录选择器，请在桌面程序中使用。"); return; }
    const selected = await open({ directory: true, multiple: false, title: volumeOnly ? "选择本地磁盘根目录" : "添加资料文件夹" });
    if (typeof selected !== "string") return;
    const fullVolume = /^[a-zA-Z]:\\?$/.test(selected);
    if (volumeOnly && !fullVolume) { setError("添加整个磁盘时请选择盘符根目录，例如 D:\\。"); return; }
    if (fullVolume && !window.confirm("扫描整个磁盘可能耗时较长，系统与凭据目录仍会被强制排除。确认添加吗？")) return;
    setBusy(true);
    try { await bridge.root_add({ path: selected, label: null, watch_mode: "realtime", authorization_source: "user_selected", full_volume_confirmed: fullVolume }); await roots.refetch(); setMessage("资料位置已添加，扫描会在后台继续。"); }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
  };

  const rebuild = async () => {
    if (!window.confirm("重建会删除拾忆生成的正文与向量索引并重新解析，不会修改任何源文件。确认继续吗？")) return;
    setBusy(true); setError(null); setMessage(null);
    try { const result = await bridge.index_rebuild("REBUILD_INDEX"); setMessage(`已重置 ${result.reset_files} 份资料、移除 ${result.removed_chunks} 个旧文本块；后台正在重建。源文件修改：否。`); await maintenance.refetch(); }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
  };

  const clearLogs = async () => {
    if (!window.confirm("清除本地诊断日志？此操作不会影响资料和索引。")) return;
    setBusy(true); setError(null);
    try { const count = await bridge.maintenance_logs_clear(); setMessage(`已清除 ${count} 条本地诊断日志。`); await Promise.all([logs.refetch(), maintenance.refetch()]); }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
  };

  const exportDiagnostics = async () => {
    setError(null); setMessage(null);
    if (!isTauri()) { setError("浏览器预览不写入电脑文件，请在桌面程序中使用。"); return; }
    const target = await save({ title: "导出本地诊断包", defaultPath: `拾忆-诊断-${new Date().toISOString().slice(0, 10)}.json`, filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!target) return;
    if (!window.confirm("导出诊断包？包内包含运行状态和最近 200 条已脱敏日志，不包含文档正文。目标已存在时拾忆会拒绝覆盖。")) return;
    setBusy(true);
    try {
      const result = await bridge.diagnostic_export(target, true);
      setMessage(`诊断包已新建：${result.size_bytes ? bytes(result.size_bytes) : "完成"}。拾忆没有修改源文件。`);
    } catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
  };

  const checkDatabase = async (level: "quick" | "full") => {
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.maintenance_check(level);
      if (result.database_result !== "ok") throw new Error(`数据库检查未通过：${result.database_result}`);
      setMessage(`${level === "full" ? "完整" : "快速"}数据库检查通过，用时 ${result.elapsed_ms} 毫秒；源文件修改：否。`);
      await maintenance.refetch();
    } catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
  };

  const clearCache = async (category: "temporary_cache" | "failed_downloads", label: string) => {
    if (!window.confirm(`清理“${label}”？\n\n只删除拾忆可重建或已判定无效的缓存，不会修改源文件、索引数据库、已安装模型和可续传下载。`)) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.cache_clear(category, "CLEAR_CACHE");
      setMessage(`已清理“${label}”：释放 ${bytes(result.freed_bytes)}，移除 ${result.removed_entries} 个缓存项；源文件修改：否。`);
      await storage.refetch();
    } catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
  };

  const resetApplicationData = async () => {
    const confirmation = window.prompt("这会关闭拾忆，并在下次启动前把本应用的 Roaming/Local 数据移动到带时间戳的隔离目录。源文件不会被修改。\n\n请输入 RESET_APPLICATION_DATA 继续：");
    if (confirmation !== "RESET_APPLICATION_DATA") {
      if (confirmation !== null) setError("确认短语不匹配，未执行重置。");
      return;
    }
    setBusy(true); setError(null); setMessage("正在安排安全重置，拾忆将重新启动……");
    try { await bridge.app_data_reset_schedule("RESET_APPLICATION_DATA"); }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); setBusy(false); }
  };

  const disableRoot = async (rootId: string, label: string) => {
    if (!window.confirm(`从拾忆移除“${label}”？\n\n这会停止扫描并取消该位置的读取授权，但不会删除、移动或修改任何源文件。`)) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      await bridge.root_disable(rootId);
      setMessage(`已从拾忆移除“${label}”。原文件没有变化；以后仍可重新添加。`);
      await roots.refetch();
    } catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
  };

  const ruleValueText = (value: unknown) => typeof value === "string" ? value : JSON.stringify(value);

  const saveRule = () => {
    if (!ruleRootId || !ruleValue.trim()) return;
    upsertRule.mutate({ rule_id: null, root_id: ruleRootId, rule_type: ruleType, value: ruleValue.trim(), enabled: true });
  };

  const saveStorageQuota = async () => {
    const parsed = Number(quotaGb);
    if (!Number.isFinite(parsed) || parsed < 1 || parsed > 2048) {
      setError("存储软配额需要在1GB到2048GB之间。");
      return;
    }
    if (!window.confirm(`将拾忆的存储软配额设置为 ${parsed} GB？达到配额后会暂停图片理解、OCR和语义索引，但搜索与预览继续可用。`)) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      const snapshot = await bridge.storage_policy_set(Math.round(parsed * 1024 ** 3), "SET_STORAGE_QUOTA");
      setMessage(`存储软配额已设为 ${bytes(snapshot.soft_quota_bytes)}。`);
      setQuotaGb("");
      await storage.refetch();
    } catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
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
          {tab === "index" && <section><h2>存储软配额</h2><p>默认使用目标磁盘容量的10%，下限10GB、上限50GB。达到软配额时只暂停可恢复的后台增强任务。</p><div className="metric-strip"><span><strong>{storage.data ? bytes(storage.data.total_bytes) : "—"}</strong>当前占用</span><span><strong>{storage.data ? bytes(storage.data.soft_quota_bytes) : "—"}</strong>软配额</span><span><strong>{storage.data?.disk_available_bytes != null ? bytes(storage.data.disk_available_bytes) : "—"}</strong>磁盘可用</span></div>{storage.data?.notice && <p role="status" className="inline-error">{storage.data.notice}</p>}<small>应用管理目录：{storage.data?.data_directory ?? "正在读取"}</small><div className="settings-actions"><input aria-label="存储软配额GB" type="number" min={1} max={2048} step={1} value={quotaGb} onChange={(event) => setQuotaGb(event.target.value)} placeholder={storage.data ? String(Math.round(storage.data.soft_quota_bytes / 1024 ** 3)) : "GB"} /><button type="button" disabled={busy || !quotaGb} onClick={() => void saveStorageQuota()}>保存配额</button></div></section>}
          {tab === "roots" && <><section><h2>资料目录</h2><p>拾忆只扫描你明确授权的位置，不会自动添加桌面、文档、下载或图片。</p><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void addRoot(false)}><FolderOpenOutlined /> 添加文件夹</button><button type="button" disabled={busy} onClick={() => void addRoot(true)}><FolderOpenOutlined /> 添加整个磁盘</button></div></section><section><h2>已授权位置</h2><div className="settings-list">{roots.data?.map((root) => <div key={root.root_id}><span><strong>{root.label}</strong><small>{root.path}</small></span><em>{root.enabled ? `${root.file_count} 个文件` : "已停用"}</em><button type="button" className="text-button" disabled={busy} onClick={() => void disableRoot(root.root_id, root.label)}>从拾忆移除</button></div>)}{!roots.isLoading && roots.data?.length === 0 && <p>当前没有资料目录。添加前拾忆不会读取本地文件。</p>}</div></section><section><h2>排除规则</h2><p>硬保护规则不可关闭；可覆盖规则会在下一扫描批次立即生效。</p><div className="exclusion-rule-editor"><select aria-label="规则资料位置" value={ruleRootId} onChange={(event) => setRuleRootId(event.target.value)}><option value="">选择资料位置</option>{roots.data?.filter((root) => root.enabled).map((root) => <option key={root.root_id} value={root.root_id}>{root.label}</option>)}</select><select aria-label="排除规则类型" value={ruleType} onChange={(event) => setRuleType(event.target.value as ExclusionRuleInput["rule_type"])}><option value="path_name">目录或文件名</option><option value="path_glob">路径通配规则</option><option value="extension">扩展名</option></select><input aria-label="排除规则内容" value={ruleValue} maxLength={260} onChange={(event) => setRuleValue(event.target.value)} placeholder={ruleType === "extension" ? "例如 tmp" : ruleType === "path_glob" ? "例如 **/cache/**" : "例如 node_modules"} /><button type="button" disabled={!ruleRootId || !ruleValue.trim() || upsertRule.isPending} onClick={saveRule}>添加规则</button></div>{(upsertRule.isError || deleteRule.isError) && <p role="alert" className="inline-error">{(upsertRule.error ?? deleteRule.error) instanceof Error ? (upsertRule.error ?? deleteRule.error as Error).message : String(upsertRule.error ?? deleteRule.error)}</p>}<div className="settings-list">{exclusionRules.data?.map((rule) => <div key={rule.rule_id}><span><strong>{rule.rule_class === "hard" ? "硬保护" : rule.rule_type}</strong><small>{ruleValueText(rule.value)}</small></span><label><input type="checkbox" checked={rule.enabled} disabled={!rule.overridable || upsertRule.isPending} onChange={(event) => { if (typeof rule.value !== "string" || !["path_name", "path_glob", "extension"].includes(rule.rule_type)) return; upsertRule.mutate({ rule_id: rule.rule_id, root_id: rule.root_id, rule_type: rule.rule_type as ExclusionRuleInput["rule_type"], value: rule.value, enabled: event.target.checked }); }} /> {rule.enabled ? "启用" : "停用"}</label>{rule.root_id && <button type="button" className="text-button" disabled={deleteRule.isPending} onClick={() => { if (window.confirm("删除这条自定义排除规则？下一次扫描将不再应用它。")) deleteRule.mutate(rule.rule_id); }}>删除</button>}</div>)}</div></section><section><h2>源文件保护</h2><div className="readonly-note"><SafetyCertificateOutlined /> 源文件始终只读。整理功能只创建虚拟集合和操作建议。</div></section></>}
          {tab === "models" && <><section><h2>当前能力</h2><div className="setting-row"><div><strong>{models.data?.state.message ?? "正在读取模型状态"}</strong><small>运行后端：{models.data?.state.runtime_backend ?? "尚未启用"} · 内存 {environment?.memory_total_gb ?? "—"} GB</small></div><button type="button" onClick={() => navigate("model_setup")}>管理模型</button></div></section><section><h2>已导入组件</h2><div className="settings-list">{models.data?.artifacts.map((artifact) => <div key={artifact.artifact_id}><span><strong>{artifact.model_id}</strong><small>{artifact.role} · {artifact.format.toUpperCase()} · {bytes(artifact.size_bytes)}</small></span><em>{artifact.embedding_dimension ? `已自检 · ${artifact.embedding_dimension}维` : artifact.status}</em></div>)}{models.data?.artifacts.length === 0 && <p>还没有导入本地模型。基础搜索不受影响。</p>}</div></section></>}
          {tab === "index" && <><section><h2>索引状态</h2><div className="metric-strip"><span><strong>{maintenance.data?.indexed_files ?? "—"}</strong>已索引文件</span><span><strong>{maintenance.data?.searchable_chunks ?? "—"}</strong>全文块</span><span><strong>{maintenance.data?.embedded_chunks ?? "—"}</strong>向量块</span><span><strong>{maintenance.data ? bytes(maintenance.data.database_size_bytes) : "—"}</strong>数据库</span></div></section><section><h2>存储分类</h2><p>只允许清理明确标记为缓存的内容；模型断点、索引与源文件不会作为缓存删除。</p><div className="settings-list">{storage.data?.categories.map((category) => <div key={category.key}><span><strong>{category.label}</strong><small>{category.detail}</small></span><em>{bytes(category.size_bytes)}</em>{category.clearable && <button type="button" className="text-button" disabled={busy || category.size_bytes === 0} onClick={() => void clearCache(category.key as "temporary_cache" | "failed_downloads", category.label)}>清理</button>}</div>)}</div></section><section><h2>自动检查站</h2><div className="health-list">{maintenance.data?.checks.map((check) => <div key={check.key} className={`health-${check.status}`}><i /><span><strong>{check.label}</strong><small>{check.detail}</small></span></div>)}</div><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void checkDatabase("quick")}><ReloadOutlined /> {busy ? "检查中" : "快速检查"}</button><button type="button" disabled={busy} onClick={() => void checkDatabase("full")}><ReloadOutlined /> {busy ? "检查中" : "完整检查"}</button><button type="button" className="danger-button" disabled={busy} onClick={() => void rebuild()}><DeleteOutlined /> 重建派生索引</button></div></section></>}
          {tab === "appearance" && <section><h2>显示与动效</h2><p>默认跟随Windows深浅色；你也可以固定使用白天渐变或夜晚暗黑。系统启用减少动态效果后，拾忆会关闭非必要动画。</p><div className="theme-options" role="radiogroup" aria-label="主题"><button type="button" role="radio" aria-checked={theme.preference === "system"} className={theme.preference === "system" ? "selected" : ""} onClick={() => void theme.setPreference("system")}><strong>跟随系统</strong><small>随Windows自动切换</small></button><button type="button" role="radio" aria-checked={theme.preference === "day_gradient"} className={theme.preference === "day_gradient" ? "selected" : ""} onClick={() => void theme.setPreference("day_gradient")}><strong>白天渐变</strong><small>雾蓝 · 浅紫 · 淡粉</small></button><button type="button" role="radio" aria-checked={theme.preference === "night_dark"} className={theme.preference === "night_dark" ? "selected" : ""} onClick={() => void theme.setPreference("night_dark")}><strong>夜晚暗黑</strong><small>黑底 · 白字</small></button></div><div className="readonly-note">当前显示：{theme.effective_theme === "night_dark" ? "夜晚暗黑" : "白天渐变"}</div></section>}
          {tab === "logs" && <><section><h2>后台与诊断</h2><p>{maintenance.data?.background_notice ?? "后台任务会自动让出资源，优先保证搜索和预览。"}</p><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void exportDiagnostics()}>导出诊断包</button></div></section><section><h2>本地诊断日志</h2><p>日志只保存在电脑中，不包含文档正文。最近 {maintenance.data?.log_events ?? 0} 条。</p><div className="log-list">{logItems.map((log) => <div key={log.log_id}><time>{new Date(log.created_at).toLocaleString("zh-CN")}</time><strong>{log.component} · {log.event_name}</strong><code>{JSON.stringify(log.fields)}</code></div>)}{logItems.length === 0 && <p>当前没有诊断日志。</p>}</div>{logs.hasNextPage && <button type="button" className="load-more-button" disabled={logs.isFetchingNextPage} onClick={() => void logs.fetchNextPage()}>{logs.isFetchingNextPage ? "正在加载" : "加载更多日志"}</button>}<button type="button" className="danger-button" disabled={busy || logItems.length === 0} onClick={() => void clearLogs()}><DeleteOutlined /> 清除日志</button></section><section><h2>重置应用数据</h2><p>重置只处理拾忆自己的数据库、模型、缓存和配置。下次启动前会先移动到时间戳隔离目录，便于出现异常时恢复；资料源文件始终不动。</p><button type="button" className="danger-button" disabled={busy} onClick={() => void resetApplicationData()}><DeleteOutlined /> 重置并重新启动</button></section></>}
        </div>
      </div>
    </section>
  );
}
