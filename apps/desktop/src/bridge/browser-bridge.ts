import type {
  AnswerResult,
  AskOperationSnapshot,
  CandidateRoot,
  CollectionRecord,
  CollectionSuggestion,
  EnvironmentCheck,
  ExclusionRule,
  FileRecord,
  HomeSummary,
  InboxPage,
  KnowledgeSpace,
  ModelEdition,
  ModelDownloadJob,
  ModelRuntimeState,
  ModelArtifact,
  ReminBridge,
  RootRecord,
  SearchRequest,
  SearchSession,
  ThemePreference,
  WelcomeState,
} from "./contracts";

const WELCOME_KEY = "remin.welcome.v1";
const THEME_KEY = "remin.theme.v1";
const MODEL_KEY = "remin.model.state.v1";

const now = () => new Date().toISOString();

const initialCandidates: CandidateRoot[] = [
  {
    candidate_id: "018f0000-0000-7000-8000-000000000101",
    candidate_type: "onedrive",
    label: "OneDrive",
    display_path: "D:\\OneDrive",
    status: "suggested",
  },
  {
    candidate_id: "018f0000-0000-7000-8000-000000000102",
    candidate_type: "wechat",
    label: "微信接收文件",
    display_path: "D:\\WeChat Files",
    status: "suggested",
  },
];

let candidates = structuredClone(initialCandidates);
const browserAskOperations = new Map<string, AskOperationSnapshot>();

const recentFiles = [
  {
    file_id: "018f0000-0000-7000-8000-000000000201",
    name: "项目总结.docx",
    extension: "docx",
    subtitle: "今天 10:24",
    modified_at: now(),
  },
  {
    file_id: "018f0000-0000-7000-8000-000000000202",
    name: "面试记录.pdf",
    extension: "pdf",
    subtitle: "昨天 16:18",
    modified_at: now(),
  },
  {
    file_id: "018f0000-0000-7000-8000-000000000203",
    name: "学习资料.xlsx",
    extension: "xlsx",
    subtitle: "昨天 09:32",
    modified_at: now(),
  },
];

const makeSummary = (localDate: string): HomeSummary => ({
  local_date: localDate,
  metrics: [
    { key: "today_added", label: "今日新增", value: 12 },
    { key: "awaiting_confirmation", label: "待确认", value: 5 },
    { key: "possible_duplicates", label: "可能重复", value: 3 },
    { key: "processing_failed", label: "处理失败", value: 2 },
  ],
  scan_progress: {
    scan_job_id: "018f0000-0000-7000-8000-000000000301",
    status: "running",
    discovered_files: 1284,
    searchable_files: 830,
    parsed_files: 830,
    embedded_files: 326,
    ocr_pages: 42,
    progress: 0.84,
  },
  recent_files: recentFiles,
  favorite_files: [
    { ...recentFiles[0]!, name: "重要项目资料", extension: "folder", subtitle: "12 项" },
    { ...recentFiles[1]!, name: "产品设计规范.pdf", subtitle: "2024/05/12" },
    { ...recentFiles[0]!, file_id: "018f0000-0000-7000-8000-000000000204", name: "读书笔记.docx", subtitle: "2024/04/28" },
  ],
  collections: [
    { collection_id: "c1", name: "本周工作相关", item_count: 86, tone: "purple" },
    { collection_id: "c2", name: "2024年项目资料", item_count: 152, tone: "green" },
    { collection_id: "c3", name: "未归档资料", item_count: 27, tone: "pink" },
  ],
  candidate_roots: candidates.filter((candidate) => candidate.status === "suggested"),
});

const defaultEnvironment: EnvironmentCheck = {
  status: "ready",
  memory_total_gb: 16,
  disk_available_gb: 86,
  gpu_name: null,
  gpu_memory_gb: null,
  recommended_edition: "light",
  runtime_backend: "cpu",
  checked_at: now(),
  warnings: [],
};

const defaultModelState: ModelRuntimeState = {
  status: "unconfigured",
  active_profile_id: null,
  active_profile_name: null,
  runtime_backend: null,
  message: "未配置本地模型",
  checked_at: now(),
  capabilities: { generation: false, embedding: false, vision: false, reranker: false, ocr: false },
  rag_complete: false,
  semantic_index_coverage: 0,
  embedding_migration: null,
};

let demoArtifacts: ModelArtifact[] = [];
let demoDownloadJobs: ModelDownloadJob[] = [];
const demoActiveRoles = new Set<string>();
const demoEmbeddingArtifact: ModelEdition["artifacts"][number] = {
  model_id: "bge-small-zh-v1.5-onnx-int8",
  role: "embedding",
  format: "onnx",
  source: "huggingface",
  repository_id: "onnx-community/bge-small-zh-v1.5-ONNX",
  revision: "9507db33464b5da99a532ac26b2a251767cbc62b",
  file_name: "model_quantized.onnx",
  url: "https://huggingface.co/",
  sha256: "99a6e522710c00220c89f8c52e0cc5aa09d4cbb1c34c0e932eab3a9dfdc65df3",
  size_bytes: 168_002,
  companion_files: [
    { file_name: "model_quantized.onnx_data", remote_path: "onnx/model_quantized.onnx_data", url: "https://huggingface.co/", sha256: "952623481ca8beea884e3d3c9ecaf8a3c7bf1d0c21de29e970cd31af9d37a90b", size_bytes: 23_774_208 },
    { file_name: "tokenizer.json", remote_path: "tokenizer.json", url: "https://huggingface.co/", sha256: "3d09c84edca9190e376e5dd8de731aa4d36b12f97f1a359f837e43390b0a4f8e", size_bytes: 362_603 },
  ],
  license_name: "MIT",
  query_prefix: "为这个句子生成表示以用于检索相关文章：",
  max_length: 512,
};
const demoModelEditions: ModelEdition[] = [
  { edition_id: "light", name: "轻量版", description: "0.6B 本地生成模型与中文语义检索组件", recommended_memory_gb: 8, download_size_bytes: 663_751_501, capabilities: ["generation", "embedding", "rag"], artifacts: [{ model_id: "Qwen3-0.6B-Q8_0", role: "generation", format: "gguf", source: "huggingface", repository_id: "Qwen/Qwen3-0.6B-GGUF", revision: "1eaf4d9657fe65ad10a51eab76a8db5b363bddaa", file_name: "Qwen3-0.6B-Q8_0.gguf", url: "https://huggingface.co/", sha256: "9465e63e7ce6826db57337865524b63eb4cda2ed96645ea2961819577e8c2031", size_bytes: 639_446_688, companion_files: [], license_name: "Apache-2.0", query_prefix: null, max_length: null }, demoEmbeddingArtifact] },
  { edition_id: "standard", name: "标准版", description: "4B 本地生成模型与中文语义检索组件", recommended_memory_gb: 12, download_size_bytes: 2_521_585_069, capabilities: ["generation", "embedding", "rag"], artifacts: [{ model_id: "Qwen3-4B-Q4_K_M", role: "generation", format: "gguf", source: "huggingface", repository_id: "Qwen/Qwen3-4B-GGUF", revision: "a9a60d009fa7ff9606305047c2bf77ac25dbec49", file_name: "Qwen3-4B-Q4_K_M.gguf", url: "https://huggingface.co/", sha256: "7485fe1dba41cf264d151854608429340702f1f212eaec0ad44391f06366bdf5", size_bytes: 2_497_280_256, companion_files: [], license_name: "Apache-2.0", query_prefix: null, max_length: null }, demoEmbeddingArtifact] },
];

