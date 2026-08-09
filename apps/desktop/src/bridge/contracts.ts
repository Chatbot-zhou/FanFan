export type UtcDateTime = string;

export type AppRoute =
  | "home"
  | "search"
  | "ask"
  | "library"
  | "collections"
  | "inbox"
  | "settings"
  | "model_setup";

export type Availability = "present" | "missing" | "unreadable";

export interface ScopeFilter {
  root_ids: string[];
  collection_ids: string[];
  file_ids: string[];
  extensions: string[];
  modified_from: UtcDateTime | null;
  modified_to: UtcDateTime | null;
  availability: Availability;
}

export interface SourceLocator {
  kind: "pdf" | "docx" | "spreadsheet" | "presentation" | "text" | "image";
  page_no: number | null;
  slide_no: number | null;
  sheet_name: string | null;
  cell_range: string | null;
  paragraph_no: number | null;
  line_start: number | null;
  line_end: number | null;
  shape_no: number | null;
  bbox: { x0: number; y0: number; x1: number; y1: number } | null;
  heading_path: string[];
}

export interface EvidenceRef {
  evidence_id: string;
  file_id: string;
  revision_id: string;
  node_id: string;
  chunk_id: string;
  quote: string;
  locator: SourceLocator;
  retrieval_score: number;
}

export interface AppError {
  code: string;
  message: string;
  retryable: boolean;
  user_action: string | null;
  file_id: string | null;
  details: Record<string, unknown> | null;
}

export type JobStatus =
  | "queued"
  | "running"
  | "paused"
  | "awaiting_user"
  | "succeeded"
  | "partial"
  | "failed"
  | "cancelled";

export interface JobRecord {
  job_id: string;
  job_type: string;
  status: JobStatus;
  stage: string;
  progress: number;
  processed_items: number;
  total_items: number;
  error: AppError | null;
  created_at: UtcDateTime;
  started_at: UtcDateTime | null;
  finished_at: UtcDateTime | null;
}

export type CheckRuleType = "schema" | "invariant" | "evidence" | "permission" | "resource" | "quality";

export interface CheckRule {
  rule_id: string;
  rule_type: CheckRuleType;
  description: string;
  parameters: Record<string, unknown>;
  required: boolean;
}

export interface RetryPolicy {
  max_attempts: number;
  backoff_ms: number;
  backoff_multiplier: number;
  retryable_codes: string[];
}

export interface ExecutionUnit {
  unit_id: string;
  unit_type: string;
  input_schema: string;
  output_schema: string;
  inputs: Record<string, unknown>;
  preconditions: CheckRule[];
  postconditions: CheckRule[];
  timeout_ms: number;
  retry_policy: RetryPolicy;
  idempotency_key: string;
  risk_level: "low" | "medium" | "high";
  checkpoint_policy: "always" | "on_success" | "none";
  fallback_unit_types: string[];
}

export interface ValidationCheckpoint {
  checkpoint_id: string;
  job_id: string;
  unit_id: string;
  checkpoint_type: CheckRuleType;
  status: "passed" | "failed" | "warning";
  rules_version: string;
  metrics: Record<string, unknown>;
  error: AppError | null;
  created_at: UtcDateTime;
  resume_token: string | null;
}

export interface ExplorationCandidate {
  candidate_id: string;
  job_id: string;
  strategy: string;
  status: "pending" | "running" | "valid" | "rejected" | "selected";
  result_ref: string | null;
  quality_score: number | null;
  evidence_score: number | null;
  latency_ms: number | null;
  resource_cost: number | null;
  rejection_reasons: string[];
}

export interface DegradationState {
  level: "full" | "balanced" | "core";
  triggers: string[];
  disabled_features: string[];
  entered_at: UtcDateTime | null;
  recover_after: UtcDateTime | null;
  manual_override: boolean;
}

