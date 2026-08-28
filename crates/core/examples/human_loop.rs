//! 翻翻：真实用户场景完整链路 harness（忠实生产口径）。
//!
//! 目的：对 50 道真实场景题（寒暄/闲聊/问答/文档定位/摘要/比较/存在性/
//! 找不到证据/模糊），跑真实完整链路并列印逐题结果：
//!   节点0  source_router（LLM，复用 source_router_prompt/schema/parse）
//!   节点1  → General 且无个人引用 → 自由闲聊（生产 run_general_chat_answer 同口径）
//!          → 否则 → RAG：answer_extractively（真实 catalog 检索）
//!            ├─ 有证据且 grounded → 真实 LLM 合成（generation_prompt + grounded schema）
//!            └─ insufficient_evidence → rag_refusal（诚实无证据，不编造）
//!
//! 本文件只做通用链路运行与展示，不对具体问题/文件/关键词做特判或硬编码。
//!
//! 环境变量：
//!   FANFAN_CHAT_MODEL   生成模型 tag（默认 qwen3.5:2b）
//!   FANFAN_EMBED_MODEL  嵌入模型 tag（默认 qwen3-embedding:0.6b）
//!   REAL50_SCENARIOS    场景 JSONL（默认 .evaluation-tmp/real50_scenarios.jsonl）
//!   FANFAN_DATA_DIR     catalog db 目录（默认 E:\Desktop\FanFan\DATA\FanFanData）

use std::{
    collections::HashMap,
    env, fs,
    io::Write,
    path::PathBuf,
    sync::atomic::AtomicBool,
};

use fanfan_core::ask::document_summary::{
    SectionChunk, SectionSummary, build_document_sections, digests_json,
    document_overview_prompt, document_summary_prompt, match_section_digests,
    merge_tail_sections, overview_schema, parse_overview, parse_section_summaries,
    section_summary_schema,
};
use fanfan_core::ask::query_parser::{finalize_query_plan, parse_query_plan, query_parser_prompt, query_parser_schema};
use fanfan_core::ask::query_plan::{
    QueryIntent, QueryOperation, QueryPlan, QueryTarget, ResolutionStatus, SourceIntent,
};
use fanfan_core::ask::source_router::{
    apply_ambiguous_override, apply_capability_override, parse_source_routing,
    personal_reference_hit, source_router_prompt,
    source_routing_schema,
};
use fanfan_core::{
    AnswerShape, AnswerStyle, AskRequest, AskSessionContext, Availability, CatalogStore,
    CompareResults, ModelManager, ModelRole, OllamaChatOptions, OllamaClient, ScopeFilter,
    SemanticQuery, MAX_SECTION_CHARS, MAX_SECTIONS, apply_grounded_generation,
    classify_answer_shape, compare_prompt, compare_schema, generation_prompt,
    grounded_answer_json_schema, parse_compare_results, resolve_documents,
};
use fanfan_core::ask::document_resolver::ResolverInput;
use fanfan_core::generation::LocalGenerationRuntime;

use serde_json::json;

const MAX_ROUTING_TOKENS: u32 = 128;
const MAX_CHAT_TOKENS: u32 = 512;
// 有证据的多条 claim 合成会产出较长 JSON；512 在高证据量时会被截断成不完整
// JSON 导致解析失败、回退陈列原文。提升预算让多证据真正完成"增强生成"。
const MAX_GENERATION_TOKENS: u32 = 1024;
const FALLBACK_CHAT_MODEL: &str = "qwen3.5:2b";
const FALLBACK_EMBED_MODEL: &str = "qwen3-embedding:0.6b";

// 文档分层摘要常量（生产 run_document_summary_answer 同口径）。
const SUMMARY_BATCH_CHARS: usize = 3_500;
const SUMMARY_SECTION_CAP_CHARS: usize = 1_200;
const SUMMARY_FALLBACK_CHARS: usize = 220;

/// 嵌入通道配置（与生产检索同口径：真实 artifact_id + 查询前缀）。
struct EmbedConfig {
    artifact_id: String,
    model_id: String,
    query_prefix: String,
}

/// 场景（与 .evaluation-tmp/real50_scenarios.jsonl 字段一致）。
#[derive(serde::Deserialize)]
struct Scenario {
    id: String,
    category: String,
    question: String,
    expected: String,
    note: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("real50_chain 未完成: code={} message={}", error.code, error.message);
        std::process::exit(1);
    }
}

