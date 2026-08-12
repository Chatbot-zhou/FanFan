import { Component, type ErrorInfo, type ReactNode } from "react";
import { recordDiagnosticEvent } from "../../bridge/observed-bridge";

interface AppErrorBoundaryProps {
  children: ReactNode;
}

interface AppErrorBoundaryState {
  failed: boolean;
  errorCode: string;
}

export class AppErrorBoundary extends Component<AppErrorBoundaryProps, AppErrorBoundaryState> {
  state: AppErrorBoundaryState = { failed: false, errorCode: "" };

  static getDerivedStateFromError(): AppErrorBoundaryState {
    const errorCode = globalThis.crypto?.randomUUID?.().slice(0, 8)
      ?? Date.now().toString(36).slice(-8);
    return { failed: true, errorCode };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    recordDiagnosticEvent({
      level: "error",
      component: "frontend.react",
      event_name: "render.failed",
      correlation_id: this.state.errorCode,
      fields: {
        error_type: error.name,
        message: error.message,
        component_stack: info.componentStack?.slice(0, 4_000),
      },
    });
  }

  render(): ReactNode {
    if (!this.state.failed) return this.props.children;
    return (
      <main className="fatal-diagnostic" role="alert">
        <div className="fatal-diagnostic__card">
          <h1>拾忆遇到了界面错误</h1>
          <p>诊断信息已经保存在本机日志中。错误编号：{this.state.errorCode}</p>
          <p>你可以先重新加载；如果再次出现，请在设置中导出诊断包交给开发者。</p>
          <button type="button" className="primary-button" onClick={() => window.location.reload()}>
            重新加载
          </button>
        </div>
      </main>
    );
  }
}