export interface WelcomeState {
  welcome_version: string;
  welcome_completed: boolean;
  welcome_completed_at: UtcDateTime | null;
}

export interface StartupState {
  phase: "opening_catalog" | "opening_models" | "recovering_jobs" | "scheduling_background_work" | "ready" | "degraded";
  ready: boolean;
  progress: number;
  pending_files: number;
  degradation_level: "full" | "balanced" | "core";
  blocker: AppError | null;
  recovery_actions: Array<"open_settings" | "retry_startup">;
}

export type RuntimeMode = "full" | "balanced" | "core" | "basic";
export type ModelStatus = "checking" | "unconfigured" | "ready" | "unavailable";

export interface ModelRuntimeState {
  status: ModelStatus;
  runtime_mode: RuntimeMode;
  active_profile_id: string | null;
  active_profile_name: string | null;
  runtime_backend: "gpu" | "cpu" | null;
  message: string;
  checked_at: UtcDateTime | null;
  capabilities: {
    generation: boolean;
    embedding: boolean;
    reranker: boolean;
    ocr: boolean;
  };
}

export type ModelRole = "generation" | "embedding" | "reranker" | "ocr";
export type ModelFormat = "gguf" | "onnx";

export interface ImportCandidate {
  candidate_id: string;
  source_path: string;
  display_name: string;
  format: ModelFormat;
  suggested_role: ModelRole | null;
  size_bytes: number;
  sha256: string;
  companion_files: string[];
  warnings: string[];
}

export interface ModelArtifact {
  artifact_id: string;
  role: ModelRole;
  format: ModelFormat;
  model_id: string;
  model_version: string | null;
  source: "local_import" | "modelscope" | "huggingface";
  repository_id: string | null;
  revision: string | null;
  sha256: string;
  size_bytes: number;
  local_path: string;
  quantization: string | null;
  context_length: number | null;
  embedding_dimension: number | null;
  license_name: string | null;
  status: string;
  imported_at: UtcDateTime;
}

export interface ModelEdition {
  edition_id: "light" | "standard";
  name: string;
  description: string;
  recommended_memory_gb: number;
  download_size_bytes: number;
  capabilities: string[];
  artifact: {
    model_id: string;
    role: ModelRole;
    format: ModelFormat;
    source: "huggingface" | "modelscope";
    repository_id: string;
    revision: string;
    file_name: string;
    url: string;
    sha256: string;
    size_bytes: number;
    license_name: string;
  };
}

export interface EnvironmentCheck {
  status: "checking" | "ready" | "degraded" | "failed";
  memory_total_gb: number | null;
  disk_available_gb: number | null;
  gpu_name: string | null;
  gpu_memory_gb: number | null;
  recommended_edition: "light" | "standard" | null;
  runtime_backend: "gpu" | "cpu" | null;
  checked_at: UtcDateTime | null;
  warnings: string[];
}

export interface SummaryMetric {
  key: "today_added" | "awaiting_confirmation" | "possible_duplicates" | "processing_failed";
  label: string;
  value: number;
}

export interface RecentFile {
  file_id: string;
  name: string;
  extension: string;
  subtitle: string;
  modified_at: UtcDateTime;
}

export interface CollectionSummary {
  collection_id: string;
  name: string;
  item_count: number;
  tone: "blue" | "purple" | "green" | "pink";
}

export interface CandidateRoot {
  candidate_id: string;
  candidate_type: "onedrive" | "wechat" | "qq";
  label: string;
  display_path: string;
  status: "suggested" | "adding" | "added" | "ignored";
}

export interface ScanProgress {
  scan_job_id: string;
  status: JobStatus;
  discovered_files: number;
  searchable_files: number;
  parsed_files: number;
  embedded_files: number;
  ocr_pages: number;
  progress: number;
}

export interface HomeSummary {
  local_date: string;
  metrics: SummaryMetric[];
  scan_progress: ScanProgress | null;
  recent_files: RecentFile[];
  favorite_files: RecentFile[];
  collections: CollectionSummary[];
  candidate_roots: CandidateRoot[];
}

