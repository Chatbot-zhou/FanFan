import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { Progress } from "antd";
import { useEffect, useState } from "react";
import { RUNTIME_EVENTS } from "../../bridge/runtime-events";

interface MigrationProgressPayload {
  phase: "copying" | "verifying";
  done_bytes: number;
  total_bytes: number;
}

const PHASE_LABELS: Record<MigrationProgressPayload["phase"], string> = {
  copying: "复制数据中",
  verifying: "校验数据中",
};

/** 右上角存储迁移进度条：监听 storage:migration-progress，迁移完成/失败后隐藏。 */
export function StorageMigrationProgress() {
  const [progress, setProgress] = useState<MigrationProgressPayload | null>(null);

  useEffect(() => {
    if (!window.__TAURI_INTERNALS__) return; // 浏览器/jsdom 环境下不注册 Tauri 事件监听
    let disposed = false;
    const unlisteners: UnlistenFn[] = [];
    const register = async () => {
      const listeners = await Promise.all([
        listen<MigrationProgressPayload>(RUNTIME_EVENTS.storageMigrationProgress, (event) => {
          setProgress(event.payload);
        }),
        listen(RUNTIME_EVENTS.storageMigrationCompleted, () => setProgress(null)),
        listen(RUNTIME_EVENTS.storageMigrationFailed, () => setProgress(null)),
      ]);
      if (disposed) listeners.forEach((unlisten) => unlisten());
      else unlisteners.push(...listeners);
    };
    void register();
    return () => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, []);

  if (!progress || progress.total_bytes <= 0) return null;
  const percent = Math.min(100, Math.round((progress.done_bytes / progress.total_bytes) * 100));
  return (
    <div className="migration-progress" role="status" aria-label="数据迁移进度">
      <Progress
        percent={percent}
        size="small"
        showInfo={false}
        strokeColor="#7468cf"
        trailColor="rgba(116,104,207,.13)"
        className="migration-progress__bar"
      />
      <span className="migration-progress__label">{PHASE_LABELS[progress.phase]} {percent}%</span>
    </div>
  );
}
