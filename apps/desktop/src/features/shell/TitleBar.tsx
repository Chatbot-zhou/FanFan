import type { ModelRuntimeState } from "../../bridge";
import { BrandMark } from "../../components/BrandMark";
import { WindowControls } from "../../components/WindowControls";
import { useAppStore } from "../../state/app-store";

interface TitleBarProps {
  model_state: ModelRuntimeState | null;
  welcome?: boolean;
}

export function TitleBar({ model_state, welcome = false }: TitleBarProps) {
  const navigate = useAppStore((state) => state.navigate);
  const dismissed = useAppStore((state) => state.model_prompt_dismissed);
  const dismiss = useAppStore((state) => state.dismiss_model_prompt);
  const showPrompt = !welcome && !dismissed && model_state?.status === "unconfigured";

  return (
    <header className={`title-bar${welcome ? " title-bar--welcome" : ""}`} data-tauri-drag-region>
      <div className="title-bar__brand" data-tauri-drag-region>
        <BrandMark compact />
      </div>
      <div className="title-bar__spacer" data-tauri-drag-region />
      {showPrompt && (
        <div className="model-prompt" aria-label="模型配置提示">
          <span className="model-prompt__dot" aria-hidden="true" />
          <span>未配置本地模型</span>
          <button type="button" onClick={() => navigate("model_setup")}>去配置</button>
          <button type="button" onClick={dismiss}>稍后</button>
        </div>
      )}
      {model_state?.status === "ready" && <div className="model-ready">{model_state.message}</div>}
      <WindowControls />
    </header>
  );
}
