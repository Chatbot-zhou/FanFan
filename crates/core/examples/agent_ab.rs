//! 翻翻：真实语料闭环 A/B harness —— Agent 路径 vs Legacy 路径。
//!
//! 目的：对真实用户口语化评测集逐题跑两条路径并对比，产出第一阶段的
//! 核心指标（围绕「先验证库概览接管不劣化，再按档位扩大工具覆盖」）：
//!
//!   - Tool Selection Accuracy：Agent 决策选中的高层 Tool 是否与预期一致
//!     （当前默认档位仅放行 LibraryOverview，故其它工具必须正确回落 Legacy）；
//!   - 库概览接管：Agent(LibraryOverview) 是否给出确定性正确概览，且明显快于
//!     走 RAG 的 Legacy；
//!   - 默认路径不劣化：非库概览题言 `decision.agent=false`，Legacy 分支不受破坏。
//!
//! 评测集为真实 catalog（`DATA/FanFanData/fanfan.db`)里 35 份真实资料。评测集
//! JSONL 每行 `{id, category, question, expected, expected_tool, answer_hint, note}`，
//! 其中 `expected` 是 Legacy 分支标签，`expected_tool` 是 Agent 应选的高层 Tool。
//!
//! 本文件只做通用链路运行与对比，不对具体问题/文件/关键词做特判或硬编码。
//! 环境变量：
//!   REAL_AB_SCENARIOS  评测集 JSONL（默认 .evaluation-tmp/real_spoken.jsonl）
//!   FANFAN_CHAT_MODEL  生成模型 tag（默认 qwen3.5:2b）
//!   FANFAN_EMBED_MODEL 嵌入模型 tag（默认 qwen3-embedding:0.6b）
//!   FANFAN_AGENT_TIER  Planner 档位（默认最保守 0.8b）
//!   FANFAN_DATA_DIR / FANFAN_MODEL_STORE 同 real50_chain

use std::{
    collections::HashMap,
    env, fs,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::Instant,
};

use fanfan_core::ask::query_parser::{finalize_query_plan, parse_query_plan, query_parser_prompt, query_parser_schema};
use fanfan_core::ask::query_plan::{QueryIntent, QueryPlan};
use fanfan_core::ask::source_router::{
    apply_ambiguous_override, parse_source_routing, personal_reference_hit, source_router_prompt,
    source_routing_schema,
};
use fanfan_core::ask::document_resolver::ResolverInput;
use fanfan_core::ask::document_summary::{
    build_document_sections, merge_tail_sections, SectionChunk, MAX_SECTIONS, MAX_SECTION_CHARS,
};
use fanfan_core::{
    resolve_documents, AskRequest, AskSessionContext, Availability, CatalogStore, ModelManager,
    ModelRole, OllamaClient, ScopeFilter, SemanticQuery,
};
use fanfan_core::generation::LocalGenerationRuntime;
use fanfan_core::{KnowledgeTool, PlannerTier, plan_question};

const MAX_ROUTING_TOKENS: u32 = 128;
const FALLBACK_CHAT_MODEL: &str = "qwen3.5:2b";
const FALLBACK_EMBED_MODEL: &str = "qwen3-embedding:0.6b";

/// 嵌入通道配置（与生产检索同口径）。
struct EmbedConfig {
    artifact_id: String,
    model_id: String,
    query_prefix: String,
}

/// 评测场景。
#[derive(serde::Deserialize)]
struct Scenario {
    id: String,
    category: String,
    question: String,
    expected: String,
    #[serde(default)]
    expected_tool: String,
    #[serde(default)]
    answer_hint: String,
    #[serde(default)]
    note: String,
}

/// Legacy 分支（判定用，覆盖评测集所需的最小分类）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyBranch {
    Chat,
    AmbiguousClarify,
    Find,
    Rag,
    NoEvidence,
    RoutingFailed,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("agent_ab 未完成: code={} message={}", error.code, error.message);
        std::process::exit(1);
    }
}