export interface SearchRequest {
  query: string;
  scope: ScopeFilter;
  mode: "hybrid" | "filename" | "fulltext" | "semantic";
  sort: "relevance" | "modified_desc" | "name_asc";
  page_size: number;
  cursor: string | null;
}

export interface SearchScore {
  filename: number | null;
  fulltext: number | null;
  semantic: number | null;
  fused: number;
}

export interface SearchHit {
  file: FileRecord;
  revision_id: string;
  snippet: string;
  locator: SourceLocator;
  matched_by: string[];
  scores: SearchScore;
  evidence_ids: string[];
}

export interface SearchBatch {
  search_id: string;
  phase: "filename" | "fulltext" | "semantic" | "completed";
  hits: SearchHit[];
  next_cursor: string | null;
  elapsed_ms: number;
}

export interface SearchResult {
  file_id: string;
  name: string;
  extension: string;
  display_path: string;
  modified_at: UtcDateTime;
  snippet: string;
  match_reasons: Array<"filename" | "path" | "fulltext" | "semantic" | "time_filter">;
  locator: SourceLocator | null;
  revision_id: string | null;
  scores: SearchScore;
}

export interface SearchSession {
  search_id: string;
  status: "running" | "completed" | "cancelled";
  channels: {
    filename: "pending" | "completed" | "unavailable";
    fulltext: "pending" | "completed" | "unavailable";
    semantic: "pending" | "completed" | "unavailable";
  };
  results: SearchResult[];
  next_cursor: string | null;
  elapsed_ms: number;
}

export interface AskRequest {
  question: string;
  session_id: string | null;
  scope: ScopeFilter;
  answer_style: "concise" | "detailed" | "list";
  retrieval_limit: number;
  max_source_files: number;
  strict_evidence: true;
}

export interface AnswerClaim {
  claim_id: string;
  text: string;
  support_status: "supported" | "partial" | "unsupported";
  citations: EvidenceRef[];
}

export interface AnswerSourceFile {
  file_id: string;
  display_name: string;
  display_path: string;
}

export interface AnswerResult {
  session_id: string;
  message_id: string;
  answer: string;
  grounding_status: "grounded" | "partial" | "insufficient";
  insufficient_evidence: boolean;
  claims: AnswerClaim[];
  source_files: AnswerSourceFile[];
  used_file_ids: string[];
  elapsed_ms: number;
  answer_mode: "extractive" | "generated";
}

export interface OperationHandle {
  operation_id: string;
  kind: "ask";
  status: "queued" | "running" | "completed" | "failed" | "cancelled";
  created_at: UtcDateTime;
}

export interface AskOperationSnapshot {
  handle: OperationHandle;
  result: AnswerResult | null;
  error: AppError | null;
}

export interface InboxItem {
  inbox_id: string;
  file_id: string;
  display_name: string;
  display_path: string;
  event_type: "discovered" | "modified" | "renamed" | "missing" | "restored" | "ocr_required" | "parse_failed";
  observed_at: UtcDateTime;
  previous_display_path: string | null;
  triage_status: "new" | "reviewed" | "ignored" | "error";
  suggested_collection_ids: string[];
  duplicate_group_id: string | null;
  summary: string | null;
  error_code: string | null;
}

export interface InboxQuery {
  status: "new" | "reviewed" | "ignored" | "error" | "all";
  event_types: InboxItem["event_type"][];
  root_ids: string[];
  date_from: UtcDateTime | null;
  date_to: UtcDateTime | null;
  cursor: string | null;
  page_size?: number;
}

export interface InboxPage {
  items: InboxItem[];
  next_cursor: string | null;
}

export type TriageStatus = InboxItem["triage_status"];
export type CollectionKind = "manual" | "rule";
export type RuleOperator = "all" | "any";

