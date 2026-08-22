//! Lightweight Query Planner（Fast Path First，Model Escalation Last）。
//!
//! 在调用 LLM Query Parser **之前**，先用确定性的轻量规划器解析「用户指什么、
//! 想对它做什么」。只有当它对目标对象有高置信时（命中文档类型原型、归属者、
//! 内容分离），才短路返回一个结构化 `QueryPlan`，从而完全跳过生成模型；
//! 否则返回 None，由调用方继续走 LLM Parser 做结构化规划。
//!
//! 设计约束：
//! - **禁止针对具体文档/类型/测试问题写特判**。"简历/毕业"这类概念只出现在
//!   通用原型（[`DocumentType`] 的展示名 + [`type_keywords_for`]）里，同等地
//!   覆盖所有类型；意图词（有没有/在哪里/总结/哪些）是通用问句词，与文档无关。
//! - **精确点名不短路**：问题含《》书名号引用的完整标题，或 `xxx.ext` 形式的
//!   完整文件名时，目标对象依赖模型的精确判定（precise_named_document），
//!   轻量规划器置信不足，主动让位给 LLM——绝不在这条路径上偷改精确点名。
//! - **毕业类引用让位**：无类型锚词（如「我毕业时候那个材料」）时没有可复用
//!   的原型信号，置信低，交给 LLM 与 Resolver 的毕业扩展处理。
//!
//! 本模块纯函数、无 IO、无模型调用，单测直接覆盖合成与真实指代用例。

use crate::AskSessionContext;
use crate::ask::query_normalize::meaningful_tokens;
use crate::ask::query_plan::{QueryIntent, QueryOperation, QueryPlan, QuestionShape, SourceIntent};
use crate::contracts::DocumentType;
use crate::profile_builder::type_keywords_for;

/// Fast Path 置信度门槛：达到即短路 LLM。
pub const FAST_PATH_CONFIDENCE_THRESHOLD: f32 = 0.60;

/// Fast Path 的输出：结构化计划 + 置信度 + 命中的信号（供 trace 归因）。
#[derive(Debug, Clone, PartialEq)]
pub struct FastPathPlan {
    pub plan: QueryPlan,
    pub confidence: f32,
    /// 命中的信号标签（document_type / owner_self / content_extract / deictic /
    /// context_recovered / session_active_type…），进 trace 便于判断是哪个信号短路。
    pub signals: Vec<String>,
}

/// 意图词表（通用问句词，覆盖所有类型，与文档概念无关）。
struct IntentMatch {
    boolean: bool,
    locate: bool,
    summarize: bool,
    list: bool,
}

fn classify_intent(question: &str) -> IntentMatch {
    let q = question;
    IntentMatch {
        boolean: [
            "有没有",
            "是否有",
            "是否",
            "有没有提到",
            "有没有写",
            "有写过",
            "提过",
            "做过",
            "是否讲过",
        ]
        .iter()
        .any(|w| q.contains(w)),
        locate: [
            "在哪",
            "在哪里",
            "在哪呢",
            "位置",
            "第几页",
            "哪个文件",
            "哪里找",
            "在哪儿",
        ]
        .iter()
        .any(|w| q.contains(w)),
        summarize: [
            "总结",
            "概括",
            "归纳",
            "主要内容",
            "主要写了",
            "有什么内容",
            "讲了什么",
            "写了什么",
            "写了哪些内容",
            "整体",
            "概述",
            "回顾一下",
        ]
        .iter()
        .any(|w| q.contains(w)),
        list: [
            "哪些",
            "有哪些",
            "列出",
            "提取",
            "列举",
            "罗列",
            "几个",
            "都是些什么",
        ]
        .iter()
        .any(|w| q.contains(w)),
    }
}

/// 是否「精确点名」了某份文档：含《》完整标题，或形如 `xxx.ext` 的完整文件名。
/// 这类请求必须交给 LLM 判定 precise_named_document，Fast Path 不短路。
fn is_precise_document_naming(question: &str) -> bool {
    if question.contains('《') || question.contains('》') {
        return true;
    }
    // 形如 `周晨博_简历_final_v3.pdf` 的完整文件名（含扩展名）或目录样路径
    let has_extension = {
        let lower = question.to_ascii_lowercase();
        [
            ".docx", ".doc", ".pdf", ".ppt", ".pptx", ".xls", ".xlsx", ".md", ".txt", ".wps",
        ]
        .iter()
        .any(|ext| lower.contains(ext))
    };
    if has_extension {
        return true;
    }
    // 含分隔符的「标题式」引用（书名号之外的下划线/连字符学名）
    question
        .split(|c: char| c == '_' || c == '-' || c == ' ')
        .count()
        >= 4
        && question.contains('.')
}

