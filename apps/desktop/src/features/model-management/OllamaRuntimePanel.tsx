import { useQuery } from "@tanstack/react-query";
import { isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";
import { bridge, type OllamaStatusSnapshot } from "../../bridge";
import { RUNTIME_EVENTS } from "../../bridge/runtime-events";
import { errorMessage } from "../../utils/app-error";

const OLLAMA_DOWNLOAD_URL = "https://ollama.com/download";
const OLLAMA_LOCAL_URL = "http://127.0.0.1:11434";

async function openExternal(url: string, onError: (message: string) => void) {
  try {
    await bridge.ollama_open_url(url);
  } catch (cause) {
    onError(errorMessage(cause));
  }
}

type PanelMessage = {
  tone: "success" | "error";
  text: string;
};

/**
 * Ollama 运行环境管理区块（模型管理 tab 内）。
 * 展示三态（已就绪 / 已装未运行 / 未安装），提供“下载引导 / 启动 / 测试连接 / 关闭”入口；
 * 未安装时只引导用户自行到官方下载，不静默安装、不下载第三方安装包。
 *
 * 状态刷新策略：
 * - 启动中（本地 starting=true）保持 1.5s 轮询，直到探测到就绪/失败；
 * - 同时订阅后端 `ollama:state` 事件做即时刷新（服务就绪或失败都会发事件）；
 * - 就绪后清除“正在后台启动”残留文案，避免误导。
 */
export function OllamaRuntimePanel() {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<PanelMessage | null>(null);
  // 本地“启动中”标记：ollama_status_get 的快照不含 starting 字段，必须用本地
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
  // 面板据此更新状态并清除“正在后台启动”残留文案。
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
            setMessage({ tone: "success", text: "Ollama 已就绪。" });
          } else if (payload.status === "installed_not_running") {
            setStarting(!!payload.starting);
            if (payload.error_code) setMessage({ tone: "error", text: `启动失败：${payload.error_code}` });
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

  // 轮询探测到就绪时：停止启动轮询并清除残留的“正在后台启动”文案。
  useEffect(() => {
    if (status.data?.status === "ready") {
      setStarting(false);
      setMessage((current) => (current?.text.includes("正在后台启动") ? null : current));
    } else if (status.data?.status === "not_installed") {
      setStarting(false);
    }
  }, [status.data]);

  const refresh = async () => {
    setMessage(null);
    const result = await status.refetch();
    if (result.error) setMessage({ tone: "error", text: `检测失败：${errorMessage(result.error)}` });
    else setMessage({ tone: "success", text: "已重新检测 Ollama 状态。" });
  };

  /** 请求启动本机 Ollama 服务。 */
  const start = async () => {
    setBusy(true);
    setMessage(null);
    setStarting(true);
    try {
      const snapshot = await bridge.ollama_start();
      if (snapshot.status === "ready") {
        setStarting(false);
        setMessage({ tone: "success", text: "Ollama 已就绪。" });
      } else if (snapshot.status === "installed_not_running" && snapshot.starting) {
        setMessage({ tone: "success", text: "正在后台启动，稍候自动刷新。" });
      } else {
        setStarting(false);
      }
      await status.refetch();
    } catch (cause) {
      setStarting(false);
      setMessage({ tone: "error", text: `启动失败：${errorMessage(cause)}` });
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
      setMessage({ tone: "error", text: `关闭失败：${errorMessage(cause)}` });
    } finally {
      setBusy(false);
    }
  };

  const snapshot = status.data;
  const isReady = snapshot?.status === "ready";
  const isStopped = snapshot?.status === "installed_not_running" && !snapshot.starting;
  const isNotInstalled = snapshot?.status === "not_installed";
  const stateLabel = isReady ? "已就绪" : isStopped ? "已关闭" : snapshot?.status === "installed_not_running" ? "已安装，服务未启动" : "未安装";
  const stateClass = isReady ? "ok" : isStopped ? "warn" : snapshot?.status === "installed_not_running" ? "warn" : "alert";
  const buttonLabel = isReady ? "关闭 Ollama" : isNotInstalled ? "打开官方下载页" : "启动 Ollama";
  const showExternalError = (text: string) => setMessage({ tone: "error", text });
  const primaryAction = isReady ? stop : isNotInstalled ? () => openExternal(OLLAMA_DOWNLOAD_URL, showExternalError) : start;

  return (
    <section>
      <div className="runtime-panel__header">
        <div>
          <h2>Ollama 运行环境</h2>
          <p>问资料和语义检索会连接本机 Ollama；翻翻不会静默下载、安装或连接远程模型服务。</p>
        </div>
        <button
          type="button"
          className={`runtime-panel__action ${isReady ? "runtime-panel__action--outline" : "runtime-panel__action--amber"}`}
          disabled={busy}
          onClick={() => void primaryAction()}
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
      <div className="runtime-panel__links" aria-label="Ollama 操作">
        <button type="button" onClick={() => void refresh()}>重新检测</button>
        <button type="button" onClick={() => void openExternal(OLLAMA_DOWNLOAD_URL, showExternalError)}>打开 Ollama 下载页</button>
        <button type="button" disabled={!isReady} onClick={() => void openExternal(OLLAMA_LOCAL_URL, showExternalError)}>打开本机 Ollama 地址</button>
      </div>
      {isNotInstalled && (
        <div role="alert" className="inline-error">
          <strong>本机未安装 Ollama。</strong>
          <p>请从 Ollama 官网下载并安装 Windows 版本。安装完成后回到这里点击“重新检测”；后续模型仍需由你确认后才会拉取。</p>
        </div>
      )}
      {message && (
        <p role={message.tone === "error" ? "alert" : "status"} className={message.tone === "error" ? "inline-error" : "inline-success"}>
          {message.text}
        </p>
      )}
      <p className="settings-hint">仅连接本机 Ollama（127.0.0.1:11434），不连接局域网、远程或公网服务。</p>
    </section>
  );
}