fn run() -> Result<(), fanfan_core::AppError> {
    let chat_model = env::var("FANFAN_CHAT_MODEL").unwrap_or_else(|_| FALLBACK_CHAT_MODEL.to_owned());
    let embed_model = env::var("FANFAN_EMBED_MODEL").unwrap_or_else(|_| FALLBACK_EMBED_MODEL.to_owned());
    let scenarios_file = env::var("REAL_AB_SCENARIOS").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.evaluation-tmp/real_spoken.jsonl")
    });
    let data_dir = env::var("FANFAN_DATA_DIR")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let model_store = env::var("FANFAN_MODEL_STORE")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanModelStore".to_owned());

    let tier = PlannerTier::from_env();
    let scenarios = load_scenarios(&scenarios_file)?;
    println!(
        "agent_ab tier={} chat={chat_model} embed={embed_model} scenarios={} src={}",
        tier.as_str(),
        scenarios.len(),
        scenarios_file.display()
    );

    let catalog = CatalogStore::open(PathBuf::from(&data_dir).join("fanfan.db"))?;
    let mut file_names = HashMap::new();
    for file in catalog.list_files()? {
        if file.availability == Availability::Present {
            file_names.insert(file.file_id, file.display_name);
        }
    }
    let profiles = catalog.list_document_profiles(None, 10_000)?;
    let expected_total = profiles.len();

    let mut runtime = LocalGenerationRuntime::new();
    let threads = (std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4) / 2)
        .clamp(1, 4);
    runtime.activate(chat_model.as_str(), 4096, threads)?;
    let ollama = OllamaClient::local();
    let cancelled = AtomicBool::new(false);
    let embed = load_embed_config(&model_store)?;

    let mut tool_sel_ok = 0_usize;
    let mut tool_sel_checked = 0_usize;
    let mut legacy_ok = 0_usize;
    let mut legacy_total = 0_usize;
    // Agent 找文件接管质量：expect 为 search_files 的题里，被 Agent 接管后定位结果
    // 是否命中评测集 hint 指向的真实文件（复用与 legacy find 同源的 resolve_documents）。
    let mut find_checked = 0_usize;
    let mut find_hits = 0_usize;
    let mut find_take_over = 0_usize;
    let mut overview_latencies: Vec<u128> = Vec::new();
    let mut legacy_rag_latencies: Vec<u128> = Vec::new();
    // 大纲工具质量探测：expect 为 get_outline 的题，独立评估工具在真实文件上
    // 能否产出有效章节标题（解耦「工具能力」与「planner 决策」，见 run_agent_outline）。
    let mut outline_checked = 0_usize;
    let mut outline_latencies: Vec<u128> = Vec::new();

    // 诊断辅助：设置 FILTER_SCENARIO 只跑指定 id（如 rspk-018），并在每题前打印
    // plan 摘要（intent / content_query），便于单独定位某题的真实链路。仅影响本 harness，
    // 不影响生产逻辑。
    let filter = env::var("FILTER_SCENARIO").ok();

    println!("== 逐题 ==");
    for (index, scenario) in scenarios.iter().enumerate() {
        if let Some(ref want) = filter {
            if &scenario.id != want {
                continue;
            }
        }
        let question = scenario.question.trim();
        let vec = encode(&embed, &ollama, question)?;

        let routing = run_source_router(&mut runtime, &cancelled, question);
        let plan = run_query_parser(&mut runtime, &cancelled, question);
        if filter.is_some() {
            if let Some(p) = plan.as_ref() {
                println!(
                    "[{}] plan intent={:?} op={:?} shape={:?} content_query={:?} target_ref={:?} project={} resolve={}",
                    scenario.id, p.intent,
                    p.operation,
                    p.question_shape,
                    p.content_query,
                    p.target.reference,
                    p.requires_project_context,
                    p.requires_document_resolution
                );
            } else {
                println!("[{}] plan=parse_failed routing={:?}", scenario.id, routing);
            }
        }
        let decision = plan.as_ref().map(|p| plan_question(tier, p));

        let (branch, legacy_label, legacy_detail, total_ms, rag_ms) =
            run_legacy(index, question, &vec, &catalog, &mut runtime, &ollama, &embed, &file_names, routing, plan.as_ref())?;

        // 已经评测放行的确定性单步工具（与 planner.APPROVED 保持一致）。当前
        // 放行集 = 库概览 + 找文件 + 大纲；其中 get_outline 在结构 heading_path
        // 缺失时由正文首行标题兜底提取，已能输出真实章节标题。此处与 planner
        // 许可集同步，让 tool_ok 如实反映 Agent 规划决策，避免误报「未放行」。
        let is_permitted = matches!(
            scenario.expected_tool.as_str(),
            "library_overview" | "search_files" | "get_outline"
        );
        let agent_takes = decision.as_ref().map(|d| d.agent).unwrap_or(false);
        let agent_tool = decision.as_ref().and_then(|d| d.tool).map(|t| t.as_str().to_owned());
        let mapped_tool = plan.as_ref().and_then(fanfan_core::tool_for_plan).map(|t| t.as_str().to_owned());

        let mut overview_report = String::new();
        if agent_takes && agent_tool.as_deref() == Some("library_overview") {
            let started = Instant::now();
            let by_type = count_overview_by_type(&catalog);
            overview_latencies.push(started.elapsed().as_millis());
            overview_report = format!(
                "overview(total={}) by_type=[{}]",
                expected_total,
                by_type.iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",")
            );
        }

        // Agent 找文件接管质量评测：对 expect 为 search_files 的题，若 Agent 本次
        // 恰好选中 search_files，则执行 Agent 定位并核对候选是否命中 hint 文件。
        let mut search_report = String::new();
        if scenario.expected_tool == "search_files" {
            find_checked += 1;
            if agent_takes && agent_tool.as_deref() == Some("search_files") {
                find_take_over += 1;
                if let Some(plan) = plan.as_ref() {
                    match run_agent_search_files(&catalog, plan, &file_names) {
                        Ok(names) => {
                            let hit = hit_hint(&names, &scenario.answer_hint);
                            if hit {
                                find_hits += 1;
                            }
                            search_report = format!(
                                "search_files(found={} hits_hint={hit}) candidates=[{}]",
                                names.len(),
                                names.join("; ")
                            );
                        }
                        Err(error) => search_report = format!("search_files(err) code={}", error.code),
                    }
                }
            } else {
                search_report = "search_files(not_taken→回落)".to_owned();
            }
        }

        // 大纲工具质量探测：对 expect 为 get_outline 的题，无论 Agent 是否接管，都
        // 独立评估其在真实文件上能否产出有效章节标题（解耦工具能力与 planner 决策）。
        let mut outline_report = String::new();
        if scenario.expected_tool == "get_outline" {
            outline_checked += 1;
            let started = Instant::now();
            match plan
                .as_ref()
                .and_then(|p| run_agent_outline(&catalog, p, &file_names).ok())
            {
                Some(titles) if !titles.is_empty() => {
                    outline_latencies.push(started.elapsed().as_millis());
                    let joined = titles.join("; ");
                    let shown: String = joined.chars().take(240).collect();
                    outline_report = format!(
                        "outline(n={}) {}",
                        titles.len(),
                        if joined.chars().count() > 240 { format!("{shown}…") } else { shown }
                    );
                }
                Some(_) => outline_report = "outline(empty)".to_owned(),
                None => outline_report = "outline(run_err)".to_owned(),
            }
        }

        let tool_ok = if is_permitted {
            agent_takes && agent_tool.as_deref() == Some(scenario.expected_tool.as_str())
        } else {
            !agent_takes
        };
        tool_sel_checked += 1;
        if tool_ok {
            tool_sel_ok += 1;
        }
        legacy_total += 1;
        let branch_expected = expected_branch(&scenario.expected);
        let legacy_ok_here = branch == branch_expected;
        if legacy_ok_here {
            legacy_ok += 1;
        }
        if rag_ms.is_some() {
            legacy_rag_latencies.push(rag_ms.unwrap());
        }

        println!(
            "\n[{:>3}] {:<12} {}\n  expect: legacy={:?} tool={}",
            index + 1, scenario.category, question, branch_expected, &scenario.expected_tool
        );
        println!(
            "  agent : tool_ok={} takes={} tool={:?} mapped={:?} tier={}",
            tool_ok, agent_takes, agent_tool, mapped_tool, tier.as_str()
        );
        if !search_report.is_empty() {
            println!("  agent_find: {search_report}");
        }
        println!(
            "  legacy: branch={} ({legacy_label}) ok={} detail={} timeline(total={}ms rag={}ms)",
            branch_label(&branch), legacy_ok_here, legacy_detail, total_ms,
            rag_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".into())
        );
        if !overview_report.is_empty() {
            println!("  agent_overview: {overview_report}");
        }
        if !outline_report.is_empty() {
            println!("  agent_outline: {outline_report}");
        }
    }

    let tool_acc = pct(tool_sel_ok, tool_sel_checked);
    let legacy_acc = pct(legacy_ok, legacy_total);
    println!("\n== 汇总 ==\n\tTool Selection Accuracy = {}/{tool_sel_checked} ({:.1}%)", tool_sel_ok, tool_acc);
    println!("\tLegacy 分支准确率          = {}/{legacy_total} ({:.1}%)", legacy_ok, legacy_acc);
    if find_checked > 0 {
        let taken_hit = pct(find_hits, find_take_over);
        let expect_hit = pct(find_hits, find_checked);
        println!(
            "\tAgent SearchFiles 接管 = {}/{}, 接管后定位命中 = {:.0}%, 期望命中({}/{}) = {:.0}%",
            find_take_over,
            find_checked,
            taken_hit,
            find_hits,
            find_checked,
            expect_hit
        );
    }
    if !overview_latencies.is_empty() {
        let (p50, p95) = percentiles(&overview_latencies);
        println!("\tAgent LibraryOverview 延迟 = n={} p50={}ms p95={}ms", overview_latencies.len(), p50, p95);
    }
    if !legacy_rag_latencies.is_empty() {
        let (p50, p95) = percentiles(&legacy_rag_latencies);
        println!("\tLegacy RAG 延迟           = n={} p50={}ms p95={}ms", legacy_rag_latencies.len(), p50, p95);
    }
    if !outline_latencies.is_empty() {
        let (p50, p95) = percentiles(&outline_latencies);
        println!("\tAgent GetOutline 探测 = {}/{} valid, 延迟 = p50={}ms p95={}ms",
            outline_latencies.len(),
            outline_checked,
            p50,
            p95
        );
    }
    Ok(())
}

