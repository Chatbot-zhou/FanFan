import { CheckOutlined, CloseOutlined, DeleteOutlined } from "@ant-design/icons";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { bridge } from "../../bridge";
import type { MemorySummary } from "../../bridge";
import { confirmAction } from "../../components/AppConfirm";
import { errorMessage } from "../../utils/app-error";

/**
 * 设置 → 记忆（Phase 4.2 spec 二十二~三十六）。
 *
 * 用户视角是「翻翻记住了关于我的哪些东西」，不是数据库表：
 * - 顶部「使用记忆」总开关（关闭不删除数据、不阻断当前会话上下文）；
 * - 「已保存的记忆」：confirmed 摘要卡片，支持查看详情 / 删除；
 * - 「待确认」：翻翻的推测，用户确认 / 不是；
 * - 底部「清除全部记忆」：二次确认短语 CLEAR_MEMORY，复用后端 memory_clear。
 */
export function MemoryPanel() {
  const queryClient = useQueryClient();
  const [error, setError] = useState<string | null>(null);
  const [detail, setDetail] = useState<MemorySummary | null>(null);

  const settings = useQuery({
    queryKey: ["memory-settings"],
    queryFn: () => bridge.memory_settings_get(),
  });
  const summaries = useQuery({
    queryKey: ["memory-summaries"],
    queryFn: () => bridge.memory_summary_list(),
  });

  /** 操作后统一刷新摘要列表并清空详情抽屉。 */
  const refreshAfter = async () => {
    setDetail(null);
    await queryClient.invalidateQueries({ queryKey: ["memory-summaries"] });
  };

  const toggleMutation = useMutation({
    mutationFn: (enabled: boolean) => bridge.memory_settings_update({ enabled }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ["memory-settings"] }),
    onError: (mutationError) => setError(errorMessage(mutationError)),
  });

  const confirmMutation = useMutation({
    mutationFn: (summaryId: string) => bridge.memory_confirm(summaryId),
    onSuccess: () => void refreshAfter(),
    onError: (mutationError) => setError(errorMessage(mutationError)),
  });

  const rejectMutation = useMutation({
    mutationFn: (summaryId: string) => bridge.memory_reject(summaryId),
    onSuccess: () => void refreshAfter(),
    onError: (mutationError) => setError(errorMessage(mutationError)),
  });

  const deleteMutation = useMutation({
    mutationFn: (summaryId: string) => bridge.memory_delete(summaryId),
    onSuccess: () => void refreshAfter(),
    onError: (mutationError) => setError(errorMessage(mutationError)),
  });

  /** 单条删除：说明后果后执行（只删这条记忆，不动文件/索引/聊天记录）。 */
  const removeOne = async (summary: MemorySummary) => {
    setError(null);
    const approved = await confirmAction({
      actionKey: "memory_delete",
      title: `删除“${summary.title}”？`,
      description: "删除这条记忆后，翻翻以后不会再使用它来理解你的问题。你的文件、索引和聊天记录不受影响。",
      confirmLabel: "删除",
      danger: true,
    });
    if (approved) deleteMutation.mutate(summary.id);
  };

  /** 清除全部记忆：危险操作，需要输入确认短语 CLEAR_MEMORY。 */
  const clearAll = async () => {
    setError(null);
    const approved = await confirmAction({
      actionKey: "memory_clear_all",
      title: "清除全部记忆？",
      description: "这会删除翻翻保存的所有长期记忆，但不会删除你的文件、索引或聊天记录。",
      confirmLabel: "清除全部记忆",
      danger: true,
      confirmPhrase: "CLEAR_MEMORY",
    });
    if (!approved) return;
    try {
      await bridge.memory_clear({ confirmation: "CLEAR_MEMORY" });
      setError(null);
      await refreshAfter();
    } catch (clearError) {
      setError(errorMessage(clearError));
    }
  };

  const enabled = settings.data?.enabled ?? true;
  const confirmed = summaries.data?.confirmed ?? [];
  const candidates = summaries.data?.candidates ?? [];
  const busy = toggleMutation.isPending || confirmMutation.isPending || rejectMutation.isPending || deleteMutation.isPending;

  return (
    <>
      {error && <p role="alert" className="inline-error">{error}</p>}
      <section>
        <h2>记忆</h2>
        <p>翻翻可以记住你确认过的文件关系、名称和常用称呼。记忆仅保存在本地。</p>
        <div className="memory-switch">
          <span>使用记忆<small>允许翻翻在对话中使用已保存的记忆来理解你的指代和常用资料。</small></span>
          <button
            type="button"
            role="switch"
            aria-checked={enabled}
            className={`memory-switch__toggle ${enabled ? "on" : ""}`}
            disabled={toggleMutation.isPending}
            onClick={() => toggleMutation.mutate(!enabled)}
          >
            <i />
          </button>
        </div>
      </section>

      <section>
        <h2>已保存的记忆</h2>
        <div className="settings-list">
          {confirmed.map((summary) => (
            <MemoryRow
              key={summary.id}
              summary={summary}
              busy={busy}
              onView={() => setDetail(summary)}
              onDelete={() => void removeOne(summary)}
            />
          ))}
          {!summaries.isLoading && confirmed.length === 0 && (
            <p>翻翻还没有保存任何已确认的记忆。你在对话中确认过的称呼和关系会出现在这里。</p>
          )}
        </div>
      </section>

      {candidates.length > 0 && (
        <section>
          <h2>待确认</h2>
          <p>翻翻猜测的记忆需要你确认后才会使用。</p>
          <div className="settings-list">
            {candidates.map((summary) => (
              <div key={summary.id} className="memory-row memory-row--candidate">
                <span>
                  <strong>{summary.title}</strong>
                  <small>{summary.summary}</small>
                  <small className="memory-row__source">翻翻的推测 · 是否正确？</small>
                </span>
                <span className="memory-row__actions">
                  <button type="button" className="text-button" disabled={busy} onClick={() => confirmMutation.mutate(summary.id)}><CheckOutlined /> 确认</button>
                  <button type="button" className="text-button" disabled={busy} onClick={() => rejectMutation.mutate(summary.id)}><CloseOutlined /> 不是</button>
                </span>
              </div>
            ))}
          </div>
        </section>
      )}

      <section>
        <div className="settings-actions">
          <button type="button" className="danger-button" disabled={busy || (confirmed.length === 0 && candidates.length === 0)} onClick={() => void clearAll()}>
            <DeleteOutlined /> 清除全部记忆
          </button>
        </div>
      </section>

      {detail && (
        <div className="memory-detail" role="dialog" aria-label="记忆详情">
          <div className="memory-detail__card">
            <header>
              <strong>{detail.title}</strong>
              <button type="button" className="text-button" aria-label="关闭" onClick={() => setDetail(null)}><CloseOutlined /></button>
            </header>
            <p>{detail.summary}</p>
            <dl>
              <div><dt>来源</dt><dd>{detail.source_label}</dd></div>
              <div><dt>状态</dt><dd>已确认</dd></div>
              <div><dt>关联文件</dt><dd>{detail.target_available ? (detail.target_display_name ?? "—") : "关联文件当前不可用"}</dd></div>
              <div><dt>更新时间</dt><dd>{detail.updated_at.slice(0, 10)}</dd></div>
            </dl>
            {!detail.target_available && (
              <>
                <p role="status" className="inline-error">关联文件当前不可用（可能已删除、离线或未授权）。</p>
                <div className="settings-actions">
                  <button type="button" className="danger-button" disabled={busy} onClick={() => void removeOne(detail)}><DeleteOutlined /> 删除这条记忆</button>
                </div>
              </>
            )}
          </div>
        </div>
      )}
    </>
  );
}

/** 单条已确认记忆的卡片行：标题 + 自然语言摘要 + 来源 + 操作。 */
function MemoryRow({ summary, busy, onView, onDelete }: {
  summary: MemorySummary;
  busy: boolean;
  onView: () => void;
  onDelete: () => void;
}) {
  return (
    <div className={`memory-row ${summary.target_available ? "" : "memory-row--stale"}`}>
      <span>
        <strong>{summary.title}</strong>
        <small>{summary.summary}</small>
        <small className="memory-row__source">
          {summary.source_label}
          {!summary.target_available && " · 关联文件当前不可用"}
        </small>
      </span>
      <span className="memory-row__actions">
        <button type="button" className="text-button" disabled={busy} onClick={onView}>查看</button>
        <button type="button" className="text-button" disabled={busy} onClick={onDelete}>删除</button>
      </span>
    </div>
  );
}
