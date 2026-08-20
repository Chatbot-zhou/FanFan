//! Phase 4.2 真实问题回归集（spec 十七）：把本轮真实测试暴露的 7 个
//! case 固化为不变量，防止后续改动回退。
//!
//! 每个测试对应一次真实用户测试（RAG 串答 / Agent 项目存在性 /
//! Transformer 外部知识泄漏 / 简历模板幻觉 / 毕业材料澄清 / 项目抽取
//! 描述句 / LangGraph 模型能力）。全部用确定性 fixture 驱动纯函数，
//! 不依赖模型——模型相关的部分断言「门控与约束的确定性一侧」。
//!
//! Case 7（LangGraph）说明：GENERAL 技术知识质量受当前 0.6B 模型上限
//! 限制（spec 十三），回归集只固定「路由到 GENERAL」这一确定性事实，
//! 不硬编码 LangGraph 答案。

use std::collections::HashMap;

use uuid::Uuid;

use crate::ask::answer_gate::{
    AnswerShape, AnswerabilityInput, AnswerabilityStatus, GateEvidence, answer_shape_directive,
    claim_subject_mismatch, classify_answer_shape, evaluate_answerability,
    existence_requires_project_context, find_external_knowledge_marker, local_no_evidence_answer,
};
use crate::ask::document_resolver::{ResolverInput, resolve_documents};
use crate::ask::document_summary::{SectionChunk, build_document_sections};
use crate::ask::extract::{extract_item_is_entity_like, extract_prompt};
use crate::ask::query_parser::parse_query_plan;
use crate::ask::query_plan::{QueryIntent, QueryOperation, QuestionShape, ResolutionStatus};
use crate::ask::source_router::parse_source_routing;
use crate::contracts::{DocumentType, SourceLocator};
use crate::knowledge::{AskSessionContext, DocumentProfile};

// ---------------------------------------------------------------- fixture 工具

/// 构造 QA（operation=answer）计划：与真实 Query Parser 输出同构，
/// Answerability Gate / answer_shape 走「普通回答」语义。
fn qa_answer_plan(
    reference: &str,
    content_query: Option<&str>,
) -> crate::ask::query_plan::QueryPlan {
    let content_query_json = content_query
        .map(|query| format!("\"{query}\""))
        .unwrap_or_else(|| "null".to_owned());
    parse_query_plan(&format!(
        r#"{{"source":"local","intent":"document_qa","operation":"qa",
            "target":{{"reference":"{reference}","document_type":null,"document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null}},
            "content_query":{content_query_json},"filters":{{"time":null,"file_type":null,"path":null}},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.93}}"#
    ))
    .expect("qa plan fixture parses")
}

/// 证据 fixture。
fn evidence(text: &str, heading: Option<&str>) -> GateEvidence {
    GateEvidence {
        text: text.to_owned(),
        heading: heading.map(str::to_owned),
    }
}

/// 文档画像 fixture。
fn profile(file_id: Uuid, document_type: Option<DocumentType>, title: &str) -> DocumentProfile {
    DocumentProfile {
        file_id,
        revision_id: Uuid::now_v7(),
        title: title.to_owned(),
        summary: String::new(),
        keywords: Vec::new(),
        entities: Vec::new(),
        document_type,
        type_confidence: None,
        section_titles: Vec::new(),
        representative_text_hash: None,
        updated_at: chrono::Utc::now(),
    }
}

// ---------------------------------------------------------------- CASE 1：RAG

