import { DeleteOutlined, FolderOpenOutlined, ReloadOutlined, SafetyCertificateOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type EnvironmentCheck } from "../bridge";
import { useAppStore } from "../state/app-store";

type SettingsTab = "roots" | "models" | "index" | "appearance" | "logs";
const bytes = (value: number) => value < 1024 * 1024 ? `${Math.round(value / 1024)} KB` : value < 1024 * 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MB` : `${(value / 1024 / 1024 / 1024).toFixed(2)} GB`;

export function SettingsPage({ environment }: { environment: EnvironmentCheck | null }) {
  const navigate = useAppStore((state) => state.navigate);
  const [tab, setTab] = useState<SettingsTab>("roots");
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const roots = useQuery({ queryKey: ["settings-roots"], queryFn: () => bridge.root_list() });
  const models = useQuery({ queryKey: ["settings-models"], queryFn: async () => ({ state: await bridge.model_state_get(), artifacts: await bridge.model_artifact_list() }) });
  const maintenance = useQuery({ queryKey: ["maintenance"], queryFn: () => bridge.maintenance_get() });
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

  const checkDatabase = async () => {
    setBusy(true); setError(null); setMessage(null);
    try {
      const result = await bridge.maintenance_check("quick");
      if (result.database_result !== "ok") throw new Error(`数据库检查未通过：${result.database_result}`);
      setMessage(`数据库完整性检查通过，用时 ${result.elapsed_ms} 毫秒；源文件修改：否。`);
      await maintenance.refetch();
    } catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setBusy(false); }
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

  return (
    <section className="page">
      <header className="page-heading"><div><h1>设置</h1><p>管理资料位置、本地模型、索引和异常恢复。</p></div></header>
      {error && <p role="alert" className="inline-error">{error}</p>}{message && <p className="inline-success">{message}</p>}
      <div className="settings-grid">
        <aside className="settings-nav">
          <button type="button" className={tab === "roots" ? "active" : ""} onClick={() => setTab("roots")}>资料目录</button><button type="button" className={tab === "models" ? "active" : ""} onClick={() => setTab("models")}>本地模型</button><button type="button" className={tab === "index" ? "active" : ""} onClick={() => setTab("index")}>索引与存储</button><button type="button" className={tab === "appearance" ? "active" : ""} onClick={() => setTab("appearance")}>外观与辅助功能</button><button type="button" className={tab === "logs" ? "active" : ""} onClick={() => setTab("logs")}>日志与恢复</button>
        </aside>
        <div className="settings-content">
          {tab === "roots" && <><section><h2>资料目录</h2><p>首次启动默认注册桌面、文档、下载和图片。可以继续添加文件夹或整块本地磁盘。</p><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void addRoot(false)}><FolderOpenOutlined /> 添加文件夹</button><button type="button" disabled={busy} onClick={() => void addRoot(true)}><FolderOpenOutlined /> 添加整个磁盘</button></div></section><section><h2>已授权位置</h2><div className="settings-list">{roots.data?.map((root) => <div key={root.root_id}><span><strong>{root.label}</strong><small>{root.path}</small></span><em>{root.enabled ? `${root.file_count} 个文件` : "已停用"}</em><button type="button" className="text-button" disabled={busy} onClick={() => void disableRoot(root.root_id, root.label)}>从拾忆移除</button></div>)}{!roots.isLoading && roots.data?.length === 0 && <p>当前没有资料目录。可以重新添加文件夹。</p>}</div></section><section><h2>源文件保护</h2><div className="readonly-note"><SafetyCertificateOutlined /> 源文件始终只读。整理功能只创建虚拟集合和操作建议。</div></section></>}
          {tab === "models" && <><section><h2>当前能力</h2><div className="setting-row"><div><strong>{models.data?.state.message ?? "正在读取模型状态"}</strong><small>运行后端：{models.data?.state.runtime_backend ?? "基础模式"} · 内存 {environment?.memory_total_gb ?? "—"} GB</small></div><button type="button" onClick={() => navigate("model_setup")}>管理模型</button></div></section><section><h2>已导入组件</h2><div className="settings-list">{models.data?.artifacts.map((artifact) => <div key={artifact.artifact_id}><span><strong>{artifact.model_id}</strong><small>{artifact.role} · {artifact.format.toUpperCase()} · {bytes(artifact.size_bytes)}</small></span><em>{artifact.embedding_dimension ? `已自检 · ${artifact.embedding_dimension}维` : artifact.status}</em></div>)}{models.data?.artifacts.length === 0 && <p>还没有导入本地模型。基础搜索不受影响。</p>}</div></section></>}
          {tab === "index" && <><section><h2>索引状态</h2><div className="metric-strip"><span><strong>{maintenance.data?.indexed_files ?? "—"}</strong>已索引文件</span><span><strong>{maintenance.data?.searchable_chunks ?? "—"}</strong>全文块</span><span><strong>{maintenance.data?.embedded_chunks ?? "—"}</strong>向量块</span><span><strong>{maintenance.data ? bytes(maintenance.data.database_size_bytes) : "—"}</strong>数据库</span></div></section><section><h2>自动检查站</h2><div className="health-list">{maintenance.data?.checks.map((check) => <div key={check.key} className={`health-${check.status}`}><i /><span><strong>{check.label}</strong><small>{check.detail}</small></span></div>)}</div><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void checkDatabase()}><ReloadOutlined /> {busy ? "检查中" : "完整性检查"}</button><button type="button" className="danger-button" disabled={busy} onClick={() => void rebuild()}><DeleteOutlined /> 重建派生索引</button></div></section></>}
          {tab === "appearance" && <section><h2>显示与动效</h2><p>拾忆跟随 Windows 的缩放和辅助功能设置。欢迎页只在首次使用时播放，减少动态效果由系统偏好自动接管。</p><div className="readonly-note">当前主题：雾蓝—浅紫—淡粉 · 现代无衬线字体 · 大圆角</div></section>}
          {tab === "logs" && <><section><h2>运行状态</h2><div className={`degradation degradation--${maintenance.data?.degradation_level ?? "full"}`}><strong>{maintenance.data?.degradation_level === "core" ? "核心模式" : maintenance.data?.degradation_level === "balanced" ? "均衡模式" : "完整模式"}</strong><span>{maintenance.data?.degradation_reasons.join("；") || "未检测到需要降级的异常。"}</span></div><div className="settings-actions"><button type="button" disabled={busy} onClick={() => void exportDiagnostics()}>导出诊断包</button></div></section><section><h2>本地诊断日志</h2><p>日志只保存在电脑中，不包含文档正文。最近 {maintenance.data?.log_events ?? 0} 条。</p><div className="log-list">{logItems.map((log) => <div key={log.log_id}><time>{new Date(log.created_at).toLocaleString("zh-CN")}</time><strong>{log.component} · {log.event_name}</strong><code>{JSON.stringify(log.fields)}</code></div>)}{logItems.length === 0 && <p>当前没有诊断日志。</p>}</div>{logs.hasNextPage && <button type="button" className="load-more-button" disabled={logs.isFetchingNextPage} onClick={() => void logs.fetchNextPage()}>{logs.isFetchingNextPage ? "正在加载" : "加载更多日志"}</button>}<button type="button" className="danger-button" disabled={busy || logItems.length === 0} onClick={() => void clearLogs()}><DeleteOutlined /> 清除日志</button></section></>}
        </div>
      </div>
    </section>
  );
}
