import { CloseOutlined, MinusOutlined } from "@ant-design/icons";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { recordDiagnosticEvent } from "../bridge/observed-bridge";

async function callWindow(action: "minimize" | "toggleMaximize" | "close") {
  if (!window.__TAURI_INTERNALS__) {
    if (action === "close") window.close();
    return;
  }
  try {
    await getCurrentWindow()[action]();
  } catch (error) {
    recordDiagnosticEvent({
      level: "error",
      component: "frontend.window_controls",
      event_name: "window_control.failed",
      fields: { action, error: error instanceof Error ? error.message : String(error) },
    });
  }
}

export function WindowControls() {
  return (
    <div className="window-controls" aria-label="窗口控制">
      <button className="window-control" type="button" aria-label="最小化" onClick={() => void callWindow("minimize")}>
        <MinusOutlined />
      </button>
      <button className="window-control" type="button" aria-label="最大化或还原" onClick={() => void callWindow("toggleMaximize")}>
        <span className="window-control__maximize" aria-hidden="true" />
      </button>
      <button className="window-control window-control--close" type="button" aria-label="关闭" onClick={() => void callWindow("close")}>
        <CloseOutlined />
      </button>
    </div>
  );
}