/// CASE 1：「我的资料里是怎么介绍 RAG 的？」——检索到的是 Agent 概念
/// 证据时，即使 Embedding 给了分数也必须 NOT_ANSWERABLE（不得把 Agent
/// 证据包装成 RAG 回答，也不得展开成 Reactive Aggregation 之类的
/// 错误全称）。只有真实包含 RAG/检索增强语义的证据才 ANSWERABLE。
#[test]
fn case_1_rag_question_rejects_agent_evidence() {
    let question = "我的资料里是怎么介绍 RAG 的？";
    let plan = qa_answer_plan("我的资料", Some("RAG"));
    let input = AnswerabilityInput {
        question,
        content_query: Some("RAG"),
        plan: &plan,
        evidence: &[
            evidence(
                "Agent 会根据目标判断下一步、选择工具并读取工具结果，动态调整执行路径。",
                Some("Agent 概述"),
            ),
            evidence("大模型不仅负责生成文本，还会根据目标决定行为。", None),
        ],
    };
    let verdict = evaluate_answerability(&input);
    assert_eq!(
        verdict.status,
        AnswerabilityStatus::NotAnswerable,
        "Agent 证据不足以回答 RAG 问题：{}",
        verdict.reason
    );
    assert!(
        verdict
            .missing_entities
            .iter()
            .any(|entity| entity.eq_ignore_ascii_case("rag"))
    );

    // 反向：真实 RAG 证据 → ANSWERABLE
    let answerable = AnswerabilityInput {
        question,
        content_query: Some("RAG"),
        plan: &plan,
        evidence: &[
            evidence(
                "RAG（检索增强生成）先检索知识库再生成回答，缓解幻觉。",
                None,
            ),
            evidence(
                "retrieval-augmented generation 通过检索外部知识增强上下文。",
                None,
            ),
        ],
    };
    let verdict = evaluate_answerability(&answerable);
    assert_eq!(verdict.status, AnswerabilityStatus::Answerable);

    // Unsupported Claim Gate：Evidence 讲 Agent、Claim 说 RAG → 拦截
    let mismatch = claim_subject_mismatch(
        "RAG（Reactive Aggregation）是大模型在处理复杂任务时的一种能力。",
        &["Agent 根据目标选择工具并读取工具结果，判断下一步行动。"],
    );
    assert!(mismatch.is_some(), "主体不一致的 claim 必须被拦截");
}

// ---------------------------------------------------------- CASE 2：Agent 项目

/// CASE 2：「我以前有没有做过 Agent 项目？」——answer_shape 必须是
/// BOOLEAN_EXISTENCE（第一句明确「有 / 没有找到证据 / 资料不足」）；
/// Agent 概念解释证据不能证明「做过项目」，项目语境证据才可以。
#[test]
fn case_2_agent_project_existence_needs_project_context() {
    let question = "我以前有没有做过 Agent 项目？";
    // LLM Parser 输出：存在性断言 + 需要项目语境证据
    let mut plan = qa_answer_plan("我的经历", Some("Agent 项目"));
    plan.question_shape = QuestionShape::BooleanExistence;
    plan.requires_project_context = true;
    let shape = classify_answer_shape(question, &plan);
    assert_eq!(shape, AnswerShape::BooleanExistence);
    // 生成指令必须约束第一条 claim 给出存在性结论
    let directive = answer_shape_directive(shape);
    assert!(directive.contains("第一条"));

    // 「有没有做过项目」需要 PROJECT_CONTEXT 证据（依据 LLM 解析字段）
    assert!(existence_requires_project_context(question, &plan));

    // 纯概念证据 → 不可回答为「有」
    let concept_only = AnswerabilityInput {
        question,
        content_query: Some("Agent 项目"),
        plan: &plan,
        evidence: &[
            evidence(
                "Agent 是一种能够感知环境、自主决策的智能体。",
                Some("Agent 概念"),
            ),
            evidence("Agent 会选择工具并读取工具结果。", None),
        ],
    };
    let verdict = evaluate_answerability(&concept_only);
    assert_ne!(
        verdict.status,
        AnswerabilityStatus::Answerable,
        "概念解释不能证明用户做过 Agent 项目"
    );

    // 项目语境证据（项目经历/负责/成果）→ 可回答
    let with_project = AnswerabilityInput {
        question,
        content_query: Some("Agent 项目"),
        plan: &plan,
        evidence: &[
            evidence(
                "项目经历：智能客服 Agent 系统，负责多轮对话编排与工具调用链路。",
                Some("项目经历"),
            ),
            evidence("成果：客服 Agent 项目上线后问题解决率提升。", None),
        ],
    };
    let verdict = evaluate_answerability(&with_project);
    assert_eq!(verdict.status, AnswerabilityStatus::Answerable);

    // 无证据文案：项目存在性断言必须是「没有项目记录，无法确认」式
    let answer = local_no_evidence_answer(question, &[], true);
    assert!(answer.contains("项目记录"));
    assert!(find_external_knowledge_marker(&answer).is_none());
}