fn run_source_router(
    runtime: &mut LocalGenerationRuntime,
    cancelled: &AtomicBool,
    question: &str,
) -> Option<fanfan_core::ask::source_router::SourceRouting> {
    for _ in 0..2 {
        let (system, user) = source_router_prompt(question, &[]);
        let raw = runtime
            .complete_json_cancellable(&system, &user, MAX_ROUTING_TOKENS, &source_routing_schema(), cancelled)
            .ok();
        if let Some(raw) = raw
            && let Some(mut routing) = parse_source_routing(&raw)
        {
            apply_ambiguous_override(question, &mut routing);
            return Some(routing);
        }
    }
    None
}

fn run_query_parser(
    runtime: &mut LocalGenerationRuntime,
    cancelled: &AtomicBool,
    question: &str,
) -> Option<QueryPlan> {
    for _ in 0..2 {
        let (system, user) = query_parser_prompt(question, &[]);
        let raw = runtime
            .complete_json_cancellable(&system, &user, 320, &query_parser_schema(), cancelled)
            .ok();
        if let Some(raw) = raw
            && let Some(plan) = parse_query_plan(&raw).and_then(|p| finalize_query_plan(p, question, &[]))
        {
            return Some(plan);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn run_legacy(
    index: usize,
    question: &str,
    vec: &[f32],
    catalog: &CatalogStore,
    runtime: &mut LocalGenerationRuntime,
    ollama: &OllamaClient,
    embed: &EmbedConfig,
    file_names: &HashMap<uuid::Uuid, String>,
    routing: Option<fanfan_core::ask::source_router::SourceRouting>,
    plan: Option<&QueryPlan>,
) -> Result<(LegacyBranch, String, String, u128, Option<u128>), fanfan_core::AppError> {
    let _ = (index, runtime, vec);
    let started = Instant::now();
    let Some(routing) = routing else {
        return Ok((LegacyBranch::RoutingFailed, "routing_failed".into(), "路由解析失败→诚实澄清".into(), started.elapsed().as_millis(), None));
    };
    if routing.source == fanfan_core::ask::query_plan::SourceIntent::General
        && personal_reference_hit(question).is_none()
    {
        return Ok((LegacyBranch::Chat, "free_chat".into(), "走自由闲聊".into(), started.elapsed().as_millis(), None));
    }
    if routing.source == fanfan_core::ask::query_plan::SourceIntent::Ambiguous {
        return Ok((LegacyBranch::AmbiguousClarify, "ambiguous_clarify".into(), "语义不明→诚实澄清".into(), started.elapsed().as_millis(), None));
    }

    let mut resolved_scope: Vec<uuid::Uuid> = Vec::new();
    if let Some(plan) = plan {
        let profiles = catalog.list_document_profiles(None, 2000)?;
        let session = AskSessionContext::default();
        let input = ResolverInput::new(
            plan,
            &session,
            profiles.iter().map(|(profile, _)| profile.clone()).collect(),
            file_names.clone(),
        );
        let resolution = fanfan_core::resolve_documents(&input);
        resolved_scope = resolution.resolved_file_ids.clone();
        if resolved_scope.is_empty() {
            resolved_scope.extend(resolution.candidates.iter().map(|c| c.file_id));
        }
        resolved_scope.dedup();
    }

    if plan.is_some_and(|p| p.intent == QueryIntent::DocumentFind) {
        let found = !resolved_scope.is_empty();
        let names = resolved_scope
            .iter()
            .filter_map(|id| file_names.get(id))
            .cloned()
            .collect::<Vec<_>>()
            .join("; ");
        let label = if found { format!("find(n={})", resolved_scope.len()) } else { "find(not found)".into() };
        let detail = if found { format!("定位到: {names}") } else { "未定位到目标文件".into() };
        return Ok((LegacyBranch::Find, label, detail, started.elapsed().as_millis(), Some(0)));
    }

    let rag_started = Instant::now();
    let mut ask_request = AskRequest {
        question: question.to_owned(),
        session_id: None,
        scope: ScopeFilter {
            root_ids: Vec::new(),
            collection_ids: Vec::new(),
            file_ids: Vec::new(),
            extensions: Vec::new(),
            modified_from: None,
            modified_to: None,
            availability: Availability::Present,
        },
        answer_style: fanfan_core::AnswerStyle::Detailed,
        retrieval_limit: 10,
        max_source_files: 6,
        strict_evidence: true,
        clarification_selection: None,
        clarification_message_id: None,
        think_mode: false,
    };
    if let Some(plan) = plan {
        if let Some(content_query) = plan.content_query.clone() {
            ask_request.question = content_query;
        }
        if plan.requires_document_resolution && !resolved_scope.is_empty() {
            ask_request.scope.file_ids = resolved_scope.clone();
        }
    }
    let retrieval_vec = encode(embed, ollama, &ask_request.question)?;
    let semantic = Some(SemanticQuery {
        model_artifact_id: &embed.artifact_id,
        vector: &retrieval_vec,
    });
    let answer = if ask_request.scope.file_ids.is_empty() {
        catalog.answer_extractively(&ask_request, semantic)
    } else {
        catalog.answer_extractively_in_authoritative_scope(&ask_request, semantic)
    }?;
    let rag_ms = rag_started.elapsed().as_millis();

    let gate_evidence: Vec<fanfan_core::GateEvidence> = answer
        .claims
        .iter()
        .map(|claim| fanfan_core::GateEvidence {
            text: claim.text.clone(),
            heading: claim
                .citations
                .first()
                .and_then(|c| c.locator.heading_path.last().cloned()),
        })
        .collect();
    let gate_plan = plan.cloned().unwrap_or_else(|| QueryPlan {
        operation: fanfan_core::ask::query_plan::QueryOperation::Qa,
        ..QueryPlan::default()
    });
    let gate_input = fanfan_core::AnswerabilityInput {
        question,
        content_query: Some(ask_request.question.trim()),
        plan: &gate_plan,
        evidence: &gate_evidence,
    };
    let verdict = fanfan_core::evaluate_answerability(&gate_input);
    // 诊断辅助：仅当 FILTER_NOEV 命中当前场景 id 时打印 gate 详情与前 3 条证据，
    // 用于取证「存在性命中错误肯定/正确肯定」的实际命中词。不影响生产逻辑。
    if let Ok(want) = std::env::var("FILTER_NOEV") {
        if want == question.trim() {
            println!(
                "  GATE verdict={:?} shape={:?} content_query={:?} entities={:?} missing={:?}",
                verdict.status,
                verdict.answer_shape,
                ask_request.question.trim(),
                verdict.query_entities,
                verdict.missing_entities,
            );
            for (i, gate) in gate_evidence.iter().take(3).enumerate() {
                let shown: String = gate.text.chars().take(90).collect();
                println!("  GATE ev[{i}] {:?}", shown);
            }
        }
    }
    if verdict.status == fanfan_core::AnswerabilityStatus::NotAnswerable || answer.insufficient_evidence {
        let source_names = answer
            .used_file_ids
            .iter()
            .filter_map(|id| file_names.get(id))
            .cloned()
            .collect::<Vec<_>>();
        let detail = format!(
            "no_evidence reason={:?} coverage={:.2} files={} verdict={}",
            answer.no_evidence_reason, answer.index_coverage, source_names.len(), verdict.reason
        );
        return Ok((
            LegacyBranch::NoEvidence,
            "rag_no_evidence".into(),
            detail,
            started.elapsed().as_millis(),
            Some(rag_ms),
        ));
    }
    let cite_n = answer.claims.iter().flat_map(|c| &c.citations).count();
    let detail = format!("rag(cites={cite_n}) claims={} grounded={:?}", answer.claims.len(), answer.grounding_status);
    Ok((
        LegacyBranch::Rag,
        format!("rag(cites={cite_n})"),
        detail,
        started.elapsed().as_millis(),
        Some(rag_ms),
    ))
}

fn count_overview_by_type(catalog: &CatalogStore) -> Vec<(String, u64)> {
    let mut counts: HashMap<Option<fanfan_core::DocumentType>, u64> = HashMap::new();
    match catalog.list_document_profiles(None, 10_000) {
        Ok(profiles) => {
            for (profile, _) in profiles {
                *counts.entry(profile.document_type).or_insert(0) += 1;
            }
        }
        Err(_) => return Vec::new(),
    }
    let mut by_type: Vec<(String, u64)> = counts
        .into_iter()
        .map(|(ty, count)| (ty.map(|t| t.display_name().to_owned()).unwrap_or_else(|| "unknown".into()), count))
        .collect();
    by_type.sort_by(|a, b| b.1.cmp(&a.1));
    by_type
}

/// Agent 找文件定位执行：复用与 legacy find 同源的 `resolve_documents`，返回命中的
/// 文件显示名清单（不触底检索、不返回正文，仅定位元数据）。
fn run_agent_search_files(
    catalog: &CatalogStore,
    plan: &fanfan_core::ask::query_plan::QueryPlan,
    file_names: &HashMap<uuid::Uuid, String>,
) -> Result<Vec<String>, fanfan_core::AppError> {
    let profiles = catalog.list_document_profiles(None, 10_000)?;
    let profile_vec: Vec<_> = profiles.iter().map(|(profile, _)| profile.clone()).collect();
    let session = AskSessionContext::default();
    let input = ResolverInput::new(plan, &session, profile_vec, file_names.clone());
    let resolution = resolve_documents(&input);
    let mut ids: Vec<uuid::Uuid> = resolution.resolved_file_ids;
    if ids.is_empty() {
        ids = resolution.candidates.iter().map(|c| c.file_id).collect();
    }
    let mut names: Vec<String> = Vec::with_capacity(ids.len());
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if seen.insert(id) {
            if let Some(name) = file_names.get(&id) {
                names.push(name.clone());
            }
        }
    }
    Ok(names)
}

/// 大纲工具质量探测（与 planner 决策解耦）：对大纲类题，独立解析目标文档并
/// 用与生产 `get_outline` 同源的分节逻辑（复用 legacy 同源的 resolve_documents
/// 定位 + 分页读 heading_path + `build_document_sections`）产出章节标题。
///
/// 只用于评估「大纲工具本身是否可用/准确」，不进入生产决策路径；据此决定
/// 是否值得把 `GetOutline` 放入档位许可集（只有评测证明可用才放行）。
fn run_agent_outline(
    catalog: &CatalogStore,
    plan: &QueryPlan,
    file_names: &HashMap<uuid::Uuid, String>,
) -> Result<Vec<String>, fanfan_core::AppError> {
    let profiles = catalog.list_document_profiles(None, 10_000)?;
    let profile_vec: Vec<_> = profiles.iter().map(|(profile, _)| profile.clone()).collect();
    let session = AskSessionContext::default();
    let input = ResolverInput::new(plan, &session, profile_vec, file_names.clone());
    let resolution = resolve_documents(&input);
    let mut ids: Vec<uuid::Uuid> = resolution.resolved_file_ids;
    if ids.is_empty() {
        ids = resolution.candidates.iter().map(|c| c.file_id).collect();
    }
    let Some(&file_id) = ids.first() else {
        return Ok(Vec::new());
    };
    // 分页读标题路径（上限 4000 防失控，与摘要管线同源）。
    let mut headings: HashMap<uuid::Uuid, Vec<String>> = HashMap::new();
    let mut offset = 0usize;
    loop {
        let preview = catalog.file_preview_page(&file_id, offset, 200, None)?;
        if preview.revision_id.is_none() {
            break;
        }
        let batch_len = preview.nodes.len();
        for node in preview.nodes {
            headings.insert(node.node_id, node.heading_path);
        }
        offset = match preview.next_offset {
            Some(next) => next as usize,
            None => break,
        };
        if batch_len == 0 || headings.len() > 4_000 {
            break;
        }
    }
    let section_chunks: Vec<SectionChunk> = catalog
        .file_chunks(&file_id)?
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
    let mut sections = build_document_sections(&section_chunks, &headings, MAX_SECTION_CHARS);
    merge_tail_sections(&mut sections, MAX_SECTIONS);
    Ok(sections
        .into_iter()
        .map(|section| section.title)
        .collect())
}

/// 判断 Agent 定位出的候选文件中，是否有与评测集 hint 指向文件的同一内容。
/// hint 是人工标注的期望命中文件/片段（如「周晨博-大模型开发.pdf」）。这里做
/// 去除空白后的整串子串互含判定，宽松但只用于评测归因，不进入生产决策路径。
fn hit_hint(candidates: &[String], hint: &str) -> bool {
    let clean: String = hint
        .chars()
        .filter(|c| !matches!(c, '\u{3000}' | ' ' | '\t' | '；' | '，' | ','))
        .collect::<String>()
        .trim()
        .to_owned();
    if clean.is_empty() || candidates.is_empty() {
        return false;
    }
    candidates
        .iter()
        .any(|name| name.contains(&clean) || clean.contains(name.as_str()))
}

fn expected_branch(expected: &str) -> LegacyBranch {
    match expected {
        "rag_find" => LegacyBranch::Find,
        "rag_no_evidence" => LegacyBranch::NoEvidence,
        "clarify" => LegacyBranch::AmbiguousClarify,
        "chat" => LegacyBranch::Chat,
        _ => LegacyBranch::Rag,
    }
}

fn branch_label(branch: &LegacyBranch) -> &'static str {
    match branch {
        LegacyBranch::Chat => "chat",
        LegacyBranch::AmbiguousClarify => "clarify",
        LegacyBranch::Find => "find",
        LegacyBranch::Rag => "rag",
        LegacyBranch::NoEvidence => "no_evidence",
        LegacyBranch::RoutingFailed => "routing_failed",
    }
}

fn pct(ok: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        (ok as f64) * 100.0 / (total as f64)
    }
}

