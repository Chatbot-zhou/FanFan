import type {
  AnswerResult,
  AskMessage,
  AskOperationSnapshot,
  AskSessionSummary,
  CandidateRoot,
  CollectionRecord,
  CollectionSuggestion,
  EnvironmentCheck,
  ExclusionRule,
  FileRecord,
  HomeSummary,
  InboxPage,
  ModelCatalogEntry,
  ModelEdition,
  ModelDownloadJob,
  ModelPreset,
  ModelRuntimeState,
  InferenceRuntimeState,
  AiRuntimeSnapshot,
  ModelArtifact,
  FanFanBridge,
  RootRecord,
  SearchRequest,
  SearchSession,
  ThemePreference,
  WelcomeState,
} from "./contracts";

const WELCOME_KEY = "fanfan.welcome.v1";
const THEME_KEY = "fanfan.theme.v1";
const MODEL_KEY = "fanfan.model.state.v1";

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
const browserAskSessions = new Map<string, AskSessionSummary>();
const browserAskMessages = new Map<string, AskMessage[]>();

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
  runtime_devices: [],
  gpu_runtime_available: false,
  checked_at: now(),
  warnings: [],
};

const defaultInferenceRuntime: InferenceRuntimeState = {
  backend: "cpu",
  device_names: [],
  gpu_available: false,
  gpu_offload_layers: 0,
  gpu_offload_mode: "disabled",
  thread_budget: 4,
  batch_thread_budget: 2,
  active: false,
  pressure_reason: null,
  hardware: {
    physical_core_count: 4,
    logical_thread_count: 8,
    memory_total_bytes: 8 * 1024 * 1024 * 1024,
    memory_available_bytes: 4 * 1024 * 1024 * 1024,
    gpu_name: null,
    gpu_memory_bytes: null,
  },
  runtime_package: {
    backend: "cpu",
    device_count: 0,
    gpu_capable: false,
    cpu_fallback_available: false,
    validated: true,
  },
  budget: {
    foreground_threads: 4,
    background_threads: 2,
    batch_size: 256,
    ubatch_size: 128,
    gpu_reserve_bytes: 512 * 1024 * 1024,
    system_memory_reserve_bytes: 2 * 1024 * 1024 * 1024,
  },
};

const defaultAiRuntime: AiRuntimeSnapshot = {
  budget: {
    physical_core_count: 4,
    foreground_cpu_threads: 2,
    background_cpu_threads: 2,
    total_memory_bytes: 8 * 1024 * 1024 * 1024,
    reserved_memory_bytes: 2 * 1024 * 1024 * 1024,
    total_gpu_memory_bytes: null,
    reserved_gpu_memory_bytes: null,
  },
  tasks: [],
  instances: [],
  queued_count: 0,
  running_count: 0,
  active_heavy_task: null,
  pressure_reason: null,
  checked_at: now(),
};

