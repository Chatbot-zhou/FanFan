import { CheckCircleOutlined, DownOutlined, ExclamationCircleOutlined, LoadingOutlined, RightOutlined } from "@ant-design/icons";
import { useEffect, useMemo, useState } from "react";
import type { AskExecutionState, AskExecutionNode } from "./ask-execution-state";

const formatDuration = (durationMs: number | null) => {
  if (durationMs === null || Number.isNaN(durationMs)) return "";
  if (durationMs < 1_000) return `${durationMs}ms`;
  return `${(durationMs / 1_000).toFixed(durationMs < 10_000 ? 1 : 0)}s`;
};

const nodeIcon = (node: AskExecutionNode) => {
  if (node.status === "running") return <LoadingOutlined />;
  if (node.status === "failed") return <ExclamationCircleOutlined />;
  return <CheckCircleOutlined />;
};

export function AskExecutionPanel({ execution, onToggleNode }: {
  execution: AskExecutionState;
  onToggleNode?: (nodeId: string) => void;
}) {
  const [processExpanded, setProcessExpanded] = useState(execution.status === "running");
  const [localExpanded, setLocalExpanded] = useState<Record<string, boolean>>({});
  const visibleNodes = useMemo(() => execution.nodes.filter((node) => node.status !== "skipped"), [execution.nodes]);
  const isTerminal = execution.status === "completed" || execution.status === "failed" || execution.status === "cancelled";
  const processOpen = isTerminal ? processExpanded : true;
  const total = formatDuration(execution.total_duration_ms);
  const summary = `资料处理过程 · ${execution.step_count || visibleNodes.length} 步${total ? ` · ${total}` : ""}`;

  const toggleNode = (node: AskExecutionNode) => {
    if (onToggleNode) {
      onToggleNode(node.node_id);
      return;
    }
    setLocalExpanded((current) => ({ ...current, [node.node_id]: !current[node.node_id] }));
  };

  return (
    <section className={`ask-execution${isTerminal ? " ask-execution--terminal" : ""}`} aria-label="资料处理过程">
      {isTerminal && (
        <button type="button" className="ask-execution__summary" onClick={() => setProcessExpanded((open) => !open)}>
          {processOpen ? <DownOutlined /> : <RightOutlined />}
          <span>{summary}</span>
        </button>
      )}
      {processOpen && (
        <div className="ask-execution__nodes">
          {visibleNodes.map((node) => {
            const expanded = node.status === "running" || node.auto_expanded || node.user_expanded || Boolean(localExpanded[node.node_id]);
            const canToggle = node.status !== "running";
            const duration = formatDuration(node.duration_ms);
            return (
              <article key={node.node_id} className={`ask-execution-node ask-execution-node--${node.status}`}>
                <button type="button" className="ask-execution-node__header" disabled={!canToggle} onClick={() => toggleNode(node)}>
                  <span className="ask-execution-node__icon">{nodeIcon(node)}</span>
                  <span className="ask-execution-node__title">
                    {node.status === "running" ? `正在${node.public_label}` : node.public_label}
                  </span>
                  {node.public_summary && node.status !== "running" && <span className="ask-execution-node__summary">· {node.public_summary}</span>}
                  {duration && node.status !== "running" && <span className="ask-execution-node__duration">· {duration}</span>}
                </button>
                {expanded && node.progress_lines.length > 0 && (
                  <div className="ask-execution-node__body">
                    {node.progress_lines.slice(-3).map((line, index) => <p key={`${node.node_id}-${index}`}>{line}</p>)}
                  </div>
                )}
              </article>
            );
          })}
        </div>
      )}
    </section>
  );
}

