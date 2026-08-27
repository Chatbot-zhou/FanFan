//! 翻翻：闲聊/寒暄/资料问答链路逐节点检查 harness。
//!
//! 目的：与生产 app_data.rs 的 ask_start 路由逻辑保持一致口径，用真实生成模型
//! 跑一遍「来源路由 → 自由闲聊 / 严格 RAG」的决策，逐节点打印 Input / Output，
//! 以便定位路由或回答节点的问题。本文件只做通用链路运行与展示，不做针对具体
//! 问题/文件/关键词的特判。
//!
//! 复用的生产模块（均在核心库内，不复制实现）：
//! - `LocalGenerationRuntime`：真实生成运行时（本机 Ollama /api/chat）
//! - `source_router_prompt` + `source_routing_schema` + `parse_source_routing`
//! - `personal_reference_hit`
//!
//! 环境变量：
//!   FANFAN_CHAT_MODEL   生成模型 tag（默认 qwen3.5:2b）
//!   CHAT_SCENARIOS      场景 JSONL 路径（默认 .evaluation-tmp/chat_scenarios.jsonl）
//!
//! 场景判读（不低于生产语义）：
//!   expected=chat_free          → 必须命中「general 且无个人资料引用 → 自由闲聊」
//!   expected=rag|rag_or_*|no_evidence_honest → 必须 NOT 自由闲聊（进入严格 RAG 分支）

use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::Instant,
};

use fanfan_core::ask::query_plan::SourceIntent;
use fanfan_core::ask::source_router::{
    parse_source_routing, personal_reference_hit, source_router_prompt, source_routing_schema,
};
use fanfan_core::generation::LocalGenerationRuntime;

const MAX_ROUTING_TOKENS: u32 = 128;
const MAX_CHAT_TOKENS: u32 = 512;
const FALLBACK_MODEL: &str = "qwen3.5:2b";

/// 场景（与 .evaluation-tmp/chat_scenarios.jsonl 的字段一致）。
#[derive(serde::Deserialize)]
struct Scenario {
    id: String,
    category: String,
    question: String,
    expected: String,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("chat_chain_check 未完成: code={} message={}", error.code, error.message);
        std::process::exit(1);
    }
}

/// 主流程：构建运行时 → 逐场景跑路由节点 → 汇总判读。
fn run() -> Result<(), fanfan_core::AppError> {
    let model_tag = env::var("FANFAN_CHAT_MODEL").unwrap_or_else(|_| FALLBACK_MODEL.to_owned());
    let scenarios_file = env::var("CHAT_SCENARIOS").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.evaluation-tmp/chat_scenarios.jsonl")
    });
    let scenarios = load_scenarios(&scenarios_file)?;
    println!(
        "chat_chain_check model={model_tag} scenarios={} src={}",
        scenarios.len(),
        scenarios_file.display()
    );

    // 激活真实生成运行时（启动即冷启动，理应可等待；超时/失败返回具体错误码）。
    let mut runtime = LocalGenerationRuntime::new();
    let threads = (std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4) / 2)
        .clamp(1, 4);
    runtime.activate(model_tag.as_str(), 4096, threads)?;
    let cancelled = AtomicBool::new(false);

    let mut pass = 0_usize;
    let mut total = 0_usize;
    let mut table: Vec<Row> = Vec::new();
    for scenario in &scenarios {
        total += 1;
        let row = run_scenario(&mut runtime, &cancelled, scenario)?;
        if row.ok {
            pass += 1;
        }
        table.push(row);
    }

    println!("\n== 逐节点结果 ==");
    for row in &table {
        println!(
            "{:<4} [{:<12}] ok={} latency_ms={:<6} source={:<10} branch={}",
            row.id,
            row.category,
            row.ok,
            row.latency_ms,
            row.source,
            row.branch
        );
        if let Some(note) = &row.note {
            println!("      note: {note}");
        }
        if let Some(response) = &row.response {
            let preview: String = response.chars().take(140).collect();
            println!("      out: {preview}");
        }
    }
    println!(
        "\nPASS={pass}/{total} 路由不符预期数={}",
        table.iter().filter(|row| !row.ok).count()
    );
    Ok(())
}

struct Row {
    id: String,
    category: String,
    ok: bool,
    latency_ms: u64,
    source: String,
    branch: String,
    response: Option<String>,
    note: Option<String>,
}

