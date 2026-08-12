export const RUNTIME_EVENTS = {
  startupState: "startup:state",
  jobProgress: "job:progress",
  catalogChanged: "catalog:changed",
  indexChanged: "index:changed",
  indexRebuildStarted: "index:rebuild_started",
  indexFailed: "index:failed",
  collectionSuggestionsChanged: "collection:suggestions_changed",
  modelDownloadStarted: "model:download_started",
  modelDownloadState: "model:download_state",
  modelDownloadFailed: "model:download_failed",
  modelDownloadCompleted: "model:download_completed",
  modelState: "model:state",
  embeddingIndexPhase: "embedding:index_phase",
  embeddingFailed: "embedding:failed",
  catalogWatchDegraded: "catalog:watch_degraded",
  askToken: "ask:token",
  askPhase: "ask:phase",
  askCancelled: "ask:cancelled",
  collectionSuggestionPhase: "collection:suggestion_phase",
  visionProgress: "vision:progress",
  visionCompleted: "vision:completed",
} as const;

export type RuntimeEventName = typeof RUNTIME_EVENTS[keyof typeof RUNTIME_EVENTS];

export function isValidRuntimeEventName(value: string): boolean {
  return /^[A-Za-z0-9_:/-]+$/.test(value);
}