/// 主流程：构建运行时与 catalog → 逐场景跑完整链路 → 汇总。
fn run() -> Result<(), fanfan_core::AppError> {
    let chat_model = env::var("FANFAN_CHAT_MODEL").unwrap_or_else(|_| FALLBACK_CHAT_MODEL.to_owned());
    let embed_model = env::var("FANFAN_EMBED_MODEL").unwrap_or_else(|_| FALLBACK_EMBED_MODEL.to_owned());
    let scenarios_file = env::var("REAL50_SCENARIOS").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.evaluation-tmp/real50_scenarios.jsonl")
    });
    let data_dir = env::var("FANFAN_DATA_DIR")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let model_store = env::var("FANFAN_MODEL_STORE")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanModelStore".to_owned());
    let scenarios = load_scenarios(&scenarios_file)?;
    println!(
        "real50_chain chat_model={chat_model} embed_model={embed_model} scenarios={} src={} data={}",
        scenarios.len(),
        scenarios_file.display(),
        data_dir
    );

    // 打开真实 catalog（只读检索，不写数据）。
    let catalog = CatalogStore::open(PathBuf::from(&data_dir).join("fanfan.db"))?;
    // 预构建 file_id → 显示名映射（索引检索用的 file_id 需还原为文件名）。
    let mut file_names = HashMap::new();
    for file in catalog.list_files()? {
        if file.availability == Availability::Present {
            file_names.insert(file.file_id, file.display_name);
        }
    }

    // 激活真实生成运行时。
    let mut runtime = LocalGenerationRuntime::new();
    let threads = (std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4) / 2)
        .clamp(1, 4);
    runtime.activate(chat_model.as_str(), 4096, threads)?;
    let ollama = OllamaClient::local();
    let cancelled = AtomicBool::new(false);

    // 取真实嵌入通道（与生产检索同口径）：artifact_id + 查询前缀。
    let embed = load_embed_config(&model_store)?;

    // 逐题完整链路并输出「问题/最终回答/引用来源/总耗时」四位报告。
    // 内部 plan/resolution/evidence/citation 等节点信息额外写入 trace 文件，
    // 供后续对"坏"题读取完整 Trace 定位最早错误节点（不进用户可见输出）。
    let trace_out = env::var("HUMAN_TRACE_OUT").map(PathBuf::from).ok();
    println!("== 逐题结果（真人待评价） ==");
    for (index, scenario) in scenarios.iter().enumerate() {
        let started_at = std::time::Instant::now();
        let row = run_scenario(index, &catalog, &mut runtime, &ollama, &embed, &file_names, &cancelled, scenario)?;
        let elapsed_ms = started_at.elapsed().as_millis() as u64;
        println!("\n### 题{} [{category}]  {question}", index + 1, category = row.category, question = row.question);
        println!("回答: {}", row.answer);
        println!("引用/来源: {}", if row.sources.is_empty() { "（无）".to_owned() } else { row.sources.join("、") });
        println!("总耗时: {elapsed_ms} ms");
        if let Some(path) = &trace_out {
            let record = serde_json::json!({
                "index": index,
                "id": row.id,
                "category": row.category,
                "question": row.question,
                "branch": row.branch_label,
                "answer": row.answer,
                "sources": row.sources,
                "elapsed_ms": elapsed_ms,
                "nodes": row.details,
            });
            // trace 是辅助诊断文件，写入失败不影响主流程结果。
            match fs::OpenOptions::new().create(true).append(true).open(path) {
                Ok(mut file) => {
                    if writeln!(file, "{record}").is_err() {
                        eprintln!("[human_loop] 无法写入 trace 记录");
                    }
                }
                Err(error) => {
                    eprintln!(
                        "[human_loop] 无法打开 trace 文件 {}: {error}",
                        path.display()
                    );
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Branch {
    Chat,
    Rag,
    Find,
    NoEvidence,
    Clarify,
}

struct Row {
    id: String,
    category: String,
    question: String,
    branch: Branch,
    branch_label: String,
    ok: bool,
    verbose: bool,
    judgement: String,
    /// 本题的细节输出行（out/plan/resolution/引用等），主循环在题头后统一打印，
    /// 避免 `run_scenario` 内的即时 println 与下一题的题头交错（逐题对齐）。
    details: Vec<String>,
    /// 最终回答正文（完整，供真人评价）。
    answer: String,
    /// 引用/来源（文件显示名；Agent 概览等无文件引用分支为空）。
    sources: Vec<String>,
}

/// 完整链路：source_router → chat / rag。
#[allow(clippy::too_many_arguments)]
fn run_scenario(
    index: usize,
    catalog: &CatalogStore,
    runtime: &mut LocalGenerationRuntime,
    ollama: &OllamaClient,
    embed: &EmbedConfig,
    file_names: &HashMap<uuid::Uuid, String>,
    cancelled: &AtomicBool,
    scenario: &Scenario,
) -> Result<Row, fanfan_core::AppError> {
    let question = scenario.question.trim();
    let vec = encode(embed, ollama, question)?;

    // 本题细节行收集器：run_scenario 内不再直接 println，行由主循环在题头后统一输出，
    // 保证 plan/resolution/out 与所属题头严格对齐（此前逐题交错导致结果误读）。
    let mut detail = Vec::<String>::new();
    macro_rules! detail {
        ($($arg:tt)*) => {
            detail.push(format!($($arg)*))
        };
    }

    // —— 节点0：Source Router（LLM，重试一次）——
    let mut routing = None;
    for _ in 0..2 {
        let (system, user) = source_router_prompt(question, &[]);
        match runtime.complete_json_cancellable(
            &system,
            &user,
            MAX_ROUTING_TOKENS,
            &source_routing_schema(),
            cancelled,
        ) {
            Ok(raw) => {
                routing = parse_source_routing(&raw);
                if routing.is_some() {
                    break;
                }
            }
            Err(_) => {}
        }
    }
    let Some(mut routing) = routing else {
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::Clarify,
            branch_label: "routing_failed_clarify".into(),
            ok: true,
            verbose: true,
            judgement: "路由解析失败→诚实澄清".into(),
            details: detail.clone(),
            answer: clarify_message(question),
            sources: Vec::new(),
        });
    };

    // 确定性澄清兜底（与生产 app_data 同源）：会话记忆恢复 / 缺失所指祈使句 → ambiguous。
    apply_ambiguous_override(question, &mut routing);
    // 确定性能力询问兜底（与生产 app_data 同源）：「你能把……比一比吗」类能力询问
    // 从 local 拉回 general，自然介绍本地资料助手能力，不再误进 RAG 检索。
    apply_capability_override(question, &mut routing);

    // —— General 且无个人引用 → 自由闲聊（生产口径）——
    if routing.source == SourceIntent::General && personal_reference_hit(question).is_none() {
        let system = "你是翻翻，一个运行在用户电脑上的本地资料助手，职责是整理、搜索并基于已授权本地资料回答问题。现在用户是在和你闲聊或寒暄，不需要检索任何资料，请用自然、友好、简洁的中文回应，可以正常聊。注意：只有用户明显在询问本地资料的具体内容时才提示去问资料（对已授权目录发起资料问答）；绝不编造或引用任何本地文件，不要出现页码、引用或“资料显示”之类的措辞。";
        let user = format!("用户说：{}\n\n请自然回应：", question);
        let answer = runtime.complete_cancellable(system, &user, MAX_CHAT_TOKENS, cancelled)?
            .trim()
            .to_owned();
        let expected_chat = matches!(scenario.expected.as_str(), "chat");
        // 印出聊天内容便于核验（verbose 由外部决定，这里统一收集到细节行）。
        detail!("out[chat]: {}", truncate(&answer, 120));
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::Chat,
            branch_label: "free_chat".into(),
            ok: expected_chat && !answer.is_empty(),
            verbose: false,
            judgement: if expected_chat { "聊天命中预期".into() } else { format!("预期{}却走聊天", scenario.expected) }.into(),
            details: detail.clone(),
            answer: answer.trim().to_owned(),
            sources: Vec::new(),
        });
    }

    // —— AMBIGUOUS → 澄清（生产口径：来源不明时请用户澄清，不强行检索）——
    if routing.source == SourceIntent::Ambiguous {
        let expected_clarify = matches!(scenario.expected.as_str(), "clarify" | "rag_or_clarify");
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::Clarify,
            branch_label: "ambiguous_clarify".into(),
            ok: expected_clarify,
            verbose: true,
            judgement: if expected_clarify {
                "语义不明→诚实澄清".into()
            } else {
                format!("预期{}却走澄清", scenario.expected).into()
            },
            details: detail.clone(),
            answer: clarify_message(question),
            sources: Vec::new(),
        });
    }

    // —— RAG：query_parser → resolve_documents → 检索（生产口径）——
    // 先语义解析目标对象与内容查询，再按 intent 分支（DocumentFind 只定位
    // 不检索；其余用 content_query 检索并在解析出的 scope 内检索）。
    let mut scoped = AskRequest {
        question: question.to_owned(),
        session_id: None,
        scope: all_authorized_scope(),
        answer_style: AnswerStyle::Detailed,
        retrieval_limit: 10,
        max_source_files: 6,
        strict_evidence: true,
        clarification_selection: None,
        clarification_message_id: None,
        think_mode: false,
    };
    let mut plan: Option<QueryPlan> = None;
    for _ in 0..2 {
        let (system, user) = query_parser_prompt(question, &[]);
        match runtime.complete_json_cancellable(
            &system,
            &user,
            320,
            &query_parser_schema(),
            cancelled,
        ) {
            Ok(raw) => {
                if let Some(parsed) = parse_query_plan(&raw)
                    .and_then(|p| finalize_query_plan(p, question, &[]))
                {
                    plan = Some(parsed);
                    break;
                }
            }
            Err(_) => {}
        }
    }
    detail!("plan: {}", plan_label(plan.as_ref()));

    // Document Resolver：解析目标 → file_id 白名单（生产口径）。
    let mut resolved_scope: Vec<uuid::Uuid> = Vec::new();
    if let Some(plan) = plan.as_ref() {
        let profiles = catalog.list_document_profiles(None, 2000)?;
        let session = AskSessionContext::default();
        let input = ResolverInput::new(
            plan,
            &session,
            profiles.iter().map(|(profile, _)| profile.clone()).collect(),
            file_names.clone(),
        );
        let resolution = resolve_documents(&input);
        // scope 口径与生产一致（app_data.rs finish_retrieval_with_plan 只取
        // resolved_file_ids，candidates 仅用于澄清，不进检索 scope）：
        // Resolved/MultipleCandidates 已锁定目标文件族 → scope 即锁定结果；
        // 仅当 resolver 未锁定（Unresolved、resolved_file_ids 为空）才退回宽
        // scope，此时用 candidate 池补召回。避免已锁定后还把低分候选尾巴
        // （同族真题等）一并带进检索，淹没被点名的目标文件。
        resolved_scope = resolution.resolved_file_ids.clone();
        if resolved_scope.is_empty() {
            resolved_scope.extend(
                resolution
                    .candidates
                    .iter()
                    .map(|candidate| candidate.file_id),
            );
        }
        resolved_scope.dedup();
        // 候选明细（文件名+得分+命中信号），用于诊断 resolver 为何锁/不锁某文档。
        let candidate_debug = resolution
            .candidates
            .iter()
            .map(|candidate| {
                let name = file_names
                    .get(&candidate.file_id)
                    .map(String::as_str)
                    .unwrap_or("?");
                format!(
                    "{}@score={:.3}[{}]",
                    name,
                    candidate.score,
                    candidate.signals.join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(" || ");
        detail!(
            "resolution: status={} scope={} ${:?}",
            resolution_status_label(&resolution),
            resolved_scope.len(),
            resolution.fallback_reason.as_deref().unwrap_or("")
        );
        detail!("resolution.candidates: {candidate_debug}");
    }

    // LIBRARY_OVERVIEW：整库概览（生产 run_agent_library_overview 口径）。
    // 纯概览问句（LibraryQa + 无 content_query）时，只读画像元数据聚合
    // 文件数/类型分布/最近更新，不触发底层 chunk 检索，避免对无正文实体
    // 的整库盘点误走 rag 而 no_evidence。与生产 ask_tools::library_overview
    // 同统计口径（list_document_profiles 全量）。通用聚合，不暴露授权外细节。
    if plan
        .as_ref()
        .is_some_and(|plan| {
            plan.intent == QueryIntent::LibraryQa && plan.content_query.is_none()
        })
    {
        let profiles = catalog.list_document_profiles(None, 10_000)?;
        let total_files = profiles.len();
        let mut counts = HashMap::<Option<String>, usize>::new();
        for (profile, _file_name) in &profiles {
            let key = profile.document_type.map(|ty| ty.as_str().to_owned());
            *counts.entry(key).or_insert(0) += 1;
        }
        let mut type_parts = counts
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .collect::<Vec<_>>();
        type_parts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let type_desc = type_parts
            .iter()
            .map(|(ty, count)| {
                let label = ty.as_deref().unwrap_or("unknown");
                format!("{label} {count} 份")
            })
            .collect::<Vec<_>>()
            .join("、");
        let answer = if type_desc.is_empty() {
            format!("知识库里共有 {total_files} 份已索引资料。")
        } else {
            format!("知识库里共有 {total_files} 份已索引资料。\n其中：{type_desc}。")
        };
        detail!("out[library_overview] total={total_files} types={type_desc}");
        let is_overview_expected = matches!(scenario.expected.as_str(), "rag")
            || matches!(scenario.expected.as_str(), "rag_or_overview");
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::Rag,
            branch_label: format!("library_overview(total={total_files})"),
            ok: is_overview_expected,
            verbose: false,
            judgement: if is_overview_expected {
                "整库概览已聚合".into()
            } else {
                format!("预期{}却整库概览", scenario.expected)
            }
            .into(),
            details: detail.clone(),
            answer,
            // 整库概览是元数据聚合，不是对某份文件的抽取引用；不按文件名逐条陈列
            // 来源（ground truth 明确「不逐条陈列文件名」），避免把全库文件名泄出。
            sources: Vec::new(),
        });
    }

    // DOCUMENT_FIND：只定位目标文件，不跑 chunk 检索（生产 run_document_find_answer 口径）。
    if plan
        .as_ref()
        .is_some_and(|plan| plan.intent == QueryIntent::DocumentFind)
    {
        let found_names = resolved_scope
            .iter()
            .filter_map(|id| file_names.get(id))
            .cloned()
            .collect::<Vec<_>>();
        let found = !resolved_scope.is_empty();
        let expected_find = matches!(scenario.expected.as_str(), "rag_find");
        for name in &found_names {
            detail!("↳ {name}");
        }
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::Find,
            branch_label: if found {
                format!("find(n={})", resolved_scope.len())
            } else {
                "find(not found)".into()
            },
            ok: found == expected_find,
            verbose: true,
            judgement: if found && expected_find {
                "定位到目标文件".into()
            } else if !found && !expected_find {
                "诚实未定位到目标文件".into()
            } else {
                format!("预期{}却定位{}", scenario.expected, if found { "到" } else { "un到" })
            }.into(),
            details: detail.clone(),
            answer: if found { found_names.join("、") } else { "未定位到目标文件".to_owned() },
            sources: found_names,
        });
    }

    // DOCUMENT_SUMMARY：整文分层摘要（生产 run_document_summary_answer 同口径）：
    // 章节分组 → 分批逐节摘要（LLM，失败确定性节内摘录回退）→ 总览聚合。
    if plan
        .as_ref()
        .is_some_and(|plan| plan.intent == QueryIntent::DocumentSummary)
    {
        return run_summary_scenario(
            catalog,
            runtime,
            file_names,
            cancelled,
            &resolved_scope,
            scenario,
            detail,
        );
    }

    // COMPARE_DOCUMENTS：两侧分别取证 → 比较生成（生产 run_compare_answer 同口径）：
    // 从 resolved_scope + candidates 取 top-2 文件，两侧各跑单文件 extractive 检索，
    // 取两侧真实 chunk 原文（quote/evidence）进 compare_prompt，LLM 输出
    // similarities/differences/conclusion；任一侧无取证材料或解析失败时，按生产
    // 语义确定性回退为「两侧原文并排呈现」，绝不凭空生成比较结论。
    if let Some(compare_plan) = plan.as_ref() {
        if compare_plan.intent == QueryIntent::CompareDocuments {
            return run_compare_scenario(
                catalog,
                runtime,
                ollama,
                embed,
                &vec,
                file_names,
                cancelled,
                compare_plan,
                &resolved_scope,
                scenario,
                detail,
            );
        }
    }

    // 其余 intent：用 content_query 作为检索词 + scope 限定。
    if let Some(plan) = plan.as_ref() {
        if let Some(content_query) = plan.content_query.clone() {
            scoped.question = content_query;
        }
        if plan.requires_document_resolution && !resolved_scope.is_empty() {
            scoped.scope.file_ids = resolved_scope.clone();
        }
    }
    let ask_request = scoped;
    // 与生产 RAG 对齐：QueryPlan 已把目标文件身份与 content_query 分离，
    // 检索请求改成 content_query 后必须同步重算语义向量。继续复用原始整句
    // 向量会把年份/文件名信号带进 chunk 检索，造成正确 scope 内的内容漏召回。
    let retrieval_vec = encode(embed, ollama, &ask_request.question)?;
    let semantic_query = Some(SemanticQuery {
        model_artifact_id: &embed.artifact_id,
        vector: &retrieval_vec,
    });
    let mut answer = if ask_request.scope.file_ids.is_empty() {
        catalog.answer_extractively(&ask_request, semantic_query)
    } else {
        catalog.answer_extractively_in_authoritative_scope(&ask_request, semantic_query)
    }?;
    // Answerability Gate（生产 RAG 同口径，Phase 4.2）：用证据引文 + 章节标题
    // 做实体/中文内容词一致性门控；NotAnswerable → 按 LOCAL 无证据统一文案拒答。
    let gate_evidence: Vec<fanfan_core::GateEvidence> = answer
        .claims
        .iter()
        .map(|claim| fanfan_core::GateEvidence {
            text: claim.text.clone(),
            heading: claim
                .citations
                .first()
                .and_then(|citation| citation.locator.heading_path.last().cloned()),
        })
        .collect::<Vec<_>>();
    let gate_plan = plan.clone().unwrap_or_else(|| QueryPlan {
        operation: QueryOperation::Qa,
        ..QueryPlan::default()
    });
    let gate_input = fanfan_core::AnswerabilityInput {
        question,
        content_query: Some(ask_request.question.trim()),
        plan: &gate_plan,
        evidence: &gate_evidence,
    };
    let gate_verdict = fanfan_core::evaluate_answerability(&gate_input);
    // 生产 RAG 同源的概念退化：门控拒绝 + 教科书概念题（shape=Description）→ 本地
    // 无证据时用通用知识讲解（绝不引用本地文件）。非概念题（boolean/list 等）仍走
    // 统一拒答——绝不替用户杜撰资料里是否有某内容。
    let shape = classify_answer_shape(question, &gate_plan);
    let mut concept_degraded = false;
    if gate_verdict.status == fanfan_core::AnswerabilityStatus::NotAnswerable {
        detail!(
            "out[rag:gate] verdict={} reason={}",
            gate_verdict.status.as_str(),
            gate_verdict.reason
        );
        answer.insufficient_evidence = true;
        answer.claims.clear();
        answer.source_files.clear();
        answer.used_file_ids.clear();
        answer.grounding_status = fanfan_core::GroundingStatus::Insufficient;
        if shape == AnswerShape::Description {
            let concept_system = "你是翻翻，运行在用户电脑上的本地资料助手。用户在问一道教科书概念/知识题（不是问具体某份文件里的内容）。你检查了本地资料库，没有检索到可以直接引用的相关证据，因此**不再尝试引用任何本地文件，改用你的通用知识作答**。要求：1) 先在开头用一句话说明「本地资料中没有找到直接对应的内容，以下为通用概念讲解」；2) 然后准确、简洁、有条理地讲解该概念，用 Markdown 分点；3) 绝对不要出现页码、\"根据资料\"\"资料显示\"等措辞，不要编造任何本地文件名。";
            let concept_user = format!("用户问：{}\n\n请用通用知识讲解：", question.trim());
            // ollama.chat 返回正文 String（含 message.content）；空/失败则回退统一拒答。
            let concept_text = ollama
                .chat(
                    &chat_model_for(ollama, &chat_model_or_default())?,
                    serde_json::json!([
                        { "role": "system", "content": concept_system },
                        { "role": "user", "content": concept_user }
                    ]),
                    OllamaChatOptions {
                        num_predict: Some(512),
                        temperature: Some(0.0),
                        num_ctx: Some(4096),
                        think: Some(false),
                    },
                    None,
                    Some(&cancelled),
                )
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty());
            if let Some(concept_text) = concept_text {
                concept_degraded = true;
                answer.answer = concept_text;
                answer.degradation_reason = Some(
                    "概念题本地无证据→退化为通用知识作答（未引用本地文件）".to_owned(),
                );
            } else {
                answer.answer = fanfan_core::local_no_evidence_answer(
                    question,
                    &gate_verdict.missing_entities,
                    false,
                );
            }
            answer.answer_mode = fanfan_core::AnswerMode::RagRefusal;
            answer.no_evidence_reason = Some(fanfan_core::NoEvidenceReason::AnswerabilityRejected);
        } else {
            answer.answer = fanfan_core::local_no_evidence_answer(
                question,
                &gate_verdict.missing_entities,
                false,
            );
            answer.answer_mode = fanfan_core::AnswerMode::RagRefusal;
            answer.no_evidence_reason = Some(fanfan_core::NoEvidenceReason::AnswerabilityRejected);
        }
    }
    if answer.insufficient_evidence {
        // 诚实无证据。若 expected 是 no_evidence → 正确；若 expected 是 rag 且属
        // 于「有资料却拒绝」→ 标记为问题（note 提供线索）。
        let idx = index;
        detail!("out[rag:no_evidence] reason={:?} coverage={:.2} files={}",
            answer.no_evidence_reason, answer.index_coverage, answer.used_file_ids.len());
        let expected_noev = matches!(scenario.expected.as_str(), "rag_no_evidence");
        let expected_clarify = matches!(scenario.expected.as_str(), "clarify");
        // 概念退化：本地无证据已退化为通用知识讲解（未引用本地文件）——对预期 rag 的
        // 概念题是有效结果（用户拿到了讲解），不算「有资料却拒绝」。
        let ok = expected_noev || concept_degraded;
        let judgement = if expected_noev {
            "诚实无证据，符合预期".into()
        } else if concept_degraded {
            "概念题本地无证据→通用知识讲解（未引用本地文件）".into()
        } else if expected_clarify {
            "应澄清却直接无证据拒绝".into()
        } else {
            format!("第{}题预期{}却无证据拒绝(可能需要澄清/降级)", idx+1, scenario.expected)
        };
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::NoEvidence,
            branch_label: format!("rag_no_evidence({:?})", answer.no_evidence_reason),
            ok,
            verbose: !expected_noev,
            judgement,
            details: detail.clone(),
            answer: answer.answer.clone(),
            sources: Vec::new(),
        });
    }

    // —— 有证据 → 真实 LLM 合成（grounded generation）——
    let prompt = generation_prompt(&ask_request, &answer, &[]);
    let messages = serde_json::json!([
        { "role": "system", "content": "你是翻翻的本地资料回答器。只能使用用户提供的证据；每个事实、数字、日期、姓名都必须原样来自证据，证据中未出现的信息一律不得写出；每个事实必须通过citation_ids关联证据。" },
        { "role": "user", "content": prompt }
    ]);
    let generated = ollama.chat(
        &chat_model_for(ollama, &chat_model_or_default())?,
        messages,
        OllamaChatOptions {
            num_predict: Some(MAX_GENERATION_TOKENS),
            temperature: Some(0.0),
            num_ctx: Some(8192),
            think: Some(false),
        },
        Some(grounded_answer_json_schema()),
        Some(cancelled),
    );
    // 先按引用借用原始输出，避免在 if-let 守卫里移动 `generated`。
    let generated_ref = generated.as_ref().map(|text| text.as_str()).ok();
    let final_answer = if let Some(raw) = generated_ref
        && let Some(grounded) = apply_grounded_generation(&answer, raw)
    {
        grounded
    } else {
        // 诊断用：合成失败时把模型原始输出落盘，便于定位生成是否真的产出 JSON。
        if let Some(path) = env::var("HUMAN_DUMP_GEN").map(PathBuf::from).ok()
            && let Ok(text) = &generated
        {
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
                let _ = writeln!(
                    file,
                    "### {} raw_chars={}\n{}\n-----",
                    scenario.id,
                    text.chars().count(),
                    text
                );
            }
        }
        answer
    };
    let source_names = final_answer
        .used_file_ids
        .iter()
        .filter_map(|id| file_names.get(id))
        .cloned()
        .collect::<Vec<_>>();
    let coverage = final_answer
        .claims
        .iter()
        .flat_map(|c| &c.citations)
        .filter(|c| c.locator.page_no.is_some())
        .count();
    let citation_n = final_answer.claims.iter().flat_map(|c| &c.citations).count();
    detail!("out[rag] files={} cites={} loc_ok={} grounded={:?}",
        source_names.len(), citation_n, coverage, final_answer.grounding_status);
    for name in &source_names {
        detail!("↳ {name}");
    }
    // 输出回答正文（首段）。
    detail!("answer: {}", truncate(&final_answer.answer, 220));
    let is_rag_expected = match scenario.expected.as_str() {
        "chat" | "rag_no_evidence" | "clarify" | "rag_find" => false,
        _ => true,
    };
    let ok = is_rag_expected && !final_answer.answer.trim().is_empty();
    return Ok(Row {
        id: scenario.id.clone(),
        category: scenario.category.clone(),
        question: question.to_owned(),
        branch: Branch::Rag,
        branch_label: format!("rag(cites={citation_n},loc={coverage})"),
        ok,
        verbose: false,
        judgement: if is_rag_expected {
            format!("命中证据，grounded={:?}", final_answer.grounding_status)
        } else {
            format!("预期{}却走RAG", scenario.expected)
        }.into(),
        details: detail.clone(),
        answer: final_answer.answer.clone(),
        sources: source_names,
    });
}

