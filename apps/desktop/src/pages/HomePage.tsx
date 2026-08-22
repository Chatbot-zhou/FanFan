import {
  ClockCircleOutlined,
  CloseOutlined,
  CopyOutlined,
  ExclamationCircleOutlined,
  FileAddOutlined,
  FileExcelOutlined,
  FilePdfOutlined,
  FileWordOutlined,
  FolderOutlined,
  InboxOutlined,
  PlusOutlined,
  PauseOutlined,
  CaretRightOutlined,
  StopOutlined,
  RightOutlined,
  StarFilled,
} from "@ant-design/icons";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge, type CandidateRoot, type HomeSummary, type MaintenanceSnapshot, type RecentFile } from "../bridge";
import { confirmAction } from "../components/AppConfirm";
import { useAppStore } from "../state/app-store";
import { errorMessage } from "../utils/app-error";

interface HomePageProps {
  summary: HomeSummary | null;
  loading: boolean;
  /** 检索覆盖数据源：与状态栏「检索覆盖」同源（active USearch keys / searchable chunks），
   *  保证首页索引进度与下方状态一致。 */
  maintenance: MaintenanceSnapshot | null;
}

const metricIcons = {
  today_added: <FileAddOutlined />,
  awaiting_confirmation: <ClockCircleOutlined />,
  possible_duplicates: <CopyOutlined />,
  processing_failed: <ExclamationCircleOutlined />,
};

function FileIcon({ extension }: { extension: string }) {
  if (extension === "pdf") return <FilePdfOutlined className="file-icon file-icon--pdf" />;
  if (extension === "xlsx") return <FileExcelOutlined className="file-icon file-icon--excel" />;
  if (extension === "folder") return <FolderOutlined className="file-icon file-icon--folder" />;
  return <FileWordOutlined className="file-icon file-icon--word" />;
}

function FileList({ files, favorite = false, onOpen }: { files: RecentFile[]; favorite?: boolean; onOpen: (file: RecentFile) => void }) {
  return (
    <div className="home-file-list">
      {files.map((file) => (
        <button type="button" className="home-file" key={file.file_id} onClick={() => onOpen(file)}>
          <FileIcon extension={file.extension} />
          <span className="home-file__copy"><strong>{file.name}</strong><small>{file.subtitle}</small></span>
          {favorite && <StarFilled className="home-file__star" />}
        </button>
      ))}
    </div>
  );
}

function CandidateSourceCard({ candidate, onResolved }: { candidate: CandidateRoot; onResolved: (id: string) => void }) {
  const [busyAction, setBusyAction] = useState<"add" | "ignore" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const act = async (action: "add" | "ignore") => {
    setBusyAction(action);
    setError(null);
    try {
      await bridge.candidate_root_action(candidate.candidate_id, action);
      onResolved(candidate.candidate_id);
    } catch (actionError) {
      setError(errorMessage(actionError));
    } finally {
      setBusyAction(null);
    }
  };
  return (
    <div className="candidate-source">
      <div className="candidate-source__icon"><InboxOutlined /></div>
      <div><strong>{candidate.label}</strong><small>{candidate.display_path}</small></div>
      <button type="button" disabled={busyAction !== null} onClick={() => void act("add")}><PlusOutlined /> {busyAction === "add" ? "正在添加" : "添加到翻翻"}</button>
      <button type="button" aria-label={`暂不添加${candidate.label}`} disabled={busyAction !== null} onClick={() => void act("ignore")}><CloseOutlined />{busyAction === "ignore" && <span className="sr-only">正在忽略</span>}</button>
      {error && <small role="alert" className="inline-error candidate-source__error">{error}</small>}
    </div>
  );
}

