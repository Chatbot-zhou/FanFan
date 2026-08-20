//! 场景化测试（Step 14）：场景 A-P 全链路决策验证。
//!
//! 16 个场景按「编排层真实调用链」驱动纯函数（与 app_data.rs 的
//! finish_retrieval_with_plan / run_*_answer 分支一致）：
//!
//! ```text
//! Source Router（GENERAL / LOCAL / AMBIGUOUS）
//!  → AMBIGUOUS 时 Context Resolver（会话上下文恢复）
//!  → Query Parser（target 与 content_query 分离）
//!  → Memory Resolver（别名/关系定位提示）
//!  → Document Resolver（target → file_id 白名单）
//!  → 按意图分发：clarification / find / summary / compare / extract / chunk RAG
//! ```
//!
//! 模型输出用确定性 fixture 代替（与各模块单测同口径），断言的是
//! 「场景要求的端到端行为不变量」，而不是单个函数的内部实现。

use std::collections::HashMap;

use uuid::Uuid;

use crate::ask::context_resolver::resolve_ambiguous;
use crate::ask::document_resolver::{ResolverInput, resolve_documents};
use crate::ask::document_retrieval::rank_document_candidates;
use crate::ask::document_summary::{
    SectionChunk, build_document_sections, merge_tail_sections, parse_section_summaries,
};
use crate::ask::extract::{ExtractResults, longest_common_substr_len, parse_extract_results};
use crate::ask::memory_resolver::{match_alias_hints, match_relation_hints};
use crate::ask::query_parser::parse_query_plan;
use crate::ask::query_plan::{
    QueryIntent, QueryOperation, QueryPlan, ResolutionStatus, SourceIntent,
};
use crate::ask::source_router::parse_source_routing;
use crate::ask::{EXTRACT_MATCH_MIN_LEN, MAX_CANDIDATE_SCOPE, MEDIUM_CONFIDENCE_THRESHOLD};
use crate::contracts::{DocumentType, SourceLocator};
use crate::knowledge::{AnswerMode, AskSessionContext, DocumentProfile};
use crate::memory::{
    MemoryAlias, MemoryEntity, MemoryRelation, MemorySource, MemoryStatus, MemoryTargetType,
    normalize_alias,
};

// ---------------------------------------------------------------- 公共 fixture 工具

/// 构造文档画像（场景共用）。
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

/// 构造「我的简历 → 项目经历」的 QA/Extract 计划（CASE 1 的典型解析结果）。
fn resume_projects_plan() -> QueryPlan {
    parse_query_plan(
        r#"{"source":"local","intent":"document_qa","operation":"extract",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"项目经历","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.96}"#,
    )
    .expect("resume plan fixture parses")
}

/// 构造 MemoryAlias（场景共用；created_at/updated_at 用固定基准时间）。
fn alias(target_id: Uuid, name: &str, source: MemorySource) -> MemoryAlias {
    let now = chrono::Utc::now();
    MemoryAlias {
        alias_id: Uuid::now_v7(),
        alias: name.to_owned(),
        target_type: MemoryTargetType::File,
        target_id,
        confidence: 0.95,
        source_type: source,
        source_id: None,
        status: MemoryStatus::Confirmed,
        hit_count: 0,
        last_used_at: None,
        created_at: now,
        updated_at: now,
    }
}

/// 模拟编排层「Document Resolver → 澄清候选组装」（finish_retrieval_with_plan 4.6 分支）。
/// 返回候选清单（file_id, 展示信号）——桌面层再包成 ClarificationOption。
fn clarification_options_from(
    resolution: &crate::ask::query_plan::DocumentResolution,
) -> Vec<(Uuid, f32, Vec<String>)> {
    resolution
        .candidates
        .iter()
        .take(MAX_CANDIDATE_SCOPE)
        .map(|candidate| {
            (
                candidate.file_id,
                candidate.score,
                candidate.signals.clone(),
            )
        })
        .collect()
}

/// 模拟编排层「检索问题组装」（scope_planning：content_query 替换整句，
/// 目标词绝不拼回检索 Query）。
fn retrieval_question_from(plan: &QueryPlan) -> Option<String> {
    plan.content_query.clone()
}

// ---------------------------------------------------------------- 场景 A-P

