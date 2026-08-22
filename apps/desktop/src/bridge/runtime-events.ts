export const RUNTIME_EVENTS = {
  startupState: "startup:state",
  runtimeState: "runtime:state",
  ollamaState: "ollama:state",
  jobProgress: "job:progress",
  catalogChanged: "catalog:changed",
  indexChanged: "index:changed",
  indexRebuildStarted: "index:rebuild_started",
  indexRebuildProgress: "index:rebuild_progress",
  indexFailed: "index:failed",
  collectionSuggestionsChanged: "collection:suggestions_changed",
  modelDownloadStarted: "model:download_started",
  modelDownloadState: "model:download_state",
  modelDownloadFailed: "model:download_failed",
  modelDownloadCompleted: "model:download_completed",
  modelDownloadRemoved: "model:download_removed",
  modelState: "model:state",
  embeddingIndexPhase: "embedding:index_phase",
  embeddingFailed: "embedding:failed",
  catalogWatchDegraded: "catalog:watch_degraded",
  askStream: "ask:stream",
  askToken: "ask:token",
  askPhase: "ask:phase",
  askThinking: "ask:thinking",
  askCancelled: "ask:cancelled",
  speechPartial: "speech:partial",
  speechFinal: "speech:final",
  collectionSuggestionPhase: "collection:suggestion_phase",
  visionProgress: "vision:progress",
  visionCompleted: "vision:completed",
  storageMigrationProgress: "storage:migration-progress",
  storageMigrationCompleted: "storage:migration-completed",
  storageMigrationFailed: "storage:migration-failed",
} as const;

export type RuntimeEventName = typeof RUNTIME_EVENTS[keyof typeof RUNTIME_EVENTS];

export function isValidRuntimeEventName(value: string): boolean {
  return /^[A-Za-z0-9_:/-]+$/.test(value);
}