const roots: RootRecord[] = [];

const searchCatalog = [
  { file_id: "s1", name: "RAG项目总结.docx", extension: "docx", path: "文档\\项目资料", modified_at: "2025-11-08T03:00:00Z", snippet: "通过混合召回和重排提升召回率，并对关键词与语义结果进行融合。" },
  { file_id: "s2", name: "检索效果评估.xlsx", extension: "xlsx", path: "文档\\大模型学习", modified_at: "2025-10-16T03:00:00Z", snippet: "记录不同召回策略、Top K 与命中率的对比结果。" },
  { file_id: "s3", name: "产品设计规范.pdf", extension: "pdf", path: "桌面\\拾忆", modified_at: "2025-09-20T03:00:00Z", snippet: "本地统一搜索采用文件名、全文和语义三个通道。" },
];

const inbox: InboxPage = {
  items: [
    { inbox_id: "i1", file_id: "s1", display_name: "RAG项目总结.docx", display_path: "文档\\项目资料\\RAG项目总结.docx", event_type: "discovered", observed_at: now(), previous_display_path: null, triage_status: "new", suggested_collection_ids: [], duplicate_group_id: null, summary: "新增资料，已完成正文提取，可以全文搜索", error_code: null },
    { inbox_id: "i2", file_id: "s2", display_name: "检索效果评估.xlsx", display_path: "文档\\大模型学习\\检索效果评估.xlsx", event_type: "modified", observed_at: now(), previous_display_path: null, triage_status: "new", suggested_collection_ids: [], duplicate_group_id: "duplicate-demo-1", summary: "资料有新版本，并发现一份字节完全相同的副本", error_code: null },
    { inbox_id: "i3", file_id: "s3", display_name: "产品设计规范.pdf", display_path: "桌面\\拾忆\\产品设计规范.pdf", event_type: "ocr_required", observed_at: now(), previous_display_path: null, triage_status: "error", suggested_collection_ids: [], duplicate_group_id: null, summary: "当前未安装 OCR 模型，已保留文件名索引", error_code: "OCR_REQUIRED" },
  ],
  next_cursor: null,
};

let collectionRecords: CollectionRecord[] = [
  { collection_id: "018f0000-0000-7000-8000-000000001001", name: "最近7天", description: "最近7天修改过的资料", icon: "calendar", color: "#8c7cf0", kind: "rule", rule: { operator: "all", extensions: [], filename_keywords: [], path_keywords: [], text_keywords: [], parse_statuses: [], modified_within_days: 7, min_size_bytes: null, max_size_bytes: null, exclude_extensions: [], exclude_filename_keywords: [], exclude_path_keywords: [], exclude_text_keywords: [] }, file_count: 12, built_in: true, created_at: now(), updated_at: now() },
  { collection_id: "018f0000-0000-7000-8000-000000001002", name: "待处理资料", description: "等待解析、OCR或处理失败的资料", icon: "pending", color: "#e7a6ba", kind: "rule", rule: { operator: "any", extensions: [], filename_keywords: [], path_keywords: [], text_keywords: [], parse_statuses: ["pending", "ocr_pending", "failed"], modified_within_days: null, min_size_bytes: null, max_size_bytes: null, exclude_extensions: [], exclude_filename_keywords: [], exclude_path_keywords: [], exclude_text_keywords: [] }, file_count: 2, built_in: true, created_at: now(), updated_at: now() },
  { collection_id: "018f0000-0000-7000-8000-000000001003", name: "PDF资料", description: "全部可访问的PDF资料", icon: "pdf", color: "#71a7ca", kind: "rule", rule: { operator: "all", extensions: ["pdf"], filename_keywords: [], path_keywords: [], text_keywords: [], parse_statuses: [], modified_within_days: null, min_size_bytes: null, max_size_bytes: null, exclude_extensions: [], exclude_filename_keywords: [], exclude_path_keywords: [], exclude_text_keywords: [] }, file_count: 31, built_in: true, created_at: now(), updated_at: now() },
];

const demoFileRecords = (): FileRecord[] => searchCatalog.map((file) => ({
  file_id: file.file_id,
  volume_id: "vol-demo",
  display_path: `${file.path}\\${file.name}`,
  display_name: file.name,
  extension: file.extension,
  mime_type: "application/octet-stream",
  size_bytes: 1024,
  fs_created_at: null,
  fs_modified_at: file.modified_at,
  windows_file_id: null,
  content_sha256: null,
  availability: "present",
  current_revision_id: "018f0000-0000-7000-8000-000000000601",
  parse_status: "parsed",
  first_seen_at: file.modified_at,
  last_seen_at: file.modified_at,
}));
let demoSuggestions: CollectionSuggestion[] = [];
let demoKnowledgeSpaces: KnowledgeSpace[] = [];
let demoExclusionRules: ExclusionRule[] = [
  { rule_id: "018f0000-0000-7000-8000-000000009001", root_id: null, rule_class: "hard", rule_type: "path_name", value: "windows", enabled: true, overridable: false },
  { rule_id: "018f0000-0000-7000-8000-000000009002", root_id: null, rule_class: "default", rule_type: "path_name", value: "node_modules", enabled: true, overridable: true },
];