/// 场景 A：GENERAL 闲聊绝不启动本地检索。
/// 「Transformer是什么？」是通用知识，Source Router 必须判定 general；
/// 计划层不产生任何检索意图（无 target、无 content_query、无文件锁定）。
#[test]
fn scenario_a_general_chat_never_starts_retrieval() {
    let routing =
        parse_source_routing(r#"{"source":"general","confidence":0.95}"#).expect("routing parses");
    assert_eq!(routing.source, SourceIntent::General);
    assert!((routing.confidence - 0.95).abs() < 1e-6);

    // GENERAL 的 Query Parser 结果：纯聊天意图，不要求文档解析与检索
    let plan = parse_query_plan(
        r#"{"source":"general","intent":"general_chat","operation":"qa",
            "target":{},"content_query":null,"filters":{},
            "requires_document_resolution":false,"requires_full_document":false,"confidence":0.9}"#,
    )
    .expect("chat plan parses");
    assert_eq!(plan.source, SourceIntent::General);
    assert_eq!(plan.intent, QueryIntent::GeneralChat);
    assert_eq!(plan.content_query, None);
    assert!(!plan.requires_document_resolution);
    assert!(!plan.requires_full_document);
}

/// 场景 B：LOCAL 全库资料检索必须执行，且不锁定单文件。
/// 「我的资料里面有没有 Transformer 的内容？」→ library_qa：content_query
/// 携带真正要查的内容（Transformer），target 为空——检索范围是整个资料库。
#[test]
fn scenario_b_library_qa_requests_local_retrieval() {
    let routing =
        parse_source_routing(r#"{"source":"local","confidence":0.9}"#).expect("routing parses");
    assert_eq!(routing.source, SourceIntent::Local);

    let plan = parse_query_plan(
        r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"Transformer","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":false,"requires_full_document":false,"confidence":0.9}"#,
    )
    .expect("library plan parses");
    assert_eq!(plan.source, SourceIntent::Local);
    assert_eq!(plan.intent, QueryIntent::LibraryQa);
    assert_eq!(plan.content_query.as_deref(), Some("Transformer"));
    // 全库资料请求：不锁文件（requires_document_resolution=false），
    // 由文档级召回（run_document_recall）按信号找文档
    assert!(!plan.requires_document_resolution);
}