/// 通用指代（deictic）标记：会话中指代上一对象的词。
const DEICTIC_MARKERS: &[&str] = &[
    "这个",
    "那个",
    "这份",
    "那份",
    "这部",
    "那个文件",
    "这个文件",
    "上次那个",
    "上次的",
    "上一篇",
    "上一份",
    "之前那个",
    "上一版",
];

/// Fast Path 规划入口。返回 Some 表示高置信（应用该计划并跳过 LLM），
/// None 表示置信不足、应升级到 LLM Parser。
pub fn fast_path_plan(question: &str, session: &AskSessionContext) -> Option<FastPathPlan> {
    let question = question.trim();
    if question.is_empty() {
        return None;
    }
    // 精确点名让位：绝不在此短路，避免覆盖模型对完整标题/文件名的判定。
    if is_precise_document_naming(question) {
        return None;
    }

    let mut signals: Vec<String> = Vec::new();
    let mut confidence: f32 = 0.0;

    // 1. 文档类型原型命中（所有类型统一，用展示名 + 规则关键词）。
    let owner_self = question.contains("我的") || question.starts_with("我本");
    let detected_type = detect_document_type(question);
    let mut plan = QueryPlan {
        source: SourceIntent::Local,
        ..QueryPlan::default()
    };

    // 2. 会话上下文恢复优先：active_file + 疑问代词（它/里面/这份）指代
    //    已回复的上一份文档。此时不需要类型，靠上下文即高置信。
    if session.active_file_id.is_some() && session_referencing(question) {
        confidence += 0.72;
        signals.push("context_recovered".to_owned());
        plan.target.owner = Some("self".to_owned());
    } else if let Some(active_type) = session.active_document_type {
        // 会话当前文档类型 + 指代 → 复用该类型定位
        if session_referencing(question) {
            confidence += 0.55;
            plan.target.document_type = Some(active_type);
            plan.target.owner = Some("self".to_owned());
            signals.push("session_active_type".to_owned());
        }
    }

    // 3. 文档类型原型 + 归属者 → 目标对象理解的核心信号。
    if let Some(document_type) = detected_type {
        confidence += 0.45;
        plan.target.document_type = Some(document_type);
        plan.target.document_name = None;
        plan.target.precise_named_document = false;
        signals.push("document_type".to_owned());
        if owner_self || question.contains('我') {
            confidence += 0.25;
            plan.target.owner = Some("self".to_owned());
            signals.push("owner_self".to_owned());
        } else {
            // 无「我的」，但带指代标记（上次那个合同/这份简历）也补充归属语境
            if DEICTIC_MARKERS
                .iter()
                .any(|marker| question.contains(marker))
            {
                confidence += 0.15;
                plan.target.owner = Some("self".to_owned());
                signals.push("deictic".to_owned());
            } else {
                return None;
            }
        }
    }

    // 无任何强的目标对象信号 → 升级 LLM（全库/闲聊/学科材料都由模型判断）。
    if confidence < 0.30 {
        return None;
    }

    // 4. reference：复现「我的/指代 + 类型展示名」的文档对象短语，供 Resolver
    //    的标题/词元匹配使用（与 document_type 信号互补，不做精确文件指定）。
    if let Some(document_type) = plan.target.document_type {
        let display = document_type.display_name();
        plan.target.reference = if owner_self {
            Some(format!("我的{display}"))
        } else {
            Some(display.to_owned())
        };
    }

    // 5. 意图判定（通用问句词）→ operation / question_shape / requires_full_document。
    let intent = classify_intent(question);
    let tokens = meaningful_tokens(question);
    // 内容词元：去掉已判定的类型锚词，剩下的是真正要检索的内容。
    let mut content_tokens: Vec<String> = Vec::new();
    if let Some(document_type) = plan.target.document_type {
        let type_keywords = type_keywords_for(document_type);
        for token in &tokens {
            if type_keywords.iter().any(|kw| kw.to_lowercase() == *token) {
                continue;
            }
            content_tokens.push(token.clone());
        }
    } else {
        content_tokens = tokens.clone();
    }
    let intent_words = [
        "有没有",
        "是否有",
        "是否",
        "在哪",
        "在哪里",
        "总结",
        "概括",
        "哪些",
        "有哪些",
        "列出",
        "提取",
        "什么",
        "内容",
        "介绍",
        "提到",
    ];
    // 剥掉内容词元前导的意图词：JieBa 分词缺失时「哪些项目」「有些什么」
    // 等会粘连成单 token，需逐个剥掉前导意图词前缀，防止 content_query 混入
    // 意图词（「我的简历里有哪些项目」→ content_query 应为「项目」而非「哪些项目」）。
    let mut filtered: Vec<String> = Vec::new();
    for token in content_tokens {
        let stripped = strip_intent_prefixes(&token, &intent_words);
        if !stripped.is_empty() && !filtered.contains(&stripped) {
            filtered.push(stripped);
        }
    }
    content_tokens = filtered;

    if intent.boolean {
        plan.operation = QueryOperation::Qa;
        plan.intent = QueryIntent::DocumentQa;
        plan.question_shape = QuestionShape::BooleanExistence;
        plan.content_query = if content_tokens.is_empty() {
            None
        } else {
            Some(content_tokens.join(" "))
        };
    } else if intent.locate {
        plan.operation = QueryOperation::Find;
        plan.intent = QueryIntent::DocumentFind;
        plan.question_shape = QuestionShape::Location;
        plan.content_query = None;
    } else if intent.summarize && content_tokens.is_empty() {
        plan.operation = QueryOperation::Summary;
        plan.intent = QueryIntent::DocumentSummary;
        plan.question_shape = QuestionShape::Summary;
        plan.content_query = None;
        plan.requires_full_document = true;
    } else if intent.list && !content_tokens.is_empty() {
        plan.operation = QueryOperation::Extract;
        plan.intent = QueryIntent::DocumentQa;
        plan.question_shape = QuestionShape::List;
        plan.requires_entity_items = true;
        plan.content_query = Some(content_tokens.join(" "));
    } else if !content_tokens.is_empty() {
        plan.operation = QueryOperation::Qa;
        plan.intent = QueryIntent::DocumentQa;
        plan.question_shape = QuestionShape::Description;
        plan.content_query = Some(content_tokens.join(" "));
    } else {
        // 有文档对象但没明确内容词：保守看成整文摘要或普通问答。
        plan.operation = QueryOperation::Qa;
        plan.intent = QueryIntent::DocumentQa;
        plan.question_shape = QuestionShape::Description;
        plan.content_query = None;
    }

    // 6. 末尾：目标对象必须经 Document Resolver 定位。
    plan.requires_document_resolution = true;

    // 7. 置信度门槛：只有足够明确的目标理解才短路，否则让位 LLM。
    if confidence < FAST_PATH_CONFIDENCE_THRESHOLD {
        return None;
    }
    plan.confidence = confidence;
    Some(FastPathPlan {
        plan,
        confidence,
        signals,
    })
}

