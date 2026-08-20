import { ArrowLeftOutlined, FolderOpenOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import { bridge, type ImportCandidate, type ModelDownloadJob, type ModelRole } from "../../bridge";
import { recordDiagnosticEvent } from "../../bridge/observed-bridge";
import { AppSelect } from "../../components/AppSelect";
import { ModelDownloadList, type ModelDownloadAction } from "../model-downloads/ModelDownloadList";
import {
  modelDownloadIsActive,
  summarizeModelDownloads,
  visibleModelDownloadJobs,
} from "../model-downloads/model-downloads";
import { errorMessage, normalizeAppError } from "../../utils/app-error";

/**
 * 官方档位页面底部的收敛区：只保留「官方 4 档对应的模型下载任务」与「本地模型导入」。
 * 不再提供按角色逐模型配置、已验证模型选择池或本地组件管理（已在预设选择槽位承接）。
 */
export function ModelManagementPanel() {
  const queryClient = useQueryClient();
  const [step, setStep] = useState<"main" | "import">("main");
  const [candidates, setCandidates] = useState<ImportCandidate[]>([]);
  const [roles, setRoles] = useState<Record<string, ModelRole>>({});
  const [pendingDownloadActions, setPendingDownloadActions] = useState<Record<string, ModelDownloadAction>>({});
  const [downloadActionErrors, setDownloadActionErrors] = useState<Record<string, string>>({});
  const downloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
  });
  const visibleDownloads = useMemo(() => visibleModelDownloadJobs(downloads.data ?? []), [downloads.data]);
  const downloadSummary = useMemo(() => summarizeModelDownloads(visibleDownloads), [visibleDownloads]);

  const scanImport = useMutation({
    mutationFn: (paths: string[]) => bridge.model_import_scan(paths),
    onSuccess: (items) => {
      setCandidates(items);
      setRoles(Object.fromEntries(items.map((item) => [item.candidate_id, item.suggested_role ?? (item.format === "gguf" ? "generation" : "embedding")])));
    },
  });
  const importModels = useMutation({
    mutationFn: () => bridge.model_import_confirm(candidates.map((candidate) => ({ source_path: candidate.source_path, role: roles[candidate.candidate_id] ?? "embedding" }))),
    onSuccess: async () => {
      setCandidates([]);
      await queryClient.invalidateQueries({ queryKey: ["model-artifacts"] });
    },
  });

  const runDownloadAction = async (job: ModelDownloadJob, action: ModelDownloadAction) => {
    if (pendingDownloadActions[job.job_id]) return;
    const startedAt = performance.now();
    setPendingDownloadActions((current) => ({ ...current, [job.job_id]: action }));
    setDownloadActionErrors((current) => {
      const next = { ...current };
      delete next[job.job_id];
      return next;
    });
    recordDiagnosticEvent({
      level: "info",
      component: "frontend.model_downloads",
      event_name: "model_download.action_started",
      fields: { job_id: job.job_id, action, phase: job.phase },
    });
    try {
      let updated: ModelDownloadJob | null = null;
      if (action === "pause") updated = await bridge.model_download_pause(job.job_id);
      else if (action === "cancel") await bridge.model_download_cancel(job.job_id);
      else if (action === "resume") updated = await bridge.model_download_resume(job.job_id);
      else if (action === "remove") await bridge.model_download_remove(job.job_id);
      else updated = await bridge.model_download_retry(job.job_id);
      queryClient.setQueryData<ModelDownloadJob[]>(["model-downloads"], (current) => {
        if (!current) return updated ? [updated] : [];
        if (action === "cancel" || action === "remove") return current.filter((item) => item.job_id !== job.job_id);
        return current.map((item) => item.job_id === job.job_id && updated ? updated : item);
      });
      await queryClient.invalidateQueries({ queryKey: ["model-downloads"] });
      await queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
      recordDiagnosticEvent({
        level: "info",
        component: "frontend.model_downloads",
        event_name: "model_download.action_completed",
        fields: { job_id: job.job_id, action, elapsed_ms: Math.round(performance.now() - startedAt) },
      });
    } catch (cause) {
      const error = normalizeAppError(cause);
      setDownloadActionErrors((current) => ({ ...current, [job.job_id]: error.message }));
      recordDiagnosticEvent({
        level: "error",
        component: "frontend.model_downloads",
        event_name: "model_download.action_failed",
        fields: {
          job_id: job.job_id,
          action,
          elapsed_ms: Math.round(performance.now() - startedAt),
          error_code: error.code,
          retryable: error.retryable,
        },
      });
    } finally {
      setPendingDownloadActions((current) => {
        const next = { ...current };
        delete next[job.job_id];
        return next;
      });
    }
  };

  const chooseModels = async (directory: boolean) => {
    if (!isTauri()) {
      scanImport.mutate([directory ? "D:\\离线模型\\bge-small-zh" : "D:\\离线模型\\model.onnx"]);
      return;
    }
    const selected = await open(directory ? { directory: true, multiple: false, title: "选择模型目录" } : { multiple: true, title: "选择模型文件", filters: [{ name: "本地模型", extensions: ["gguf", "onnx"] }] });
    if (!selected) return;
    scanImport.mutate(Array.isArray(selected) ? selected : [selected]);
  };

  return (
    <div className="model-management">
      {(visibleDownloads.some(modelDownloadIsActive) || downloadSummary.attention_count > 0) && (
        <section className="model-download-section" aria-label="模型下载任务">
          <header>
            <div><h2>模型下载</h2></div>
          </header>
          <ModelDownloadList
            jobs={visibleDownloads}
            pending_actions={pendingDownloadActions}
            action_errors={downloadActionErrors}
            on_action={(job, action) => void runDownloadAction(job, action)}
          />
        </section>
      )}

      {step === "main" && (
        <section className="model-import-entry">
          <button type="button" className="primary-button" onClick={() => setStep("import")}><FolderOpenOutlined /> 导入本地模型</button>
          <p>支持 GGUF / ONNX 模型，导入后会列在已下载模型中。</p>
        </section>
      )}

      {step === "import" && <div className="import-panel">
        <button type="button" className="back-button model-import-back" aria-label="返回" onClick={() => setStep("main")}><ArrowLeftOutlined /></button>
        <FolderOpenOutlined /><h2>导入常见格式模型</h2>
        <p>支持生成与多模态模型 GGUF，向量、重排与语音识别模型 ONNX，以及 JSON、tokenizer.json 和 SentencePiece 配置。OCR 继续使用 Windows 本地运行时。</p>
        <div className="import-panel__actions"><button type="button" className="primary-button" disabled={scanImport.isPending} onClick={() => void chooseModels(false)}>选择模型文件</button><button type="button" disabled={scanImport.isPending} onClick={() => void chooseModels(true)}>选择模型目录</button></div>
        {scanImport.isError && <p role="alert" className="inline-error">{errorMessage(scanImport.error)}</p>}
        {candidates.length > 0 && <div className="import-candidates">
          {candidates.map((candidate) => <article key={candidate.candidate_id}><div><strong>{candidate.display_name}</strong><small>{candidate.format.toUpperCase()} · {(candidate.size_bytes / 1024 / 1024).toFixed(1)} MB · SHA-256 {candidate.sha256.slice(0, 12)}…</small>{candidate.warnings.map((warning) => <em key={warning}>{warning}</em>)}</div><label>用途<AppSelect ariaLabel={`${candidate.display_name}用途`} value={roles[candidate.candidate_id] ?? "generation"} onChange={(value) => setRoles((current) => ({ ...current, [candidate.candidate_id]: value as ModelRole }))} options={[{ value: "generation", label: "问答基础模型" }, { value: "embedding", label: "Embedding" }, { value: "vision", label: "多模态理解" }, { value: "reranker", label: "Rerank" }, { value: "ocr", label: "OCR 识别" }, { value: "asr", label: "语音识别" }]} /></label></article>)}
          {importModels.isError && <p role="alert" className="inline-error">{errorMessage(importModels.error)}</p>}
          <button type="button" className="primary-button" disabled={importModels.isPending} onClick={() => importModels.mutate()}>{importModels.isPending ? "正在校验并导入" : "确认导入到翻翻"}</button>
        </div>}
        <small>翻翻不会执行模型目录中的 Python、Shell 或远程自定义代码。</small>
      </div>}
    </div>
  );
}