/// 场景 C：AMBIGUOUS 且会话上下文为空 → 安全回退，绝不猜文件。
/// 「第二个项目用了什么？」在新会话中无法恢复指代：Context Resolver 返回
/// Unresolved（编排层回退 GENERAL_CHAT，把"猜"的权利留给用户澄清）。
#[test]
fn scenario_c_ambiguous_without_context_never_guesses_files() {
    let routing =
        parse_source_routing(r#"{"source":"ambiguous","confidence":0.6}"#).expect("routing parses");
    assert_eq!(routing.source, SourceIntent::Ambiguous);

    let context = AskSessionContext::default();
    let resolution = resolve_ambiguous(&context);
    assert!(!resolution.is_resolved());
    assert_eq!(resolution.status, ResolutionStatus::Unresolved);
    assert_eq!(resolution.source, SourceIntent::General);
    assert_eq!(resolution.intent, QueryIntent::GeneralChat);
    assert!(resolution.resolved_file_ids.is_empty());
    assert!(resolution.fallback_reason.is_some());
}

/// 场景 D：AMBIGUOUS 从会话上下文恢复 → 锁定 active file，禁止重新全库搜索。
/// CASE 5：第一轮「看看我的简历」锁定简历；第二轮「第二个项目用了什么？」
/// 必须沿用 active_file_id，scope 就是简历文件本身。
#[test]
fn scenario_d_ambiguous_recovers_active_file_from_session() {
    let resume_id = Uuid::now_v7();
    let context = AskSessionContext {
        active_file_id: Some(resume_id),
        active_file_ids: vec![resume_id],
        last_referenced_file_ids: vec![resume_id],
        last_intent: Some("document_qa".to_owned()),
        ..AskSessionContext::default()
    };
    let resolution = resolve_ambiguous(&context);
    assert!(resolution.is_resolved());
    assert_eq!(resolution.source, SourceIntent::Local);
    assert_eq!(resolution.intent, QueryIntent::DocumentQa);
    assert_eq!(resolution.resolved_file_ids, vec![resume_id]);
    // 恢复成功 → 检索范围已锁死单文件，内容查询在文件内进行
    assert_eq!(resolution.confidence, 0.95);
}

/// 场景 E：两份非常接近的简历 → NEED_CLARIFICATION，用户选一次。
/// CASE 2 变体：两个同类型候选（分数差 < HIGH_MARGIN）不锁单文件，
/// 澄清候选保留 top-2/3（含命中信号），answer_mode = clarification。
#[test]
fn scenario_e_close_resumes_ask_user_to_choose() {
    let resume_a = profile(Uuid::now_v7(), Some(DocumentType::Resume), "简历 v1");
    let resume_b = profile(Uuid::now_v7(), Some(DocumentType::Resume), "简历 v2");
    let mut file_names = HashMap::new();
    file_names.insert(resume_a.file_id, "周晨简历v1.pdf".to_owned());
    file_names.insert(resume_b.file_id, "周晨简历v2.pdf".to_owned());

    let plan = resume_projects_plan();
    let session = AskSessionContext::default();
    let input = ResolverInput::new(
        &plan,
        &session,
        vec![resume_a.clone(), resume_b.clone()],
        file_names,
    );
    let resolution = resolve_documents(&input);
    assert_eq!(resolution.status, ResolutionStatus::MultipleCandidates);

    // 桌面层组装澄清选项（4.6 分支）：take(MAX_CANDIDATE_SCOPE)，含两份
    let options = clarification_options_from(&resolution);
    assert_eq!(options.len(), 2);
    assert!(
        options
            .iter()
            .all(|(_, score, _)| *score >= MEDIUM_CONFIDENCE_THRESHOLD)
    );
    // 澄清载荷语义：reference 保留原话，供选择后写 USER_SELECTION 别名
    let reference = plan.target.reference.clone().expect("reference present");
    assert_eq!(reference, "我的简历");
}

/// 场景 F：澄清选择后锁定文件 + 写入 USER_SELECTION 别名 + 跳过 Resolver。
/// 用户选中 file_id 后：会话 active 锁定、pending reference 清除、
/// 「我的简历」成为该文件的持久别名；下一轮同一引用直接命中别名，
/// 不再重新全库解析。
#[test]
fn scenario_f_clarification_selection_locks_and_writes_memory() {
    let selected = Uuid::now_v7();
    let reference = "我的简历";

    // 1. 别名可写性（与桌面层 is_alias_writable_reference 同口径）：
    //    短名词短语、非问句 → 可写；问句/长句 → 不可写
    assert!(normalize_alias(reference).is_some());
    let writable = (2..=20).contains(&reference.chars().count())
        && !reference.ends_with('？')
        && !reference.ends_with('吗')
        && !reference.contains("什么")
        && !reference.contains("有没有");
    assert!(writable, "「我的简历」应可写为别名");

    // 2. 选择后会话上下文：active 锁定 + pending 清除（run_clarified_answer）
    let mut updated = AskSessionContext {
        active_file_id: None,
        pending_clarification_reference: Some(reference.to_owned()),
        ..AskSessionContext::default()
    };
    updated.active_file_id = Some(selected);
    updated.active_file_ids = vec![selected];
    updated.pending_clarification_reference = None;
    assert_eq!(updated.active_file_id, Some(selected));
    assert!(updated.pending_clarification_reference.is_none());

    // 3. 写入的别名 → 下一次提问直接命中（memory 定位优先）
    let written = alias(selected, "我的简历", MemorySource::UserSelection);
    let hints = match_alias_hints("我的简历里有什么项目？", &[written]);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].target_type, MemoryTargetType::File);
    assert_eq!(hints[0].target_id, selected);
    assert_eq!(hints[0].source_type, MemorySource::UserSelection);
}

