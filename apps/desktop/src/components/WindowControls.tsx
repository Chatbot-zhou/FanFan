import { CloseOutlined, MinusOutlined } from "@ant-design/icons";

async function callWindow(action: "minimize" | "toggleMaximize" | "close") {
  if (!window.__TAURI_INTERNALS__) {
    if (action === "close") window.close();
    return;
  }
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  await getCurrentWindow()[action]();
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