fn percentiles(values: &[u128]) -> (u128, u128) {
    if values.is_empty() {
        return (0, 0);
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = |q: f64| -> usize { ((sorted.len() as f64 - 1.0) * q).round() as usize };
    (sorted[idx(0.5)], sorted[idx(0.95)])
}

fn encode(embed: &EmbedConfig, ollama: &OllamaClient, text: &str) -> Result<Vec<f32>, fanfan_core::AppError> {
    let prefixed = format!("{}{}", embed.query_prefix, text);
    let (vectors, _dim) = ollama.embed(&embed.model_id, &[prefixed])?;
    vectors.into_iter().next().ok_or_else(|| {
        fanfan_core::AppError::new("AGENT_AB_EMBEDDING_EMPTY", "Embedding 返回为空", true)
    })
}

fn load_embed_config(model_store: &str) -> Result<EmbedConfig, fanfan_core::AppError> {
    let manager = ModelManager::open_store(PathBuf::from(model_store))?;
    let artifact = manager
        .list_artifacts()?
        .into_iter()
        .find(|artifact| artifact.role == ModelRole::Embedding)
        .ok_or_else(|| {
            fanfan_core::AppError::new("AGENT_AB_NO_EMBED_ARTIFACT", "注册表没有 Embedding 模型", true)
        })?;
    if artifact.format == fanfan_core::ModelFormat::Ollama {
        manager.ollama_embedding_ready(&artifact.model_id)?;
    }
    let active = manager.active_artifact(ModelRole::Embedding)?.ok_or_else(|| {
        fanfan_core::AppError::new("AGENT_AB_NO_ACTIVE_EMBED", "没有激活的 Embedding 模型", true)
    })?;
    Ok(EmbedConfig {
        artifact_id: active.artifact_id.to_string(),
        model_id: active.model_id,
        query_prefix: active.query_prefix.unwrap_or_default(),
    })
}

fn load_scenarios(path: &PathBuf) -> Result<Vec<Scenario>, fanfan_core::AppError> {
    let content = fs::read_to_string(path).map_err(|error| {
        fanfan_core::AppError::new(
            "AGENT_AB_READ_FAILED",
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
                    "AGENT_AB_SCENARIO_INVALID",
                    format!("场景 JSON 无效: {error}"),
                    false,
                )
            })
        })
        .collect()
}