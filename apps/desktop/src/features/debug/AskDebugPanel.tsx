import { DeleteOutlined, ExportOutlined, SearchOutlined, PlayCircleOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { useState } from "react";
import { bridge } from "../../bridge";
import { confirmAction } from "../../components/AppConfirm";
import { errorMessage } from "../../utils/app-error";
import type { AskEvaluationRunReport, AskTrace, AskTraceStage } from "../../bridge/contracts";

/** 阶段中文名（12+ 阶段展示用） */
const STAGE_LABELS: Record<string, string> = {
  source_routing: "Source Routing 路由",
  query_parsing: "Query Parsing 意图解析",
  context_resolution: "Context Resolution 上下文恢复",
  memory_resolution: "Memory Resolution 记忆定位",
  document_resolution: "Document Resolution 文档定位",
  scope_planning: "Scope Planning 范围规划",
  query_rewrite: "Query Rewrite 查询改写",
  document_recall: "Document Recall 文档召回",
  retrieval: "Chunk Retrieval 块检索",
  reranking: "Rerank 重排",
  generation: "Generation 生成",
  verification: "Citation Validation 引用核验",
  repair: "Repair 结构修复",
  clarification_selection: "Clarification 澄清选择",
  document_find: "Document Find 找文件",
  document_compare: "Document Compare 对比",
  document_summary: "Document Summary 摘要",
  extract: "Extract 抽取",
  operation_execution: "Operation 执行分支",
  memory_candidate_write: "Memory Writer 记忆候选",
  completed: "Completed 完成",
};

const compactJson = (value: unknown): string => {
  const text = JSON.stringify(value);
  return text.length > 600 ? `${text.slice(0, 600)}…[已截断]` : text;
};

export function AskDebugPanel() {
  const [traceOperationId, setTraceOperationId] = useState("");
  const [trace, setTrace] = useState<AskTrace | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [includeDetailedText, setIncludeDetailedText] = useState(false);
  const [evalTargetPath, setEvalTargetPath] = useState("");
  const [evalOutputPath, setEvalOutputPath] = useState("");
  const [report, setReport] = useState<AskEvaluationRunReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const loadTrace = async (operationId: string) => {
    setError(null); setMessage(null); setBusy(true);
    try {
      const loaded = await bridge.ask_trace_get(operationId.trim());
      setTrace(loaded);
      setExpanded(new Set());
      setMessage(`已加载 ${loaded.stages.length} 个阶段${loaded.diagnostic_summary ? `：${loaded.diagnostic_summary}` : ""}`);
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const exportTrace = async () => {
    if (!trace) return;
    if (!isTauri()) { setError("浏览器预览不写入电脑文件，请在翻翻桌面程序中导出。"); return; }
    const target = await save({ title: "导出 Ask Debug Trace", defaultPath: `ask-trace-${trace.operation_id.slice(0, 8)}.json`, filters: [{ name: "JSON", extensions: ["json"] }] });
    if (!target) return;
    if (!await confirmAction({ actionKey: "ask_trace_export", title: "导出 Debug Trace？", description: `包含本次问答 ${trace.stages.length} 个阶段的输入输出快照与耗时${includeDetailedText ? "，并保留详细文本（chunk 全文 / 模型 prompt）" : "（已脱敏：路径脱敏、文本截断、不含模型完整 prompt）"}。`, confirmLabel: "导出" })) return;
    setBusy(true);
    try {
      const result = await bridge.ask_trace_export({ operation_id: trace.operation_id, target_path: target, include_detailed_text: includeDetailedText, confirmed: true });
      setMessage(`已新建导出文件：${result.size_bytes} 字节，SHA256 ${result.sha256.slice(0, 12)}…`);
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const runEvaluation = async () => {
    if (!isTauri()) { setError("浏览器预览不批量运行评估，请在翻翻桌面程序中运行。"); return; }
    if (!evalTargetPath.trim() || !evalOutputPath.trim()) { setError("请先选择测试集与结果输出路径。"); return; }
    if (!await confirmAction({ actionKey: "ask_evaluation_run", title: "运行评估测试集？", description: "会真实调用问答管线逐例运行（每例独立会话，跑完即删；不写 Memory、不改任何参数）。结果写入所选 JSON 文件；测试集与结果路径会显示在下方。", confirmLabel: "开始运行", danger: true })) return;
    setBusy(true); setError(null); setMessage(null);
    try {
      const loaded = await bridge.ask_evaluation_run({ target_path: evalTargetPath.trim(), output_path: evalOutputPath.trim(), confirmed: true });
      setReport(loaded);
      setMessage(`评估完成：${loaded.total} 例，通过 ${loaded.passed}，失败 ${loaded.failed}。结果已写入所选文件。`);
    } catch (actionError) { setError(errorMessage(actionError)); }
    finally { setBusy(false); }
  };

  const toggleStage = (node: string) => {
    setExpanded((previous) => {
      const next = new Set(previous);
      if (next.has(node)) next.delete(node);
      else next.add(node);
      return next;
    });
  };

  return (
    <section className="debug-panel">
      {error && <p role="alert" className="inline-error">{error}</p>}
      {message && <p className="inline-success">{message}</p>}

      <h3>Ask Trace Viewer</h3>
      <p>输入一次问答的 operation_id（可在评估结果中复制）查看 12+ 阶段追踪。完整 prompt 默认不显示，勾选「保留详细文本」后导出时才会包含。</p>
      <div className="settings-actions">
        <input className="debug-input" placeholder="operation_id（如 018f0000-…）" value={traceOperationId} onChange={(event) => setTraceOperationId(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") void loadTrace(traceOperationId); }} />
        <button type="button" disabled={busy || !traceOperationId.trim()} onClick={() => void loadTrace(traceOperationId)}><SearchOutlined /> 加载 Trace</button>
        {trace && <button type="button" disabled={busy} onClick={() => void exportTrace()}><ExportOutlined /> 导出 JSON</button>}
        {trace && <label className="debug-check"><input type="checkbox" checked={includeDetailedText} onChange={(event) => setIncludeDetailedText(event.target.checked)} /> Include detailed text（导出时保留 chunk/prompt 全文）</label>}
      </div>

      {trace && <>
        <div className="readonly-note debug-summary"><strong>诊断摘要</strong><span>{trace.diagnostic_summary || "（无）"}</span><span>answer_mode：{trace.answer_mode ?? "—"}</span><span>总耗时：{trace.timing.total_ms ?? "—"} ms</span></div>
        <div className="debug-stages">
          {trace.stages.map((stage) => <TraceStageRow key={stage.node} stage={stage} expanded={expanded.has(stage.node)} onToggle={() => toggleStage(stage.node)} />)}
        </div>
      </>}

      <h3>Ask Evaluation Runner</h3>
      <p>选择 JSONL/JSON 测试集与结果输出路径，批量运行问答管线（每例独立会话，跑完即删，不污染 Ask History；不写 Memory）。运行后每例可在上方输入其 operation_id 查看 Trace。</p>
      <div className="settings-actions">
        <input className="debug-input" placeholder="测试集路径（JSONL 或 JSON 数组）" value={evalTargetPath} onChange={(event) => setEvalTargetPath(event.target.value)} />
        <button type="button" disabled={busy} onClick={async () => { if (!isTauri()) return; const selected = await open({ multiple: false, title: "选择评估测试集" }); if (typeof selected === "string") setEvalTargetPath(selected); }}><SearchOutlined /> 选择</button>
        <input className="debug-input" placeholder="结果输出路径（绝对路径 .json，已存在会拒绝）" value={evalOutputPath} onChange={(event) => setEvalOutputPath(event.target.value)} />
        <button type="button" disabled={busy} onClick={async () => { if (!isTauri()) return; const selected = await save({ title: "评估结果输出位置", defaultPath: "ask-evaluation-results.json", filters: [{ name: "JSON", extensions: ["json"] }] }); if (typeof selected === "string") setEvalOutputPath(selected); }}><ExportOutlined /> 选择</button>
        <button type="button" className="danger-button" disabled={busy || !evalTargetPath.trim() || !evalOutputPath.trim()} onClick={() => void runEvaluation()}><PlayCircleOutlined /> 运行评估</button>
      </div>

      {report && <>
        <div className="readonly-note debug-summary">
          <span>run_id：{report.run_id.slice(0, 8)}…</span>
          <span>{report.total} 例 · 通过 {report.passed} · 失败 {report.failed}</span>
          <span>Source Router：{(report.metrics.source_router_accuracy * 100).toFixed(1)}%</span>
          <span>Intent：{(report.metrics.intent_accuracy * 100).toFixed(1)}%</span>
          <span>Top-1 文档定位：{(report.metrics.document_resolution_top1_accuracy * 100).toFixed(1)}%</span>
          <span>Top-3 召回：{(report.metrics.document_resolution_top3_recall * 100).toFixed(1)}%</span>
          <span>证据召回：{(report.metrics.retrieval_evidence_recall * 100).toFixed(1)}%</span>
          <span>无证据误拒：{(report.metrics.no_evidence_false_negative_rate * 100).toFixed(1)}%</span>
          <span>Grounded：{(report.metrics.grounded_answer_rate * 100).toFixed(1)}%</span>
          <span>引用通过：{(report.metrics.citation_pass_rate * 100).toFixed(1)}%</span>
          <span>平均 {report.metrics.avg_total_ms.toFixed(0)} ms · P50 {report.metrics.p50_total_ms} ms · P95 {report.metrics.p95_total_ms} ms</span>
        </div>
        <div className="debug-eval-list">
          {report.results.map((result) => (
            <div key={result.case_id} className={result.pass_fail ? "eval-pass" : "eval-fail"}>
              <span><strong>{result.pass_fail ? "✓" : "✗"} {result.case_id}</strong><small>{result.question.length > 40 ? `${result.question.slice(0, 40)}…` : result.question}</small></span>
              <em>{result.actual_source ?? "—"} / {result.actual_intent ?? "—"}</em>
              <em>{result.error_category ? `错误：${result.error_category}` : result.failed_fields.length ? `失败：${result.failed_fields.join("、")}` : "通过"}</em>
              <em>{result.latency_ms} ms</em>
              <button type="button" className="text-button" disabled={busy} onClick={() => { setTraceOperationId(result.operation_id); void loadTrace(result.operation_id); }}>查看 Trace</button>
            </div>
          ))}
        </div>
      </>}
    </section>
  );
}

function TraceStageRow({ stage, expanded, onToggle }: { stage: AskTraceStage; expanded: boolean; onToggle: () => void }) {
  const elapsed = stage.records.reduce((sum, record) => sum + (record.elapsed_ms ?? 0), 0);
  return (
    <div className="debug-stage">
      <button type="button" className="debug-stage__head" onClick={onToggle}>
        <span>{expanded ? "▾" : "▸"} {STAGE_LABELS[stage.node] ?? stage.node}</span>
        <em>{stage.records.length} 条 · {elapsed} ms</em>
      </button>
      {expanded && <div className="debug-stage__body">
        {stage.records.map((record) => (
          <div key={record.trace_id} className="debug-record">
            <span className={record.status === "ok" ? "trace-ok" : "trace-error"}>{record.status === "ok" ? "ok" : "error"}</span>
            <pre>{compactJson(record.input_json)}</pre>
            <pre>{compactJson(record.output_json)}</pre>
            {record.elapsed_ms != null && <small>{record.elapsed_ms} ms</small>}
          </div>
        ))}
      </div>}
    </div>
  );
}

