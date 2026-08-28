import type { ModelRuntimeState, OllamaStatusSnapshot } from "../../bridge";

export type ModelSetupStatus = "not_installed" | "not_running" | "no_model" | "ready" | "error";

export interface ModelSetupState {
  status: ModelSetupStatus;
  label: string;
  description: string;
  action_label: string;
}

export function deriveModelSetupState(
  ollama: OllamaStatusSnapshot | null | undefined,
  model: ModelRuntimeState | null | undefined,
  error: unknown = null,
): ModelSetupState {
  if (error) {
    return {
      status: "error",
      label: "Ollama 状态暂时无法确认",
      description: "翻翻没有读取到本机 Ollama 的状态，可以稍后重新检测；不会自动连接远程服务。",
      action_label: "重新检测",
    };
  }

  if (!ollama) {
    return {
      status: "error",
      label: "正在检测 Ollama",
      description: "翻翻正在确认本机问资料运行环境。",
      action_label: "查看模型管理",
    };
  }

  if (ollama.status === "not_installed") {
    return {
      status: "not_installed",
      label: "需要先安装 Ollama",
      description: "问资料和语义检索依赖本机 Ollama。翻翻只提供引导，不会静默下载或安装第三方软件。",
      action_label: "配置 Ollama",
    };
  }

  if (ollama.status === "installed_not_running") {
    return {
      status: "not_running",
      label: ollama.starting ? "Ollama 正在启动" : "Ollama 已安装，尚未启动",
      description: ollama.starting ? "服务启动后会自动刷新状态。" : "启动本机 Ollama 后，翻翻才能准备问答和语义检索模型。",
      action_label: "启动 Ollama",
    };
  }

  if (ollama.status !== "ready") {
    return {
      status: "error",
      label: "Ollama 状态异常",
      description: "翻翻无法判断本机 Ollama 当前状态，请重新检测或查看模型管理。",
      action_label: "查看模型管理",
    };
  }

  const hasCoreModels = model?.capabilities.generation === true && model?.capabilities.embedding === true;
  if (!hasCoreModels) {
    return {
      status: "no_model",
      label: "还没有可用的问资料模型",
      description: "Ollama 已就绪，但生成模型或语义检索模型尚未准备好。请选择一个官方配置并下载缺失模型。",
      action_label: "选择模型配置",
    };
  }

  return {
    status: "ready",
    label: "问资料已准备好",
    description: "可以基于已授权资料进行本地问答、搜索和整理。",
    action_label: "开始问资料",
  };
}