/// 逐场景：节点0=来源路由（LLM），节点1=个人引用安全网，节点2=分支分发与回答。
fn run_scenario(
    runtime: &mut LocalGenerationRuntime,
    cancelled: &AtomicBool,
    scenario: &Scenario,
) -> Result<Row, fanfan_core::AppError> {
    let started = Instant::now();
    let question = scenario.question.trim();

    // —— 节点0：Source Router（LLM 语义判定，重试一次）——
    let mut routing_raw = String::new();
    let mut routing = None;
    let mut routing_error: Option<String> = None;
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
                routing_raw = raw.clone();
                routing = parse_source_routing(&raw);
                if routing.is_some() {
                    break;
                }
            }
            Err(error) => routing_error = Some(format!("{error}")),
        }
    }
    let source_label = routing
        .map(|r| r.source.as_str().to_owned())
        .unwrap_or_else(|| "parse_failed".to_owned());
    let Some(routing) = routing else {
        // 与生产一致：无法解析 → 诚实澄清，绝不进自由闲聊。
        let elapsed = started.elapsed().as_millis() as u64;
        let ok = expects_non_chat(&scenario.expected);
        return Ok(Row {
            id: scenario.id.clone(),
            category: scenario.category.clone(),
            ok,
            latency_ms: elapsed,
            source: source_label,
            branch: "routing_failed_clarify".into(),
            response: None,
            note: Some(format!(
                "路由解析失败(raw={routing_raw:?} err={routing_error:?})→诚实澄清，非自由闲聊"
            )),
        });
    };

    // —— 节点1 + 节点2：General 分流（个人引用安全网）——
    let branch: &str;
    let mut response: Option<String> = None;
    let mut note: Option<String> = None;
    let final_source;
    if routing.source == SourceIntent::General && personal_reference_hit(question).is_none() {
        // 自由闲聊分支（生产 run_general_chat_answer 同口径）。
        let system = "你是翻翻，一个运行在用户电脑上的本地资料助手，职责是整理、搜索并基于已授权本地资料回答问题。现在用户是在和你闲聊或寒暄，不需要检索任何资料，请用自然、友好、简洁的中文回应，可以正常聊。注意：只有用户明显在询问本地资料的具体内容时才提示去问资料（对已授权目录发起资料问答）；绝不编造或引用任何本地文件，不要出现页码、引用或“资料显示”之类的措辞。";
        let user = format!("用户说：{}\n\n请自然回应：", question);
        match runtime.complete_cancellable(system, &user, MAX_CHAT_TOKENS, cancelled) {
            Ok(answer) => {
                let answer = answer.trim().to_owned();
                response = Some(answer.clone());
                note = if answer.is_empty() {
                    Some("自由闲聊返回空回答".to_owned())
                } else {
                    None
                };
            }
            Err(error) => {
                note = Some(format!("自由闲聊生成失败: {error}"));
            }
        }
        branch = "free_chat";
        final_source = "general";
    } else {
        // 严格 RAG 分支（含 General 但命中个人引用 → 收敛为 LOCAL）。
        final_source = if routing.source == SourceIntent::General {
            "general→local(personal_ref)"
        } else {
            routing.source.as_str()
        };
        response = Some(format!(
            "(进入严格 RAG 分支，未自由闲聊；本机无真实语料时不在此处合成。source={final_source})"
        ));
        branch = "strict_rag";
    }

    let elapsed = started.elapsed().as_millis() as u64;
    // 判读对齐预期。
    let mut ok = expects_non_chat(&scenario.expected);
    if scenario.expected == "chat_free" {
        ok = branch == "free_chat"
            && response
                .as_deref()
                .is_some_and(|answer| !answer.is_empty());
        if branch != "free_chat" {
            note = Some(format!(
                "预期闲聊却被路由到 {branch}(source={final_source})，raw={routing_raw:?}"
            ));
        }
    } else if branch == "free_chat" {
        ok = false;
        note = Some(format!(
            "预期非闲聊({})却被误路由为自由闲聊，raw={routing_raw:?}",
            scenario.expected
        ));
    }
    Ok(Row {
        id: scenario.id.clone(),
        category: scenario.category.clone(),
        ok,
        latency_ms: elapsed,
        source: final_source.to_owned(),
        branch: branch.to_owned(),
        response,
        note,
    })
}

/// 非闲聊场景的预期：必须进入严格 RAG（不给自由闲聊）。
fn expects_non_chat(expected: &str) -> bool {
    !matches!(expected, "chat_free" | "chat")
        && (expected == "rag"
            || expected == "rag_or_clarify"
            || expected == "no_evidence_honest"
            || expected == "rag_or_summarize")
}

/// 读取场景 JSONL。
fn load_scenarios(path: &PathBuf) -> Result<Vec<Scenario>, fanfan_core::AppError> {
    let content = fs::read_to_string(path).map_err(|error| {
        fanfan_core::AppError::new(
            "CHAT_SCENARIOS_READ_FAILED",
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
                    "CHAT_SCENARIO_INVALID",
                    format!("场景 JSON 无效: {error}"),
                    false,
                )
            })
        })
        .collect()
}