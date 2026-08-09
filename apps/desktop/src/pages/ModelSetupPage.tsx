import { ArrowLeftOutlined, CloudDownloadOutlined, FolderOpenOutlined, InfoCircleOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type ImportCandidate, type ModelDownloadJob, type ModelEdition, type ModelRole } from "../bridge";
import { useAppStore } from "../state/app-store";

type SetupStep = "choice" | "download" | "confirm" | "import";

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
  const navigate = useAppStore((state) => state.navigate);
  const [step, setStep] = useState<SetupStep>("choice");
  const [edition, setEdition] = useState<ModelEdition["edition_id"]>("light");
  const [source, setSource] = useState<"huggingface" | "modelscope">("huggingface");
  const [candidates, setCandidates] = useState<ImportCandidate[]>([]);
  const [roles, setRoles] = useState<Record<string, ModelRole>>({});
  const [showDeviceDetails, setShowDeviceDetails] = useState(false);
  const environment = useQuery({ queryKey: ["environment"], queryFn: async () => (await bridge.environment_get_latest()) ?? bridge.environment_detect() });
  const artifacts = useQuery({ queryKey: ["model-artifacts"], queryFn: () => bridge.model_artifact_list() });
  const roleConfigs = useQuery({ queryKey: ["model-role-configs"], queryFn: () => bridge.model_role_config_list() });
  const catalog = useQuery({ queryKey: ["model-catalog"], queryFn: () => bridge.model_catalog_list() });
  const downloads = useQuery({
    queryKey: ["model-downloads"],
    queryFn: () => bridge.model_download_list(),
    refetchInterval: (query) => query.state.data?.some((job) => job.status === "queued" || job.status === "running") ? 500 : false,
  });
  const latestJob = downloads.data?.[0] ?? null;
  const selectedEdition = catalog.data?.find((item) => item.edition_id === edition);

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
    mutationFn: () => bridge.model_download_start(edition, source, true),
    onSuccess: async () => { await refreshModels(); },
  });
  const pauseDownload = useMutation({ mutationFn: (jobId: string) => bridge.model_download_pause(jobId), onSuccess: refreshModels });
  const cancelDownload = useMutation({ mutationFn: (jobId: string) => bridge.model_download_cancel(jobId), onSuccess: refreshModels });
  const retryDownload = useMutation({ mutationFn: ({ jobId, nextSource }: { jobId: string; nextSource?: "huggingface" | "modelscope" }) => bridge.model_download_retry(jobId, nextSource), onSuccess: refreshModels });

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
    <section className="page page--model">
      <header className="page-heading model-heading">
        <button type="button" className="back-button" onClick={() => step === "choice" ? navigate("home") : setStep("choice")}><ArrowLeftOutlined /></button>
        <div><h1>本地模型配置</h1><p>模型包完整安装并自检后才会启用；推理、索引和资料始终留在本地。</p></div>
      </header>
      <div className="device-summary">
        <div><small>内存</small><strong>{environment.data?.memory_total_gb ?? "检测中"} GB</strong></div>
        <div><small>可用空间</small><strong>{environment.data?.disk_available_gb ?? "检测中"} GB</strong></div>
        <div><small>推荐组合</small><strong>{environment.data?.recommended_edition === "standard" ? "增强问答组合" : "8GB 省内存组合"}</strong></div>
        <button type="button" aria-expanded={showDeviceDetails} onClick={() => setShowDeviceDetails((value) => !value)}><InfoCircleOutlined /> {showDeviceDetails ? "收起详情" : "查看详情"}</button>
      </div>
      {showDeviceDetails && <div className="inline-notice" role="status">当前使用 {environment.data?.runtime_backend === "gpu" ? `GPU（${environment.data.gpu_name ?? "名称未知"}，${environment.data.gpu_memory_gb ?? "显存未知"} GB）` : "CPU 回退"}；推荐依据为可用内存与磁盘空间。{environment.data?.warnings.length ? ` 注意：${environment.data.warnings.join("；")}` : "未检测到资源警告。"}</div>}

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

      {step === "choice" && <>
        <h2 className="section-title">选择获取方式</h2>
        <div className="model-methods">
          <button type="button" onClick={() => setStep("import")}><span><FolderOpenOutlined /></span><strong>从本地导入模型</strong><small>选择 GGUF、ONNX 及常见配套配置</small></button>
          <button type="button" onClick={() => setStep("download")}><span><CloudDownloadOutlined /></span><strong>联网安装推荐组合</strong><small>先按设备建议安装，之后可逐角色替换</small></button>
        </div>
        <section className="model-roles" aria-label="模型角色配置">
          <h2>按角色配置</h2>
          <p>生成、Embedding、多模态与可选 Rerank 相互独立；更换 Embedding 会单独触发新索引代际。</p>
          <div>{roleConfigs.data?.map((config) => {
            const active = artifacts.data?.find((artifact) => artifact.artifact_id === config.active_artifact_id);
            const names: Record<typeof config.role, string> = { generation: "问答基础模型", embedding: "Embedding", vision: "多模态模型", reranker: "Rerank（可选）" };
            return <article key={config.role}><strong>{names[config.role]}</strong><span>{active?.model_id ?? "未配置"}</span><small>{config.required_for} · {config.load_policy === "background_index" ? "后台索引" : config.load_policy === "serial_on_demand" ? "串行按需加载" : "按需加载"}</small></article>;
          })}</div>
        </section>
        <div className="current-model"><h2>已管理的模型</h2><div><span>{artifacts.data?.length ? `${artifacts.data.length} 个本地组件` : "尚未导入任何模型"}</span><small>{artifacts.data?.length ? artifacts.data.map((item) => `${item.model_id}（${item.role}）`).join("、") : "当前只提供文件名和全文搜索；完整 RAG 需要生成与 Embedding 组件。"}</small></div></div>
        <button className="text-button model-skip" type="button" onClick={() => navigate("home")}>暂不配置</button>
      </>}

      {step === "import" && <div className="import-panel">
        <FolderOpenOutlined /><h2>导入常见格式模型</h2>
        <p>支持生成与多模态模型 GGUF，向量与重排模型 ONNX，以及 JSON、tokenizer.json 和 SentencePiece 配置。OCR 继续使用 Windows 本地运行时。</p>
        <div className="import-panel__actions"><button type="button" className="primary-button" disabled={scanImport.isPending} onClick={() => void chooseModels(false)}>选择模型文件</button><button type="button" disabled={scanImport.isPending} onClick={() => void chooseModels(true)}>选择模型目录</button></div>
        {scanImport.isError && <p role="alert" className="inline-error">{scanImport.error instanceof Error ? scanImport.error.message : String(scanImport.error)}</p>}
        {candidates.length > 0 && <div className="import-candidates">
          {candidates.map((candidate) => <article key={candidate.candidate_id}><div><strong>{candidate.display_name}</strong><small>{candidate.format.toUpperCase()} · {(candidate.size_bytes / 1024 / 1024).toFixed(1)} MB · SHA-256 {candidate.sha256.slice(0, 12)}…</small>{candidate.warnings.map((warning) => <em key={warning}>{warning}</em>)}</div><label>用途<select value={roles[candidate.candidate_id]} onChange={(event) => setRoles((current) => ({ ...current, [candidate.candidate_id]: event.target.value as ModelRole }))}><option value="generation">问答基础模型</option><option value="embedding">Embedding</option><option value="vision">多模态理解</option><option value="reranker">Rerank</option></select></label></article>)}
          {importModels.isError && <p role="alert" className="inline-error">{importModels.error instanceof Error ? importModels.error.message : String(importModels.error)}</p>}
          <button type="button" className="primary-button" disabled={importModels.isPending} onClick={() => importModels.mutate()}>{importModels.isPending ? "正在校验并导入" : "确认导入到拾忆"}</button>
        </div>}
        <small>拾忆不会执行模型目录中的 Python、Shell 或远程自定义代码。</small>
      </div>}

      {step === "download" && <div className="download-config">
        <h2 className="section-title">选择设备推荐组合</h2>
        <p className="inline-notice">组合只负责首次安装，不会把角色永久绑定；安装完成后可在下方逐个替换和启用。</p>
        <div className="edition-grid">
          {catalog.isPending && <p>正在读取已锁定的模型清单…</p>}
          {catalog.isError && <p role="alert" className="inline-error">{catalog.error instanceof Error ? catalog.error.message : String(catalog.error)}</p>}
          {catalog.data?.map((item) => <button key={item.edition_id} type="button" className={edition === item.edition_id ? "selected" : ""} onClick={() => setEdition(item.edition_id)}>
            {item.edition_id === environment.data?.recommended_edition && <span className="edition-badge">设备推荐</span>}
            <h3>{item.name}</h3><p>{item.description}</p>
            <ul><li>{item.artifacts.map((artifact) => artifact.role === "generation" ? artifact.model_id : "中文 Embedding").join(" + ")}</li><li>下载 {formatBytes(item.download_size_bytes)}</li><li>建议内存 {item.recommended_memory_gb} GB</li></ul>
          </button>)}
        </div>
        <h2 className="section-title">选择下载来源</h2>
        <div className="source-options">
          <label><input type="radio" name="model-source" checked={source === "huggingface"} onChange={() => setSource("huggingface")} /> Hugging Face <em>独立固定修订</em></label>
          <label><input type="radio" name="model-source" checked={source === "modelscope"} onChange={() => setSource("modelscope")} /> 魔搭社区 <em>独立固定修订</em></label>
        </div>
        <p className="inline-notice">每个来源使用各自锁定的仓库、修订、大小和 SHA-256；断点不会跨来源复用。Windows OCR 不重复下载模型。</p>
        <button type="button" className="primary-button download-next" disabled={!selectedEdition} onClick={() => setStep("confirm")}>查看安装计划</button>
      </div>}

      {step === "confirm" && <div className="install-plan">
        <h2>确认下载与安装</h2>
        <dl>
          <div><dt>模型方案</dt><dd>{selectedEdition?.name ?? "读取中"}</dd></div>
          <div><dt>完整组件</dt><dd>{selectedEdition?.artifacts.map((artifact) => `${artifact.model_id}（${artifact.role}）`).join("；") ?? "—"}</dd></div>
          <div><dt>总下载量</dt><dd>{selectedEdition ? formatBytes(selectedEdition.download_size_bytes) : "—"}</dd></div>
          <div><dt>下载来源</dt><dd>{source === "modelscope" ? "魔搭社区" : "Hugging Face"} · 每个组件独立固定修订</dd></div>
          <div><dt>模型许可</dt><dd>{[...new Set(selectedEdition?.artifacts.map((artifact) => artifact.license_name) ?? [])].join("、") || "—"}</dd></div>
          <div><dt>安装位置</dt><dd>%APPDATA%\com.remin.desktop\models（由拾忆管理）</dd></div>
          <div><dt>完整性</dt><dd>逐文件校验精确字节数与 SHA-256，通过自检后原子启用</dd></div>
          <div><dt>网络边界</dt><dd>只下载模型；文件、索引与 AI 处理仍在本地完成</dd></div>
        </dl>
        <p className="inline-notice">点击后立即创建持久任务。离开本页仍会在右上角显示进度；退出应用会保留有效断点，异常断点将隔离并受控重试。</p>
        {startDownload.isError && <p role="alert" className="inline-error">{startDownload.error instanceof Error ? startDownload.error.message : String(startDownload.error)}</p>}
        <div className="plan-actions"><button type="button" disabled={startDownload.isPending} onClick={() => setStep("download")}>返回修改</button><button type="button" className="primary-button" disabled={!selectedEdition || startDownload.isPending} onClick={() => startDownload.mutate()}>{startDownload.isPending ? "正在创建下载任务…" : `确认下载 ${selectedEdition?.name ?? "模型包"}`}</button></div>
      </div>}

      {(artifacts.data?.length ?? 0) > 0 && <section className="managed-models">
        <h2>本地模型组件</h2>
        {artifacts.data?.map((artifact) => {
          const activatable = (artifact.role === "embedding" && artifact.format === "onnx") || ((artifact.role === "generation" || artifact.role === "vision") && artifact.format === "gguf");
          return <article key={artifact.artifact_id}><div><strong>{artifact.model_id}</strong><small>{artifact.role} · {artifact.format.toUpperCase()} · {(artifact.size_bytes / 1024 / 1024).toFixed(1)} MB{artifact.embedding_dimension ? ` · ${artifact.embedding_dimension}维` : ""}</small></div>{activatable ? <button type="button" disabled={activate.isPending} onClick={() => activate.mutate(artifact.artifact_id)}>{activate.isPending ? "正在加载与自检" : artifact.role === "generation" ? "加载并启用" : artifact.role === "vision" ? "自检并启用图片理解" : artifact.embedding_dimension ? "重建并切换索引" : "自检并建立索引"}</button> : <span>{artifact.role === "ocr" ? "使用 Windows OCR" : "运行时待接入"}</span>}</article>;
        })}
        {activate.data?.embedding_migration && <p role="status" className="inline-notice">{activate.data.message}</p>}
        {activate.isError && <p role="alert" className="inline-error">{activate.error instanceof Error ? activate.error.message : String(activate.error)}</p>}
      </section>}
    </section>
  );
}