// -------------------------------------------------------- CASE 3：Transformer

/// CASE 3：「我的文件里有没有提到 Transformer？」——LOCAL 无证据回答
/// 只能说「当前资料中没有找到明确提到 Transformer 的内容」，禁止追加
/// BERT/GPT/「通常用于」等通用知识。
#[test]
fn case_3_transformer_local_refusal_stays_local() {
    let question = "我的文件里有没有提到 Transformer？";
    let answer = local_no_evidence_answer(question, &["Transformer".to_owned()], false);
    assert_eq!(answer, "当前资料中没有找到明确提到 Transformer 的内容。");
    assert!(find_external_knowledge_marker(&answer).is_none());

    // 真实事故文案必须被外部知识标记检出（回归：后半句不允许）
    let leaky = "资料库中没有找到。Transformer通常用于自然语言处理领域，如BERT、GPT。";
    assert!(
        find_external_knowledge_marker(leaky).is_some(),
        "「通常用于」等通用知识补充必须被检出"
    );
}

// ------------------------------------------------------------- CASE 4：简历

/// CASE 4：「我的简历主要写了什么？」——DOCUMENT_SUMMARY 只能围绕真实
/// 存在的章节总结；文件里没有「工作经历」就绝不因「简历通常有」而出现。
#[test]
fn case_4_resume_summary_only_real_sections() {
    let plan = parse_query_plan(
        r#"{"source":"local","intent":"document_summary","operation":"summary",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":null,"filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":true,"confidence":0.95}"#,
    )
    .expect("summary plan parses");
    assert_eq!(plan.intent, QueryIntent::DocumentSummary);
    assert!(plan.requires_full_document);

    // 简历真实章节只有：教育经历 / 专业技能 / 项目经历（没有「工作经历」）
    let chunk = |node_id: Uuid, ordinal: u64, text: &str| SectionChunk {
        chunk_id: Uuid::now_v7(),
        node_id,
        revision_id: Uuid::now_v7(),
        ordinal,
        text: text.to_owned(),
        locator: SourceLocator::default(),
    };
    let edu = Uuid::now_v7();
    let skills = Uuid::now_v7();
    let projects = Uuid::now_v7();
    let chunks = vec![
        chunk(edu, 0, "2019-2023 就读于 X 大学计算机学院"),
        chunk(skills, 1, "熟悉 LangChain / RAG / Agent 开发"),
        chunk(projects, 2, "法律 RAG 项目：负责检索链路与引用验证"),
        chunk(projects, 3, "简历项目经历第二条：大模型评测平台"),
    ];
    let headings = HashMap::from([
        (edu, vec!["教育经历".to_owned()]),
        (skills, vec!["专业技能".to_owned()]),
        (projects, vec!["项目经历".to_owned()]),
    ]);
    let sections = build_document_sections(&chunks, &headings, 10_000);
    let titles: Vec<&str> = sections.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, vec!["教育经历", "专业技能", "项目经历"]);
    // 模板章节绝不出现（summary 的每一项输入都来自真实 section）
    assert!(!titles.contains(&"工作经历"));
    assert!(!titles.contains(&"个人成就或兴趣"));
}

// --------------------------------------------------------- CASE 5：毕业材料