export function HomePage({ summary, loading, maintenance }: HomePageProps) {
  const queryClient = useQueryClient();
  const startSearch = useAppStore((state) => state.start_search);
  const navigate = useAppStore((state) => state.navigate);
  const openInbox = useAppStore((state) => state.open_inbox);
  const openCollection = useAppStore((state) => state.open_collection);
  const [resolvedCandidates, setResolvedCandidates] = useState<string[]>([]);
  const candidates = summary?.candidate_roots.filter((item) => !resolvedCandidates.includes(item.candidate_id)) ?? [];
  const scan = summary?.scan_progress ?? null;
  const scanControl = useMutation({
    mutationFn: ({ action, jobId }: { action: "pause" | "resume" | "cancel"; jobId: string }) => action === "pause" ? bridge.scan_pause(jobId) : action === "resume" ? bridge.scan_resume(jobId) : bridge.scan_cancel(jobId),
    onSuccess: async () => {
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ["home-summary"] }),
        queryClient.invalidateQueries({ queryKey: ["roots"] }),
        queryClient.invalidateQueries({ queryKey: ["maintenance"] }),
      ]);
    },
  });
  // 「检索覆盖」与状态栏同源：活动 USearch 向量键 / 全部可搜索文本块。
  // 扫描任务结束（scan 为 null）后嵌入和 USearch 激活仍在后台进行，首页进度必须继续跟随后台。
  const searchableChunks = maintenance?.searchable_chunks ?? 0;
  const activeVectorKeys = maintenance?.active_vector_keys ?? 0;
  const coveragePercent = searchableChunks > 0
    ? Math.min(100, Math.round((activeVectorKeys / searchableChunks) * 100))
    : 0;
  const progress = coveragePercent / 100;
  const scanLabel = scan?.status === "running" ? "正在整理你的资料" : scan?.status === "paused" ? "资料整理已暂停" : coveragePercent >= 100 ? "资料已整理" : summary?.index_initialized ? "正在整理你的资料" : "尚未开始整理";
  const searchableHint = scan ? `已有 ${scan.searchable_files} 个文件可以搜索` : coveragePercent >= 100 ? `检索覆盖 ${coveragePercent}%` : "正在建立语义检索索引…";

  return (
    <section className="page page--home">
      <div className="metric-grid" aria-busy={loading}>
        {loading && Array.from({ length: 4 }, (_, index) => <div className="metric-card metric-card--loading" key={index}>正在读取…</div>)}
        {(summary?.metrics ?? []).map((metric) => (
          <button className={`metric-card metric-card--${metric.key}`} type="button" key={metric.key} onClick={() => {
            if (metric.key === "today_added") openInbox("all", true);
            else if (metric.key === "processing_failed") openInbox("error");
            else if (metric.key === "possible_duplicates") navigate("library");
            else openInbox("new");
          }}>
            <span className="metric-card__icon">{metricIcons[metric.key]}</span>
            <span><small>{metric.label}</small><strong>{metric.value}</strong></span>
          </button>
        ))}
      </div>

      <div className="home-grid">
        <article className="content-card">
          <h2>最近资料</h2>
          <FileList files={summary?.recent_files ?? []} onOpen={(file) => startSearch(file.name)} />
          <button className="card-link" type="button" onClick={() => navigate("library")}>查看全部 <RightOutlined /></button>
        </article>
        <article className="content-card">
          <h2>我的收藏</h2>
          <FileList files={summary?.favorite_files ?? []} favorite onOpen={(file) => startSearch(file.name)} />
          <button className="card-link" type="button" onClick={() => navigate("library")}>查看全部 <RightOutlined /></button>
        </article>
        <article className="content-card">
          <h2>智能集合</h2>
          <div className="collection-list">
            {(summary?.collections ?? []).map((collection) => (
              <button type="button" className="collection-row" key={collection.collection_id} onClick={() => openCollection(collection.collection_id)}>
                <span className={`collection-row__icon collection-row__icon--${collection.tone}`}><FolderOutlined /></span>
                <span><strong>{collection.name}</strong><small>{collection.item_count} 项</small></span>
              </button>
            ))}
          </div>
          <button className="card-link" type="button" onClick={() => navigate("collections")}>查看全部 <RightOutlined /></button>
        </article>
        <article className="content-card content-card--progress">
          <h2>{scanLabel}</h2>
          <div className="progress-ring" style={{ "--progress": `${Math.round(progress * 360)}deg` } as React.CSSProperties}>
            <div><strong>{Math.round(progress * 100)}%</strong></div>
          </div>
          <strong>检索覆盖 {Math.round(progress * 100)}%</strong>
          <small>{searchableHint}</small>
          {scan && <div className="scan-controls">
            {scan.status === "running" && <button type="button" disabled={scanControl.isPending} onClick={() => scanControl.mutate({ action: "pause", jobId: scan.scan_job_id })}><PauseOutlined /> 暂停</button>}
            {scan.status === "paused" && <button type="button" disabled={scanControl.isPending} onClick={() => scanControl.mutate({ action: "resume", jobId: scan.scan_job_id })}><CaretRightOutlined /> 继续</button>}
            {(scan.status === "running" || scan.status === "paused") && <button type="button" disabled={scanControl.isPending} onClick={() => void confirmAction({ actionKey: "scan_cancel", title: "取消本次扫描？", description: "已经提交的索引会保留，源文件不会发生变化。", confirmLabel: "取消扫描", danger: true }).then((confirmed) => { if (confirmed) scanControl.mutate({ action: "cancel", jobId: scan.scan_job_id }); })}><StopOutlined /> 取消</button>}
          </div>}
          {scanControl.isError && <small className="inline-error">{errorMessage(scanControl.error)}</small>}
          <button className="card-link" type="button" onClick={() => navigate("settings")}>查看详情 <RightOutlined /></button>
        </article>
      </div>

      {candidates.length > 0 && (
        <section className="candidate-panel">
          <div><h2>发现可以添加的资料来源</h2><p>只检查目录是否存在，添加后才会读取其中的文件。</p></div>
          <div className="candidate-panel__items">
            {candidates.map((candidate) => <CandidateSourceCard key={candidate.candidate_id} candidate={candidate} onResolved={(id) => setResolvedCandidates((current) => [...current, id])} />)}
          </div>
        </section>
      )}
    </section>
  );
}