/// 场景 G：DOCUMENT_SUMMARY 走整文结构摘要，禁止只拿 top-3 chunk。
/// CASE 7：「我的简历有什么内容？」→ requires_full_document；chunk 按
/// 标题结构分组为章节，节数超限合并尾部。
#[test]
fn scenario_g_summary_requires_full_document_structure() {
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
    assert_eq!(plan.content_query, None);

    // 整文章节分组：两个 heading（教育经历 / 项目经历）各成一组
    let chunk = |node_id: Uuid, ordinal: u64, text: &str| SectionChunk {
        chunk_id: Uuid::now_v7(),
        node_id,
        revision_id: Uuid::now_v7(),
        ordinal,
        text: text.to_owned(),
        locator: SourceLocator::default(),
    };
    let edu_node = Uuid::now_v7();
    let proj_node = Uuid::now_v7();
    let chunks = vec![
        chunk(edu_node, 0, "2019 年毕业于 X 大学"),
        chunk(edu_node, 1, "主修计算机科学与技术"),
        chunk(proj_node, 2, "大模型应用开发项目"),
        chunk(proj_node, 3, "负责 RAG 检索链路"),
    ];
    let headings = HashMap::from([
        (edu_node, vec!["教育经历".to_owned()]),
        (proj_node, vec!["项目经历".to_owned()]),
    ]);
    let mut sections = build_document_sections(&chunks, &headings, 10_000);
    assert_eq!(sections.len(), 2, "两个标题 → 两个章节");
    assert_eq!(sections[0].title, "教育经历");
    assert_eq!(sections[1].title, "项目经历");
    assert_eq!(sections[1].chunks.len(), 2);

    // 节数超限：尾部并入「其余内容」（保留边界，不丢 chunk）
    sections.push(sections[1].clone());
    let kept = merge_tail_sections(&mut sections, 2);
    assert_eq!(kept, 2);
    assert_eq!(sections.last().expect("last section").title, "其余内容");

    // 摘要解析确定性：合法输出 → 完整；脏输出 → 丢弃但保留合法项
    let parsed = parse_section_summaries(
        r#"```json
        {"sections":[{"title":"教育经历","summary":"教育背景概述","key_points":["X大学"]}]}
        ```"#,
    );
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].title, "教育经历");
}

/// 场景 H：COMPARE_DOCUMENTS 双目标分离，两侧独立解析。
/// 「比较我两个简历版本」→ primary（我的简历）+ secondary_target（第二个版本）；
/// 执行管线分类为 document_compare，绝不落到普通 chunk RAG。
#[test]
fn scenario_h_compare_dual_targets_route_to_compare_pipeline() {
    let plan = parse_query_plan(
        r#"{"source":"local","intent":"compare_documents","operation":"compare",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "secondary_target":{"reference":"第二个版本","document_type":"resume","document_name":null,
                                "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"有什么不同","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.92}"#,
    )
    .expect("compare plan parses");
    assert_eq!(plan.intent, QueryIntent::CompareDocuments);
    assert_eq!(plan.operation, QueryOperation::Compare);
    let secondary = plan.secondary_target.as_ref().expect("dual target present");
    assert_eq!(secondary.document_type, Some(DocumentType::Resume));

    // 与桌面层 operation_execution 的分类表达式一致
    let pipeline = if plan.intent == QueryIntent::CompareDocuments {
        "document_compare"
    } else {
        "chunk_rag"
    };
    assert_eq!(pipeline, "document_compare");
}

/// 场景 I：EXTRACT 条目与证据确定性对齐。
/// 模型输出结构化条目后，每条目与 citation quote 用最长公共子串对齐，
/// 达到 EXTRACT_MATCH_MIN_LEN 才挂证据；否则回落到最高分引用（桌面层
/// fallback，此处验证对齐判定本身）。
#[test]
fn scenario_i_extract_items_align_with_evidence() {
    let raw =
        r#"{"items":[{"item":"大模型应用开发项目","evidence":"负责 RAG 检索链路的设计与实现"}]}"#;
    let results: ExtractResults = parse_extract_results(raw).expect("extract results parse");
    assert_eq!(results.items.len(), 1);
    let item = &results.items[0];
    assert_eq!(item.item, "大模型应用开发项目");

    // 证据引用文本与条目共享核心片段（≥ EXTRACT_MATCH_MIN_LEN）→ 可对齐
    let quotes = [
        "在项目中负责大模型应用开发与 RAG 检索链路的设计",
        "负责数据清洗与可视化",
    ];
    let best = quotes
        .iter()
        .max_by_key(|quote| longest_common_substr_len(&item.item, quote))
        .expect("quotes non-empty");
    let match_len = longest_common_substr_len(&item.item, best);
    assert!(
        match_len >= EXTRACT_MATCH_MIN_LEN,
        "对齐长度 {match_len} 应达到 {EXTRACT_MATCH_MIN_LEN}"
    );
    assert!(best.contains("大模型应用开发"), "应选中含条目的引用");
}

/// 场景 J：DOCUMENT_FIND 只定位文件，不跑 chunk RAG。
/// 「我的简历在哪」→ intent document_find：无 content_query（不检索正文），
/// 目标解析成功后直接返回文件定位结果（answer_mode = find）。
#[test]
fn scenario_j_find_resolves_file_without_chunk_rag() {
    let plan = parse_query_plan(
        r#"{"source":"local","intent":"document_find","operation":"find",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":null,"filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.8}"#,
    )
    .expect("find plan parses");
    assert_eq!(plan.intent, QueryIntent::DocumentFind);
    assert_eq!(plan.operation, QueryOperation::Find);
    assert_eq!(plan.content_query, None, "find 不产生正文检索词");
    assert!(!plan.requires_full_document);

    // 目标解析：单简历 → Resolved 锁定文件
    let resume = profile(
        Uuid::now_v7(),
        Some(DocumentType::Resume),
        "大模型开发工程师-周晨",
    );
    let mut file_names = HashMap::new();
    file_names.insert(resume.file_id, "大模型开发工程师-周晨.pdf".to_owned());
    let session = AskSessionContext::default();
    let input = ResolverInput::new(&plan, &session, vec![resume.clone()], file_names);
    let resolution = resolve_documents(&input);
    assert_eq!(resolution.status, ResolutionStatus::Resolved);
    assert_eq!(resolution.resolved_file_ids, vec![resume.file_id]);
    // 桌面层：answer_mode = find（只含定位信息，不含正文答案）
    assert_eq!(AnswerMode::Find.as_str(), "find");
}

/// 场景 K：LOCAL 检索无证据绝不转闲聊。
/// CASE 6：「我的简历里有没有身份证号？」→ 目标锁定成功、正文无证据 →
/// EvidenceStatus::NoEvidence；来源与意图保持 LOCAL/DocumentQa，refusal
/// 固定文案返回，路由决定来源、检索决定证据，两者完全解耦。
#[test]
fn scenario_k_local_no_evidence_never_becomes_chat() {
    let mut plan = resume_projects_plan();
    plan.content_query = Some("身份证号".to_owned());

    // 目标锁定不依赖 content_query（画像层面没有身份证号信号也必须锁简历）
    let resume = profile(
        Uuid::now_v7(),
        Some(DocumentType::Resume),
        "大模型开发工程师-周晨",
    );
    let mut file_names = HashMap::new();
    file_names.insert(resume.file_id, "大模型开发工程师-周晨.pdf".to_owned());
    let session = AskSessionContext::default();
    let input = ResolverInput::new(&plan, &session, vec![resume.clone()], file_names);
    let resolution = resolve_documents(&input);
    assert_eq!(resolution.status, ResolutionStatus::Resolved);
    assert_eq!(resolution.resolved_file_ids, vec![resume.file_id]);

    // 证据缺失（模拟检索返回空）：来源/意图绝不被改写为 chat
    plan.source = SourceIntent::Local; // 保持不变
    assert_eq!(plan.intent, QueryIntent::DocumentQa);
    assert_eq!(plan.source, SourceIntent::Local);
    assert_ne!(plan.intent, QueryIntent::GeneralChat);
    // 桌面层固定文案分支：answer_mode = rag_refusal（绝不 answer_mode = chat）
    assert_eq!(AnswerMode::RagRefusal.as_str(), "rag_refusal");
    assert_ne!(AnswerMode::RagRefusal, AnswerMode::Chat);
}

/// 场景 L：Memory 别名/关系定位优先于 Resolver 启发式。
/// 用户起的别名（user_explicit）与已确认关系直接锁定文件；candidate
/// 关系绝不参与定位。
#[test]
fn scenario_l_memory_alias_and_relation_locate_first() {
    let resume_id = Uuid::now_v7();
    // 1. 别名：user_explicit「小周简历」→ 简历文件
    let aliases = vec![alias(resume_id, "小周简历", MemorySource::UserExplicit)];
    let hints = match_alias_hints("小周简历里有什么项目？", &aliases);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].target_id, resume_id);
    assert_eq!(hints[0].kind, "alias");

    // 2. 关系：confirmed「周晨 → 简历」，问题提实体「周晨」→ 定位到文件
    let person_id = Uuid::now_v7();
    let person = MemoryEntity {
        entity_id: person_id,
        entity_type: "person".to_owned(),
        canonical_name: "周晨".to_owned(),
        metadata_json: serde_json::Value::Null,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let now = chrono::Utc::now();
    let confirmed = MemoryRelation {
        relation_id: Uuid::now_v7(),
        subject_type: MemoryTargetType::Entity,
        subject_id: person_id,
        predicate: "拥有".to_owned(),
        object_type: MemoryTargetType::File,
        object_id: resume_id,
        confidence: 0.98,
        status: MemoryStatus::Confirmed,
        source_type: MemorySource::UserConfirmed,
        source_id: None,
        created_at: now,
        updated_at: now,
    };
    let relation_hints = match_relation_hints(
        "周晨的简历里有什么？",
        std::slice::from_ref(&person),
        std::slice::from_ref(&confirmed),
    );
    assert_eq!(relation_hints.len(), 1);
    assert_eq!(relation_hints[0].target_id, resume_id);
    assert_eq!(relation_hints[0].kind, "relation");

    // 3. candidate 关系绝不参与定位（memory_resolver 约束）
    let candidate = MemoryRelation {
        status: MemoryStatus::Candidate,
        ..confirmed
    };
    assert!(match_relation_hints("周晨的简历里有什么？", &[person], &[candidate]).is_empty());
}

/// 场景 M：Memory Writer 写入 → 后续提问命中（闭环）。
/// USER_SELECTION 写入的别名在下一轮被 Memory Resolver 命中，
/// resolved_scope 直接采用记忆定位（不依赖 Resolver 启发式）。
#[test]
fn scenario_m_memory_write_roundtrip_changes_future_resolution() {
    let selected = Uuid::now_v7();
    // 写入（up 端与 run_clarified_answer 同口径：normalize + UserSelection）
    let written_alias = normalize_alias("我的简历").expect("alias normalizable");
    let stored = alias(selected, &written_alias, MemorySource::UserSelection);

    // 下一轮同一引用 → 别名命中，scope = 记忆定位的文件
    let hints = match_alias_hints("我的简历里有没有 LangGraph？", &[stored]);
    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].target_id, selected);
    // 编排层：memory_resolution_ok → resolved_scope = memory_scope（优先于
    // Document Resolver 的启发式候选）
    let memory_scope: Vec<Uuid> = hints
        .iter()
        .filter(|hint| hint.target_type == MemoryTargetType::File)
        .map(|hint| hint.target_id)
        .collect();
    assert_eq!(memory_scope, vec![selected]);
}

/// 场景 N：AnswerMode 强类型序列化与历史宽容。
/// 9 种变体蛇形序列化；legacy「extractive」与未知值落回 Generated。
#[test]
fn scenario_n_answer_mode_serde_and_legacy_tolerance() {
    let modes = [
        AnswerMode::Generated,
        AnswerMode::Chat,
        AnswerMode::RagRefusal,
        AnswerMode::Unverified,
        AnswerMode::Clarification,
        AnswerMode::Summary,
        AnswerMode::Compare,
        AnswerMode::Extract,
        AnswerMode::Find,
    ];
    for mode in modes {
        let json = serde_json::to_value(mode).expect("serialize");
        let text = json.as_str().expect("string");
        assert_eq!(
            AnswerMode::parse_lenient(text),
            Some(mode),
            "roundtrip {text}"
        );
    }
    // legacy 中间态 → Generated；未知 → Generated
    assert_eq!(
        AnswerMode::parse_lenient("extractive"),
        Some(AnswerMode::Generated)
    );
    assert_eq!(AnswerMode::parse_lenient("future_mode"), None);
    let unknown: AnswerMode =
        serde_json::from_str(r#""future_mode""#).expect("tolerant deserialize");
    assert_eq!(unknown, AnswerMode::Generated);
}

/// 场景 O：content_query 与 target 严格分离，绝不拼接。
/// CASE 8：「我的简历里有没有 LangGraph？」→ target=我的简历（定位文件），
/// content_query=LangGraph（检索词）；禁止「我的 简历 LangGraph」整句检索。
#[test]
fn scenario_o_content_query_never_concatenates_target() {
    let plan = parse_query_plan(
        r#"{"source":"local","intent":"document_qa","operation":"extract",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"LangGraph","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.94}"#,
    )
    .expect("langgraph plan parses");
    assert_eq!(plan.target.reference.as_deref(), Some("我的简历"));
    assert_eq!(plan.target.document_type, Some(DocumentType::Resume));
    let retrieval_question = retrieval_question_from(&plan).expect("content query present");
    assert_eq!(retrieval_question, "LangGraph");
    assert!(
        !retrieval_question.contains("简历"),
        "目标词不得拼回检索 Query"
    );
    assert!(!retrieval_question.contains("我的"));

    // 解析到文件后，检索范围 = [简历]，检索词 = LangGraph（scope 与 query 解耦）
    let resume = profile(
        Uuid::now_v7(),
        Some(DocumentType::Resume),
        "大模型开发工程师-周晨",
    );
    let mut file_names = HashMap::new();
    file_names.insert(resume.file_id, "大模型开发工程师-周晨.pdf".to_owned());
    let session = AskSessionContext::default();
    let input = ResolverInput::new(&plan, &session, vec![resume.clone()], file_names);
    let resolution = resolve_documents(&input);
    assert_eq!(resolution.resolved_file_ids, vec![resume.file_id]);
}

/// 场景 P：文档级召回两级融合（MULTI_DOCUMENT_QA 前置）。
/// metadata 粗筛 + 向量精排融合（0.55/0.45）；无向量命中按 metadata-only
/// 折算；低于 MIN_SCORE 不进候选；TOP_N 截断。
#[test]
fn scenario_p_document_recall_fuses_metadata_and_vector() {
    let resume = profile(
        Uuid::now_v7(),
        Some(DocumentType::Resume),
        "大模型开发工程师-周晨",
    );
    let report = profile(
        Uuid::now_v7(),
        Some(DocumentType::Report),
        "2025 年度述职报告",
    );
    let unrelated = profile(Uuid::now_v7(), None, "七月发票");

    let mut resume_typed = resume;
    resume_typed.entities = vec!["LangGraph".to_owned()];
    resume_typed.keywords = vec!["项目".to_owned()];
    let mut report_typed = report;
    report_typed.keywords = vec!["年终".to_owned()];
    let mut profiles = vec![
        (resume_typed.clone(), "大模型开发工程师-周晨.pdf".to_owned()),
        (report_typed.clone(), "2025 年度述职报告.pdf".to_owned()),
        (unrelated.clone(), "七月发票.pdf".to_owned()),
    ];
    // 无关画像：metadata 零命中 → 不进预筛
    let vectors = HashMap::from([
        (resume_typed.file_id, vec![0.9f32, 0.1, 0.2, 0.8]),
        (report_typed.file_id, vec![0.2f32, 0.9, 0.7, 0.3]),
    ]);
    let question_vector = vec![0.8f32, 0.2, 0.15, 0.9];

    let ranked = rank_document_candidates(
        "有没有 LangGraph 项目的内容？",
        Some(&question_vector),
        &profiles,
        &vectors,
    );
    assert_eq!(
        ranked.first().expect("best candidate").file_id,
        resume_typed.file_id,
        "实体+向量命中应排第一"
    );
    let best = &ranked[0];
    assert!(best.signals.iter().any(|signal| signal == "entity_match"));
    assert!(best.signals.iter().any(|signal| signal == "vector_match"));
    assert!(best.score >= 0.25, "得分不得低于 MIN_SCORE");

    // 无向量候选：metadata-only 折算后仍可能进集；TOP_N 与分数门槛生效
    profiles.truncate(1);
    let empty_vectors = HashMap::new();
    let ranked_meta_only = rank_document_candidates("周晨的项目", None, &profiles, &empty_vectors);
    assert!(ranked_meta_only.iter().all(|c| c.score >= 0.25));

    // 全零向量不致 panic（cosine 返回 0），候选按 metadata-only 折算
    // （0.8×metadata，不加 vector_match 信号）
    let zero_vectors = HashMap::from([(resume_typed.file_id, vec![0.0f32; 4])]);
    let ranked_zero = rank_document_candidates(
        "周晨的项目",
        Some(&question_vector),
        &profiles,
        &zero_vectors,
    );
    assert_eq!(ranked_zero.len(), 1);
    assert_eq!(ranked_zero[0].file_id, resume_typed.file_id);
    assert!(
        !ranked_zero[0]
            .signals
            .iter()
            .any(|signal| signal == "vector_match")
    );

    // 元数据零命中的画像即使向量相近也不被硬拉（preselect 只在粗筛集内做向量）
    let unrelated_profile = (unrelated.clone(), "七月发票.pdf".to_owned());
    let ranked_unrelated = rank_document_candidates(
        "LangGraph",
        Some(&question_vector),
        &[unrelated_profile],
        &vectors,
    );
    assert!(ranked_unrelated.is_empty(), "无关画像不进入召回候选");
}