export interface CollectionRule {
  operator: RuleOperator;
  extensions: string[];
  filename_keywords: string[];
  path_keywords: string[];
  text_keywords: string[];
  parse_statuses: FileRecord["parse_status"][];
  modified_within_days: number | null;
}

export interface CreateCollectionRequest {
  name: string;
  description: string | null;
  icon: string;
  color: string;
  kind: CollectionKind;
  rule: CollectionRule | null;
}

export interface CollectionRecord {
  collection_id: string;
  name: string;
  description: string | null;
  icon: string;
  color: string;
  kind: CollectionKind;
  rule: CollectionRule | null;
  file_count: number;
  built_in: boolean;
  created_at: UtcDateTime;
  updated_at: UtcDateTime;
}

export type RelationType = "exact_duplicate" | "version_candidate" | "related";

export interface FileRelation {
  relation_id: string;
  relation_type: RelationType;
  left_file: FileRecord;
  right_file: FileRecord;
  confidence: number;
  reasons: string[];
  review_status: string;
  created_at: UtcDateTime;
}

export interface RelationQuery {
  cursor: string | null;
  page_size: number;
}

export interface RelationPage {
  items: FileRelation[];
  next_cursor: string | null;
  total: number;
}

export interface RelationRefreshResult {
  hashed_files: number;
  exact_duplicate_pairs: number;
  version_candidate_pairs: number;
}

export interface RootRecord {
  root_id: string;
  path: string;
  canonical_path: string;
  path_key: string;
  root_file_id: string | null;
  volume_id: string;
  volume_type: "fixed" | "removable";
  authorization_source: "system_default" | "user_selected" | "candidate_confirmed";
  root_kind: "known_folder" | "folder" | "volume_root" | "app_candidate";
  label: string;
  enabled: boolean;
  status: "discovering" | "ready" | "scanning" | "partial_denied" | "permission_denied" | "paused" | "offline" | "failed" | "removing";
  watch_mode: "realtime" | "manual";
  coverage_parent_root_id: string | null;
  file_count: number;
  permission_error_count: number;
  last_scan_at: UtcDateTime | null;
}

export interface RootDiscoveryResult {
  roots: RootRecord[];
  failures: Array<{ label: string; code: string; message: string }>;
}

export interface AddRootRequest {
  path: string;
  label: string | null;
  watch_mode: "realtime" | "manual";
  authorization_source: "system_default" | "user_selected" | "candidate_confirmed";
  full_volume_confirmed: boolean;
}

export interface FileRecord {
  file_id: string;
  volume_id: string;
  display_path: string;
  display_name: string;
  extension: string;
  mime_type: string;
  size_bytes: number;
  fs_created_at: UtcDateTime | null;
  fs_modified_at: UtcDateTime;
  windows_file_id: string | null;
  content_sha256: string | null;
  availability: Availability;
  current_revision_id: string | null;
  parse_status: "pending" | "parsing" | "ocr_pending" | "parsed" | "unsupported" | "encrypted" | "failed";
  first_seen_at: UtcDateTime;
  last_seen_at: UtcDateTime;
}

export interface FileQuery {
  cursor: string | null;
  page_size: number;
}

export interface FilePage {
  items: FileRecord[];
  next_cursor: string | null;
  total: number;
}

export interface DocumentNode {
  node_id: string;
  parent_id: string | null;
  ordinal: number;
  node_type: string;
  text: string | null;
  table_data: Record<string, unknown> | null;
  locator: SourceLocator;
  heading_path: string[];
}

export interface FilePreview {
  file: FileRecord;
  revision_id: string | null;
  nodes: DocumentNode[];
  offset: number;
  next_offset: number | null;
  anchor_node_id: string | null;
  truncated: boolean;
}

export interface ExtractionField {
  key: string;
  label: string;
  field_type: string;
  description: string;
  required: boolean;
  multiple: boolean;
  hints: string[];
}