/// 检测问题是否命中某文档类型原型（展示名 + 规则关键词，覆盖全部类型）。
fn detect_document_type(question: &str) -> Option<DocumentType> {
    let mut best: Option<(DocumentType, usize)> = None;
    for candidate in ALL_DOCUMENT_TYPES {
        let mut hits = 0;
        let display = candidate.display_name();
        if !display.is_empty() && question.contains(display) {
            hits += 2;
        }
        for keyword in type_keywords_for(*candidate) {
            if keyword.len() >= 2 && question.contains(keyword) {
                hits += 1;
            }
        }
        if hits > 0
            && best
                .as_ref()
                .map(|(_, best_hits)| hits > *best_hits)
                .unwrap_or(true)
        {
            best = Some((*candidate, hits));
        }
    }
    best.map(|(candidate, _)| candidate)
}

/// 全部文档类型（用于统一原型遍历，不针对任何具体类型）。
const ALL_DOCUMENT_TYPES: &[DocumentType] = &[
    DocumentType::Resume,
    DocumentType::Contract,
    DocumentType::Invoice,
    DocumentType::Paper,
    DocumentType::ProjectDocument,
    DocumentType::Meeting,
    DocumentType::LearningMaterial,
    DocumentType::Certificate,
    DocumentType::Report,
    DocumentType::Spreadsheet,
    DocumentType::Other,
];

/// 会话指代（疑问代词/方位/上一份）是否命中：表示用户引用「上一份/这个」。
fn session_referencing(question: &str) -> bool {
    DEICTIC_MARKERS
        .iter()
        .any(|marker| question.contains(marker))
}