/// CASE 5：「我毕业时候那个材料在哪」——DOCUMENT_FIND 命中多个候选时
/// 必须 CLARIFICATION（返回候选文件），不允许模型自由生成追问文本。
#[test]
fn case_5_graduation_material_multiple_candidates_require_clarification() {
    let plan = qa_answer_plan("毕业时候那个材料", Some("位置"));
    let thesis = profile(Uuid::now_v7(), None, "毕业论文");
    let design = profile(Uuid::now_v7(), None, "毕业设计");
    let defense = profile(Uuid::now_v7(), None, "毕业答辩材料");
    let mut file_names = HashMap::new();
    file_names.insert(thesis.file_id, "毕业论文.pdf".to_owned());
    file_names.insert(design.file_id, "毕业设计.docx".to_owned());
    file_names.insert(defense.file_id, "毕业答辩材料.pptx".to_owned());
    let session = AskSessionContext::default();
    let input = ResolverInput::new(&plan, &session, vec![thesis, design, defense], file_names);
    let resolution = resolve_documents(&input);
    assert_eq!(
        resolution.status,
        ResolutionStatus::MultipleCandidates,
        "多份毕业材料 → 必须澄清，不允许生成式追问"
    );
    assert!(resolution.candidates.len() >= 2);
}

// ------------------------------------------------------ CASE 6：项目抽取验证

/// CASE 6：「我那个大模型的材料里面有啥项目」——LLM Query Parser 语义判定
/// 为实体清单（requires_entity_items=true），prompt 追加实体规范；模型输出
/// 的完整描述句必须被类型验证拒绝（不是项目名称实体）。
#[test]
fn case_6_project_extract_rejects_narrative_sentences() {
    let question = "我那个大模型的材料里面有啥项目";
    // LLM Query Parser 语义判断：项目清单 → operation=extract + requires_entity_items=true
    let plan = crate::ask::query_plan::QueryPlan {
        operation: QueryOperation::Extract,
        question_shape: QuestionShape::List,
        requires_entity_items: true,
        ..crate::ask::query_plan::QueryPlan::default()
    };
    assert!(plan.requires_entity_items);
    // 实体规范写入抽取 prompt（不再用关键词表猜「项目清单」）
    let (_, user) = extract_prompt(
        question,
        plan.requires_entity_items,
        &["大模型应用开发".to_owned()],
    );
    assert!(user.contains("实体/名称"));
    assert!(user.contains("严禁输出整段技术点描述或叙事长句"));

    // 真实事故输出：整段描述句 → 拒绝
    let narrative = "大模型不仅负责生成文本，还会根据目标判断下一步、选择工具并读取工具结果";
    assert!(!extract_item_is_entity_like(narrative));

    // 概念定义句 → 拒绝（不是项目名）
    assert!(!extract_item_is_entity_like(
        "Agent 是一种能够自主决策的智能体"
    ));

    // 真实项目名（短实体/标题式）→ 接受
    assert!(extract_item_is_entity_like("法律RAG项目"));
    assert!(extract_item_is_entity_like("大模型评测平台"));
    assert!(extract_item_is_entity_like("基于 LangGraph 的客服工作流"));
}

// ------------------------------------------------------------ CASE 7：LangGraph

/// CASE 7：「帮我解释 LangGraph」——路由必须是 GENERAL（模型知识域）。
/// 0.6B 模型会给出事实错误回答（"表示和管理语言的图结构"），这是模型
/// 能力上限问题：Prompt 约束无法解决，记录为 model capability
/// evaluation，回归集不硬编码 LangGraph 答案。
#[test]
fn case_7_langgraph_routes_to_general_model_capability_noted() {
    let routing = parse_source_routing(
        r#"{"source":"general","confidence":0.9,"reason":"通用技术概念解释"}"#,
    )
    .expect("general routing fixture parses");
    assert_eq!(
        routing.source,
        crate::ask::query_plan::SourceIntent::General
    );
    // 能力限制记录：LOCAL 严格证据约束不适用于 GENERAL，但本地资料
    // 回答永远不串用 GENERAL 模型知识（两模式 prompt 完全分离）。
    assert!(crate::ask::answer_gate::LOCAL_STRICT_SYSTEM_PROMPT.contains("LOCAL STRICT MODE"));
}