export interface ExtractionPreset {
  preset_id: string;
  name: string;
  description: string;
  fields: ExtractionField[];
}

export interface ExtractedValue {
  field_key: string;
  raw_value: unknown;
  normalized_value: unknown;
  confidence: number;
  method: string;
  review_state: "auto" | "missing" | "needs_review" | "confirmed" | "rejected";
  evidence: EvidenceRef[];
  validation_errors: string[];
}

export interface ExtractionRunResult {
  run_id: string;
  preset: ExtractionPreset;
  status: string;
  rows: Array<{ file: FileRecord; values: ExtractedValue[] }>;
  completed_at: UtcDateTime;
  warnings: string[];
}

export interface ExportResult {
  target_path: string;
  format: "csv" | "json" | "xlsx" | "docx";
  row_count: number;
  size_bytes: number;
  sha256: string;
}

export interface HealthCheckItem {
  key: string;
  label: string;
  status: "passed" | "warning" | "failed";
  detail: string;
}

export interface MaintenanceSnapshot {
  schema_version: number;
  database_size_bytes: number;
  indexed_files: number;
  searchable_chunks: number;
  embedded_chunks: number;
  pending_files: number;
  failed_files: number;
  active_jobs: number;
  log_events: number;
  degradation_level: "full" | "balanced" | "core";
  degradation_reasons: string[];
  checks: HealthCheckItem[];
  checked_at: UtcDateTime;
}

export interface MaintenanceCheckResult {
  level: "quick" | "full";
  database_result: string;
  elapsed_ms: number;
  source_files_modified: false;
}

export interface AppLogRecord {
  log_id: string;
  level: string;
  component: string;
  event_name: string;
  fields: Record<string, unknown>;
  created_at: UtcDateTime;
}

export interface LogQuery {
  cursor: string | null;
  page_size: number;
}

export interface LogPage {
  items: AppLogRecord[];
  next_cursor: string | null;
  total: number;
}

export interface IndexRebuildResult {
  reset_files: number;
  removed_nodes: number;
  removed_chunks: number;
  removed_embeddings: number;
  source_files_modified: false;
}

export interface SkillDefinition {
  skill_id: string;
  name: string;
  description: string;
  available: boolean;
  unavailable_reason: string | null;
  risk_level: "low" | "medium" | "high";
  source_files_readonly: true;
  export_required: boolean;
}

export interface TaskStep {
  step_id: string;
  ordinal: number;
  step_type: string;
  label: string;
  inputs: Record<string, unknown>;
  expected_outputs: Record<string, unknown>;
  status: "pending" | "running" | "succeeded" | "failed" | "skipped";
  attempt_count: number;
  checkpoint: string;
  error: AppError | null;
}

export interface TaskPlan {
  task_id: string;
  skill_id: string;
  skill_version: string;
  summary: string;
  steps: TaskStep[];
  estimated_file_count: number;
  warnings: string[];
}

export interface TaskExecutionResult {
  plan: TaskPlan;
  job: JobRecord;
  result: ExtractionRunResult;
  checkpoints: ValidationCheckpoint[];
  candidates: ExplorationCandidate[];
}

