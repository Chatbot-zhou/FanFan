import { ArrowLeftOutlined, FolderOpenOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Modal } from "antd";
import { bridge, type ImportCandidate, type ModelDownloadJob, type ModelRole } from "../bridge";
import { confirmAction } from "../components/AppConfirm";
import { AppSelect } from "../components/AppSelect";
import { useAppStore } from "../state/app-store";
import { errorMessage } from "../utils/app-error";

type SetupStep = "main" | "import";

const formatBytes = (value: number) => value >= 1024 ** 3
  ? `${(value / 1024 ** 3).toFixed(2)} GB`
  : `${(value / 1024 ** 2).toFixed(value < 1024 ** 2 ? 2 : 0)} MB`;

const phaseLabels: Record<ModelDownloadJob["phase"], string> = {
  queued: "等待下载",
  downloading: "正在下载",
  verifying: "正在校验",
  installing: "正在安装",
  self_testing: "正在自检",
  activating: "正在原子启用",
  indexing: "正在构建新语义索引",
  paused: "已暂停，可继续",
  completed: "完整 RAG 已就绪",
  failed: "下载失败",
  cancelled: "已取消",
};

export function ModelSetupPage() {
  const queryClient = useQueryClient();
  const goBack = useAppStore((state) => state.go_back);
  const [step, setStep] = useState<SetupStep>("main");
  const [source] = useState<"huggingface" | "modelscope">("huggingface");
  const [selectedRole, setSelectedRole] = useState<ModelRole>("generation");
  const [selectedCardId, setSelectedCardId] = useState<string | null>(null);
  const [candidates, setCandidates] = useState<ImportCandidate[]>([]);
  const [roles, setRoles] = useState<Record<string, ModelRole>>({});
  const [installTarget, setInstallTarget] = useState<NonNullable<typeof roleCatalog.data>[number] | null>(null);
  const [installSource, setInstallSource] = useState<"huggingface" | "modelscope">("huggingface");
  const artifacts = useQuery({ queryKey: ["model-artifacts"], queryFn: () => bridge.model_artifact_list() });
  const roleConfigs = useQuery({ queryKey: ["model-role-configs"], queryFn: () => bridge.model_role_config_list() });
  const roleCatalog = useQuery({ queryKey: ["model-role-catalog"], queryFn: () => bridge.model_role_catalog_list() });
  const downloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
  });
  const latestJob = downloads.data?.[0] ?? null;

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
  const pauseDownload = useMutation({ mutationFn: (jobId: string) => bridge.model_download_pause(jobId), onSuccess: refreshModels });
  const cancelDownload = useMutation({ mutationFn: (jobId: string) => bridge.model_download_cancel(jobId), onSuccess: refreshModels });
  const retryDownload = useMutation({ mutationFn: ({ jobId, nextSource }: { jobId: string; nextSource?: "huggingface" | "modelscope" }) => bridge.model_download_retry(jobId, nextSource), onSuccess: refreshModels });
  const disable = useMutation({ mutationFn: (role: ModelRole) => bridge.model_role_disable(role), onSuccess: refreshModels });

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
        if (!await confirmAction({ actionKey: "embedding_replace_prepare", title: "准备更换 Embedding？", description: "拾忆会为全部已索引内容建立一套新向量索引，旧索引继续提供服务，源文件不会改变。", confirmLabel: "继续检查" })) return;
        if (!await confirmAction({ actionKey: "embedding_replace_confirm", title: "再次确认更换 Embedding", description: "重建可能持续较长时间并占用额外磁盘；只有新索引完整校验通过后才会原子切换，失败会保留旧索引。", confirmLabel: "确认更换", danger: true, confirmPhrase: "REBUILD_EMBEDDING_INDEX" })) return;
      } else if (!await confirmAction({ actionKey: "embedding_activate", title: "启用这个 Embedding？", description: "拾忆会在后台建立语义索引；期间文件名、全文搜索和预览继续可用。", confirmLabel: "启用并建立索引" })) return;
    }
    activate.mutate(artifactId);
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
    <section className="page page--model">
      <header className="page-heading model-heading">
        <button type="button" className="back-button" aria-label="返回上一页" onClick={() => step === "main" ? goBack() : setStep("main")}><ArrowLeftOutlined /></button>
        <div><h1>本地模型配置</h1><p>模型包完整安装并自检后才会启用；推理、索引和资料始终留在本地。</p></div>
      </header>

      {latestJob && <section className={`model-job model-job--${latestJob.status}`} aria-live="polite">
        <div><strong>{latestJob.edition_name} · {phaseLabels[latestJob.phase]}</strong><small>{latestJob.current_file ? `当前：${latestJob.current_file}` : `来源：${latestJob.source === "modelscope" ? "魔搭社区" : "Hugging Face"}`}</small></div>
        <progress value={latestJob.progress} max={1} />
        <span>{Math.round(latestJob.progress * 100)}% · {formatBytes(latestJob.downloaded_bytes)} / {formatBytes(latestJob.total_bytes)}{latestJob.bytes_per_second > 0 ? ` · ${formatBytes(latestJob.bytes_per_second)}/s` : ""}{latestJob.eta_seconds != null ? ` · 约 ${Math.ceil(latestJob.eta_seconds / 60)} 分钟` : ""}</span>
        {latestJob.error && <p role="alert" className="inline-error">{latestJob.error.message}</p>}
        <div className="model-job__actions">
          {(latestJob.status === "queued" || latestJob.status === "running") && <><button type="button" onClick={() => pauseDownload.mutate(latestJob.job_id)}>暂停</button><button type="button" onClick={() => cancelDownload.mutate(latestJob.job_id)}>取消</button></>}
          {(latestJob.status === "paused" || latestJob.status === "failed" || latestJob.status === "cancelled") && <><button type="button" className="primary-button" onClick={() => retryDownload.mutate({ jobId: latestJob.job_id })}>继续/重试</button><button type="button" onClick={() => retryDownload.mutate({ jobId: latestJob.job_id, nextSource: latestJob.source === "modelscope" ? "huggingface" : "modelscope" })}>切换来源</button></>}
        </div>
      </section>}

      {step === "main" && <>
        <section className="model-roles" aria-label="模型角色配置">
          <h2>按角色配置</h2>
          <p>生成、Embedding、多模态与可选 Rerank 相互独立；更换 Embedding 会单独触发新索引代际。</p>
          <div>{roleConfigs.data?.map((config) => {
            const active = artifacts.data?.find((artifact) => artifact.artifact_id === config.active_artifact_id);
            const names: Record<typeof config.role, string> = { generation: "问答基础模型", embedding: "Embedding", vision: "多模态模型", reranker: "Rerank（可选）", ocr: "OCR", tts: "语音合成", asr: "语音识别" };
            return <article key={config.role} className={selectedRole === config.role ? "selected" : ""}>
              <button type="button" onClick={() => { setSelectedRole(config.role); setSelectedCardId(null); }}><strong>{names[config.role]}</strong><span>{active?.model_id ?? "点击配置"}</span><small>{config.required_for} · {config.load_policy === "background_index" ? "后台索引" : config.load_policy === "serial_on_demand" ? "串行按需加载" : "按需加载"}</small></button>
              {active && config.role !== "ocr" && <button type="button" className="text-button" onClick={() => void disableRole(config.role)}>停用</button>}
            </article>;
          })}</div>
          {disable.isError && <p role="alert" className="inline-error">{errorMessage(disable.error)}</p>}
        </section>
        <section className="role-model-pool" aria-label="已验证模型选择池">
          <header><div><h2>{selectedRole === "generation" ? "问答基础模型" : selectedRole === "embedding" ? "Embedding 模型" : selectedRole === "vision" ? "多模态模型" : selectedRole === "ocr" ? "OCR 模型" : selectedRole === "tts" ? "语音合成模型" : selectedRole === "asr" ? "语音识别模型" : "Rerank 模型"}</h2><p>只展示固定版本的验证模型；未开放远程安装的条目仍可通过本地导入配置。</p></div><button type="button" onClick={() => setStep("import")}>导入本地模型</button></header>
          <div className="role-model-grid">{roleCatalog.data?.filter((item) => item.role === selectedRole).map((item) => <article key={item.catalog_id} className={selectedCardId === item.catalog_id ? "selected" : ""} onClick={() => setSelectedCardId(item.catalog_id)}>
            <div className="role-model-card__heading"><strong>{item.name}</strong></div>
            {item.recommended && <em className="role-model-card__badge">推荐</em>}
            <p>{item.description}</p>
            <dl><div><dt>下载</dt><dd>{item.download_size_bytes ? formatBytes(item.download_size_bytes) : "本地导入"}</dd></div><div><dt>预计内存</dt><dd>{item.estimated_memory_gb} GB</dd></div><div><dt>预计显存</dt><dd>{item.estimated_vram_gb ? `${item.estimated_vram_gb} GB` : "不依赖"}</dd></div><div><dt>CPU速度</dt><dd>{item.cpu_speed}</dd></div></dl>
            <ul>{item.strengths.map((value) => <li key={value}>{value}</li>)}</ul>
            <small>{item.limitations.join("；")} · {item.license_name}</small>
            <p className="role-model-card__fit">{item.device_guidance}</p>
            <button type="button" className={selectedCardId === item.catalog_id ? "primary-button" : ""} disabled={startDownload.isPending} onClick={() => openInstallDialog(item)}>{item.install_edition_id ? "联网安装并自检" : "选择本地文件"}</button>
          </article>)}</div>
          {roleCatalog.isError && <p role="alert" className="inline-error">{errorMessage(roleCatalog.error)}</p>}
        </section>
        <div className="current-model"><h2>已管理的模型</h2><div><span>{artifacts.data?.length ? `${artifacts.data.length} 个本地组件` : "尚未导入任何模型"}</span><small>{artifacts.data?.length ? artifacts.data.map((item) => `${item.model_id}（${item.role}）`).join("、") : "当前只提供文件名和全文搜索；完整 RAG 需要生成与 Embedding 组件。"}</small></div></div>
      </>}

      {step === "import" && <div className="import-panel">
        <FolderOpenOutlined /><h2>导入常见格式模型</h2>
        <p>支持生成与多模态模型 GGUF，向量、重排、语音合成与语音识别模型 ONNX，以及 JSON、tokenizer.json 和 SentencePiece 配置。OCR 继续使用 Windows 本地运行时。</p>
        <div className="import-panel__actions"><button type="button" className="primary-button" disabled={scanImport.isPending} onClick={() => void chooseModels(false)}>选择模型文件</button><button type="button" disabled={scanImport.isPending} onClick={() => void chooseModels(true)}>选择模型目录</button></div>
        {scanImport.isError && <p role="alert" className="inline-error">{errorMessage(scanImport.error)}</p>}
        {candidates.length > 0 && <div className="import-candidates">
          {candidates.map((candidate) => <article key={candidate.candidate_id}><div><strong>{candidate.display_name}</strong><small>{candidate.format.toUpperCase()} · {(candidate.size_bytes / 1024 / 1024).toFixed(1)} MB · SHA-256 {candidate.sha256.slice(0, 12)}…</small>{candidate.warnings.map((warning) => <em key={warning}>{warning}</em>)}</div><label>用途<AppSelect ariaLabel={`${candidate.display_name}用途`} value={roles[candidate.candidate_id] ?? "generation"} onChange={(value) => setRoles((current) => ({ ...current, [candidate.candidate_id]: value as ModelRole }))} options={[{ value: "generation", label: "问答基础模型" }, { value: "embedding", label: "Embedding" }, { value: "vision", label: "多模态理解" }, { value: "reranker", label: "Rerank" }, { value: "ocr", label: "OCR 识别" }, { value: "tts", label: "语音合成" }, { value: "asr", label: "语音识别" }]} /></label></article>)}
          {importModels.isError && <p role="alert" className="inline-error">{errorMessage(importModels.error)}</p>}
          <button type="button" className="primary-button" disabled={importModels.isPending} onClick={() => importModels.mutate()}>{importModels.isPending ? "正在校验并导入" : "确认导入到拾忆"}</button>
        </div>}
        <small>拾忆不会执行模型目录中的 Python、Shell 或远程自定义代码。</small>
      </div>}

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

      {(artifacts.data?.length ?? 0) > 0 && <section className="managed-models">
        <h2>本地模型组件</h2>
        {artifacts.data?.map((artifact) => {
          const activatable = ((artifact.role === "embedding" || artifact.role === "reranker" || artifact.role === "tts" || artifact.role === "asr") && artifact.format === "onnx") || ((artifact.role === "generation" || artifact.role === "vision") && artifact.format === "gguf");
          const roleName = artifact.role === "generation" ? "问答基础模型" : artifact.role === "embedding" ? "Embedding" : artifact.role === "vision" ? "多模态模型" : artifact.role === "reranker" ? "Rerank" : artifact.role === "tts" ? "语音合成" : artifact.role === "asr" ? "语音识别" : "OCR";
          return <article key={artifact.artifact_id}><div><strong>{artifact.model_id}</strong><small>{roleName} · {artifact.format.toUpperCase()} · {(artifact.size_bytes / 1024 / 1024).toFixed(1)} MB{artifact.embedding_dimension ? ` · ${artifact.embedding_dimension}维` : ""}</small></div>{activatable ? <button type="button" disabled={activate.isPending} onClick={() => void activateModel(artifact.artifact_id, artifact.role)}>{activate.isPending ? "正在加载与自检" : artifact.role === "generation" ? "加载并启用" : artifact.role === "vision" ? "自检并启用图片理解" : artifact.role === "reranker" ? "自检并启用重排" : artifact.role === "tts" || artifact.role === "asr" ? "自检并启用" : artifact.embedding_dimension ? "重建并切换索引" : "自检并建立索引"}</button> : <span>{artifact.role === "ocr" ? "使用 Windows OCR" : "当前格式不受支持"}</span>}</article>;
        })}
        {activate.data?.embedding_migration && <p role="status" className="inline-notice">{activate.data.message}</p>}
        {activate.isError && <p role="alert" className="inline-error">{errorMessage(activate.error)}</p>}
      </section>}
    </section>
  );
}