/// COMPARE_DOCUMENTS：两侧分别取证 → 比较生成（生产 run_compare_answer 同口径）。
/// 从 resolved_scope 取 top-2 文件作为两侧，各跑一次单文件 extractive 检索拿到真实
/// chunk 原文（quote/evidence）；用 compare_prompt 让 LLM 输出 similarities /
/// differences / conclusion。任一侧无取证材料或 LLM 解析失败时，按生产语义确定性
/// 回退为「两侧原文并排呈现」或诚实拒答，绝不凭空生成比较结论。
#[allow(clippy::too_many_arguments)]
fn run_compare_scenario(
    catalog: &CatalogStore,
    runtime: &mut LocalGenerationRuntime,
    ollama: &OllamaClient,
    embed: &EmbedConfig,
    _vec: &[f32],
    file_names: &HashMap<uuid::Uuid, String>,
    cancelled: &AtomicBool,
    plan: &QueryPlan,
    resolved_scope: &[uuid::Uuid],
    scenario: &Scenario,
    detail: Vec<String>,
) -> Result<Row, fanfan_core::AppError> {
    let question = scenario.question.trim();
    let mut detail = detail;
    macro_rules! detail {
        ($($arg:tt)*) => {
            detail.push(format!($($arg)*))
        };
    }
    // 独立解析一份目标引用 → 单个 file_id（生产 run_compare_answer 的
    // secondary_target 解析同口径：先用该引用做 Resolver，再落到首个候选）。
    let resolve_one = |target: &QueryTarget, candidates: &[uuid::Uuid]| -> Option<uuid::Uuid>
    {
        let mut secondary_plan = plan.clone();
        secondary_plan.target = target.clone();
        let profiles = catalog
            .list_document_profiles(None, 2000)
            .ok()?
            .into_iter()
            .map(|(profile, _)| profile)
            .collect::<Vec<_>>();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&secondary_plan, &session, profiles, file_names.clone());
        let resolution = resolve_documents(&input);
        resolution
            .resolved_file_ids
            .first()
            .copied()
            .or_else(|| resolution.candidates.first().map(|c| c.file_id))
            .or_else(|| candidates.first().copied())
    };
    // 1) 确定两侧文件：primary 从 target 独立解析，secondary 从 secondary_target
    //    独立解析（parser 拆出成对引用时优先）；都找不到再退回 resolved_scope top-2。
    let side_a = if !plan
        .target
        .reference
        .as_deref()
        .map(|r| r.trim())
        .unwrap_or("")
        .is_empty()
    {
        resolve_one(&plan.target, resolved_scope)
    } else {
        resolved_scope.first().copied()
    };
    let side_b = plan
        .secondary_target
        .as_ref()
        .and_then(|secondary| resolve_one(secondary, resolved_scope))
        .or_else(|| {
            resolved_scope
                .iter()
                .copied()
                .find(|id| Some(*id) != side_a)
        });
    let (Some(side_a), Some(side_b)) = (side_a, side_b) else {
        let found = resolved_scope
            .iter()
            .map(|id| file_names.get(id).cloned().unwrap_or_default())
            .collect::<Vec<_>>()
            .join(", ");
        detail!("out[compare] 两侧文件不足2个（{found}），无法比较");
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::NoEvidence,
            branch_label: "compare(insufficient target)".into(),
            ok: matches!(scenario.expected.as_str(), "rag_no_evidence"),
            verbose: true,
            judgement: "比较场景未能锁定两侧文件，诚实拒答".into(),
            details: detail,
            answer: "比较场景未能锁定两侧文件，无法给出比较结果。".to_owned(),
            sources: Vec::new(),
        });
    };
    if side_a == side_b {
        detail!("out[compare] 两侧解析到同一文件，无法比较");
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::NoEvidence,
            branch_label: "compare(same side)".into(),
            ok: matches!(scenario.expected.as_str(), "rag_no_evidence"),
            verbose: true,
            judgement: "比较场景两侧指向同一文件，诚实拒答".into(),
            details: detail,
            answer: "比较场景两侧指向同一文件，无法比较。".to_owned(),
            sources: Vec::new(),
        });
    }
    let a_name = file_names
        .get(&side_a)
        .cloned()
        .unwrap_or_else(|| side_a.to_string());
    let b_name = file_names
        .get(&side_b)
        .cloned()
        .unwrap_or_else(|| side_b.to_string());

    // 2) 编码对比问题（与生产同口径：content_query 或原问题 + 刷新语义，用实际模型）。
    let question_text = format!("{}{}", embed.query_prefix, question);
    let vec = encode(embed, ollama, &question_text)?;

    // 3) 两侧分别取证：scope = 单侧文件，各跑一次 extractive 检索。
    let mut side_materials: Vec<(uuid::Uuid, String, Vec<String>, Vec<fanfan_core::EvidenceRef>)> =
        Vec::new();
    for file_id in [side_a, side_b] {
        let sub_request = AskRequest {
            question: question_text.clone(),
            session_id: None,
            scope: ScopeFilter {
                file_ids: vec![file_id],
                ..all_authorized_scope()
            },
            answer_style: AnswerStyle::Detailed,
            retrieval_limit: 6,
            max_source_files: 1,
            strict_evidence: true,
            clarification_selection: None,
        clarification_message_id: None,
        think_mode: false,
        };
        let result = catalog.answer_extractively_in_authoritative_scope(
            &sub_request,
            Some(SemanticQuery {
                model_artifact_id: &embed.artifact_id,
                vector: &vec,
            }),
        )?;
        let name = file_names
            .get(&file_id)
            .cloned()
            .unwrap_or_else(|| file_id.to_string());
        let mut quotes = Vec::<String>::new();
        let mut evidence = Vec::<fanfan_core::EvidenceRef>::new();
        for claim in result.claims.into_iter().take(5) {
            for citation in claim.citations {
                if quotes.len() >= 5 {
                    break;
                }
                quotes.push(compact_for_prompt(&citation.quote, 500));
                evidence.push(citation);
            }
        }
        side_materials.push((file_id, name, quotes, evidence));
    }
    let (_, a_name_used, a_quotes, a_evidence) = &side_materials[0];
    let (_, b_name_used, b_quotes, b_evidence) = &side_materials[1];
    if a_quotes.is_empty() || b_quotes.is_empty() {
        detail!(
            "out[compare] 两侧取证不足（{} 命中 {}，{} 命中 {}）→ 诚实拒绝",
            a_name_used,
            a_quotes.len(),
            b_name_used,
            b_quotes.len()
        );
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::NoEvidence,
            branch_label: "compare(no material)".into(),
            ok: matches!(scenario.expected.as_str(), "rag_no_evidence"),
            verbose: true,
            judgement: "两侧未找到可比较内容，诚实拒答".into(),
            details: detail,
            answer: "两侧未找到可比较的内容，无法给出比较结果。".to_owned(),
            sources: Vec::new(),
        });
    }

    // 4) 比较生成（JSON Schema 约束）；失败 → 确定性回退并排侧原文。
    let (system, user) = compare_prompt(
        &a_name,
        a_quotes,
        &b_name,
        b_quotes,
        &format!("{}{}", embed.query_prefix, question),
    );
    let results = runtime
        .complete_json_cancellable(&system, &user, 800, &compare_schema(), cancelled)
        .ok()
        .and_then(|raw| parse_compare_results(&raw))
        .unwrap_or_else(|| CompareResults {
            similarities: Vec::new(),
            differences: Vec::new(),
            conclusion: String::new(),
        });

    // 5) 组装回答与真实引用。
    let mut answer_parts = Vec::<String>::new();
    let mut total_cites = 0usize;
    if !results.conclusion.is_empty() {
        answer_parts.push(format!("**结论**：{}", results.conclusion));
        total_cites += a_evidence.len().min(1) + b_evidence.len().min(1);
    }
    if !results.similarities.is_empty() {
        answer_parts.push("## 相同点".to_owned());
        for point in &results.similarities {
            answer_parts.push(format!("- {}", point.point));
        }
        total_cites += a_evidence.len().min(1) + b_evidence.len().min(1);
    }
    if !results.differences.is_empty() {
        answer_parts.push("## 差异点".to_owned());
        let mut diff_cites = 0usize;
        for difference in &results.differences {
            if diff_cites >= a_evidence.len().min(1) + b_evidence.len().min(1) {
                break; // 引用上限：两侧各取首条证据
            }
            answer_parts.push(format!("- {}", difference.point));
            diff_cites += a_evidence.len().min(1) + b_evidence.len().min(1);
        }
        total_cites += diff_cites;
    }
    let answer_text = if answer_parts.is_empty() {
        // LLM 未产出比较结构 → 确定性回退：两侧检索到的原文并排呈现。
        let mut parts = vec!["未能生成对比结论，以下为两侧检索到的原文依据：".to_owned()];
        for (index, (_, name, quotes, _)) in side_materials.iter().enumerate() {
            parts.push(format!("### 侧 {}：{}", index + 1, name));
            for (quote_index, quote) in quotes.iter().take(4).enumerate() {
                parts.push(format!("{}. {}", quote_index + 1, quote));
            }
        }
        parts.join("\n\n")
    } else {
        answer_parts.join("\n\n")
    };

    detail!(
        "out[compare] A={} B={} a_quotes={} b_quotes={} cites≈{} sim={} diff={} conv={}",
        a_name,
        b_name,
        a_quotes.len(),
        b_quotes.len(),
        total_cites,
        results.similarities.len(),
        results.differences.len(),
        !results.conclusion.is_empty(),
    );
    let source_names = [side_a, side_b]
        .iter()
        .filter_map(|id| file_names.get(id))
        .cloned()
        .collect::<Vec<_>>();
    for name in &source_names {
        detail!("↳ {name}");
    }
    detail!("answer: {}", truncate(&answer_text, 220));
    let is_rag_expected = match scenario.expected.as_str() {
        "chat" | "rag_no_evidence" | "clarify" | "rag_find" => false,
        _ => true,
    };
    let ok = is_rag_expected && !answer_text.trim().is_empty();
    return Ok(Row {
        id: scenario.id.clone(),
        category: scenario.category.clone(),
        question: question.to_owned(),
        branch: Branch::Rag,
        branch_label: format!("compare(A={},B={})", a_name, b_name),
        ok,
        verbose: true,
        judgement: if ok {
            "比较两侧已取出真实证据并生成结论".into()
        } else {
            format!("预期{}却走比较", scenario.expected)
        }
        .into(),
        details: detail,
        answer: answer_text,
        sources: source_names,
    });
}
fn all_authorized_scope() -> ScopeFilter {
    ScopeFilter {
        root_ids: Vec::new(),
        collection_ids: Vec::new(),
        file_ids: Vec::new(),
        extensions: Vec::new(),
        modified_from: None,
        modified_to: None,
        availability: Availability::Present,
    }
}