export interface ReminBridge {
  startup_get_state(): Promise<StartupState>;
  welcome_get_state(): Promise<WelcomeState>;
  welcome_complete(welcome_version: string): Promise<WelcomeState>;
  environment_get_latest(): Promise<EnvironmentCheck | null>;
  environment_detect(): Promise<EnvironmentCheck>;
  model_state_get(): Promise<ModelRuntimeState>;
  model_import_scan(paths: string[]): Promise<ImportCandidate[]>;
  model_import_confirm(selections: Array<{ source_path: string; role: ModelRole }>): Promise<ModelArtifact[]>;
  model_artifact_list(): Promise<ModelArtifact[]>;
  model_catalog_list(): Promise<ModelEdition[]>;
  model_download_install(edition_id: ModelEdition["edition_id"], source: "huggingface" | "modelscope", confirmed: true): Promise<ModelArtifact>;
  model_artifact_activate(artifact_id: string): Promise<ModelRuntimeState>;
  home_get_summary(local_date: string): Promise<HomeSummary>;
  candidate_root_action(candidate_id: string, action: "add" | "ignore"): Promise<CandidateRoot>;
  search_start(request: SearchRequest): Promise<SearchSession>;
  ask_start(request: AskRequest): Promise<OperationHandle>;
  ask_operation_get(operation_id: string): Promise<AskOperationSnapshot>;
  ask_cancel(operation_id: string): Promise<AskOperationSnapshot>;
  preview_get(file_id: string, offset?: number, limit?: number, anchor_node_id?: string | null): Promise<FilePreview>;
  file_open(file_id: string): Promise<void>;
  file_reveal(file_id: string): Promise<void>;
  inbox_query(request: InboxQuery): Promise<InboxPage>;
  inbox_update(inbox_id: string, triage_status: TriageStatus): Promise<InboxItem>;
  ocr_retry(file_id: string): Promise<boolean>;
  collection_list(): Promise<CollectionRecord[]>;
  collection_create(request: CreateCollectionRequest): Promise<CollectionRecord>;
  collection_update(collection_id: string, request: CreateCollectionRequest): Promise<CollectionRecord>;
  collection_delete(collection_id: string): Promise<void>;
  collection_rule_preview(rule: CollectionRule, limit: number): Promise<FileRecord[]>;
  collection_file_query(collection_id: string, request: FileQuery): Promise<FilePage>;
  collection_add_file(collection_id: string, file_id: string): Promise<void>;
  collection_remove_file(collection_id: string, file_id: string): Promise<void>;
  relation_refresh(max_files: number): Promise<RelationRefreshResult>;
  relation_query(request: RelationQuery): Promise<RelationPage>;
  relation_review(relation_id: string, action: "accepted" | "rejected"): Promise<void>;
  file_query(request: FileQuery): Promise<FilePage>;
  extraction_preset_list(): Promise<ExtractionPreset[]>;
  extraction_run(file_ids: string[], preset_id: string): Promise<ExtractionRunResult>;
  skill_list(): Promise<SkillDefinition[]>;
  task_plan(skill_id: string, file_ids: string[], parameters: Record<string, unknown>, user_instruction?: string | null): Promise<TaskPlan>;
  task_execute(skill_id: string, file_ids: string[], parameters: Record<string, unknown>, planned_task_id: string, user_instruction?: string | null): Promise<TaskExecutionResult>;
  task_recoverable(): Promise<TaskPlan | null>;
  task_resume(task_id: string): Promise<TaskExecutionResult>;
  extraction_export(run: ExtractionRunResult, format: ExportResult["format"], target_path: string): Promise<ExportResult>;
  maintenance_get(): Promise<MaintenanceSnapshot>;
  maintenance_check(level: "quick" | "full"): Promise<MaintenanceCheckResult>;
  maintenance_log_query(request: LogQuery): Promise<LogPage>;
  maintenance_logs_clear(): Promise<number>;
  diagnostic_export(target_path: string, confirmed: true): Promise<ExportResult>;
  index_rebuild(confirmation: "REBUILD_INDEX"): Promise<IndexRebuildResult>;
  root_discover_defaults(): Promise<RootDiscoveryResult>;
  root_list(): Promise<RootRecord[]>;
  root_add(request: AddRootRequest): Promise<RootRecord>;
  root_disable(root_id: string): Promise<void>;
  scan_start(root_id: string, reason: string): Promise<JobRecord>;
  scan_pause(job_id: string): Promise<JobRecord>;
  scan_resume(job_id: string): Promise<JobRecord>;
  scan_cancel(job_id: string): Promise<JobRecord>;
}