/// 反复剥掉 token 前导的任一意图词前缀（最长优先），返回剥离后的剩余串；
/// 剥空返回空串（该 token 只剩意图词，不参与 content_query）。
/// 例：`哪些项目` → `项目`；`有没有项目` → `项目`；`项目` → `项目`。
fn strip_intent_prefixes(token: &str, intent_words: &[&str]) -> String {
    let mut remainder = token.to_owned();
    loop {
        let mut stripped = false;
        // 最长意图词优先，避免「哪些」被「哪」残词先吃掉
        for word in intent_words {
            if let Some(rest) = remainder.strip_prefix(word) {
                if rest.is_empty() {
                    // 整体被意图词吃掉（如 token=「内容」= 意图词）→ 返回空
                    return String::new();
                }
                remainder = rest.to_owned();
                stripped = true;
            }
        }
        if !stripped {
            break;
        }
    }
    remainder
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::query_plan::{QueryIntent, QueryOperation, QuestionShape};

    #[test]
    fn resume_projects_fast_path_plan() {
        // 「我的简历里有哪些项目」→ 类型原型 + 归属者 + 内容分离 → 高置信短路
        let fp = fast_path_plan("我的简历里有哪些项目", &AskSessionContext::default())
            .expect("fast path fires");
        assert!(fp.confidence >= FAST_PATH_CONFIDENCE_THRESHOLD);
        assert_eq!(fp.plan.target.document_type, Some(DocumentType::Resume));
        assert_eq!(fp.plan.target.owner.as_deref(), Some("self"));
        assert_eq!(fp.plan.content_query.as_deref(), Some("项目"));
        assert_eq!(fp.plan.operation, QueryOperation::Extract);
        assert_eq!(fp.plan.question_shape, QuestionShape::List);
        assert!(fp.plan.requires_document_resolution);
        // 命中即为路径信号，绝不拼接成整句
    }

    #[test]
    fn local_resume_wrote_what_projects() {
        // 真实 trace 曾 document_type=null → UNRESOLVED 的「我本地的简历里写了
        // 什么项目」，Fast Path 用原型 + 归属者稳定理解目标。
        let fp = fast_path_plan("我本地的简历里写了什么项目", &AskSessionContext::default())
            .expect("fast path fires");
        assert_eq!(fp.plan.target.document_type, Some(DocumentType::Resume));
        assert_eq!(fp.plan.target.owner.as_deref(), Some("self"));
        assert_eq!(fp.plan.operation, QueryOperation::Qa);
        assert!(fp.plan.content_query.is_some());
    }

    #[test]
    fn resume_location_finds_file() {
        let fp = fast_path_plan("我的简历在哪里", &AskSessionContext::default())
            .expect("fast path fires");
        assert_eq!(fp.plan.operation, QueryOperation::Find);
        assert_eq!(fp.plan.intent, QueryIntent::DocumentFind);
        assert_eq!(fp.plan.content_query, None);
    }

    #[test]
    fn resume_summary_requires_full_document() {
        let fp = fast_path_plan("我的简历有什么内容", &AskSessionContext::default())
            .expect("fast path fires");
        assert_eq!(fp.plan.operation, QueryOperation::Summary);
        assert_eq!(fp.plan.intent, QueryIntent::DocumentSummary);
        assert!(fp.plan.requires_full_document);
        assert_eq!(fp.plan.content_query, None);
    }

    #[test]
    fn precise_named_question_not_shortcircuited() {
        // 完整《》标题 或 .docx 文件名 → Fast Path 不短路，让位 LLM 精确点名
        for q in [
            "概括一下周晨博20212P2002《专业实习》课程实习总结报告.docx",
            "总结一下周晨博_简历_final_v3.pdf",
        ] {
            assert!(
                fast_path_plan(q, &AskSessionContext::default()).is_none(),
                "精确点名不得被 Fast Path 短路: {q}"
            );
        }
    }

    #[test]
    fn how_to_write_resume_not_shortcircuited() {
        // 「如何写好简历」没有归属者/内容分离 → 置信不足，交 LLM
        assert!(
            fast_path_plan("如何写好简历呢", &AskSessionContext::default()).is_none(),
            "知识性问题不得被短路"
        );
    }

    #[test]
    fn graduation_material_escalates_to_llm() {
        // 「我毕业时候那个材料」无类型原型 → 交 LLM + Resolver 毕业扩展处理
        assert!(fast_path_plan("我毕业时候那个材料", &AskSessionContext::default()).is_none());
    }

    #[test]
    fn empty_or_blank_escalates() {
        assert!(fast_path_plan("", &AskSessionContext::default()).is_none());
        assert!(fast_path_plan("   ", &AskSessionContext::default()).is_none());
    }

    #[test]
    fn contract_typed_reference_fast_path() {
        let fp = fast_path_plan("那份合同里有哪些条款", &AskSessionContext::default())
            .expect("contract fast path fires");
        assert_eq!(fp.plan.target.document_type, Some(DocumentType::Contract));
        assert_eq!(fp.plan.content_query.as_deref(), Some("条款"));
    }
}