/// 通过 Ollama /api/embed 编码查询，应用生产同口径的查询前缀。
fn encode(embed: &EmbedConfig, ollama: &OllamaClient, text: &str) -> Result<Vec<f32>, fanfan_core::AppError> {
    let prefixed = format!("{}{}", embed.query_prefix, text);
    let (vectors, _dim) = ollama.embed(&embed.model_id, &[prefixed])?;
    vectors.into_iter().next().ok_or_else(|| {
        fanfan_core::AppError::new("REAL50_EMBEDDING_EMPTY", "Embedding 返回为空", true)
    })
}

/// 从 ModelStore 打开当前激活的 Embedding artifact（含查询前缀），
/// 生成模型 tag 由运行参数提供，与诊断工具同口径。
fn load_embed_config(model_store: &str) -> Result<EmbedConfig, fanfan_core::AppError> {
    let manager = ModelManager::open_store(PathBuf::from(model_store))?;
    let artifact = manager
        .list_artifacts()?
        .into_iter()
        .find(|artifact| artifact.role == ModelRole::Embedding)
        .ok_or_else(|| {
            fanfan_core::AppError::new("REAL50_NO_EMBED_ARTIFACT", "注册表没有 Embedding 模型", true)
        })?;
    if artifact.format == fanfan_core::ModelFormat::Ollama {
        manager.ollama_embedding_ready(&artifact.model_id)?;
    }
    let active = manager.active_artifact(ModelRole::Embedding)?.ok_or_else(|| {
        fanfan_core::AppError::new("REAL50_NO_ACTIVE_EMBED", "没有激活的 Embedding 模型", true)
    })?;
    Ok(EmbedConfig {
        artifact_id: active.artifact_id.to_string(),
        model_id: active.model_id,
        query_prefix: active.query_prefix.unwrap_or_default(),
    })
}

