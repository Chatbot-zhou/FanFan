import { ApartmentOutlined, FolderAddOutlined, MoreOutlined, ReloadOutlined, SafetyCertificateOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type ExportResult, type ExtractionRunResult, type SkillDefinition, type TaskExecutionResult, type TaskPlan } from "../bridge";
import { useAppStore } from "../state/app-store";
import { displayPath } from "../utils/display-path";

function presetForSkill(skillId: string, selectedPreset: string) {
  if (skillId === "generate_catalog" || skillId === "export_index") return "file_catalog";
  if (skillId === "multi_document_summary") return "extractive_summary";
  if (skillId === "recommend_filename") return "filename_suggestions";
  if (skillId === "recommend_folders") return "folder_suggestions";
  if (skillId === "duplicate_review") return "duplicate_review";
  if (skillId === "version_compare") return "version_compare";
  if (skillId === "merge_tables") return "merge_tables";
  if (skillId === "rerun_ocr") return "ocr_report";
  return selectedPreset;
}

export function LibraryPage() {
  const navigate = useAppStore((state) => state.navigate);
  const queryClient = useQueryClient();
  const roots = useQuery({ queryKey: ["roots"], queryFn: () => bridge.root_list() });
  const files = useInfiniteQuery({
    queryKey: ["files"],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.file_query({ cursor: pageParam, page_size: 100 }),
    getNextPageParam: (page) => page.next_cursor,
  });
  const fileItems = files.data?.pages.flatMap((page) => page.items) ?? [];
  const fileTotal = files.data?.pages[0]?.total ?? 0;
  const presets = useQuery({ queryKey: ["extraction-presets"], queryFn: () => bridge.extraction_preset_list() });
  const skills = useQuery({ queryKey: ["registered-skills"], queryFn: () => bridge.skill_list() });
  const recoverableTask = useQuery({ queryKey: ["recoverable-task"], queryFn: () => bridge.task_recoverable() });
  const relations = useInfiniteQuery({
    queryKey: ["file-relations"],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => bridge.relation_query({ cursor: pageParam, page_size: 100 }),
    getNextPageParam: (page) => page.next_cursor,
  });
  const relationItems = relations.data?.pages.flatMap((page) => page.items) ?? [];
  const relationTotal = relations.data?.pages[0]?.total ?? 0;
  const refreshRelations = useMutation({ mutationFn: () => bridge.relation_refresh(5000), onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["file-relations"] }) });
  const reviewRelation = useMutation({
    mutationFn: ({ relationId, action }: { relationId: string; action: "accepted" | "rejected" }) => bridge.relation_review(relationId, action),
    onSuccess: async () => queryClient.invalidateQueries({ queryKey: ["file-relations"] }),
  });
  const [adding, setAdding] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [presetId, setPresetId] = useState("file_catalog");
  const [skillId, setSkillId] = useState("batch_field_extraction");
  const [processing, setProcessing] = useState(false);
  const [run, setRun] = useState<ExtractionRunResult | null>(null);
  const [plan, setPlan] = useState<TaskPlan | null>(null);
  const [execution, setExecution] = useState<TaskExecutionResult | null>(null);
  const [exportFormat, setExportFormat] = useState<ExportResult["format"]>("xlsx");
  const [exporting, setExporting] = useState(false);
  const [exportResult, setExportResult] = useState<ExportResult | null>(null);

  const addRoot = async () => {
    setError(null);
    if (!isTauri()) { setError("浏览器预览不调用系统目录选择器，请在拾忆桌面程序中添加资料位置。"); return; }
    setAdding(true);
    try {
      const selectedPath = await open({ directory: true, multiple: false, title: "添加资料位置" });
      if (typeof selectedPath !== "string") return;
      const fullVolume = /^[a-zA-Z]:\\?$/.test(selectedPath);
      if (fullVolume && !window.confirm("扫描整个磁盘可能耗时较长，并会自动排除系统和凭据目录。确认继续吗？")) return;
      await bridge.root_add({ path: selectedPath, label: null, watch_mode: "realtime", authorization_source: "user_selected", full_volume_confirmed: fullVolume });
      await roots.refetch();
    } catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setAdding(false); }
  };

  const toggleFile = (fileId: string) => { setPlan(null); setRun(null); setExecution(null); setSelected((current) => {
    const next = new Set(current);
    if (next.has(fileId)) next.delete(fileId); else next.add(fileId);
    return next;
  }); };

  const previewPlan = async () => {
    if (selected.size === 0) return;
    setProcessing(true); setError(null); setRun(null);
    const effectivePreset = presetForSkill(skillId, presetId);
    try { setPlan(await bridge.task_plan(skillId, [...selected], { preset_id: effectivePreset })); }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setProcessing(false); }
  };

  const runExtraction = async () => {
    if (selected.size === 0 || !plan) return;
    setProcessing(true); setError(null); setExportResult(null);
    const effectivePreset = presetForSkill(skillId, presetId);
    try {
      const completed = await bridge.task_execute(skillId, [...selected], { preset_id: effectivePreset }, plan.task_id);
      setExecution(completed);
      setPlan(completed.plan);
      setRun(completed.result);
    }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setProcessing(false); }
  };

  const resumeTask = async () => {
    if (!recoverableTask.data) return;
    setProcessing(true); setError(null); setExportResult(null);
    try {
      const completed = await bridge.task_resume(recoverableTask.data.task_id);
      setExecution(completed);
      setPlan(completed.plan);
      setRun(completed.result);
      await recoverableTask.refetch();
    }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setProcessing(false); }
  };

  const exportRun = async () => {
    if (!run) return;
    if (!isTauri()) { setError("浏览器预览不会写入电脑文件，请在拾忆桌面程序中导出。"); return; }
    const target = await save({ title: "导出拾忆处理结果", defaultPath: `拾忆-${run.preset.name}.${exportFormat}`, filters: [{ name: exportFormat.toUpperCase(), extensions: [exportFormat] }] });
    if (typeof target !== "string") return;
    setExporting(true); setError(null);
    try { setExportResult(await bridge.extraction_export(run, exportFormat, target)); }
    catch (actionError) { setError(actionError instanceof Error ? actionError.message : String(actionError)); }
    finally { setExporting(false); }
  };

  const valueText = (value: unknown) => value == null ? "—" : Array.isArray(value) ? value.join("、") : typeof value === "object" ? JSON.stringify(value) : String(value);

  return (
    <section className="page">
      <header className="page-heading"><div><h1>全部资料</h1><p>查看资料位置，选择文件并执行受限的本地处理。</p></div><button type="button" className="primary-button" disabled={adding} onClick={() => void addRoot()}><FolderAddOutlined /> {adding ? "正在添加" : "添加资料位置"}</button></header>
      {error && <p role="alert" className="inline-error">{error}</p>}
      <div className="readonly-note"><SafetyCertificateOutlined /> 拾忆只读取资料，不移动、重命名、删除或覆盖源文件。</div>
      {recoverableTask.data && <div className="readonly-note"><span>发现未完成任务：{recoverableTask.data.summary}，已通过的检查站不会重复执行。</span><button type="button" className="text-button" disabled={processing} onClick={() => void resumeTask()}>{processing ? "正在恢复" : "从检查点继续"}</button></div>}
      <div className="root-table">
        <div className="root-table__head"><span>资料位置</span><span>状态</span><span>文件</span><span>最近扫描</span><span /></div>
        {roots.data?.map((root) => <div className="root-table__row" key={root.root_id}><span><strong>{root.label}</strong><small>{root.path}</small></span><span><i className={`status-dot status-dot--${root.status}`} />{root.status === "scanning" ? "扫描中" : "已完成"}</span><span>{root.file_count}</span><span>{root.last_scan_at ? new Date(root.last_scan_at).toLocaleString("zh-CN") : "—"}</span><button type="button" aria-label={`管理${root.label}`} title="前往设置管理" onClick={() => navigate("settings")}><MoreOutlined /></button></div>)}
      </div>

      <section className="library-files">
        <header><div><h2>资料文件</h2><p>选择已完成索引的文件后，通过上下文操作栏进入批量处理。</p></div><span>{selected.size > 0 ? `已选择 ${selected.size} 项` : `共 ${fileTotal} 项`}</span></header>
        {files.isLoading && <p>正在读取资料目录…</p>}
        {files.isError && <p role="alert" className="inline-error">{files.error instanceof Error ? files.error.message : String(files.error)}</p>}
        <div className="file-select-table">
          {fileItems.map((file) => {
            const ready = file.parse_status === "parsed" && Boolean(file.current_revision_id);
            return <label key={file.file_id} className={!ready ? "is-disabled" : ""}><input type="checkbox" disabled={!ready} checked={selected.has(file.file_id)} onChange={() => toggleFile(file.file_id)} /><span><strong>{file.display_name}</strong><small>{displayPath(file.display_path)}</small></span><em>{ready ? "可处理" : file.parse_status === "ocr_pending" ? "等待OCR" : "尚未索引"}</em></label>;
          })}
          {!files.isLoading && fileTotal === 0 && <div className="relation-empty"><p>扫描到的资料会显示在这里。</p></div>}
        </div>
        {files.hasNextPage && <button type="button" className="load-more-button" disabled={files.isFetchingNextPage} onClick={() => void files.fetchNextPage()}>{files.isFetchingNextPage ? "正在加载" : `加载更多（还剩 ${Math.max(0, fileTotal - fileItems.length)} 项）`}</button>}
        <div className="processing-bar">
          <select aria-label="处理能力" value={skillId} onChange={(event) => { setSkillId(event.target.value); setPlan(null); setRun(null); }}>
            {skills.data?.filter((skill: SkillDefinition) => skill.available).map((skill) => <option key={skill.skill_id} value={skill.skill_id}>{skill.name}</option>)}
          </select>
          {skillId === "batch_field_extraction" && <select aria-label="处理模板" value={presetId} onChange={(event) => { setPresetId(event.target.value); setPlan(null); setRun(null); }}>{presets.data?.map((preset) => <option key={preset.preset_id} value={preset.preset_id}>{preset.name}</option>)}</select>}
          <small>{skillId === "batch_field_extraction" ? presets.data?.find((preset) => preset.preset_id === presetId)?.description : skills.data?.find((skill) => skill.skill_id === skillId)?.description}</small>
          <button type="button" className="primary-button" disabled={selected.size === 0 || processing} onClick={() => void previewPlan()}>{processing ? "正在生成计划" : "预览任务计划"}</button>
        </div>
      </section>

      {plan && !run && <section className="task-plan-card"><header><div><h2>{plan.summary}</h2><p>版本 {plan.skill_version} · {plan.estimated_file_count} 份资料</p></div><span>等待确认</span></header><ol>{plan.steps.map((step) => <li key={step.step_id}><i>{step.ordinal}</i><div><strong>{step.label}</strong><small>检查站：{step.checkpoint}</small></div><em>{step.status === "pending" ? "待执行" : step.status}</em></li>)}</ol>{plan.warnings.map((warning) => <p className="readonly-note" key={warning}>{warning}</p>)}<div className="plan-confirm"><button type="button" onClick={() => setPlan(null)}>返回调整</button><button type="button" className="primary-button" disabled={processing} onClick={() => void runExtraction()}>{processing ? "正在分析" : "确认并开始分析"}</button></div></section>}

      {run && <section className="extraction-result">
        <header><div><h2>{run.preset.name} · 结果复核</h2><p>{run.rows.length} 个文件；每个非空正文值都可以查看独立来源。</p></div>{execution && <span>{execution.checkpoints.filter((checkpoint) => checkpoint.status === "passed").length}/{execution.checkpoints.length} 检查站通过</span>}</header>
        {execution && execution.candidates.length > 0 && <div className="readonly-note"><span>已比较 {execution.candidates.length} 条处理路径，采用 <strong>{execution.candidates.find((candidate) => candidate.status === "selected")?.strategy ?? "证据最完整路径"}</strong>；其他候选保留在本地任务记录中。</span></div>}
        {run.warnings.map((warning) => <p className="readonly-note" key={warning}>{warning}</p>)}
        <div className="extraction-table-wrap"><table><thead><tr><th>文件</th>{run.preset.fields.map((field) => <th key={field.key}>{field.label}</th>)}</tr></thead><tbody>{run.rows.map((row, rowIndex) => <tr key={`${row.file.file_id}-${rowIndex}`}><th>{row.file.display_name}</th>{run.preset.fields.map((field) => { const value = row.values.find((item) => item.field_key === field.key); return <td key={field.key} className={value?.review_state === "needs_review" ? "needs-review" : ""}><span>{valueText(value?.normalized_value)}</span>{value && value.evidence.length > 0 && <details><summary>{value.method === "metadata" ? "元数据来源" : `${value.evidence.length} 处原文`}</summary>{value.evidence.map((evidence) => <p key={evidence.evidence_id}>{evidence.quote}</p>)}</details>}</td>; })}</tr>)}</tbody></table></div>
        <div className="export-bar"><label>导出格式 <select value={exportFormat} onChange={(event) => setExportFormat(event.target.value as ExportResult["format"])}><option value="xlsx">Excel (.xlsx)</option><option value="csv">CSV</option><option value="json">JSON</option><option value="docx">Word (.docx)</option></select></label><button type="button" className="primary-button" disabled={exporting} onClick={() => void exportRun()}>{exporting ? "正在校验并导出" : "选择位置并导出"}</button>{exportResult && <small>已新建 {exportResult.target_path} · {exportResult.row_count} 行 · SHA-256 {exportResult.sha256.slice(0, 12)}…</small>}</div>
      </section>}

      <section className="relation-panel"><header><div><h2><ApartmentOutlined /> 重复与版本关系</h2><p>只读取文件计算特征，不会删除、合并或改名；结果需要你确认后再处理。</p></div><button type="button" className="text-button" disabled={refreshRelations.isPending} onClick={() => refreshRelations.mutate()}><ReloadOutlined /> {refreshRelations.isPending ? "正在分析" : "重新分析"}</button></header>
        {refreshRelations.data && <p className="relation-summary">本次读取 {refreshRelations.data.hashed_files} 个同大小候选，发现 {refreshRelations.data.exact_duplicate_pairs} 组完全重复、{refreshRelations.data.version_candidate_pairs} 组版本候选。</p>}
        {refreshRelations.isError && <p role="alert" className="inline-error">{refreshRelations.error instanceof Error ? refreshRelations.error.message : String(refreshRelations.error)}</p>}
        {relations.isLoading && <p>正在读取文件关系…</p>}
        {!relations.isLoading && relationItems.length === 0 && <div className="relation-empty"><p>还没有分析结果。需要时点击“重新分析”，拾忆只会对同大小文件计算哈希。</p></div>}
        <div className="relation-list">{relationItems.map((relation) => <article key={relation.relation_id}><span>{relation.relation_type === "exact_duplicate" ? "完全重复" : relation.relation_type === "version_candidate" ? "版本候选" : "相关资料"}</span><div><strong>{relation.left_file.display_name}</strong><small>{displayPath(relation.left_file.display_path)}</small></div><i>↔</i><div><strong>{relation.right_file.display_name}</strong><small>{displayPath(relation.right_file.display_path)}</small></div><em>{relation.review_status === "accepted" ? "已确认" : `${Math.round(relation.confidence * 100)}%`}</em><div className="relation-actions"><button type="button" disabled={reviewRelation.isPending || relation.review_status === "accepted"} onClick={() => reviewRelation.mutate({ relationId: relation.relation_id, action: "accepted" })}>确认</button><button type="button" disabled={reviewRelation.isPending} onClick={() => reviewRelation.mutate({ relationId: relation.relation_id, action: "rejected" })}>排除</button></div></article>)}</div>
        {relations.hasNextPage && <button type="button" className="load-more-button" disabled={relations.isFetchingNextPage} onClick={() => void relations.fetchNextPage()}>{relations.isFetchingNextPage ? "正在加载" : `加载更多关系（还剩 ${Math.max(0, relationTotal - relationItems.length)} 项）`}</button>}
      </section>
    </section>
  );
}
