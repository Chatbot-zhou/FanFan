import { ArrowLeftOutlined, FolderOpenOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useRef, useState } from "react";
import { Modal } from "antd";
import { bridge, type ImportCandidate, type ModelDownloadJob, type ModelRole } from "../../bridge";
import { recordDiagnosticEvent } from "../../bridge/observed-bridge";
import { confirmAction } from "../../components/AppConfirm";
import { AppSelect } from "../../components/AppSelect";
import { ModelDownloadList, type ModelDownloadAction } from "../model-downloads/ModelDownloadList";
import {
  modelDownloadIsActive,
  summarizeModelDownloads,
  visibleModelDownloadJobs,
} from "../model-downloads/model-downloads";
import { errorMessage, normalizeAppError } from "../../utils/app-error";

type SetupStep = "main" | "import";

const formatBytes = (value: number) => value >= 1024 ** 3
  ? `${(value / 1024 ** 3).toFixed(2)} GB`
  : `${(value / 1024 ** 2).toFixed(value < 1024 ** 2 ? 2 : 0)} MB`;

/** 本地模型管理面板：下载任务、按角色配置、模型选择池、导入与本地组件（嵌入设置页「本地模型」tab）。 */
export function ModelManagementPanel() {
  const queryClient = useQueryClient();
  const [step, setStep] = useState<SetupStep>("main");
  const [source] = useState<"huggingface" | "modelscope">("huggingface");
  const [selectedRole, setSelectedRole] = useState<ModelRole>("generation");
  const [selectedCardId, setSelectedCardId] = useState<string | null>(null);
  const poolRef = useRef<HTMLElement | null>(null);
  const selectRole = (role: ModelRole, scrollToPool: boolean) => {
    setSelectedRole(role);
    setSelectedCardId(null);
    // 只有点击卡片上的「点击配置」时才滚动到下方模型选择池；点卡片其他区域仅切换选中。
    if (scrollToPool) {
      poolRef.current?.scrollIntoView({ behavior: "smooth", block: "start" });
    }
  };
  const [candidates, setCandidates] = useState<ImportCandidate[]>([]);
  const [roles, setRoles] = useState<Record<string, ModelRole>>({});
  const [installTarget, setInstallTarget] = useState<NonNullable<typeof roleCatalog.data>[number] | null>(null);
  const [installSource, setInstallSource] = useState<"huggingface" | "modelscope">("huggingface");
  const [pendingDownloadActions, setPendingDownloadActions] = useState<Record<string, ModelDownloadAction>>({});
  const [downloadActionErrors, setDownloadActionErrors] = useState<Record<string, string>>({});
  // 当前正在加载/自检的组件 id：只让被点击的那个模型按钮进入忙碌状态，其他模型保持可用。
  const [activatingId, setActivatingId] = useState<string | null>(null);
  const artifacts = useQuery({ queryKey: ["model-artifacts"], queryFn: () => bridge.model_artifact_list() });
  const roleConfigs = useQuery({ queryKey: ["model-role-configs"], queryFn: () => bridge.model_role_config_list() });
  const roleCatalog = useQuery({ queryKey: ["model-role-catalog"], queryFn: () => bridge.model_role_catalog_list() });
  const downloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
  });
  const visibleDownloads = useMemo(() => visibleModelDownloadJobs(downloads.data ?? []), [downloads.data]);
  const downloadSummary = useMemo(() => summarizeModelDownloads(visibleDownloads), [visibleDownloads]);

  const refreshModels = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["model-downloads"] }),
      queryClient.invalidateQueries({ queryKey: ["model-runtime"] }),
      queryClient.invalidateQueries({ queryKey: ["model-role-configs"] }),
      artifacts.refetch(),
    ]);
  };
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
      await artifacts.refetch();
    },
  });
  const activate = useMutation({ mutationFn: (artifactId: string) => bridge.model_artifact_activate(artifactId), onSuccess: refreshModels });
  const startDownload = useMutation({
    mutationFn: ({ editionId, downloadSource }: { editionId: string; downloadSource: "huggingface" | "modelscope" }) => bridge.model_download_start(editionId, downloadSource, true),
    onSuccess: async () => { await refreshModels(); },
  });
  const disable = useMutation({ mutationFn: (role: ModelRole) => bridge.model_role_disable(role), onSuccess: refreshModels });

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
      else if (action === "switch_source") updated = await bridge.model_download_switch_source(job.job_id, job.source === "modelscope" ? "huggingface" : "modelscope");
      else if (action === "remove") await bridge.model_download_remove(job.job_id);
      else updated = await bridge.model_download_retry(job.job_id);
      queryClient.setQueryData<ModelDownloadJob[]>(["model-downloads"], (current) => {
        if (!current) return updated ? [updated] : [];
        if (action === "cancel" || action === "remove") return current.filter((item) => item.job_id !== job.job_id);
        return current.map((item) => item.job_id === job.job_id && updated ? updated : item);
      });
      await refreshModels();
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

  const activateModel = async (artifactId: string, role: ModelRole) => {
    if (role === "embedding") {
      const current = roleConfigs.data?.find((config) => config.role === "embedding")?.active_artifact_id;
      if (current && current !== artifactId) {
        if (!await confirmAction({ actionKey: "embedding_replace_prepare", title: "准备更换 Embedding？", description: "翻翻会为全部已索引内容建立一套新向量索引，旧索引继续提供服务，源文件不会改变。", confirmLabel: "继续检查" })) return;
        if (!await confirmAction({ actionKey: "embedding_replace_confirm", title: "再次确认更换 Embedding", description: "重建可能持续较长时间并占用额外磁盘；只有新索引完整校验通过后才会原子切换，失败会保留旧索引。", confirmLabel: "确认更换", danger: true, confirmPhrase: "REBUILD_EMBEDDING_INDEX" })) return;
      } else if (!await confirmAction({ actionKey: "embedding_activate", title: "启用这个 Embedding？", description: "翻翻会在后台建立语义索引；期间文件名、全文搜索和预览继续可用。", confirmLabel: "启用并建立索引" })) return;
    }
    setActivatingId(artifactId);
    activate.mutate(artifactId, {
      onSettled: () => setActivatingId(null),
    });
  };

  const openInstallDialog = (entry: NonNullable<typeof roleCatalog.data>[number]) => {
    if (!entry.install_edition_id) {
      setStep("import");
      return;
    }
    setInstallSource(entry.supported_sources.includes(source) ? source : (entry.supported_sources.find((item) => item === "huggingface" || item === "modelscope") ?? "huggingface"));
    setInstallTarget(entry);
  };
  const confirmInstall = () => {
    if (!installTarget?.install_edition_id) return;
    startDownload.mutate({ editionId: installTarget.install_edition_id, downloadSource: installSource });
    setInstallTarget(null);
  };

  const disableRole = async (role: ModelRole) => {
    if (!await confirmAction({ actionKey: `model_role_disable_${role}`, title: "停用这个模型角色？", description: "模型文件仍保留在本机，可随时重新启用；依赖该角色的能力会暂时降级。", confirmLabel: "停用角色" })) return;
    disable.mutate(role);
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

      {step === "main" && <>
        <section className="model-roles" aria-label="模型角色配置">
          <h2>按角色配置</h2>
          <p>生成、Embedding、多模态与可选 Rerank 相互独立；更换 Embedding 会单独触发新索引代际。</p>
          <div>{roleConfigs.data?.filter((config) => config.role !== "router").map((config) => {
            const active = artifacts.data?.find((artifact) => artifact.artifact_id === config.active_artifact_id);
            const names: Record<typeof config.role, string> = { generation: "问答基础模型", embedding: "Embedding", vision: "多模态模型", reranker: "Rerank", ocr: "OCR", tts: "语音合成", asr: "语音识别", router: "意图路由" };
            return <article key={config.role} className={selectedRole === config.role ? "selected" : ""}>
              <button type="button" onClick={() => selectRole(config.role, false)}><strong>{names[config.role]}</strong>{active && <span>{active.model_id}</span>}<small>{config.required_for} · {config.load_policy === "background_index" ? "后台索引" : config.load_policy === "serial_on_demand" ? "串行按需加载" : "按需加载"}</small></button>
              {!active && <button type="button" className="role-card__configure" onClick={() => selectRole(config.role, true)}>点击配置</button>}
              {active && config.role !== "ocr" && <button type="button" className="text-button" onClick={() => void disableRole(config.role)}>停用</button>}
            </article>;
          })}</div>
        </section>
        <section ref={poolRef} className="role-model-pool" aria-label="已验证模型选择池">
          <header><div><h2>{selectedRole === "generation" ? "问答基础模型" : selectedRole === "embedding" ? "Embedding 模型" : selectedRole === "vision" ? "多模态模型" : selectedRole === "ocr" ? "OCR 模型" : selectedRole === "tts" ? "语音合成模型" : selectedRole === "asr" ? "语音识别模型" : "Rerank 模型"}</h2></div><button type="button" onClick={() => setStep("import")}>导入本地模型</button></header>
          <div className="role-model-grid">{(() => {
            // 每个模型家族一张卡片（family 由后端 catalog 定义，如 “Qwen3.5”/“Gemma 4”/“BGE”），
            // 家族内所有尺寸/量化版本聚合成版本按钮：
            // 尺寸取 name 中“ · ”前段去掉家族名的部分，量化取 model_id 最后一个 “-” 后的段。
            const entries = roleCatalog.data?.filter((item) => item.role === selectedRole) ?? [];
            const series = new Map<string, typeof entries>();
            for (const item of entries) {
              const family = item.family || item.name;
              const group = series.get(family);
              if (group) { group.push(item); } else { series.set(family, [item]); }
            }
            return [...series.entries()].map(([familyName, versions]) => {
              const selected = versions.find((item) => item.catalog_id === selectedCardId) ?? versions[0]!;
              const isSelected = versions.some((item) => item.catalog_id === selectedCardId);
              const sizeOf = (item: typeof entries[number]) => (item.name.split(" · ")[0] ?? item.name).replace(familyName, "").trim().replace(/^[-·\s]+/, "");
              const quantOf = (item: typeof entries[number]) => item.model_id.split("-").at(-1) ?? "";
              return <article key={familyName} className={isSelected ? "selected" : ""} onClick={() => { if (!isSelected) setSelectedCardId(versions[0]!.catalog_id); }}>
                <div className="role-model-card__heading"><strong>{familyName}</strong></div>
                {versions.some((item) => item.recommended) && <em className="role-model-card__badge">推荐</em>}
                <p>{selected.description}</p>
                {versions.length > 1 && <div className="role-model-card__versions">{versions.map((item) => <button key={item.catalog_id} type="button" className={item.catalog_id === selected.catalog_id ? "selected" : ""} onClick={(event) => { event.stopPropagation(); setSelectedCardId(item.catalog_id); }}>{[sizeOf(item), quantOf(item)].filter(Boolean).join(" · ")}{item.recommended && <em>推荐</em>}</button>)}</div>}
                <dl><div><dt>下载</dt><dd>{selected.download_size_bytes ? formatBytes(selected.download_size_bytes) : "本地导入"}</dd></div><div><dt>预计内存</dt><dd>{selected.estimated_memory_gb} GB</dd></div><div><dt>预计显存</dt><dd>{selected.estimated_vram_gb ? `${selected.estimated_vram_gb} GB` : "不依赖"}</dd></div><div><dt>CPU速度</dt><dd>{selected.cpu_speed}</dd></div></dl>
                <ul>{selected.strengths.map((value) => <li key={value}>{value}</li>)}</ul>
                <small>{selected.limitations.join("；")} · {selected.license_name}</small>
                <p className="role-model-card__fit">{selected.device_guidance}</p>
                <button type="button" className={isSelected ? "primary-button" : ""} disabled={startDownload.isPending} onClick={(event) => { event.stopPropagation(); openInstallDialog(selected); }}>{selected.install_edition_id ? "联网安装并自检" : "选择本地文件"}</button>
              </article>;
            });
          })()}</div>
          {roleCatalog.isError && <p role="alert" className="inline-error">{errorMessage(roleCatalog.error)}</p>}
        </section>
      </>}

      {step === "import" && <div className="import-panel">
        <button type="button" className="back-button model-import-back" aria-label="返回模型选择" onClick={() => setStep("main")}><ArrowLeftOutlined /></button>
        <FolderOpenOutlined /><h2>导入常见格式模型</h2>
        <p>支持生成与多模态模型 GGUF，向量、重排、语音合成与语音识别模型 ONNX，以及 JSON、tokenizer.json 和 SentencePiece 配置。OCR 继续使用 Windows 本地运行时。</p>
        <div className="import-panel__actions"><button type="button" className="primary-button" disabled={scanImport.isPending} onClick={() => void chooseModels(false)}>选择模型文件</button><button type="button" disabled={scanImport.isPending} onClick={() => void chooseModels(true)}>选择模型目录</button></div>
        {scanImport.isError && <p role="alert" className="inline-error">{errorMessage(scanImport.error)}</p>}
        {candidates.length > 0 && <div className="import-candidates">
          {candidates.map((candidate) => <article key={candidate.candidate_id}><div><strong>{candidate.display_name}</strong><small>{candidate.format.toUpperCase()} · {(candidate.size_bytes / 1024 / 1024).toFixed(1)} MB · SHA-256 {candidate.sha256.slice(0, 12)}…</small>{candidate.warnings.map((warning) => <em key={warning}>{warning}</em>)}</div><label>用途<AppSelect ariaLabel={`${candidate.display_name}用途`} value={roles[candidate.candidate_id] ?? "generation"} onChange={(value) => setRoles((current) => ({ ...current, [candidate.candidate_id]: value as ModelRole }))} options={[{ value: "generation", label: "问答基础模型" }, { value: "embedding", label: "Embedding" }, { value: "vision", label: "多模态理解" }, { value: "reranker", label: "Rerank" }, { value: "ocr", label: "OCR 识别" }, { value: "tts", label: "语音合成" }, { value: "asr", label: "语音识别" }]} /></label></article>)}
          {importModels.isError && <p role="alert" className="inline-error">{errorMessage(importModels.error)}</p>}
          <button type="button" className="primary-button" disabled={importModels.isPending} onClick={() => importModels.mutate()}>{importModels.isPending ? "正在校验并导入" : "确认导入到翻翻"}</button>
        </div>}
        <small>翻翻不会执行模型目录中的 Python、Shell 或远程自定义代码。</small>
      </div>}

      {(artifacts.data?.length ?? 0) > 0 && <section className="managed-models">
        <h2>本地模型组件</h2>
        {/* 正在使用的模型排前面，未使用的排后面；启用/停用后随数据刷新自动重排。 */}
        {[...(artifacts.data ?? [])].sort((left, right) => {
          const leftInUse = roleConfigs.data?.some((config) => config.active_artifact_id === left.artifact_id) ?? false;
          const rightInUse = roleConfigs.data?.some((config) => config.active_artifact_id === right.artifact_id) ?? false;
          return Number(rightInUse) - Number(leftInUse);
        }).map((artifact) => {
          const activatable = ((artifact.role === "embedding" || artifact.role === "reranker" || artifact.role === "tts" || artifact.role === "asr") && artifact.format === "onnx") || ((artifact.role === "generation" || artifact.role === "vision") && artifact.format === "gguf");
          const roleName = artifact.role === "generation" ? "问答基础模型" : artifact.role === "embedding" ? "Embedding" : artifact.role === "vision" ? "多模态模型" : artifact.role === "reranker" ? "Rerank" : artifact.role === "tts" ? "语音合成" : artifact.role === "asr" ? "语音识别" : "OCR";
          const inUse = roleConfigs.data?.some((config) => config.active_artifact_id === artifact.artifact_id) ?? false;
          return <article key={artifact.artifact_id}><span className={`model-status-dot${inUse ? " model-status-dot--active" : ""}`} title={inUse ? "正在使用" : "未使用"} /><div><strong>{artifact.model_id}</strong><small>{roleName} · {artifact.format.toUpperCase()} · {(artifact.size_bytes / 1024 / 1024).toFixed(1)} MB{artifact.embedding_dimension ? ` · ${artifact.embedding_dimension}维` : ""}</small></div>{activatable ? <button type="button" disabled={activatingId === artifact.artifact_id} onClick={() => void activateModel(artifact.artifact_id, artifact.role)}>{activatingId === artifact.artifact_id ? "正在加载与自检" : artifact.role === "generation" ? "加载并启用" : artifact.role === "vision" ? "自检并启用图片理解" : artifact.role === "reranker" ? "自检并启用重排" : artifact.role === "tts" || artifact.role === "asr" ? "自检并启用" : artifact.embedding_dimension ? "重建并切换索引" : "自检并建立索引"}</button> : <span>{artifact.role === "ocr" ? "使用 Windows OCR" : "当前格式不受支持"}</span>}</article>;
        })}
        {activate.isError && <p role="alert" className="inline-error">{errorMessage(activate.error)}</p>}
      </section>}

      <Modal
        open={installTarget !== null}
        title={`安装 ${installTarget?.name ?? "模型"}？`}
        className="app-confirm"
        centered
        okText="创建下载任务"
        cancelText="取消"
        confirmLoading={startDownload.isPending}
        onOk={confirmInstall}
        onCancel={() => setInstallTarget(null)}
      >
        <div className="app-confirm__content">
          <p>{installTarget?.device_guidance}。下载后会先校验文件并执行最小自检，通过后才切换当前 {installTarget?.role} 角色。</p>
          <p className="inline-notice">每个来源使用各自锁定的仓库、修订、大小和 SHA-256；断点不会跨来源复用。Windows OCR 不重复下载模型。</p>
          <div className="source-options">
            <label><input type="radio" name="install-source" checked={installSource === "huggingface"} disabled={!installTarget?.supported_sources.includes("huggingface")} onChange={() => setInstallSource("huggingface")} /> Hugging Face <em>独立固定修订</em></label>
            <label><input type="radio" name="install-source" checked={installSource === "modelscope"} disabled={!installTarget?.supported_sources.includes("modelscope")} onChange={() => setInstallSource("modelscope")} /> 魔搭社区 <em>独立固定修订</em></label>
          </div>
        </div>
      </Modal>
    </div>
  );
}