/// 占位：实际模型 tag 已在环境变量固定，这里直接返回 chat 模型。为区分见下。
fn chat_model_for(_ollama: &OllamaClient, tag: &str) -> Result<String, fanfan_core::AppError> {
    Ok(tag.to_owned())
}

fn chat_model_or_default() -> String {
    env::var("FANFAN_CHAT_MODEL").unwrap_or_else(|_| FALLBACK_CHAT_MODEL.to_owned())
}

/// 解析后的 QueryPlan 摘要（用于逐题展示，不受具体字段硬编码影响）。
fn plan_label(plan: Option<&QueryPlan>) -> String {
    match plan {
        Some(plan) => format!(
            "intent={} op={} ref={:?} dtype={:?} dname={:?} etype={:?} ename={:?} content={:?}",
            plan.intent.as_str(),
            plan.operation.as_str(),
            plan.target.reference.as_deref(),
            plan.target.document_type,
            plan.target.document_name.as_deref(),
            plan.target.entity_type.as_deref(),
            plan.target.entity_name.as_deref(),
            plan.content_query.as_deref().unwrap_or("-"),
        ),
        None => "parse_failed".into(),
    }
}

/// Document Resolver 状态名（保持枚举可显示）。
fn resolution_status_label(resolution: &fanfan_core::DocumentResolution) -> String {
    match resolution.status {
        ResolutionStatus::Resolved => "resolved".into(),
        ResolutionStatus::MultipleCandidates => "multiple".into(),
        ResolutionStatus::Unresolved => "unresolved".into(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        text
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}

/// 澄清分支的通用文案：不针对具体题，仅基于原始问句生成一次澄清请求，
/// 供真人据此判断「走澄清分支」这一行为是否恰当。
fn clarify_message(question: &str) -> String {
    format!(
        "抱歉，我还没完全确定你的意思。关于“{}”，你是指本地资料里的哪一份或哪一类内容呢？方便的话补充一下。",
        truncate(question, 40)
    )
}

/// 单节文本进入摘要 prompt 前的确定性压缩（生产同口径，见 compact_for_prompt）。
fn compact_for_prompt(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        value.to_owned()
    } else {
        let mut out: String = value.chars().take(limit).collect();
        out.push('…');
        out
    }
}

/// DOCUMENT_SUMMARY：整文分层摘要（生产 run_document_summary_answer 同口径）。
///
/// 章节分组（build_document_sections + merge_tail_sections）→ 分批逐节摘要
/// （LLM，失败按确定性节内摘录回退）→ 总览聚合（LLM，失败用逐节摘要拼接）。
/// 只对真实章节原文做概括，引用全部来自文档自身内容，不生成不存在的引用。
#[allow(clippy::too_many_arguments)]
fn run_summary_scenario(
    catalog: &CatalogStore,
    runtime: &mut LocalGenerationRuntime,
    file_names: &HashMap<uuid::Uuid, String>,
    cancelled: &AtomicBool,
    resolved_scope: &[uuid::Uuid],
    scenario: &Scenario,
    detail: Vec<String>,
) -> Result<Row, fanfan_core::AppError> {
    let question = scenario.question.trim();
    let mut detail = detail;
    macro_rules! detail {
        ($($arg:tt)*) => {
            detail.push(format!($($arg)*))
        };
    }
    let Some(&target_file) = resolved_scope.first() else {
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::NoEvidence,
            branch_label: "summary(no target)".into(),
            ok: matches!(scenario.expected.as_str(), "rag_no_evidence"),
            verbose: true,
            judgement: "摘要目标未锁定，无法概括".into(),
            details: detail,
            answer: "未定位到要概括的目标文件。".to_owned(),
            sources: Vec::new(),
        });
    };
    let file_name = file_names
        .get(&target_file)
        .cloned()
        .unwrap_or_else(|| target_file.to_string());

    // 1) 整份文档结构：document_nodes 分页读取 heading_path（与生产同口径）。
    let mut node_heading_paths = HashMap::new();
    let mut offset = 0usize;
    loop {
        let preview = catalog.file_preview_page(&target_file, offset, 200, None)?;
        let batch_len = preview.nodes.len();
        for node in preview.nodes {
            node_heading_paths.insert(node.node_id, node.heading_path);
        }
        match preview.next_offset {
            Some(next) => offset = next as usize,
            None => break,
        }
        if batch_len == 0 {
            break;
        }
    }

    // 2) 当前修订全部 chunk（保持 ordinal 顺序，映射为 SectionChunk）。
    let section_chunks: Vec<SectionChunk> = catalog
        .file_chunks(&target_file)?
        .into_iter()
        .map(|chunk| SectionChunk {
            chunk_id: chunk.chunk_id,
            node_id: chunk.node_id,
            revision_id: chunk.revision_id,
            ordinal: chunk.ordinal,
            text: chunk.text,
            locator: chunk.locator,
        })
        .collect();
    if section_chunks.is_empty() {
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::NoEvidence,
            branch_label: "summary(no chunks)".into(),
            ok: matches!(scenario.expected.as_str(), "rag_no_evidence"),
            verbose: true,
            judgement: "目标文件无可概括的正文内容（可能仍在解析或纯图片）".into(),
            details: detail,
            answer: "目标文件暂无正文内容可供概括。".to_owned(),
            sources: vec![file_name.clone()],
        });
    }

    // 3) 章节分组 + 尾部合并（约束节数）。
    let mut sections =
        build_document_sections(&section_chunks, &node_heading_paths, MAX_SECTION_CHARS);
    merge_tail_sections(&mut sections, MAX_SECTIONS);
    if sections.is_empty() {
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            question: question.to_owned(),
            branch: Branch::NoEvidence,
            branch_label: "summary(no sections)".into(),
            ok: matches!(scenario.expected.as_str(), "rag_no_evidence"),
            verbose: true,
            judgement: "目标文件未产出章节".into(),
            details: detail,
            answer: "目标文件未识别出可用章节结构。".to_owned(),
            sources: vec![file_name.clone()],
        });
    }

    // 4) 分批逐节摘要（LLM 按顺序输出；解析失败回退为确定性节内摘录）。
    let mut digests = Vec::<SectionSummary>::with_capacity(sections.len());
    let mut batch_fallbacks = 0_usize;
    let mut index = 0usize;
    while index < sections.len() {
        let mut batch = Vec::new();
        let mut batch_chars = 0usize;
        while index < sections.len() && (batch.is_empty() || batch_chars < SUMMARY_BATCH_CHARS) {
            let section = &sections[index];
            let compacted = compact_for_prompt(&section.text(), SUMMARY_SECTION_CAP_CHARS);
            if !batch.is_empty() && batch_chars.saturating_add(compacted.len()) > SUMMARY_BATCH_CHARS {
                break;
            }
            batch_chars = batch_chars.saturating_add(compacted.len());
            batch.push((index, section, compacted));
            index += 1;
        }
        let payload = batch
            .iter()
            .map(|(_, section, compacted)| json!({"title": section.title, "content": compacted}))
            .collect::<Vec<_>>();
        let (system, user) = document_summary_prompt(&file_name, None, &json!(&payload).to_string());
        let parsed =
            match runtime.complete_json_cancellable(&system, &user, 640, &section_summary_schema(), cancelled)
            {
                Ok(raw) => parse_section_summaries(&raw),
                Err(_) => Vec::new(),
            };
        let batch_start = batch.first().map(|(first, _, _)| *first).unwrap_or(index);
        let (batch_digests, batch_fallback_count) =
            match_section_digests(&sections[batch_start..index], parsed, SUMMARY_FALLBACK_CHARS);
        if batch_fallback_count > 0 {
            batch_fallbacks += 1;
        }
        digests.extend(batch_digests);
    }
    detail!(
        "out[summary] sections={} digests={} batch_fallbacks={}",
        sections.len(),
        digests.len(),
        batch_fallbacks
    );

    // 5) 总览聚合（LLM 失败时退化为逐节摘要拼接）。
    let payload = digests_json(&digests);
    let (system, user) = document_overview_prompt(&file_name, None, &payload.to_string());
    let overview = runtime
        .complete_json_cancellable(&system, &user, 512, &overview_schema(), cancelled)
        .ok()
        .and_then(|raw| parse_overview(&raw))
        .unwrap_or_else(|| {
            let structure = digests
                .iter()
                .map(|digest| fanfan_core::StructureEntry {
                    title: digest.title.clone(),
                    key_points: digest.key_points.clone(),
                })
                .collect();
            fanfan_core::DocumentOverview {
                overview: String::new(),
                overall_summary: digests
                    .iter()
                    .map(|digest| digest.summary.as_str())
                    .collect::<Vec<_>>()
                    .join("；"),
                structure,
            }
        });

    // 6) 输出与判定：摘要只使用文档自身内容，预期为 rag 时须产出节摘要。
    let summary_ok = !digests.is_empty();
    let answer_preview = if summary_ok {
        truncate(&digests[0].summary, 180)
    } else {
        String::new()
    };
    detail!("answer[summary]: {}", answer_preview);
    if !overview.overall_summary.is_empty() {
        detail!(
            "overall[summary]: {}",
            truncate(&overview.overall_summary, 200)
        );
    }
    let ok = summary_ok && matches!(scenario.expected.as_str(), "rag");
    return Ok(Row {
        id: scenario.id.clone(),
        category: scenario.category.clone(),
        question: question.to_owned(),
        branch: Branch::Rag,
        branch_label: format!("summary(sections={},fallbacks={})", sections.len(), batch_fallbacks),
        ok,
        verbose: true,
        judgement: if summary_ok {
            format!(
                "整文摘要生成（sections={} fallbacks={}）",
                sections.len(),
                batch_fallbacks
            )
        } else {
            "摘要生成失败".into()
        },
        details: detail,
        answer: if summary_ok {
            overview.overall_summary
        } else {
            String::new()
        },
        sources: vec![file_name.clone()],
    });
}

fn load_scenarios(path: &PathBuf) -> Result<Vec<Scenario>, fanfan_core::AppError> {
    let content = fs::read_to_string(path).map_err(|error| {
        fanfan_core::AppError::new(
            "REAL50_SCENARIOS_READ_FAILED",
            format!("无法读取场景文件 {}: {error}", path.display()),
            true,
        )
    })?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<Scenario>(line).map_err(|error| {
                fanfan_core::AppError::new(
                    "REAL50_SCENARIO_INVALID",
                    format!("场景 JSON 无效: {error}"),
                    false,
                )
            })
        })
        .collect()
}