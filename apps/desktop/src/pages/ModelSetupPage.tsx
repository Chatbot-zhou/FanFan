import { ArrowLeftOutlined, CloudDownloadOutlined, FolderOpenOutlined, InfoCircleOutlined } from "@ant-design/icons";
import { isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { bridge, type ImportCandidate, type ModelEdition, type ModelRole } from "../bridge";
import { useAppStore } from "../state/app-store";

type SetupStep = "choice" | "download" | "confirm" | "import";

const formatBytes = (value: number) => value >= 1024 ** 3
  ? `${(value / 1024 ** 3).toFixed(2)} GB`
  : `${(value / 1024 ** 2).toFixed(0)} MB`;

export function ModelSetupPage() {
  const queryClient = useQueryClient();
  const navigate = useAppStore((state) => state.navigate);
  const [step, setStep] = useState<SetupStep>("choice");
  const [edition, setEdition] = useState<ModelEdition["edition_id"]>("light");
  const [source, setSource] = useState<"huggingface" | "modelscope">("huggingface");
  const [candidates, setCandidates] = useState<ImportCandidate[]>([]);
  const [roles, setRoles] = useState<Record<string, ModelRole>>({});
  const [showDeviceDetails, setShowDeviceDetails] = useState(false);
  const [downloadProgress, setDownloadProgress] = useState<{ downloaded_bytes: number; total_bytes: number; progress: number } | null>(null);
  const environment = useQuery({ queryKey: ["environment"], queryFn: async () => (await bridge.environment_get_latest()) ?? bridge.environment_detect() });
  const artifacts = useQuery({ queryKey: ["model-artifacts"], queryFn: () => bridge.model_artifact_list() });
  const catalog = useQuery({ queryKey: ["model-catalog"], queryFn: () => bridge.model_catalog_list() });
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
  const activate = useMutation({
    mutationFn: (artifactId: string) => bridge.model_artifact_activate(artifactId),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["model-runtime"] });
      await artifacts.refetch();
    },
  });
  const downloadModel = useMutation({
    mutationFn: () => bridge.model_download_install(edition, source, true),
    onMutate: () => setDownloadProgress(null),
    onSuccess: async () => {
      setDownloadProgress(null);
      await artifacts.refetch();
      setStep("choice");
    },
  });

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<{ downloaded_bytes: number; total_bytes: number; progress: number }>("model.download_progress", (event) => {
      if (!disposed) setDownloadProgress(event.payload);
    }).then((release) => { if (disposed) release(); else unlisten = release; });
    return () => { disposed = true; unlisten?.(); };
  }, []);

  const selectedEdition = catalog.data?.find((item) => item.edition_id === edition);

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
        <div><h1>本地模型配置</h1><p>模型安装目录由拾忆管理，安装完成后的推理全部在本地进行。</p></div>
      </header>
      <div className="device-summary">
        <div><small>内存</small><strong>{environment.data?.memory_total_gb ?? "检测中"} GB</strong></div>
        <div><small>可用空间</small><strong>{environment.data?.disk_available_gb ?? "检测中"} GB</strong></div>
        <div><small>推荐版本</small><strong>{environment.data?.recommended_edition === "standard" ? "标准版" : "轻量版"}</strong></div>
        <button type="button" aria-expanded={showDeviceDetails} onClick={() => setShowDeviceDetails((value) => !value)}><InfoCircleOutlined /> {showDeviceDetails ? "收起详情" : "查看详情"}</button>
      </div>
      {showDeviceDetails && <div className="inline-notice" role="status">当前使用 {environment.data?.runtime_backend === "gpu" ? `GPU（${environment.data.gpu_name ?? "名称未知"}）` : "CPU"} 本地推理；推荐依据为可用内存与磁盘空间。{environment.data?.warnings.length ? ` 注意：${environment.data.warnings.join("；")}` : "未检测到资源警告。"}</div>}

      {step === "choice" && (
        <>
          <h2 className="section-title">选择获取方式</h2>
          <div className="model-methods">
            <button type="button" onClick={() => setStep("import")}>
              <span><FolderOpenOutlined /></span><strong>从本地导入模型</strong><small>选择 GGUF、ONNX 及常见配套配置</small>
            </button>
            <button type="button" onClick={() => setStep("download")}>
              <span><CloudDownloadOutlined /></span><strong>联网下载模型</strong><small>下载经过版本、大小与 SHA-256 锁定的公开模型</small>
            </button>
          </div>
          <div className="current-model"><h2>已管理的模型</h2><div><span>{artifacts.data?.length ? `${artifacts.data.length} 个本地组件` : "尚未导入任何模型"}</span><small>{artifacts.data?.length ? artifacts.data.map((item) => `${item.model_id}（${item.role}）`).join("、") : "当前处于基础模式，文件名和全文搜索可以使用。"}</small></div></div>
          <button className="text-button model-skip" type="button" onClick={() => navigate("home")}>暂不配置</button>
        </>
      )}

      {step === "import" && (
        <div className="import-panel">
          <FolderOpenOutlined /><h2>导入常见格式模型</h2>
          <p>支持生成模型 GGUF，向量与重排模型 ONNX，以及 JSON、tokenizer.json 和 SentencePiece 配置。图片和扫描 PDF 默认使用 Windows 本地 OCR。</p>
          <div className="import-panel__actions"><button type="button" className="primary-button" disabled={scanImport.isPending} onClick={() => void chooseModels(false)}>选择模型文件</button><button type="button" disabled={scanImport.isPending} onClick={() => void chooseModels(true)}>选择模型目录</button></div>
          {scanImport.isError && <p role="alert" className="inline-error">{scanImport.error instanceof Error ? scanImport.error.message : String(scanImport.error)}</p>}
          {candidates.length > 0 && <div className="import-candidates">
            {candidates.map((candidate) => <article key={candidate.candidate_id}><div><strong>{candidate.display_name}</strong><small>{candidate.format.toUpperCase()} · {(candidate.size_bytes / 1024 / 1024).toFixed(1)} MB · SHA-256 {candidate.sha256.slice(0, 12)}…</small>{candidate.warnings.map((warning) => <em key={warning}>{warning}</em>)}</div><label>用途<select value={roles[candidate.candidate_id]} onChange={(event) => setRoles((current) => ({ ...current, [candidate.candidate_id]: event.target.value as ModelRole }))}><option value="generation">对话生成</option><option value="embedding">文本向量</option><option value="reranker">重排</option><option value="ocr">OCR</option></select></label></article>)}
            {importModels.isError && <p role="alert" className="inline-error">{importModels.error instanceof Error ? importModels.error.message : String(importModels.error)}</p>}
            <button type="button" className="primary-button" disabled={importModels.isPending} onClick={() => importModels.mutate()}>{importModels.isPending ? "正在校验并导入" : "确认导入到拾忆"}</button>
          </div>}
          <small>拾忆不会执行模型目录中的 Python、Shell 或远程自定义代码。</small>
        </div>
      )}

      {step === "download" && (
        <div className="download-config">
          <h2 className="section-title">选择模型版本</h2>
          <div className="edition-grid">
            {catalog.isPending && <p>正在读取已锁定的模型清单…</p>}
            {catalog.isError && <p role="alert" className="inline-error">{catalog.error instanceof Error ? catalog.error.message : String(catalog.error)}</p>}
            {catalog.data?.map((item) => <button key={item.edition_id} type="button" className={edition === item.edition_id ? "selected" : ""} onClick={() => setEdition(item.edition_id)}>
              {item.edition_id === environment.data?.recommended_edition && <span className="edition-badge">设备推荐</span>}
              <h3>{item.name}</h3><p>{item.description}</p>
              <ul><li>{item.artifact.model_id}</li><li>下载 {formatBytes(item.download_size_bytes)}</li><li>建议内存 {item.recommended_memory_gb} GB</li></ul>
            </button>)}
          </div>
          <h2 className="section-title">选择下载来源</h2>
          <div className="source-options">
            <label><input type="radio" name="model-source" checked={source === "huggingface"} onChange={() => setSource("huggingface")} /> Hugging Face <em>固定修订与哈希</em></label>
            <label><input type="radio" name="model-source" checked={source === "modelscope"} onChange={() => setSource("modelscope")} /> 魔搭社区 <em>同文件哈希镜像</em></label>
          </div>
          <p className="inline-notice">联网方案当前只安装本地生成模型。文件名搜索、全文搜索和 Windows OCR 无需下载模型；语义搜索可另行导入 ONNX 向量模型。</p>
          <button type="button" className="primary-button download-next" disabled={!selectedEdition} onClick={() => setStep("confirm")}>查看安装计划</button>
        </div>
      )}

      {step === "confirm" && (
        <div className="install-plan">
          <h2>确认下载与安装</h2>
          <dl>
            <div><dt>模型方案</dt><dd>{selectedEdition?.name ?? "读取中"}</dd></div>
            <div><dt>模型文件</dt><dd>{selectedEdition?.artifact.model_id ?? "—"}（{selectedEdition ? formatBytes(selectedEdition.download_size_bytes) : "—"}）</dd></div>
            <div><dt>下载来源</dt><dd>{source === "modelscope" ? "魔搭社区" : "Hugging Face"} · {selectedEdition?.artifact.repository_id ?? "—"}</dd></div>
            <div><dt>修订约束</dt><dd>{source === "modelscope" ? "官方镜像 + 精确字节数与 SHA-256" : `${selectedEdition?.artifact.revision.slice(0, 12) ?? "—"}…`}</dd></div>
            <div><dt>模型许可</dt><dd>{selectedEdition?.artifact.license_name ?? "—"}</dd></div>
            <div><dt>安装位置</dt><dd>%LOCALAPPDATA%\Remin\models</dd></div>
            <div><dt>完整性</dt><dd>SHA-256 {selectedEdition?.artifact.sha256.slice(0, 16) ?? "—"}…，并校验精确字节数</dd></div>
            <div><dt>网络边界</dt><dd>只下载模型；文件与 AI 处理仍在本地完成</dd></div>
          </dl>
          <p className="inline-notice">点击下载即表示你确认联网获取上述公开模型。中断后会保留普通断点文件供下次继续；只有大小和 SHA-256 均通过后才会安装。</p>
          {downloadModel.isPending && downloadProgress && <p className="inline-notice">已下载 {formatBytes(downloadProgress.downloaded_bytes)} / {formatBytes(downloadProgress.total_bytes)}（{Math.min(100, Math.round(downloadProgress.progress * 100))}%）</p>}
          {downloadModel.isError && <p role="alert" className="inline-error">{downloadModel.error instanceof Error ? downloadModel.error.message : String(downloadModel.error)}</p>}
          <div className="plan-actions"><button type="button" disabled={downloadModel.isPending} onClick={() => setStep("download")}>返回修改</button><button type="button" className="primary-button" disabled={!selectedEdition || downloadModel.isPending} onClick={() => downloadModel.mutate()}>{downloadModel.isPending ? "正在下载、校验并安装…" : `确认下载 ${selectedEdition?.name ?? "模型"}`}</button></div>
        </div>
      )}
      {(artifacts.data?.length ?? 0) > 0 && <section className="managed-models">
        <h2>本地模型组件</h2>
        {artifacts.data?.map((artifact) => {
          const activatable = (artifact.role === "embedding" && artifact.format === "onnx") || (artifact.role === "generation" && artifact.format === "gguf");
          return <article key={artifact.artifact_id}><div><strong>{artifact.model_id}</strong><small>{artifact.role} · {artifact.format.toUpperCase()} · {(artifact.size_bytes / 1024 / 1024).toFixed(1)} MB{artifact.embedding_dimension ? ` · ${artifact.embedding_dimension}维` : ""}</small></div>{activatable ? <button type="button" disabled={activate.isPending} onClick={() => activate.mutate(artifact.artifact_id)}>{activate.isPending ? "正在加载与自检" : artifact.role === "generation" ? "加载并启用" : artifact.embedding_dimension ? "重新自检" : "自检并启用"}</button> : <span>{artifact.role === "ocr" ? "等待兼容OCR模型包" : "运行时待接入"}</span>}</article>;
        })}
        {activate.isError && <p role="alert" className="inline-error">{activate.error instanceof Error ? activate.error.message : String(activate.error)}</p>}
      </section>}
    </section>
  );
}