const defaultModelState: ModelRuntimeState = {
  status: "unconfigured",
  active_profile_id: null,
  active_profile_name: null,
  runtime_backend: null,
  inference_runtime: defaultInferenceRuntime,
  checked_at: now(),
  capabilities: { generation: false, embedding: false, vision: false, reranker: false, ocr: false, tts: false, asr: false },
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
const demoModelPresets: ModelPreset[] = [
  { preset_id: "light", name: "推荐省内存组合", description: "Qwen3 0.6B + BGE-small", recommended_memory_gb: 8, role_catalog_ids: ["qwen3-0.6b-q8", "bge-small-zh-int8"], edition_id: "light" },
  { preset_id: "standard", name: "推荐质量组合", description: "Qwen3 4B + BGE-small", recommended_memory_gb: 12, role_catalog_ids: ["qwen3-4b-q4", "bge-small-zh-int8"], edition_id: "standard" },
];
const demoRoleCatalog: ModelCatalogEntry[] = [
  ["qwen3-0.6b-q8", "generation", "Qwen3 0.6B · 省内存", "Qwen3-0.6B-Q8_0", "低资源设备上的基础中文证据问答。", 639_446_688, 2, 1.2, "较快", true, "8GB 内存可用", "generation_qwen3_0_6b", ["huggingface", "modelscope"]],
  ["qwen3-1.7b-q8", "generation", "Qwen3 1.7B · 均衡", "Qwen3-1.7B-Q8_0", "回答质量与资源占用之间更均衡。", 1_834_426_016, 4, 2.4, "中等", true, "当前设备优先推荐", "generation_qwen3_1_7b", ["huggingface"]],
  ["qwen3-4b-q4", "generation", "Qwen3 4B · 质量优先", "Qwen3-4B-Q4_K_M", "更擅长跨文件综合和复杂追问。", 2_497_280_256, 7, 4.5, "较慢", false, "建议 12GB 以上内存", "generation_qwen3_4b", ["huggingface", "modelscope"]],
  ["bge-small-zh-int8", "embedding", "BGE-small-zh-v1.5 · 默认", "bge-small-zh-v1.5-onnx-int8", "中文资料检索的轻量向量模型。", 24_304_813, 1, null, "快", true, "所有受支持设备均推荐", "embedding_bge_small", ["huggingface", "modelscope"]],
  ["bge-base-zh", "embedding", "BGE-base-zh-v1.5 · 精度优先", "bge-base-zh-v1.5-onnx", "更大的中文向量模型。", null, 2.5, null, "中等", false, "远程构建完成基准前仅支持本地导入", null, []],
  ["qwen3-vl-2b-q4", "vision", "Qwen3-VL 2B · 省显存", "Qwen3VL-2B-Instruct-Q4_K_M", "理解图片、图表和文档内嵌图片。", 1_552_463_168, 5, 3.2, "较慢", true, "4GB 显存优先选择", "vision_qwen3_vl_2b_q4", ["huggingface"]],
  ["qwen3-vl-2b-q8", "vision", "Qwen3-VL 2B · 质量优先", "Qwen3VL-2B-Instruct-Q8_0", "保留更多视觉细节。", 2_279_480_640, 7, 5, "慢", false, "建议显存高于 6GB", "vision_qwen3_vl_2b_q8", ["huggingface"]],
  ["bge-reranker-base-int8", "reranker", "BGE Reranker Base · 可选", "bge-reranker-base-onnx-int8", "对少量混合召回候选做相关性复核。", 296_335_457, 2, null, "中等", false, "更重视速度时可不配置", "reranker_bge_base_int8", ["huggingface"]],
].map(([catalog_id, role, name, model_id, description, download_size_bytes, estimated_memory_gb, estimated_vram_gb, cpu_speed, recommended, device_guidance, install_edition_id, supported_sources]) => ({
  catalog_id: catalog_id as string,
  role: role as ModelCatalogEntry["role"],
  name: name as string,
  model_id: model_id as string,
  description: description as string,
  strengths: recommended ? ["已通过兼容性检查", "适合作为推荐起点"] : ["适合更高质量或专项需求"],
  limitations: role === "reranker" ? ["会增加问答延迟"] : ["资源占用随模型规模增加"],
  download_size_bytes: download_size_bytes as number | null,
  estimated_memory_gb: estimated_memory_gb as number,
  estimated_vram_gb: estimated_vram_gb as number | null,
  cpu_speed: cpu_speed as string,
  license_name: role === "embedding" || role === "reranker" ? "MIT" : "Apache-2.0",
  recommended: recommended as boolean,
  device_guidance: device_guidance as string,
  verification_status: install_edition_id ? "verified" : "local_import_only",
  install_edition_id: install_edition_id as string | null,
  supported_sources: supported_sources as ModelCatalogEntry["supported_sources"],
}));

const roots: RootRecord[] = [];

const searchCatalog = [
  { file_id: "s1", name: "RAG项目总结.docx", extension: "docx", path: "文档\\项目资料", modified_at: "2025-11-08T03:00:00Z", snippet: "通过混合召回和重排提升召回率，并对关键词与语义结果进行融合。" },
  { file_id: "s2", name: "检索效果评估.xlsx", extension: "xlsx", path: "文档\\大模型学习", modified_at: "2025-10-16T03:00:00Z", snippet: "记录不同召回策略、Top K 与命中率的对比结果。" },
  { file_id: "s3", name: "产品设计规范.pdf", extension: "pdf", path: "桌面\\翻翻", modified_at: "2025-09-20T03:00:00Z", snippet: "本地统一搜索采用文件名、全文和语义三个通道。" },
];

const inbox: InboxPage = {
  items: [
    { inbox_id: "i1", file_id: "s1", display_name: "RAG项目总结.docx", display_path: "文档\\项目资料\\RAG项目总结.docx", event_type: "discovered", observed_at: now(), previous_display_path: null, triage_status: "new", resolution_status: "normal", attempt_count: 0, last_attempt_at: null, retry_action: null, suggested_collection_ids: [], duplicate_group_id: null, summary: "新增资料，已完成正文提取，可以全文搜索", error_code: null },
    { inbox_id: "i2", file_id: "s2", display_name: "检索效果评估.xlsx", display_path: "文档\\大模型学习\\检索效果评估.xlsx", event_type: "modified", observed_at: now(), previous_display_path: null, triage_status: "new", resolution_status: "normal", attempt_count: 0, last_attempt_at: null, retry_action: null, suggested_collection_ids: [], duplicate_group_id: "duplicate-demo-1", summary: "资料有新版本，并发现一份字节完全相同的副本", error_code: null },
    { inbox_id: "i3", file_id: "s3", display_name: "产品设计规范.pdf", display_path: "桌面\\翻翻\\产品设计规范.pdf", event_type: "ocr_required", observed_at: now(), previous_display_path: null, triage_status: "new", resolution_status: "pending_retry", attempt_count: 0, last_attempt_at: null, retry_action: "retry_ocr", suggested_collection_ids: [], duplicate_group_id: null, summary: "当前未安装 OCR 模型，已保留文件名索引", error_code: "OCR_REQUIRED" },
  ],
  next_cursor: null,
  has_more: false,
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
let demoExclusionRules: ExclusionRule[] = [
  { rule_id: "018f0000-0000-7000-8000-000000009001", root_id: null, rule_class: "hard", rule_type: "path_name", value: "windows", enabled: true, overridable: false },
  { rule_id: "018f0000-0000-7000-8000-000000009002", root_id: null, rule_class: "default", rule_type: "path_name", value: "node_modules", enabled: true, overridable: true },
];

export const browserBridge: FanFanBridge = {
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
    const preference = (window.localStorage.getItem(THEME_KEY) as ThemePreference | null) ?? "day_gradient";
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
    return embedding ? { ...defaultModelState, status: "ready", runtime_backend: "cpu", capabilities: { ...defaultModelState.capabilities, embedding: true } } : defaultModelState;
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
  async model_catalog_list(_source) {
    return structuredClone(demoModelEditions);
  },
  async model_role_catalog_list() {
    return structuredClone(demoRoleCatalog);
  },
  async model_preset_list() {
    return structuredClone(demoModelPresets);
  },
  async model_download_start(edition_id, source) {
    const edition = demoModelEditions.find((item) => item.edition_id === edition_id);
    if (!edition) throw new Error("模型版本不存在");
    const installed = edition.artifacts.map<ModelArtifact>((artifact) => ({ artifact_id: crypto.randomUUID(), role: artifact.role, format: artifact.format, model_id: artifact.model_id, model_version: null, source, repository_id: artifact.repository_id, revision: artifact.revision, sha256: artifact.sha256, size_bytes: artifact.size_bytes, local_path: `C:\\Users\\你\\AppData\\Roaming\\com.fanfan.desktop\\models\\${artifact.file_name}`, quantization: artifact.role === "generation" ? (edition_id === "standard" ? "Q4_K_M" : "Q8_0") : null, context_length: null, embedding_dimension: artifact.role === "embedding" ? 512 : null, query_prefix: artifact.query_prefix, max_length: artifact.max_length, license_name: artifact.license_name, status: "ready", imported_at: now() }));
    demoArtifacts = [...demoArtifacts, ...installed];
    installed.forEach((artifact) => demoActiveRoles.add(artifact.role));
    const job: ModelDownloadJob = { job_id: crypto.randomUUID(), edition_id, edition_name: edition.name, source, status: "completed", phase: "completed", downloaded_bytes: edition.download_size_bytes, total_bytes: edition.download_size_bytes, progress: 1, bytes_per_second: 0, eta_seconds: null, retry_count: 0, current_file: null, files: edition.artifacts.flatMap((artifact) => [{ role: artifact.role, file_name: artifact.file_name, downloaded_bytes: artifact.size_bytes, total_bytes: artifact.size_bytes, status: "completed" }, ...artifact.companion_files.map((file) => ({ role: artifact.role, file_name: file.file_name, downloaded_bytes: file.size_bytes, total_bytes: file.size_bytes, status: "completed" }))]), installed_artifact_ids: installed.map((artifact) => artifact.artifact_id), profile_id: crypto.randomUUID(), error: null, activation_status: "active", activation_error: null, created_at: now(), updated_at: now() };
    demoDownloadJobs = [job, ...demoDownloadJobs];
    return structuredClone(job);
  },
  async model_download_list() {
    return structuredClone(demoDownloadJobs);
  },
  async model_store_status_get() {
    return { store_path: "C:\\Users\\你\\AppData\\Local\\FanFan\\ModelStore\\v1", migration_state: "ready" as const, installed_artifacts: demoArtifacts.length, installed_bytes: demoArtifacts.reduce((total, artifact) => total + artifact.size_bytes, 0), integrity_status: "registry_loaded" };
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
    const before = demoDownloadJobs.length;
    demoDownloadJobs = demoDownloadJobs.filter((item) => item.job_id !== job_id);
    return { job_id, removed: demoDownloadJobs.length < before, partial_bytes_removed: 0 };
  },
  async model_download_resume(job_id) {
    const job = demoDownloadJobs.find((item) => item.job_id === job_id);
    if (!job) throw new Error("下载任务不存在");
    job.status = "running"; job.phase = "downloading"; job.error = null; job.updated_at = now();
    return structuredClone(job);
  },
  async model_download_retry(job_id) {
    const job = demoDownloadJobs.find((item) => item.job_id === job_id);
    if (!job) throw new Error("下载任务不存在");
    job.status = "queued"; job.phase = "queued"; job.error = null; job.retry_count += 1; job.updated_at = now();
    return structuredClone(job);
  },
  async model_download_switch_source(job_id, source) {
    const job = demoDownloadJobs.find((item) => item.job_id === job_id);
    if (!job) throw new Error("下载任务不存在");
    job.source = source; job.status = "queued"; job.phase = "queued"; job.error = null; job.downloaded_bytes = 0; job.progress = 0; job.updated_at = now();
    return structuredClone(job);
  },
  async model_download_remove(job_id) {
    return browserBridge.model_download_cancel(job_id);
  },
  async model_artifact_activate(artifact_id) {
    const artifact = demoArtifacts.find((item) => item.artifact_id === artifact_id);
    if (!artifact) throw new Error("模型组件不存在");
    demoActiveRoles.add(artifact.role);
    return { ...defaultModelState, status: "ready", runtime_backend: "cpu", capabilities: { ...defaultModelState.capabilities, [artifact.role]: true } };
  },
  async model_role_disable(role) {
    demoActiveRoles.delete(role);
    return { ...defaultModelState };
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
    if (!demoActiveRoles.has("generation")) {
      throw { code: "RAG_GENERATION_MODEL_REQUIRED", message: "问资料需要先配置并通过自检的本地生成模型", retryable: false, user_action: null, file_id: null, details: null };
    }
    if (!demoActiveRoles.has("embedding")) {
      throw { code: "RAG_EMBEDDING_MODEL_REQUIRED", message: "问资料需要先配置并通过自检的中文 Embedding 模型", retryable: false, user_action: null, file_id: null, details: null };
    }
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
        answer_mode: "rag_refusal",
        retrieval_channels: ["filename", "fts", "embedding", "rrf"],
        index_coverage: 1,
        degradation_reason: null,
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
          context_before: "",
          context_after: "",
          locator: match.locator!,
          retrieval_score: match.scores.fused,
        }],
      }));
      result = {
        session_id,
        message_id: crypto.randomUUID(),
        answer: `本地生成模型根据混合检索证据整理如下：\n${claims.map((claim, index) => `${index + 1}. ${claim.text}`).join("\n")}`,
        grounding_status: "grounded",
        insufficient_evidence: false,
        claims,
        source_files,
        used_file_ids: source_files.map((source) => source.file_id),
        elapsed_ms: search.elapsed_ms,
        answer_mode: "generated",
        retrieval_channels: ["filename", "fts", "embedding", "rrf"],
        index_coverage: 1,
        degradation_reason: null,
      };
    }
    const operation_id = crypto.randomUUID();
    const snapshot: AskOperationSnapshot = {
      handle: { operation_id, kind: "ask", status: "completed", created_at: now() },
      result,
      error: null,
    };
    browserAskOperations.set(operation_id, snapshot);
    const updated_at = now();
    browserAskSessions.set(session_id, {
      session_id,
      title: browserAskSessions.get(session_id)?.title ?? `${request.question.trim().slice(0, 28)}${request.question.trim().length > 28 ? "…" : ""}`,
      scope: request.scope,
      message_count: (browserAskMessages.get(session_id)?.length ?? 0) + 2,
      created_at: browserAskSessions.get(session_id)?.created_at ?? updated_at,
      updated_at,
      last_error: null,
    });
    browserAskMessages.set(session_id, [
      ...(browserAskMessages.get(session_id) ?? []),
      { message_id: crypto.randomUUID(), session_id, role: "user", content: request.question, answer: null, error: null, created_at: updated_at },
      { message_id: result.message_id, session_id, role: "assistant", content: result.answer, answer: result, error: null, created_at: now() },
    ]);
    return snapshot.handle;
  },
  async ask_session_query() {
    const items = [...browserAskSessions.values()].sort((left, right) => right.updated_at.localeCompare(left.updated_at));
    return { items: structuredClone(items), next_cursor: null, has_more: false };
  },
  async ask_message_query(session_id) {
    return { items: structuredClone(browserAskMessages.get(session_id) ?? []), next_cursor: null, has_more: false };
  },
  async ask_session_rename(session_id, title) {
    const session = browserAskSessions.get(session_id);
    if (!session) throw new Error("问答会话不存在");
    session.title = title.trim();
    session.updated_at = now();
  },
  async ask_session_delete(session_id) {
    browserAskSessions.delete(session_id);
    browserAskMessages.delete(session_id);
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
  async speech_recognize() {
    throw new Error("浏览器预览无法访问本机麦克风语音运行时，请在翻翻桌面程序中使用。");
  },
  async speech_synthesize_answer() {
    throw new Error("浏览器预览无法访问本地语音合成运行时，请在翻翻桌面程序中使用。");
  },
  async answer_export(_message_id, target_path, format) {
    return { target_path, format, row_count: 0, size_bytes: 0, sha256: "0".repeat(64) };
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
      ocr_attempts: [],
      offset,
      next_offset: null,
      anchor_node_id,
      truncated: false,
    };
  },
  async file_open() {
    throw new Error("浏览器预览不打开电脑文件，请在翻翻桌面程序中使用此操作。");
  },
  async file_reveal() {
    throw new Error("浏览器预览不打开资源管理器，请在翻翻桌面程序中使用此操作。");
  },
  async inbox_query(request) {
    const items = inbox.items.filter((item) => request.status === "all"
      || (request.status === "error" ? ["pending_retry", "retrying"].includes(item.resolution_status) : item.triage_status === request.status));
    return { items: structuredClone(items), next_cursor: null, has_more: false };
  },
  async inbox_update(inbox_id, triage_status) {
    const item = inbox.items.find((candidate) => candidate.inbox_id === inbox_id);
    if (!item) throw new Error("收件箱项目不存在");
    item.triage_status = triage_status;
    return structuredClone(item);
  },
  async inbox_retry(inbox_id) {
    const item = inbox.items.find((candidate) => candidate.inbox_id === inbox_id);
    if (!item || !item.retry_action) throw new Error("该项目当前没有可重试的处理任务");
    item.resolution_status = "retrying";
    item.attempt_count += 1;
    item.last_attempt_at = now();
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
    return { items: pageItems, next_cursor: nextOffset < items.length ? String(nextOffset) : null, has_more: nextOffset < items.length, total: items.length };
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
      demoSuggestions = [{ suggestion_id: crypto.randomUUID(), suggested_name: "RAG 检索优化资料", description: "这些资料的语义画像都在讨论混合召回与检索效果。确认后只在翻翻中形成虚拟分类。", confidence: 0.86, status: "suggested", model_version: "demo-bge-small-zh-v1.5", algorithm_version: "semantic_cluster_v3", members, created_at: now(), updated_at: now() }];
    }
    return { profiled_files: 2, candidate_edges: 1, created_suggestions: demoSuggestions.length, suggestion_ids: demoSuggestions.map((item) => item.suggestion_id), algorithm_version: "semantic_cluster_v3", model_version: "demo-bge-small-zh-v1.5", topic_groups: 1, remaining_topic_groups: 0 };
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
    return { hashed_files: 2, exact_duplicate_pairs: 1, version_candidate_pairs: 1, semantic_related_pairs: 2, contains_or_summarizes_pairs: 1, groups_created: 1 };
  },
  async relation_query() {
    return { items: [], next_cursor: null, total: 0 };
  },
  async relation_review() {
    return undefined;
  },
  async relation_batch_review(relation_ids) {
    return relation_ids.length;
  },
  async relation_group_query() {
    return { items: [], next_cursor: null, total: 0 };
  },
  async relation_group_review() {
    return undefined;
  },
  async relation_group_batch_review(group_ids) {
    return group_ids.length;
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
      has_more: nextOffset < items.length,
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
  async app_status_get() {
    const maintenance = { schema_version: 19, database_size_bytes: 12_582_912, indexed_files: 3, searchable_chunks: 18, embedded_chunks: 0, pending_files: 0, failed_files: 0, active_jobs: 0, log_events: 3, background_notice: null, checks: [{ key: "database", label: "本地数据库", status: "passed" as const, detail: "ok" }, { key: "schema", label: "数据结构", status: "passed" as const, detail: "版本 19" }, { key: "source_readonly", label: "源文件保护", status: "passed" as const, detail: "维护操作只作用于翻翻索引与日志" }], checked_at: now() };
    return { local_only: true as const, source_files_readonly: true as const, roots: [...roots], scan_progress: null, maintenance, inference_runtime: defaultInferenceRuntime, ai_runtime: defaultAiRuntime, recovery_actions: ["view_models" as const], checked_at: now() };
  },
  async runtime_state_get() { return structuredClone(defaultAiRuntime); },
  async maintenance_get() {
    return { schema_version: 19, database_size_bytes: 12_582_912, indexed_files: 3, searchable_chunks: 18, embedded_chunks: 0, pending_files: 0, failed_files: 0, active_jobs: 0, log_events: 3, background_notice: null, checks: [{ key: "database", label: "本地数据库", status: "passed" as const, detail: "ok" }, { key: "schema", label: "数据结构", status: "passed" as const, detail: "版本 19" }, { key: "source_readonly", label: "源文件保护", status: "passed" as const, detail: "维护操作只作用于翻翻索引与日志" }], checked_at: now() };
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
    return { categories, total_bytes, data_directory: "应用管理目录", disk_capacity_bytes: 512 * 1024 ** 3, disk_available_bytes: 128 * 1024 ** 3, soft_quota_bytes: 50 * 1024 ** 3, over_soft_quota: false, background_tasks_paused: false, notice: null, measured_at: now() };
  },
  async storage_location_get() {
    return { active_data_directory: "应用管理目录", pending_target_directory: null, restart_required: false, last_error: null };
  },
  async storage_migration_schedule(selected_directory) {
    return { active_data_directory: "应用管理目录", pending_target_directory: `${selected_directory}\\FanFanData`, restart_required: true, last_error: null };
  },
  async cache_clear(category) {
    return { category, removed_entries: 1, freed_bytes: category === "temporary_cache" ? 1_048_576 : 0 };
  },
  async app_data_reset_schedule() {
    throw new Error("浏览器预览不能重置桌面应用数据，请在翻翻桌面程序中使用。");
  },
  async maintenance_log_query() {
    return { items: [{ log_id: "log-demo", level: "info", component: "catalog", event_name: "scan.completed", fields: { files: 3 }, created_at: now() }], next_cursor: null, total: 1 };
  },
  async maintenance_logs_clear() {
    return 1;
  },
  async node_trace_query(request) {
    const flow = request.flow ?? "ask";
    return {
      items: [
        {
          trace_id: "trace-demo-1",
          flow,
          node: "routing",
          correlation_id: "demo-correlation",
          session_id: null,
          entity_id: null,
          input_json: { question: "你好" },
          output_json: { intent: "Retrieval", top_score: 0.466, margin: 0.066, router_active: true },
          status: "ok",
          elapsed_ms: 12,
          created_at: now(),
        },
        {
          trace_id: "trace-demo-2",
          flow,
          node: "retrieval",
          correlation_id: "demo-correlation",
          session_id: null,
          entity_id: null,
          input_json: { question: "你好", retrieval_limit: 10 },
          output_json: { channels: ["filename", "fts", "embedding", "rrf", "mmr"], candidates: [{ quote: "……", citations: 1 }], insufficient_evidence: false },
          status: "ok",
          elapsed_ms: 3200,
          created_at: now(),
        },
      ],
      next_cursor: null,
      total: 2,
    };
  },
  async node_trace_clear() {
    return 2;
  },
  async diagnostic_event_append() {},
  async diagnostic_export() {
    throw new Error("浏览器预览不写入电脑文件，请在翻翻桌面程序中导出。");
  },
  async index_rebuild() {
    return { operation_id: crypto.randomUUID(), kind: "index_rebuild" as const, status: "queued" as const, created_at: now() };
  },
  async root_list() {
    return [...roots];
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
