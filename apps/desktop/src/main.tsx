import React from "react";
import ReactDOM from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App } from "./app/App";
import { recordDiagnosticEvent } from "./bridge/observed-bridge";
import { AppErrorBoundary } from "./features/diagnostics/AppErrorBoundary";
import { ThemeProvider } from "./features/theme/ThemeProvider";
import { normalizeAppError } from "./utils/app-error";
import "./styles/global.css";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      retry: 1,
      refetchOnWindowFocus: false,
    },
  },
});

window.addEventListener("error", (event) => {
  recordDiagnosticEvent({
    level: "error",
    component: "frontend.window",
    event_name: "javascript.error",
    fields: {
      error_type: event.error instanceof Error ? event.error.name : "ErrorEvent",
      message: event.message,
      source_path: event.filename,
      line: event.lineno,
      column: event.colno,
    },
  });
});

window.addEventListener("unhandledrejection", (event) => {
  const reason = event.reason;
  const error = normalizeAppError(reason);
  recordDiagnosticEvent({
    level: "error",
    component: "frontend.window",
    event_name: "promise.unhandled_rejection",
    fields: {
      error_type: reason instanceof Error ? reason.name : typeof reason,
      error_code: error.code,
      message: error.message,
    },
  });
});

recordDiagnosticEvent({
  level: "info",
  component: "frontend",
  event_name: "application.mounted",
  fields: {},
});

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <AppErrorBoundary>
      <QueryClientProvider client={queryClient}>
        <ThemeProvider>
          <App />
        </ThemeProvider>
      </QueryClientProvider>
    </AppErrorBoundary>
  </React.StrictMode>,
);