export const browserBridge: ReminBridge = {
  async startup_get_state() {
    return {
      phase: "ready",
      ready: true,
      progress: 1,
      pending_files: 0,
      blocker: null,
      recovery_actions: [],
    };
  },
  async welcome_get_state() {
    const forced = new URLSearchParams(window.location.search).get("welcome");
    if (forced === "1") {
      return { welcome_version: "1.0", welcome_completed: false, welcome_completed_at: null, root_authorization_completed: false, root_authorization_completed_at: null };
    }
    if (forced === "0") {
      return { welcome_version: "1.0", welcome_completed: true, welcome_completed_at: now(), root_authorization_completed: false, root_authorization_completed_at: null };
    }
    const stored = window.localStorage.getItem(WELCOME_KEY);
    if (!stored) return { welcome_version: "1.0", welcome_completed: false, welcome_completed_at: null, root_authorization_completed: false, root_authorization_completed_at: null };
    try {
      return JSON.parse(stored) as WelcomeState;
    } catch {
      return { welcome_version: "1.0", welcome_completed: false, welcome_completed_at: null, root_authorization_completed: false, root_authorization_completed_at: null };
    }
  },
  async welcome_complete(welcome_version) {
    const current = await this.welcome_get_state();
    const state: WelcomeState = { ...current, welcome_version, welcome_completed: true, welcome_completed_at: now() };
    window.localStorage.setItem(WELCOME_KEY, JSON.stringify(state));
    return state;
  },
  async welcome_authorization_complete() {
    const current = await this.welcome_get_state();
    const state: WelcomeState = { ...current, root_authorization_completed: true, root_authorization_completed_at: now() };
    window.localStorage.setItem(WELCOME_KEY, JSON.stringify(state));
    return state;
  },
  async theme_get_state(system_dark) {
    const preference = (window.localStorage.getItem(THEME_KEY) as ThemePreference | null) ?? "system";
    return { preference, effective_theme: preference === "night_dark" || (preference === "system" && system_dark) ? "night_dark" : "day_gradient", updated_at: null };
  },
  async theme_set_preference(preference, system_dark) {
    window.localStorage.setItem(THEME_KEY, preference);
    return { preference, effective_theme: preference === "night_dark" || (preference === "system" && system_dark) ? "night_dark" : "day_gradient", updated_at: now() };
  },
  async environment_get_latest() {
    return defaultEnvironment;
  },
  async environment_detect() {
    return defaultEnvironment;
  },
  async model_state_get() {
    const stored = window.localStorage.getItem(MODEL_KEY);
    if (stored) return JSON.parse(stored) as ModelRuntimeState;
    const embedding = demoActiveRoles.has("embedding");
    return embedding ? { ...defaultModelState, status: "ready", message: "语义检索已就绪，问资料仍需生成模型", runtime_backend: "cpu", capabilities: { ...defaultModelState.capabilities, embedding: true } } : defaultModelState;
  },
  async model_import_scan(paths) {
    return paths.map((path, index) => ({ candidate_id: crypto.randomUUID(), source_path: path, display_name: path.split(/[\\/]/).pop() || `model-${index + 1}`, format: path.toLowerCase().endsWith(".gguf") ? "gguf" as const : "onnx" as const, suggested_role: path.toLowerCase().endsWith(".gguf") ? "generation" as const : "embedding" as const, size_bytes: 1024 * 1024 * (index + 1), sha256: "0".repeat(64), companion_files: [], warnings: [] }));
  },
  async model_import_confirm(selections) {
    const installed = selections.map((selection) => ({ artifact_id: crypto.randomUUID(), role: selection.role, format: selection.source_path.toLowerCase().endsWith(".gguf") ? "gguf" as const : "onnx" as const, model_id: selection.source_path.split(/[\\/]/).pop() || "local-model", model_version: null, source: "local_import" as const, repository_id: null, revision: null, sha256: "0".repeat(64), size_bytes: 1024 * 1024, local_path: selection.source_path, quantization: null, context_length: null, embedding_dimension: null, query_prefix: null, max_length: null, license_name: null, status: "ready", imported_at: now() }));
    demoArtifacts = [...demoArtifacts, ...installed];
    return installed;
  },
  async model_artifact_list() {
    return structuredClone(demoArtifacts);
  },
  async model_role_config_list() {
    return ([
      ["generation", "严格证据问答与回答组织", false, "on_demand"],
      ["embedding", "语义检索与文档关系", false, "background_index"],
      ["vision", "图片、图表与扫描页理解", false, "serial_on_demand"],
      ["reranker", "候选证据精排", true, "on_demand"],
    ] as const).map(([role, required_for, optional, load_policy]) => ({
      role,
      required_for,
      optional,
      load_policy,
      active_artifact_id: demoArtifacts.find((artifact) => artifact.role === role && demoActiveRoles.has(role))?.artifact_id ?? null,
    }));
  },
  async model_catalog_list() {
    return structuredClone(demoModelEditions);
  },
  async model_download_start(edition_id, source) {
    const edition = demoModelEditions.find((item) => item.edition_id === edition_id);
    if (!edition) throw new Error("模型版本不存在");
    const installed = edition.artifacts.map<ModelArtifact>((artifact) => ({ artifact_id: crypto.randomUUID(), role: artifact.role, format: artifact.format, model_id: artifact.model_id, model_version: null, source, repository_id: artifact.repository_id, revision: artifact.revision, sha256: artifact.sha256, size_bytes: artifact.size_bytes, local_path: `C:\\Users\\你\\AppData\\Roaming\\com.remin.desktop\\models\\${artifact.file_name}`, quantization: artifact.role === "generation" ? (edition_id === "standard" ? "Q4_K_M" : "Q8_0") : null, context_length: null, embedding_dimension: artifact.role === "embedding" ? 512 : null, query_prefix: artifact.query_prefix, max_length: artifact.max_length, license_name: artifact.license_name, status: "ready", imported_at: now() }));
    demoArtifacts = [...demoArtifacts, ...installed];
    installed.forEach((artifact) => demoActiveRoles.add(artifact.role));
    const job: ModelDownloadJob = { job_id: crypto.randomUUID(), edition_id, edition_name: edition.name, source, status: "completed", phase: "completed", downloaded_bytes: edition.download_size_bytes, total_bytes: edition.download_size_bytes, progress: 1, bytes_per_second: 0, eta_seconds: null, retry_count: 0, current_file: null, files: edition.artifacts.flatMap((artifact) => [{ role: artifact.role, file_name: artifact.file_name, downloaded_bytes: artifact.size_bytes, total_bytes: artifact.size_bytes, status: "completed" }, ...artifact.companion_files.map((file) => ({ role: artifact.role, file_name: file.file_name, downloaded_bytes: file.size_bytes, total_bytes: file.size_bytes, status: "completed" }))]), installed_artifact_ids: installed.map((artifact) => artifact.artifact_id), profile_id: crypto.randomUUID(), error: null, created_at: now(), updated_at: now() };
    demoDownloadJobs = [job, ...demoDownloadJobs];
    return structuredClone(job);
  },
  async model_download_list() {
    return structuredClone(demoDownloadJobs);
  },
  async model_download_get(job_id) {
    const job = demoDownloadJobs.find((item) => item.job_id === job_id);
    if (!job) throw new Error("下载任务不存在");
    return structuredClone(job);
  },
  async model_download_pause(job_id) {
    const job = demoDownloadJobs.find((item) => item.job_id === job_id);
    if (!job) throw new Error("下载任务不存在");
    job.status = "paused";
    job.phase = "paused";
    job.updated_at = now();
    return structuredClone(job);
  },
  async model_download_cancel(job_id) {
    const job = demoDownloadJobs.find((item) => item.job_id === job_id);
    if (!job) throw new Error("下载任务不存在");
    job.status = "cancelled";
    job.phase = "cancelled";
    job.updated_at = now();
    return structuredClone(job);
  },
  async model_download_retry(job_id, source) {
    const previous = demoDownloadJobs.find((item) => item.job_id === job_id);
    if (!previous) throw new Error("下载任务不存在");
    return browserBridge.model_download_start(previous.edition_id, source ?? previous.source, true);
  },
  async model_artifact_activate(artifact_id) {
    const artifact = demoArtifacts.find((item) => item.artifact_id === artifact_id);
    if (!artifact) throw new Error("模型组件不存在");
    demoActiveRoles.add(artifact.role);
    return { ...defaultModelState, status: "ready", message: artifact.role === "generation" ? "本地生成模型已配置" : "语义检索已就绪", runtime_backend: "cpu", capabilities: { ...defaultModelState.capabilities, [artifact.role]: true } };
  },
  async home_get_summary(local_date) {
    return makeSummary(local_date);
  },
  async candidate_root_action(candidate_id, action) {
    const candidate = candidates.find((item) => item.candidate_id === candidate_id);
    if (!candidate) throw new Error("候选资料来源不存在");
    candidate.status = action === "add" ? "added" : "ignored";
    return structuredClone(candidate);
  },
  async search_start(request: SearchRequest) {
    const normalized = request.query.trim().toLocaleLowerCase("zh-CN");
    const results = searchCatalog
      .filter((file) => !normalized || `${file.name} ${file.path} ${file.snippet}`.toLocaleLowerCase("zh-CN").includes(normalized) || normalized.length > 3)
      .map((file, index) => ({
        file_id: file.file_id,
        name: file.name,
        extension: file.extension,
        display_path: file.path,
        modified_at: file.modified_at,
        snippet: file.snippet,
        match_reasons: (index === 0 ? ["filename", "fulltext"] : ["fulltext"]) as Array<"filename" | "fulltext">,
        locator: { kind: file.extension === "pdf" ? "pdf" as const : file.extension === "xlsx" ? "spreadsheet" as const : "docx" as const, page_no: file.extension === "pdf" ? 3 : null, slide_no: null, sheet_name: file.extension === "xlsx" ? "评估结果" : null, cell_range: file.extension === "xlsx" ? "B4:F12" : null, paragraph_no: file.extension === "docx" ? 18 : null, line_start: null, line_end: null, shape_no: null, bbox: null, heading_path: ["检索优化"] },
        revision_id: "018f0000-0000-7000-8000-000000000601",
        scores: { filename: index === 0 ? 0.9 : null, fulltext: 0.8, semantic: null, fused: 0.03 },
      }));
      const offset = request.cursor?.startsWith("demo:") ? Number(request.cursor.slice(5)) || 0 : 0;
      const page = results.slice(offset, offset + request.page_size);
      const session: SearchSession = {
      search_id: crypto.randomUUID(),
      status: "completed",
      channels: { filename: "completed", fulltext: "completed", semantic: "unavailable" },
        results: page,
        next_cursor: offset + request.page_size < results.length ? `demo:${offset + request.page_size}` : null,
      elapsed_ms: 18,
    };
    return session;
  },
  async rag_readiness_get() {
    const generation_ready = demoActiveRoles.has("generation");
    const embedding_ready = demoActiveRoles.has("embedding");
    const vision_ready = demoActiveRoles.has("vision");
    const blockers = [
      ...(!generation_ready ? [{ code: "RAG_GENERATION_MISSING", message: "未配置已通过自检的本地生成模型", retryable: true, user_action: null, file_id: null, details: null }] : []),
      ...(!embedding_ready ? [{ code: "RAG_EMBEDDING_MISSING", message: "未配置已通过自检的中文 Embedding 模型", retryable: true, user_action: null, file_id: null, details: null }] : []),
    ];
    return { ready: blockers.length === 0, generation_ready, embedding_ready, vision_ready, semantic_index_coverage: embedding_ready ? 1 : 0, scope_index_coverage: embedding_ready ? 1 : 0, image_index_coverage: 1, pending_image_assets: 0, background_notice: null, blockers, checked_at: now() };
  },
  async ask_start(request) {
    const search = await browserBridge.search_start({
      query: request.question,
      scope: request.scope,
      mode: "hybrid",
      sort: "relevance",
      page_size: 30,
      cursor: null,
    });
    const matches = search.results
      .filter((result) => result.locator && result.revision_id)
      .slice(0, request.retrieval_limit);
    const session_id = request.session_id ?? crypto.randomUUID();
    let result: AnswerResult;
    if (matches.length === 0) {
      result = {
        session_id,
        message_id: crypto.randomUUID(),
        answer: "当前资料中未找到足够依据。你可以换一种说法、扩大检索范围，或等待相关资料完成索引。",
        grounding_status: "insufficient",
        insufficient_evidence: true,
        claims: [],
        source_files: [],
        used_file_ids: [],
        elapsed_ms: search.elapsed_ms,
        answer_mode: "extractive",
        retrieval_channels: ["filename", "fts"],
        index_coverage: 0,
        degradation_reason: request.mode === "evidence_extracts" ? "用户明确选择证据摘录模式" : null,
      };
    } else {
      const source_files = matches.map((match) => ({
        file_id: match.file_id,
        display_name: match.name,
      display_path: match.display_path,
      }));
      const claims = matches.map((match) => ({
        claim_id: crypto.randomUUID(),
        text: match.snippet,
        support_status: "supported" as const,
        citations: [{
          evidence_id: crypto.randomUUID(),
          file_id: match.file_id,
          revision_id: match.revision_id!,
          node_id: crypto.randomUUID(),
          chunk_id: crypto.randomUUID(),
          image_asset_id: null,
          quote: match.snippet,
          locator: match.locator!,
          retrieval_score: match.scores.fused,
        }],
      }));
      result = {
        session_id,
        message_id: crypto.randomUUID(),
        answer: `在你的本地资料中找到这些直接依据：\n${claims.map((claim, index) => `${index + 1}. ${claim.text}`).join("\n")}`,
        grounding_status: "grounded",
        insufficient_evidence: false,
        claims,
        source_files,
        used_file_ids: source_files.map((source) => source.file_id),
        elapsed_ms: search.elapsed_ms,
        answer_mode: request.mode === "rag" ? "generated" : "extractive",
        retrieval_channels: request.mode === "rag" ? ["filename", "fts", "embedding", "rrf"] : ["filename", "fts"],
        index_coverage: request.mode === "rag" ? 1 : 0,
        degradation_reason: request.mode === "evidence_extracts" ? "用户明确选择证据摘录模式" : null,
      };
    }
    const operation_id = crypto.randomUUID();
    const snapshot: AskOperationSnapshot = {
      handle: { operation_id, kind: "ask", status: "completed", created_at: now() },
      result,
      error: null,
    };
    browserAskOperations.set(operation_id, snapshot);
    return snapshot.handle;
  },
  async ask_operation_get(operation_id) {
    const snapshot = browserAskOperations.get(operation_id);
    if (!snapshot) throw new Error("问答操作不存在或已经过期");
    return snapshot;
  },
  async ask_cancel(operation_id) {
    const snapshot = browserAskOperations.get(operation_id);
    if (!snapshot) throw new Error("问答操作不存在或已经过期");
    if (snapshot.handle.status === "completed") return snapshot;
    const cancelled: AskOperationSnapshot = {
      handle: { ...snapshot.handle, status: "cancelled" },
      result: null,
      error: { code: "OPERATION_CANCELLED", message: "问答已取消", retryable: false, user_action: null, file_id: null, details: null },
    };
    browserAskOperations.set(operation_id, cancelled);
    return cancelled;
  },
  async preview_get(file_id, offset = 0, _limit = 80, anchor_node_id = null) {
    const file = searchCatalog.find((item) => item.file_id === file_id);
    if (!file) throw new Error("文件记录不存在");
    return {
      file: {
        file_id: file.file_id,
        volume_id: "vol-demo",
        display_path: file.path,
        display_name: file.name,
        extension: file.extension,
        mime_type: "application/octet-stream",
        size_bytes: 1024,
        fs_created_at: null,
        fs_modified_at: file.modified_at,
        windows_file_id: null,
        content_sha256: null,
        availability: "present" as const,
        current_revision_id: "018f0000-0000-7000-8000-000000000601",
        parse_status: "parsed" as const,
        first_seen_at: file.modified_at,
        last_seen_at: file.modified_at,
      },
      revision_id: "018f0000-0000-7000-8000-000000000601",
      nodes: [{
        node_id: "018f0000-0000-7000-8000-000000000602",
        parent_id: null,
        ordinal: 1,
        node_type: "paragraph",
        text: file.snippet,
        table_data: null,
        locator: { kind: file.extension === "pdf" ? "pdf" as const : file.extension === "xlsx" ? "spreadsheet" as const : "docx" as const, page_no: file.extension === "pdf" ? 3 : null, slide_no: null, sheet_name: file.extension === "xlsx" ? "评估结果" : null, cell_range: file.extension === "xlsx" ? "B4:F12" : null, paragraph_no: file.extension === "docx" ? 18 : null, line_start: null, line_end: null, shape_no: null, bbox: null, heading_path: ["检索优化"] },
        heading_path: ["检索优化"],
      }],
      image_assets: [],
      offset,
      next_offset: null,
      anchor_node_id,
      truncated: false,
    };
  },
  async file_open() {
    throw new Error("浏览器预览不打开电脑文件，请在拾忆桌面程序中使用此操作。");
  },
  async file_reveal() {
    throw new Error("浏览器预览不打开资源管理器，请在拾忆桌面程序中使用此操作。");
  },
  async inbox_query(request) {
    const items = inbox.items.filter((item) => request.status === "all" || item.triage_status === request.status);
    return { items: structuredClone(items), next_cursor: null };
  },
  async inbox_update(inbox_id, triage_status) {
    const item = inbox.items.find((candidate) => candidate.inbox_id === inbox_id);
    if (!item) throw new Error("收件箱项目不存在");
    item.triage_status = triage_status;
    return structuredClone(item);
  },
  async ocr_retry() {
    return true;
  },
  async image_understanding_retry() {
    return true;
  },
  async image_deep_analyze(asset_id, question) {
    return {
      asset_id,
      question,
      answer: "原图中可见与当前问题相关的本地资料内容；浏览器示例不会调用真实多模态模型。",
      observations: ["图片证据来自当前引用位置"],
      uncertainties: ["真实细节需在桌面版配置多模态模型后分析"],
      model_artifact_id: "018f0000-0000-7000-8000-000000000999",
      analyzed_at: now(),
    };
  },
  async knowledge_space_list() {
    return structuredClone(demoKnowledgeSpaces);
  },
  async knowledge_space_create(request) {
    const space: KnowledgeSpace = {
      space_id: crypto.randomUUID(),
      ...request,
      file_count: 0,
      created_at: now(),
      updated_at: now(),
    };
    demoKnowledgeSpaces = [space, ...demoKnowledgeSpaces];
    return structuredClone(space);
  },
  async knowledge_space_update(space_id, request) {
    const index = demoKnowledgeSpaces.findIndex((space) => space.space_id === space_id);
    if (index < 0) throw new Error("知识空间不存在");
    const updated: KnowledgeSpace = { ...demoKnowledgeSpaces[index]!, ...request, updated_at: now() };
    demoKnowledgeSpaces[index] = updated;
    return structuredClone(updated);
  },
  async knowledge_space_delete(space_id) {
    const before = demoKnowledgeSpaces.length;
    demoKnowledgeSpaces = demoKnowledgeSpaces.filter((space) => space.space_id !== space_id);
    if (before === demoKnowledgeSpaces.length) throw new Error("知识空间不存在");
  },
  async collection_list() {
    return structuredClone(collectionRecords);
  },
  async collection_create(request) {
    const collection: CollectionRecord = { collection_id: crypto.randomUUID(), ...request, file_count: 0, built_in: false, created_at: now(), updated_at: now() };
    collectionRecords = [...collectionRecords, collection];
    return structuredClone(collection);
  },
  async collection_update(collection_id, request) {
    const index = collectionRecords.findIndex((item) => item.collection_id === collection_id && !item.built_in);
    if (index < 0) throw new Error("集合不存在或不能修改");
    const updated = { ...collectionRecords[index]!, ...request, updated_at: now() };
    collectionRecords[index] = updated;
    return structuredClone(updated);
  },
  async collection_delete(collection_id) {
    const index = collectionRecords.findIndex((item) => item.collection_id === collection_id && !item.built_in);
    if (index < 0) throw new Error("集合不存在或不能删除");
    collectionRecords.splice(index, 1);
  },
  async collection_rule_preview() {
    return (await browserBridge.file_query({ cursor: null, page_size: 20 })).items;
  },
  async collection_file_query(_collection_id, request) {
    const items: FileRecord[] = [];
    const offset = Number(request.cursor ?? "0");
    const pageSize = Math.min(200, Math.max(1, request.page_size));
    const pageItems = items.slice(offset, offset + pageSize);
    const nextOffset = offset + pageItems.length;
    return { items: pageItems, next_cursor: nextOffset < items.length ? String(nextOffset) : null, total: items.length };
  },
  async collection_add_file() {
    return undefined;
  },
  async collection_remove_file() {
    return undefined;
  },
  async collection_suggestion_refresh() {
    if (demoSuggestions.length === 0) {
      const members = demoFileRecords().slice(0, 2).map((file, index) => ({ file, revision_id: file.current_revision_id!, confidence: index === 0 ? 1 : 0.86, rationale: index === 0 ? "该文档是本组语义质心候选" : "与组内核心文档的语义相似度为 86%", state: "suggested" }));
      demoSuggestions = [{ suggestion_id: crypto.randomUUID(), suggested_name: "RAG 检索优化资料", description: "这些资料的语义画像都在讨论混合召回与检索效果。确认后只在拾忆中形成虚拟分类。", confidence: 0.86, status: "suggested", model_version: "demo-bge-small-zh-v1.5", algorithm_version: "semantic_lsh_v1", members, created_at: now(), updated_at: now() }];
    }
    return { profiled_files: 2, candidate_edges: 1, created_suggestions: demoSuggestions.length, suggestion_ids: demoSuggestions.map((item) => item.suggestion_id), algorithm_version: "semantic_lsh_v1", model_version: "demo-bge-small-zh-v1.5" };
  },
  async collection_suggestion_query(cursor, page_size, status = "suggested") {
    const filtered = demoSuggestions.filter((item) => item.status === status);
    const offset = Number(cursor ?? "0");
    const items = filtered.slice(offset, offset + page_size);
    const consumed = offset + items.length;
    return { items: structuredClone(items), next_cursor: consumed < filtered.length ? String(consumed) : null, total: filtered.length };
  },
  async collection_suggestion_update(suggestion_id, suggestion) {
    const current = demoSuggestions.find((item) => item.suggestion_id === suggestion_id);
    if (!current) throw new Error("AI集合建议不存在");
    current.suggested_name = suggestion.suggested_name;
    current.description = suggestion.description;
    current.members = current.members.filter((member) => suggestion.member_file_ids.includes(member.file.file_id));
    current.updated_at = now();
    return structuredClone(current);
  },
  async collection_suggestion_confirm(suggestion_id) {
    const current = demoSuggestions.find((item) => item.suggestion_id === suggestion_id);
    if (!current) throw new Error("AI集合建议不存在");
    current.status = "confirmed";
    current.updated_at = now();
    const collection: CollectionRecord = { collection_id: crypto.randomUUID(), name: current.suggested_name, description: current.description, icon: "sparkles", color: "#8c7cf0", kind: "ai", rule: null, file_count: current.members.length, built_in: false, created_at: now(), updated_at: now() };
    collectionRecords = [...collectionRecords, collection];
    return structuredClone(collection);
  },
  async collection_suggestion_reject(suggestion_id) {
    const current = demoSuggestions.find((item) => item.suggestion_id === suggestion_id);
    if (!current) throw new Error("AI集合建议不存在");
    current.status = "rejected";
    current.updated_at = now();
  },
  async relation_refresh() {
    return { hashed_files: 2, exact_duplicate_pairs: 1, version_candidate_pairs: 1 };
  },
  async relation_query() {
    return { items: [], next_cursor: null, total: 0 };
  },
  async relation_review() {
    return undefined;
  },
  async file_query(request) {
    const normalizedQuery = request.query?.trim().toLocaleLowerCase("zh-CN") ?? "";
    const extensions = new Set((request.extensions ?? []).map((value) => value.replace(/^\./, "").toLocaleLowerCase("zh-CN")));
    const statuses = new Set(request.parse_statuses ?? []);
    const items = demoFileRecords().filter((file) => (!normalizedQuery || file.display_name.toLocaleLowerCase("zh-CN").includes(normalizedQuery))
      && (extensions.size === 0 || extensions.has(file.extension.toLocaleLowerCase("zh-CN")))
      && (statuses.size === 0 || statuses.has(file.parse_status))
      && (!request.availability || file.availability === request.availability));
    const offset = Number(request.cursor ?? "0");
    const pageSize = Math.min(200, Math.max(1, request.page_size));
    const pageItems = items.slice(offset, offset + pageSize);
    const nextOffset = offset + pageItems.length;
    return {
      items: pageItems,
      next_cursor: nextOffset < items.length ? String(nextOffset) : null,
      total: items.length,
    };
  },
  async exclusion_rule_list() { return structuredClone(demoExclusionRules); },
  async exclusion_rule_upsert(request) {
    const rule: ExclusionRule = { rule_id: request.rule_id ?? crypto.randomUUID(), root_id: request.root_id, rule_class: "default", rule_type: request.rule_type, value: request.value, enabled: request.enabled, overridable: true };
    demoExclusionRules = [...demoExclusionRules.filter((item) => item.rule_id !== rule.rule_id), rule];
    return structuredClone(rule);
  },
  async exclusion_rule_delete(rule_id) { demoExclusionRules = demoExclusionRules.filter((item) => item.rule_id !== rule_id); },
  async extraction_preset_list() {
    return [
      { preset_id: "file_catalog", name: "资料目录", description: "抽取文件名、类型、修改时间、大小和路径，用于生成资料清单。", fields: [
        { key: "file_name", label: "文件名", field_type: "string", description: "文件名", required: true, multiple: false, hints: [] },
        { key: "extension", label: "类型", field_type: "string", description: "类型", required: true, multiple: false, hints: [] },
      ] },
      { preset_id: "contact_clues", name: "联系方式", description: "从正文中查找电子邮箱和手机号码。", fields: [
        { key: "emails", label: "电子邮箱", field_type: "list", description: "电子邮箱", required: false, multiple: true, hints: [] },
        { key: "phones", label: "手机号码", field_type: "list", description: "手机号码", required: false, multiple: true, hints: [] },
      ] },
      { preset_id: "extractive_summary", name: "保守摘录摘要", description: "提取每份资料开头的关键段落并保留来源。", fields: [
        { key: "summary", label: "摘要", field_type: "string", description: "带来源的保守摘录", required: false, multiple: false, hints: [] },
      ] },
      { preset_id: "filename_suggestions", name: "文件名建议", description: "根据正文标题生成建议，不直接重命名。", fields: [
        { key: "suggested_name", label: "建议文件名", field_type: "string", description: "建议名称", required: false, multiple: false, hints: [] },
      ] },
      { preset_id: "folder_suggestions", name: "目录建议", description: "根据内容和类型生成虚拟集合建议。", fields: [
        { key: "suggested_collection", label: "建议集合", field_type: "string", description: "建议虚拟集合", required: false, multiple: false, hints: [] },
      ] },
      { preset_id: "duplicate_review", name: "重复文件审查", description: "按字节数与SHA-256列出完全重复候选。", fields: [
        { key: "file_name", label: "文件名", field_type: "string", description: "文件名", required: true, multiple: false, hints: [] },
        { key: "content_sha256", label: "SHA-256", field_type: "string", description: "内容哈希", required: false, multiple: false, hints: [] },
      ] },
      { preset_id: "version_compare", name: "多版本内容对比", description: "比较带来源的正文块增删。", fields: [
        { key: "file_name", label: "文件名", field_type: "string", description: "文件名", required: true, multiple: false, hints: [] },
        { key: "version_diff", label: "相对基准的内容变化", field_type: "object", description: "内容变化", required: true, multiple: false, hints: [] },
      ] },
      { preset_id: "merge_tables", name: "合并表格", description: "按表头对齐Word与Excel表格行。", fields: [
        { key: "source_file", label: "来源文件", field_type: "string", description: "来源文件", required: true, multiple: false, hints: [] },
        { key: "row_data", label: "按表头对齐的数据", field_type: "object", description: "行数据", required: true, multiple: false, hints: [] },
      ] },
      { preset_id: "ocr_report", name: "重新 OCR", description: "强制重新识别图片或PDF。", fields: [
        { key: "file_name", label: "文件名", field_type: "string", description: "文件名", required: true, multiple: false, hints: [] },
        { key: "ocr_status", label: "OCR状态", field_type: "string", description: "OCR状态", required: true, multiple: false, hints: [] },
      ] },
    ];
  },
  async extraction_run(file_ids, preset_id) {
    const files = demoFileRecords().filter((file) => file_ids.includes(file.file_id));
    const preset = (await browserBridge.extraction_preset_list()).find((item) => item.preset_id === preset_id);
    if (!preset) throw new Error("抽取模板不存在");
    return {
      run_id: crypto.randomUUID(), preset, status: "completed", completed_at: now(), warnings: ["浏览器预览使用演示数据。"],
      rows: files.map((file) => ({ file, values: preset.fields.map((field) => ({ field_key: field.key, raw_value: field.key === "file_name" ? file.display_name : field.key === "extension" ? file.extension : null, normalized_value: field.key === "file_name" ? file.display_name : field.key === "extension" ? file.extension : null, confidence: field.key === "file_name" || field.key === "extension" ? 1 : 0, method: "metadata", review_state: field.key === "file_name" || field.key === "extension" ? "auto" as const : "missing" as const, evidence: [], validation_errors: [] })) })),
    };
  },
  async skill_list() {
    const registered: Array<[string, string, string]> = [
      ["batch_field_extraction", "批量字段抽取", "使用固定规则模板逐字段抽取，并保留来源。"],
      ["generate_catalog", "生成资料目录", "从文件元数据生成可复核目录，并显式导出新文件。"],
      ["duplicate_review", "重复文件审查", "对选中且同大小的资料计算SHA-256，生成待人工确认的重复候选。"],
      ["multi_document_summary", "多文档摘要", "从每份资料提取带逐段来源的保守摘要，不补充外部知识。"],
      ["version_compare", "多版本内容对比", "以最早修改版本为基准比较正文块增删，并保留双侧来源。"],
      ["recommend_filename", "推荐文件名", "比较正文标题和保守回退路径，只输出建议。"],
      ["recommend_folders", "推荐目录结构", "比较元数据、正文关键词和保守回退路径，只输出虚拟集合建议。"],
      ["merge_tables", "合并表格并导出", "按首行表头对齐Word与Excel表格，保留原始值与来源。"],
      ["rerun_ocr", "重新 OCR", "使用Windows本地OCR强制重新识别选中的图片或PDF。"],
      ["export_index", "导出知识库索引", "导出经过复核的文件元数据索引。"],
    ];
    return registered.map(([skill_id, name, description]) => ({ skill_id, name, description, available: true, unavailable_reason: null, risk_level: "low" as const, source_files_readonly: true as const, export_required: true }));
  },
  async task_plan(skill_id, file_ids, parameters) {
    return { task_id: crypto.randomUUID(), skill_id, skill_version: "1.0.0", summary: `对${file_ids.length}份资料执行“批量字段抽取”`, estimated_file_count: file_ids.length, warnings: ["任务只读取源文件；产生的结果先在应用内复核，导出需要再次由你选择保存位置。"], steps: ["验证资料权限与当前修订", "固定本次处理输入快照", "逐文件执行规则抽取", "生成应用内复核表"].map((label, index) => ({ step_id: crypto.randomUUID(), ordinal: index + 1, step_type: ["scope.validate", "input.snapshot", "extraction.rules_first", "result.review"][index]!, label, inputs: index === 2 ? parameters : {}, expected_outputs: {}, status: "pending" as const, attempt_count: 0, checkpoint: ["permission.source_readonly", "invariant.revision_current", "evidence.field_level", "schema.extraction_result"][index]!, error: null })) };
  },
  async task_execute(skill_id, file_ids, parameters, planned_task_id) {
    const generated = await browserBridge.task_plan(skill_id, file_ids, parameters);
    const plan = { ...generated, task_id: planned_task_id };
    const presetId = skill_id === "generate_catalog" || skill_id === "export_index" ? "file_catalog"
      : skill_id === "multi_document_summary" ? "extractive_summary"
      : skill_id === "recommend_filename" ? "filename_suggestions"
      : skill_id === "recommend_folders" ? "folder_suggestions"
      : skill_id === "duplicate_review" ? "duplicate_review"
      : skill_id === "version_compare" ? "version_compare"
      : skill_id === "merge_tables" ? "merge_tables"
      : skill_id === "rerun_ocr" ? "ocr_report"
      : String(parameters.preset_id ?? "file_catalog");
    const result = await browserBridge.extraction_run(file_ids, presetId);
    const completedAt = now();
    const completedPlan = { ...plan, steps: plan.steps.map((step) => ({ ...step, status: "succeeded" as const, attempt_count: 1 })) };
    const strategies = skill_id === "multi_document_summary" ? ["extractive_first", "metadata_outline", "conservative_fallback"]
      : skill_id === "recommend_filename" ? ["content_heading", "existing_name_normalized", "conservative_keep_current"]
      : skill_id === "recommend_folders" ? ["content_keywords", "path_and_type", "conservative_virtual_inbox"] : [];
    return {
      plan: completedPlan,
      job: { job_id: plan.task_id, job_type: `task.${skill_id}`, status: "succeeded" as const, stage: "completed", progress: 1, processed_items: plan.steps.length, total_items: plan.steps.length, error: null, created_at: completedAt, started_at: completedAt, finished_at: completedAt },
      result,
      checkpoints: plan.steps.map((step) => ({ checkpoint_id: crypto.randomUUID(), job_id: plan.task_id, unit_id: step.step_id, checkpoint_type: step.ordinal === 1 ? "permission" as const : step.ordinal === 2 ? "invariant" as const : step.ordinal === 3 ? "evidence" as const : "schema" as const, status: "passed" as const, rules_version: "1.0.0", metrics: {}, error: null, created_at: completedAt, resume_token: null })),
      candidates: strategies.map((strategy, index) => ({ candidate_id: crypto.randomUUID(), job_id: plan.task_id, strategy, status: index === 0 ? "selected" as const : "valid" as const, result_ref: index === 0 ? `remin://extraction/${result.run_id}` : null, quality_score: index === 0 ? 0.9 : index === 1 ? 0.72 : 0.6, evidence_score: 1, latency_ms: null, resource_cost: index === 0 ? 0.5 : index === 1 ? 0.2 : 0.05, rejection_reasons: [] })),
    };
  },
  async task_recoverable() {
    return null;
  },
  async task_resume() {
    throw new Error("浏览器预览没有可恢复的本地任务");
  },
  async extraction_export() {
    throw new Error("浏览器预览不写入电脑文件，请在拾忆桌面程序中导出。");
  },
  async maintenance_get() {
    return { schema_version: 6, database_size_bytes: 12_582_912, indexed_files: 3, searchable_chunks: 18, embedded_chunks: 0, pending_files: 0, failed_files: 0, active_jobs: 0, log_events: 3, background_notice: null, checks: [{ key: "database", label: "本地数据库", status: "passed" as const, detail: "ok" }, { key: "schema", label: "数据结构", status: "passed" as const, detail: "版本 6" }, { key: "source_readonly", label: "源文件保护", status: "passed" as const, detail: "维护操作只作用于拾忆索引与日志" }], checked_at: now() };
  },
  async maintenance_check(level) {
    return { level, database_result: "ok", elapsed_ms: 1, source_files_modified: false };
  },
  async storage_usage_get() {
    const categories = [
      { key: "database" as const, label: "资料索引数据库", size_bytes: 12_582_912, clearable: false, detail: "元数据、全文索引和语义向量；请使用重建索引维护" },
      { key: "vector_indexes" as const, label: "语义向量索引", size_bytes: 2_097_152, clearable: false, detail: "可由SQLite真值重建的USearch索引" },
      { key: "installed_models" as const, label: "已安装模型", size_bytes: 0, clearable: false, detail: "已经校验并激活的本地模型组件" },
      { key: "resumable_downloads" as const, label: "可续传模型下载", size_bytes: 0, clearable: false, detail: "保留暂停任务的断点，避免重新下载" },
      { key: "temporary_cache" as const, label: "解析与预览临时缓存", size_bytes: 1_048_576, clearable: true, detail: "可安全重建，不含源文件与索引数据库" },
      { key: "failed_downloads" as const, label: "失败下载隔离区", size_bytes: 0, clearable: true, detail: "大小或哈希异常的下载副本，不用于续传" },
    ];
    const total_bytes = categories.reduce((total, item) => total + item.size_bytes, 0);
    return { categories, total_bytes, data_directory: "应用管理目录", disk_capacity_bytes: 512 * 1024 ** 3, disk_available_bytes: 128 * 1024 ** 3, soft_quota_bytes: 50 * 1024 ** 3, soft_quota_is_custom: false, over_soft_quota: false, background_tasks_paused: false, notice: null, measured_at: now() };
  },
  async storage_policy_set(quota_bytes) {
    const snapshot = await browserBridge.storage_usage_get();
    return { ...snapshot, soft_quota_bytes: quota_bytes, soft_quota_is_custom: true, over_soft_quota: snapshot.total_bytes >= quota_bytes, background_tasks_paused: snapshot.total_bytes >= quota_bytes, notice: snapshot.total_bytes >= quota_bytes ? "存储已达到软配额，暂停图片缓存、OCR和语义索引；搜索与预览继续可用" : null };
  },
  async cache_clear(category) {
    return { category, removed_entries: 1, freed_bytes: category === "temporary_cache" ? 1_048_576 : 0 };
  },
  async app_data_reset_schedule() {
    throw new Error("浏览器预览不能重置桌面应用数据，请在拾忆桌面程序中使用。");
  },
  async maintenance_log_query() {
    return { items: [{ log_id: "log-demo", level: "info", component: "catalog", event_name: "scan.completed", fields: { files: 3 }, created_at: now() }], next_cursor: null, total: 1 };
  },
  async maintenance_logs_clear() {
    return 1;
  },
  async diagnostic_export() {
    throw new Error("浏览器预览不写入电脑文件，请在拾忆桌面程序中导出。");
  },
  async index_rebuild() {
    return { reset_files: 3, removed_nodes: 18, removed_chunks: 18, removed_embeddings: 0, source_files_modified: false as const };
  },
  async root_list() {
    return roots;
  },
  async root_add(request) {
    const root: RootRecord = { root_id: "018f0000-0000-7000-8000-000000000405", path: request.path, canonical_path: request.path, path_key: request.path.toLocaleLowerCase("zh-CN"), root_file_id: null, volume_id: "vol-demo", volume_type: "fixed", authorization_source: request.authorization_source, root_kind: request.full_volume_confirmed ? "volume_root" : "folder", label: request.label || request.path, enabled: true, status: "ready", watch_mode: request.watch_mode, coverage_parent_root_id: null, file_count: 0, permission_error_count: 0, last_scan_at: null };
    roots.push(root);
    return root;
  },
  async root_disable(root_id) {
    const index = roots.findIndex((root) => root.root_id === root_id);
    if (index < 0) throw new Error("资料位置不存在或已经停用");
    roots.splice(index, 1);
  },
  async scan_start(_root_id, _reason) {
    return { job_id: "018f0000-0000-7000-8000-000000000501", job_type: "initial_scan", status: "running" as const, stage: "enumerating", progress: 0.2, processed_items: 20, total_items: 100, error: null, created_at: now(), started_at: now(), finished_at: null };
  },
  async scan_pause(job_id) {
    return { job_id, job_type: "initial_scan", status: "paused" as const, stage: "paused", progress: 0.2, processed_items: 20, total_items: 100, error: null, created_at: now(), started_at: now(), finished_at: null };
  },
  async scan_resume(job_id) {
    return { job_id, job_type: "initial_scan", status: "running" as const, stage: "enumerating", progress: 0.2, processed_items: 20, total_items: 100, error: null, created_at: now(), started_at: now(), finished_at: null };
  },
  async scan_cancel(job_id) {
    return { job_id, job_type: "initial_scan", status: "cancelled" as const, stage: "cancelled", progress: 0.2, processed_items: 20, total_items: 100, error: null, created_at: now(), started_at: now(), finished_at: now() };
  },
};
