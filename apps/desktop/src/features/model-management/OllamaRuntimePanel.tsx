import { useQuery } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";
import { bridge, type OllamaStatusSnapshot } from "../../bridge";
import { RUNTIME_EVENTS } from "../../bridge/runtime-events";
import { errorMessage } from "../../utils/app-error";

/**
 * Ollama 运行环境管理区块（模型管理 tab 内）。
 * 展示三态（已就绪 / 已装未运行 / 未安装），提供"启动 / 关闭"入口；
 * 未安装时只引导用户自行到官方下载，不静默安装、不下载第三方安装包。
 *
 * 状态刷新策略：
 * - 启动中（本地 starting=true）保持 1.5s 轮询，直到探测到就绪/失败；
 * - 同时订阅后端 `ollama:state` 事件做即时刷新（服务就绪或失败都会发事件）；
 * - 就绪后清除"正在后台启动"残留文案，避免误导。
 */
export function OllamaRuntimePanel() {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  // 本地"启动中"标记：ollama_status_get 的快照不含 starting 字段，必须用本地
  // 状态驱动启动期间的轮询，否则点启动后面板永远不自动刷新。
  const [starting, setStarting] = useState(false);
  const status = useQuery({
    queryKey: ["ollama-status"],
    queryFn: () => bridge.ollama_status_get(),
    refetchInterval: (query) => {
      const state = query.state.data as OllamaStatusSnapshot | undefined;
      // 本地标记启动中时轮询；探测到就绪/未安装后停止轮询。
      return starting && state?.status !== "ready" && state?.status !== "not_installed" ? 1500 : false;
    },
  });

  // 订阅后端 ollama:state 事件做即时刷新：服务就绪 / 启动失败都会发事件，
  // 面板据此更新状态并清除"正在后台启动"残留文案。
  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let unlisten: UnlistenFn | undefined;
    void (async () => {
      unlisten = await listen<{ status?: string; error_code?: string; starting?: boolean }>(
        RUNTIME_EVENTS.ollamaState,
        (event) => {
          if (disposed) return;
          const payload = event.payload ?? {};
          if (payload.status === "ready") {
            setStarting(false);
            setMessage("Ollama 已就绪。");
          } else if (payload.status === "installed_not_running") {
            setStarting(!!payload.starting);
            if (payload.error_code) setMessage(`启动失败：${payload.error_code}`);
            else if (!payload.starting) setMessage(null);
          }
          void status.refetch();
        },
      );
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 轮询探测到就绪时：停止启动轮询并清除残留的"正在后台启动"文案。
  useEffect(() => {
    if (status.data?.status === "ready") {
      setStarting(false);
      setMessage((current) => (current?.includes("正在后台启动") ? null : current));
    } else if (status.data?.status === "not_installed") {
      setStarting(false);
    }
  }, [status.data]);

  /** 请求启动本机 Ollama 服务。 */
  const start = async () => {
    setBusy(true);
    setMessage(null);
    setStarting(true);
    try {
      const snapshot = await bridge.ollama_start();
      if (snapshot.status === "ready") {
        setStarting(false);
        setMessage("Ollama 已就绪。");
      } else if (snapshot.status === "installed_not_running" && snapshot.starting) {
        setMessage("正在后台启动，稍候自动刷新。");
      } else {
        setStarting(false);
      }
      await status.refetch();
    } catch (cause) {
      setStarting(false);
      setMessage(`启动失败：${errorMessage(cause)}`);
    } finally {
      setBusy(false);
    }
  };

  /** 请求关闭本机 Ollama 服务（终止 ollama.exe 进程）。 */
  const stop = async () => {
    setBusy(true);
    setMessage(null);
    setStarting(false);
    try {
      await bridge.ollama_stop();
      await status.refetch();
    } catch (cause) {
      setMessage(`关闭失败：${errorMessage(cause)}`);
    } finally {
      setBusy(false);
    }
  };

  const snapshot = status.data;
  const isReady = snapshot?.status === "ready";
  const isStopped = snapshot?.status === "installed_not_running" && !snapshot.starting;
  const stateLabel = isReady ? "已就绪" : isStopped ? "已关闭" : snapshot?.status === "installed_not_running" ? "已安装，服务未启动" : "未安装";
  const stateClass = isReady ? "ok" : isStopped ? "warn" : snapshot?.status === "installed_not_running" ? "warn" : "alert";
  const buttonLabel = isReady ? "关闭 Ollama" : snapshot?.status === "not_installed" ? "重新检查" : "启动 Ollama";

  return (
    <section>
      <div className="runtime-panel__header">
        <h2>Ollama 运行环境</h2>
        <button
          type="button"
          className={`runtime-panel__action ${isReady ? "runtime-panel__action--outline" : "runtime-panel__action--amber"}`}
          disabled={busy || (snapshot?.status === "not_installed")}
          onClick={() => void (isReady ? stop() : start())}
        >
          {busy ? "处理中…" : buttonLabel}
        </button>
      </div>
      <div className={`readonly-note runtime-panel ${stateClass}`}>
        <span><strong>状态：</strong>{status.isLoading ? "检测中…" : stateLabel}</span>
        {isReady && snapshot.version && <span className="runtime-panel__sep">·</span>}
        {isReady && <span>版本 {snapshot.version}</span>}
        {snapshot?.error_code && <span className="runtime-panel__sep">·</span>}
        {snapshot?.error_code && <span>最近启动失败：{snapshot.error_code}</span>}
        {snapshot?.status === "installed_not_running" && snapshot.starting && <span className="runtime-panel__sep">·</span>}
        {snapshot?.status === "installed_not_running" && snapshot.starting && <span>正在启动…</span>}
      </div>
      {snapshot?.status === "not_installed" && (
        <div role="alert" className="inline-error">
          <strong>本机未安装 Ollama。</strong>
          <p>翻翻的本地问答与语义检索依赖本机 Ollama 服务。请自行从 Ollama 官网安装（下载对应安装包并运行）；翻翻不会替你下载或安装第三方软件。安装完成后回到这里点击"重新检查"。</p>
        </div>
      )}
      {message && <p role="status" className="inline-error">{message}</p>}
      <p className="settings-hint">仅连接本机 Ollama（127.0.0.1:11434），不连接局域网、远程或公网服务。</p>
    </section>
  );
}
