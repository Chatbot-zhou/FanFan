//! Answerability Gate + Entity/Keyword Consistency Gate + AnswerShape
//!（Phase 4.2 spec 二 / 三 / 四 / 五 / 十六）。
//!
//! CASE A 根因：问「我的资料里是怎么介绍 RAG 的？」，检索召回的是 Agent
//! 证据，Embedding/Rerank 给了足够分数，Generation 把 Agent 证据包装成
//! RAG 回答。本模块在最终 Generation 前增加**纯函数**门控：
//!
//! 1. [`classify_answer_shape`]：从问题与 QueryPlan 确定性推导回答语义
//!    （BOOLEAN_EXISTENCE / FACT_LOOKUP / LIST / …），生成侧据此约束
//!    第一句话的形态（如「有没有」必须先答「有 / 没有找到证据表明有」）；
//! 2. [`extract_query_entities`] + [`AnswerabilityVerdict`]：抽取问题中的
//!    关键技术实体（RAG / LangGraph / Transformer …），与证据文本（含
//!    等价变体，如 RAG ↔ 检索增强 / retrieval augmented）做一致性检查；
//!    实体完全缺失 → NOT_ANSWERABLE（即使相似度分数足够）；
//! 3. [`EvidenceRole`]：「提到了 Agent」≠「做过 Agent 项目」——
//!    BOOLEAN_EXISTENCE + 项目存在性断言需要 PROJECT 语境证据，
//!    纯概念解释证据不能支持；
//! 4. [`local_no_evidence_answer`]：LOCAL 无证据 / 门控拒绝的统一文案，
//!    禁止追加通用知识。
//!
//! 纪律：不做任何模型调用、不生成自由文本理由；所有判断可单测复现。

use serde::{Deserialize, Serialize};

use crate::ask::query_normalize::is_existence_question;
use crate::ask::query_plan::{QueryIntent, QueryOperation, QueryPlan};

// ============================================================
// AnswerShape（spec 四：QA 类型需要专门 Answer Semantics）
// ============================================================

/// 回答语义形态：决定生成 prompt 的第一条约束（如 BOOLEAN_EXISTENCE
/// 必须先答「有 / 没有找到证据表明有 / 资料不足以判断」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerShape {
    /// 「有没有 / 是否…过」：第一句必须是 有 / 没有找到证据表明有 / 资料不足以判断
    BooleanExistence,
    /// 「多少 / 几号 / 什么时候 / 谁」：精确事实查询
    FactLookup,
    /// 「有哪些 / 哪些 / 清单」：逐条列出
    List,
    /// 「是什么 / 怎么介绍 / 描述」：概念/内容描述
    Description,
    /// 「主要写了什么 / 总结一下」：概括
    Summary,
    /// 「在哪 / 第几页 / 哪个位置」：定位
    Location,
    /// 「A 和 B 有什么区别」：对比
    Compare,
    /// 「有哪些项目（名称）」：结构化抽取
    Extract,
}

impl AnswerShape {
    pub fn as_str(self) -> &'static str {
        match self {
            AnswerShape::BooleanExistence => "boolean_existence",
            AnswerShape::FactLookup => "fact_lookup",
            AnswerShape::List => "list",
            AnswerShape::Description => "description",
            AnswerShape::Summary => "summary",
            AnswerShape::Location => "location",
            AnswerShape::Compare => "compare",
            AnswerShape::Extract => "extract",
        }
    }
}

/// 清单型问题标记（「有哪些/哪些/列一下」）。
const LIST_MARKERS: &[&str] = &[
    "有哪些", "哪些项目", "哪些技能", "哪些内容", "都有什么", "都有哪些",
    "列一下", "列出来", "清单", "列表", "有几", "什么项目",
];
/// 描述型问题标记（「是什么/怎么介绍/描述」）。
const DESCRIPTION_MARKERS: &[&str] = &[
    "是什么", "什么是", "怎么介绍", "如何介绍", "介绍一下", "介绍下",
    "描述", "解释", "讲解", "讲了什么", "说的是什么", "是怎么",
];
/// 事实查询标记（数字/时间/人名等精确值）。
const FACT_MARKERS: &[&str] = &[
    "多少", "几号", "几点", "什么时候", "日期", "价格", "金额", "费用",
    "编号", "电话", "邮箱", "地址", "版本号", "截止", "到期", "谁",
];
/// 概括型问题标记。
const SUMMARY_MARKERS: &[&str] = &[
    "主要写了什么", "主要写什么", "主要讲了什么", "总结一下", "概括",
    "摘要", "主要内容", "写了啥", "讲了啥",
];
/// 定位型问题标记。
const LOCATION_MARKERS: &[&str] = &["在哪", "在哪儿", "哪个位置", "第几页", "位置", "哪里"];

/// 从问题与 QueryPlan 确定性推导回答语义形态。
/// operation 优先（compare / find / summary / extract 是结构性意图），
/// 其后按标记词长度优先匹配（「有哪些项目」是 List 而非 Description）。
pub fn classify_answer_shape(question: &str, plan: &QueryPlan) -> AnswerShape {
    match plan.intent {
        QueryIntent::CompareDocuments => return AnswerShape::Compare,
        QueryIntent::DocumentFind => return AnswerShape::Location,
        QueryIntent::DocumentSummary => return AnswerShape::Summary,
        _ => {}
    }
    if plan.operation == QueryOperation::Extract {
        return AnswerShape::Extract;
    }
    let folded = question.trim().to_lowercase();
    if is_existence_question(question) {
        return AnswerShape::BooleanExistence;
    }
    if SUMMARY_MARKERS.iter().any(|marker| folded.contains(marker)) {
        return AnswerShape::Summary;
    }
    if LIST_MARKERS.iter().any(|marker| folded.contains(marker)) {
        return AnswerShape::List;
    }
    if LOCATION_MARKERS.iter().any(|marker| folded.contains(marker)) {
        return AnswerShape::Location;
    }
    if DESCRIPTION_MARKERS.iter().any(|marker| folded.contains(marker)) {
        return AnswerShape::Description;
    }
    if FACT_MARKERS.iter().any(|marker| folded.contains(marker)) {
        return AnswerShape::FactLookup;
    }
    AnswerShape::Description
}

/// 各形态的生成约束（追加到 generation prompt 尾部，紧邻输出位置）。
/// 约束只描述「回答形态」，不替模型生成内容。
pub fn answer_shape_directive(shape: AnswerShape) -> String {
    match shape {
        AnswerShape::BooleanExistence => "\
【回答形态：有没有型问题】\n\
第一条 claim 必须直接回答存在性：「有。」（并给出证据）或「没有找到资料中的证据表明有。」或「当前资料不足以判断。」。\n\
之后才允许补充找到的具体内容与出处。禁止把回答写成相关概念的一般性介绍。"
            .to_owned(),
        AnswerShape::FactLookup => "\
【回答形态：精确事实查询】\n\
直接给出资料中的精确事实值（数字/日期/名称等），保留原文数字与单位；资料中没有该值时 claims 为空并 refusal，禁止估算或用常识补值。"
            .to_owned(),
        AnswerShape::List => "\
【回答形态：清单型问题】\n\
逐条列出资料中真实存在的条目，每条一个 claim 并标注证据；只列资料中存在的条目，禁止为「通常应该有的条目」补项。"
            .to_owned(),
        AnswerShape::Description => "\
【回答形态：内容描述】\n\
只围绕证据中实际出现的内容描述；证据没有覆盖的部分不要用通用知识补齐。"
            .to_owned(),
        AnswerShape::Summary => "\
【回答形态：概括】\n\
只概括证据中实际出现的内容；证据中不存在的章节/主题绝对不要出现。"
            .to_owned(),
        AnswerShape::Location => "\
【回答形态：定位】\n\
直接回答内容所在位置（文件 / 章节 / 页码，以证据 locator 为准）。"
            .to_owned(),
        AnswerShape::Compare => "\
【回答形态：对比】\n\
逐点对比两份资料的实际差异，每点都必须有双方证据支持；单方证据不足的点要说明「另一方资料未提到」。"
            .to_owned(),
        AnswerShape::Extract => "\
【回答形态：结构化抽取】\n\
每个条目必须是资料中出现的实体/短语（如项目名称），禁止输出整段描述句作为条目。"
            .to_owned(),
    }
}

// ============================================================
// Entity / Keyword Consistency Gate（spec 三：通用实体一致性）
// ============================================================

/// 抽取实体时忽略的英文功能词（问题句式词，不是实体）。
const ENGLISH_STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "what", "which",
    "who", "whose", "how", "why", "when", "where", "of", "in", "on", "at", "for",
    "to", "and", "or", "not", "no", "do", "does", "did", "have", "has", "had",
    "my", "me", "i", "you", "your", "it", "its", "this", "that", "these",
    "those", "with", "about", "there", "here", "can", "could", "should", "would",
];

/// 实体等价变体表（通用机制，非针对单一问题的硬编码）：常见技术缩写 ↔
/// 中英文全称。命中任一变体即视为「证据中出现了该实体」。表保持保守——
/// 只收真实通行的展开，避免把缩写错误扩大成别的概念。
const ENTITY_VARIANTS: &[(&str, &[&str])] = &[
    ("rag", &["检索增强", "retrieval augmented", "retrieval-augmented", "retrieval augmented generation"]),
    ("llm", &["大模型", "大语言模型", "large language model"]),
    ("agent", &["智能体"]),
    ("nlp", &["自然语言处理", "natural language processing"]),
    ("ml", &["机器学习", "machine learning"]),
    ("ai", &["人工智能", "artificial intelligence"]),
    ("kg", &["知识图谱", "knowledge graph"]),
    ("ocr", &["光学字符识别", "文字识别"]),
    ("gpt", &["generative pre"]),
    ("bm25", &["best matching 25"]),
];

/// 实体在证据中的变体展开（小写；无变体的实体返回自身）。
fn entity_surface_forms(entity: &str) -> Vec<String> {
    let mut forms = vec![entity.to_owned()];
    if let Some((_, variants)) = ENTITY_VARIANTS
        .iter()
        .find(|(key, _)| *key == entity)
    {
        forms.extend(variants.iter().map(|variant| (*variant).to_owned()));
    }
    forms
}

/// 实体是否在（已小写的）证据文本中出现。
/// 短实体（≤3 字符，如 ai / ml / kg / rag）必须按词边界命中，避免
/// `detail` 中的 `ai`、`storage` 中的 `rag` 之类的子串误命中；
/// 长实体（LangGraph / transformer）子串命中即可。
fn entity_appears_in(entity: &str, evidence_lower: &str) -> bool {
    let boundary = |ch: char| !ch.is_ascii_alphanumeric();
    for form in entity_surface_forms(entity) {
        let form_lower = form.to_lowercase();
        // 中文变体（含 CJK）按子串命中（中文没有词边界）
        let has_cjk = form_lower.chars().any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch));
        if has_cjk {
            if evidence_lower.contains(&form_lower) {
                return true;
            }
            continue;
        }
        if form_lower.chars().count() <= 3 {
            let mut search_from = 0usize;
            while let Some(found) = evidence_lower[search_from..].find(&form_lower) {
                let start = search_from + found;
                let end = start + form_lower.len();
                let before_ok = evidence_lower[..start]
                    .chars()
                    .next_back()
                    .map(boundary)
                    .unwrap_or(true);
                let after_ok = evidence_lower[end..]
                    .chars()
                    .next()
                    .map(boundary)
                    .unwrap_or(true);
                if before_ok && after_ok {
                    return true;
                }
                search_from = start + form_lower.len().max(1);
            }
        } else if evidence_lower.contains(&form_lower) {
            return true;
        }
    }
    false
}

/// 从问题与 content_query 中抽取关键技术实体（ASCII 字母数字词，长度 ≥2，
/// 非功能词、非纯数字，小写归一；去重截断到 8 个）。Embedding 是召回工具，
/// 这些实体是「最终事实支持」的一致性锚点。
pub fn extract_query_entities(question: &str, content_query: Option<&str>) -> Vec<String> {
    let combined = match content_query {
        Some(content) => format!("{} {}", question.trim(), content.trim()),
        None => question.trim().to_owned(),
    };
    let mut entities: Vec<String> = Vec::new();
    for raw in combined.split(|c: char| !(c.is_ascii_alphanumeric())) {
        let token = raw.trim().to_ascii_lowercase();
        if token.len() < 2 || token.parse::<f64>().is_ok() {
            continue;
        }
        if ENGLISH_STOPWORDS.contains(&token.as_str()) {
            continue;
        }
        if !entities.contains(&token) {
            entities.push(token);
        }
        if entities.len() >= 8 {
            break;
        }
    }
    entities
}

// ============================================================
// Evidence Role（spec 五：「提到了 Agent」≠「做过 Agent 项目」）
// ============================================================

/// 证据在回答中能扮演的角色（spec 十八 evidence_role 同枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRole {
    /// 概念解释（「X 是一种…」「X 会…」）——只能支持「提到过 X」
    Concept,
    /// 项目语境（项目经历/负责/开发/实现/职责/成果…）——
    /// 才能支持「做过 X 项目」
    Project,
    /// 人物信息（姓名/联系方式/个人简介）
    Person,
    /// 文档元数据（标题/基本信息/目录结构）
    DocumentMetadata,
    /// 具体事实（数字/日期/条款值）
    Fact,
    Other,
}

impl EvidenceRole {
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceRole::Concept => "concept",
            EvidenceRole::Project => "project",
            EvidenceRole::Person => "person",
            EvidenceRole::DocumentMetadata => "document_metadata",
            EvidenceRole::Fact => "fact",
            EvidenceRole::Other => "other",
        }
    }
}

/// 项目语境标记：出现即认为该证据处于「项目经历」语境。
const PROJECT_CONTEXT_MARKERS: &[&str] = &[
    "项目经历", "项目经验", "项目名称", "项目背景", "项目简介",
    "负责", "参与开发", "参与设计", "主导", "职责", "成果", "交付",
    "系统设计", "架构设计", "落地", "上线",
];
/// 概念解释标记。
const CONCEPT_MARKERS: &[&str] = &[
    "是一种", "是 一种", "指的是", "是指", "定义为", "概念", "定义",
    "原理", "会根据", "可以用来", "被用来",
];
/// 人物信息标记。
const PERSON_MARKERS: &[&str] = &["姓名", "联系方式", "电话", "邮箱", "个人简介", "性别", "出生"];
/// 文档元数据标记。
const METADATA_MARKERS: &[&str] = &["基本信息", "目录", "标题", "版本", "编制", "页数"];

/// 对单条证据做角色分类（文本 + 所在章节标题）。标题信号优先——
/// 「项目经历」标题下的内容天然是 PROJECT 语境；正文标记次之。
pub fn classify_evidence_role(text: &str, heading: Option<&str>) -> EvidenceRole {
    if let Some(heading) = heading {
        let heading = heading.trim();
        if !heading.is_empty() {
            if PROJECT_CONTEXT_MARKERS.iter().any(|m| heading.contains(m)) {
                return EvidenceRole::Project;
            }
            if PERSON_MARKERS.iter().any(|m| heading.contains(m)) {
                return EvidenceRole::Person;
            }
            if METADATA_MARKERS.iter().any(|m| heading.contains(m)) {
                return EvidenceRole::DocumentMetadata;
            }
        }
    }
    let has_project = PROJECT_CONTEXT_MARKERS.iter().any(|m| text.contains(m));
    let has_concept = CONCEPT_MARKERS.iter().any(|m| text.contains(m));
    // 概念句式与项目词同时出现时：项目语境优先（保守方向——宁可放行
    // PROJECT，由后续引用核验兜底；漏判 PROJECT 会让存在性断言全部拒答）。
    if has_project {
        return EvidenceRole::Project;
    }
    if has_concept {
        return EvidenceRole::Concept;
    }
    if PERSON_MARKERS.iter().any(|m| text.contains(m)) {
        return EvidenceRole::Person;
    }
    if METADATA_MARKERS.iter().any(|m| text.contains(m)) {
        return EvidenceRole::DocumentMetadata;
    }
    // 含精确数字/日期的短证据视为事实证据
    let digit_count = text.chars().filter(|c| c.is_ascii_digit()).count();
    if digit_count >= 2 && text.chars().count() <= 200 {
        return EvidenceRole::Fact;
    }
    EvidenceRole::Other
}

/// 存在性断言是否要求项目语境证据（「我以前有没有做过 Agent 项目？」）。
/// 命中条件：存在性问句 + 含「项目」+ 含「做过/参与/开发/负责/经历」。
pub fn existence_requires_project_context(question: &str) -> bool {
    is_existence_question(question)
        && question.contains("项目")
        && ["做过", "参与", "开发", "负责", "经历", "参加"]
            .iter()
            .any(|marker| question.contains(marker))
}

// ============================================================
// Answerability Gate（spec 二）
// ============================================================

/// 门控判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerabilityStatus {
    /// 证据足以直接回答当前问题
    Answerable,
    /// 证据只覆盖问题的一部分：允许回答明确找到的部分，
    /// 但必须说明资料只支持哪部分
    Partial,
    /// 证据与问题不一致 / 不构成支持：禁止进入普通生成
    NotAnswerable,
}

impl AnswerabilityStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AnswerabilityStatus::Answerable => "answerable",
            AnswerabilityStatus::Partial => "partial",
            AnswerabilityStatus::NotAnswerable => "not_answerable",
        }
    }
}

/// 单条参与门控的证据（正文引文 + 章节标题，均来自检索命中的真实 chunk）。
#[derive(Debug, Clone)]
pub struct GateEvidence {
    pub text: String,
    pub heading: Option<String>,
}

/// 门控输入。
pub struct AnswerabilityInput<'a> {
    pub question: &'a str,
    pub content_query: Option<&'a str>,
    pub plan: &'a QueryPlan,
    /// 检索选中并准备交给生成的证据（rerank 截断后）
    pub evidence: &'a [GateEvidence],
}

/// 门控判定产物（全部字段进 trace，spec 十八）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerabilityVerdict {
    pub status: AnswerabilityStatus,
    pub confidence: f32,
    /// 机器可读原因码（entity_mismatch:<e> / partial_entity_coverage:<e> /
    /// existence_requires_project_context / no_evidence / no_strong_entities）
    pub reason: String,
    pub answer_shape: AnswerShape,
    pub query_entities: Vec<String>,
    /// 证据中实际出现的查询实体（含变体命中）
    pub evidence_entities: Vec<String>,
    /// 完全缺失的查询实体
    pub missing_entities: Vec<String>,
    /// 每条证据的角色分类（与输入证据一一对应）
    pub evidence_roles: Vec<EvidenceRole>,
}

/// Answerability Gate 主入口（纯函数）。
///
/// 判定顺序：
/// 1. 证据为空 → NOT_ANSWERABLE(no_evidence)；
/// 2. 强实体（问题中的技术实体）一个都没出现 → NOT_ANSWERABLE(entity_mismatch)
///    ——即使 Embedding/Rerank 分数足够（CASE A：RAG 问题 + Agent 证据）；
/// 3. BOOLEAN_EXISTENCE + 项目存在性断言 + 无任何 PROJECT 语境证据 →
///    NOT_ANSWERABLE(existence_requires_project_context)（「提到过 Agent」
///    不能证明「做过 Agent 项目」）；
/// 4. 部分强实体缺失 → PARTIAL(partial_entity_coverage)（只答找到的部分）；
/// 5. 无强实体（纯中文问题）→ ANSWERABLE(no_strong_entities)，
///    一致性交给引用核验兜底。
pub fn evaluate_answerability(input: &AnswerabilityInput) -> AnswerabilityVerdict {
    let answer_shape = classify_answer_shape(input.question, input.plan);
    let query_entities = extract_query_entities(input.question, input.content_query);
    let evidence_roles = input
        .evidence
        .iter()
        .map(|evidence| classify_evidence_role(&evidence.text, evidence.heading.as_deref()))
        .collect::<Vec<_>>();

    if input.evidence.is_empty() {
        return AnswerabilityVerdict {
            status: AnswerabilityStatus::NotAnswerable,
            confidence: 0.0,
            reason: "no_evidence".to_owned(),
            answer_shape,
            query_entities,
            evidence_entities: Vec::new(),
            missing_entities: Vec::new(),
            evidence_roles,
        };
    }

    let evidence_lower = input
        .evidence
        .iter()
        .map(|evidence| {
            let mut combined = evidence.text.to_lowercase();
            if let Some(heading) = evidence.heading.as_deref() {
                combined.push(' ');
                combined.push_str(&heading.to_lowercase());
            }
            combined
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for entity in &query_entities {
        if entity_appears_in(entity, &evidence_lower) {
            matched.push(entity.clone());
        } else {
            missing.push(entity.clone());
        }
    }

    // 规则 2：实体明显不一致 → 拒答（CASE A）
    if !query_entities.is_empty() && matched.is_empty() {
        return AnswerabilityVerdict {
            status: AnswerabilityStatus::NotAnswerable,
            confidence: 0.15,
            reason: format!("entity_mismatch:{}", query_entities.join(",")),
            answer_shape,
            query_entities,
            evidence_entities: matched,
            missing_entities: missing,
            evidence_roles,
        };
    }

    // 规则 3：项目存在性断言需要 PROJECT 语境证据（spec 五）
    let requires_project = existence_requires_project_context(input.question);
    if requires_project
        && !evidence_roles.iter().any(|role| *role == EvidenceRole::Project)
    {
        return AnswerabilityVerdict {
            status: AnswerabilityStatus::NotAnswerable,
            confidence: 0.2,
            reason: "existence_requires_project_context".to_owned(),
            answer_shape,
            query_entities,
            evidence_entities: matched,
            missing_entities: missing,
            evidence_roles,
        };
    }

    // 规则 4：部分实体缺失 → PARTIAL（只答明确找到的部分）
    if !missing.is_empty() {
        return AnswerabilityVerdict {
            status: AnswerabilityStatus::Partial,
            confidence: 0.45,
            reason: format!("partial_entity_coverage:{}", missing.join(",")),
            answer_shape,
            query_entities,
            evidence_entities: matched,
            missing_entities: missing,
            evidence_roles,
        };
    }

    // 规则 5：无强实体 → 低置信放行（一致性由引用核验兜底）
    let (confidence, reason) = if query_entities.is_empty() {
        (0.55, "no_strong_entities".to_owned())
    } else {
        (0.9, "entities_consistent".to_owned())
    };
    AnswerabilityVerdict {
        status: AnswerabilityStatus::Answerable,
        confidence,
        reason,
        answer_shape,
        query_entities,
        evidence_entities: matched,
        missing_entities: missing,
        evidence_roles,
    }
}

// ============================================================
// LOCAL STRICT MODE（spec 六 / 十四 / 十六）
// ============================================================

/// LOCAL 生成的系统提示词（严格证据约束）。与 GENERAL（chat_prompt，
/// 允许模型知识）完全分离，禁止两个模式共用。
pub const LOCAL_STRICT_SYSTEM_PROMPT: &str = "你是翻翻的本地资料回答器，运行在 LOCAL STRICT MODE：\
1. 只能使用当前提供的证据回答；\
2. 证据无法回答时直接拒答（claims 为空并在 refusal 说明），不要勉强回答；\
3. 禁止补充模型自身知识；\
4. 禁止推荐「联系资料库管理员」等不存在的组织角色；\
5. 禁止猜测资料可能包含什么；\
6. 禁止用「通常来说 / 一般来说」补充通用知识。\
每个事实必须通过 citation_ids 关联证据，不得补充外部知识。";

/// 外部知识泄漏标记：LOCAL 生成中出现即判定该 claim 混入了通用知识
///（CASE Transformer：「资料库中没有……Transformer 通常用于自然语言处理
/// 领域，如 BERT、GPT……」的后半句不允许）。标记保持具体短语，
/// 降低与证据原文的误撞率。
pub const EXTERNAL_KNOWLEDGE_MARKERS: &[&str] = &[
    "一般来说", "通常来说", "根据一般知识", "根据常识", "根据通用知识",
    "建议联系管理员", "联系资料库管理员", "联系管理员", "最新版本可能有",
    "通常用于", "通常会", "一般会", "你可以参考", "你可以查阅",
    "一般来说包括",
];

/// 检查文本是否含外部知识泄漏标记，命中返回首个标记。
pub fn find_external_knowledge_marker(text: &str) -> Option<&'static str> {
    EXTERNAL_KNOWLEDGE_MARKERS
        .iter()
        .find(|marker| text.contains(**marker))
        .copied()
}

/// LOCAL 无证据 / 门控拒绝的统一文案（spec 十六）：
/// - 项目存在性断言：明确「没有找到项目记录，无法确认」；
/// - 含缺失实体：具体指出「没有找到明确提到 X 的内容」；
/// - 兜底：固定拒绝句，禁止追加通用知识、猜测或建议。
pub fn local_no_evidence_answer(
    question: &str,
    missing_entities: &[String],
    requires_project_context: bool,
) -> String {
    if requires_project_context {
        return "目前资料中没有找到能够证明这一点的项目记录（项目经历/负责/成果等）,\
因此暂时无法确认。".to_owned();
    }
    if let Some(entity) = missing_entities.first() {
        return format!("当前资料中没有找到明确提到 {entity} 的内容。");
    }
    // 无实体可指时保持与既有固定文案一致的兜底句
    let _ = question;
    "当前资料中没有找到足够依据。你可以换一种说法、扩大检索范围，或等待相关资料完成索引。"
        .to_owned()
}

/// Unsupported Claim Gate 的确定性主体检查（spec 十五）：claim 中的关键
/// 实体必须出现在它自己引用的证据里——Evidence 讲 Agent、Claim 说 RAG →
/// 主体不一致，UNSUPPORTED。负向/拒答式 claim（「没有找到…」）跳过：
/// 这类 claim 的证据本来就不含被问实体。
pub fn claim_subject_mismatch(claim_text: &str, evidence_quotes: &[&str]) -> Option<String> {
    const NEGATIVE_MARKERS: &[&str] = &[
        "没有找到", "未找到", "未提及", "没有提到", "未出现", "不足以", "无法确认",
    ];
    if NEGATIVE_MARKERS
        .iter()
        .any(|marker| claim_text.contains(marker))
    {
        return None;
    }
    let entities = extract_query_entities(claim_text, None);
    if entities.is_empty() {
        return None;
    }
    let evidence_lower = evidence_quotes
        .iter()
        .map(|quote| quote.to_lowercase())
        .collect::<Vec<_>>()
        .join("\n");
    entities
        .iter()
        .find(|entity| !entity_appears_in(entity, &evidence_lower))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::query_plan::{QueryTarget, SourceIntent};

    fn plan(intent: QueryIntent, operation: QueryOperation) -> QueryPlan {
        QueryPlan {
            source: SourceIntent::Local,
            intent,
            operation,
            target: QueryTarget::default(),
            secondary_target: None,
            content_query: None,
            filters: Default::default(),
            requires_document_resolution: true,
            requires_full_document: false,
            confidence: 0.9,
        }
    }

    fn evidence(text: &str, heading: Option<&str>) -> GateEvidence {
        GateEvidence {
            text: text.to_owned(),
            heading: heading.map(str::to_owned),
        }
    }

    // ---- AnswerShape ----

    #[test]
    fn classifies_boolean_existence() {
        // CASE 2：「我以前有没有做过 Agent 项目？」
        let p = plan(QueryIntent::DocumentQa, QueryOperation::Qa);
        assert_eq!(
            classify_answer_shape("我以前有没有做过 Agent 项目？", &p),
            AnswerShape::BooleanExistence
        );
        assert_eq!(
            classify_answer_shape("我的文件里有没有提到 Transformer？", &p),
            AnswerShape::BooleanExistence
        );
    }

    #[test]
    fn classifies_description_list_and_structural_shapes() {
        let qa = plan(QueryIntent::DocumentQa, QueryOperation::Qa);
        assert_eq!(
            classify_answer_shape("我的资料里是怎么介绍 RAG 的？", &qa),
            AnswerShape::Description
        );
        assert_eq!(
            classify_answer_shape("我的简历里面有哪些项目", &qa),
            AnswerShape::List
        );
        assert_eq!(
            classify_answer_shape("合同金额是多少", &qa),
            AnswerShape::FactLookup
        );
        assert_eq!(
            classify_answer_shape("我的简历主要写了什么？", &plan(
                QueryIntent::DocumentSummary,
                QueryOperation::Summary
            )),
            AnswerShape::Summary
        );
        assert_eq!(
            classify_answer_shape("随便问点什么", &plan(
                QueryIntent::CompareDocuments,
                QueryOperation::Compare
            )),
            AnswerShape::Compare
        );
    }

    #[test]
    fn shape_directives_constrain_first_sentence_for_boolean() {
        let directive = answer_shape_directive(AnswerShape::BooleanExistence);
        assert!(directive.contains("没有找到"));
        assert!(directive.contains("禁止"));
    }

    // ---- 实体抽取与一致性 ----

    #[test]
    fn extracts_technical_entities_ignoring_stopwords() {
        let entities = extract_query_entities("我的资料里是怎么介绍 RAG 的？", Some("RAG"));
        assert!(entities.contains(&"rag".to_owned()));
        let entities = extract_query_entities("帮我解释 LangGraph", None);
        assert!(entities.contains(&"langgraph".to_owned()));
        // 功能词不进实体表
        let entities = extract_query_entities("what is the my ai", None);
        assert!(!entities.iter().any(|entity| entity == "what"));
        assert!(entities.contains(&"ai".to_owned()));
    }

    #[test]
    fn short_entities_require_word_boundary() {
        // ai 不是 detail 的子串命中
        assert!(entity_appears_in("ai", "artificial intelligence is ai"));
        assert!(!entity_appears_in("ai", "detail about storage"));
        // rag 不是 storage 的子串命中（storage 不含 rag，本例防守）
        assert!(!entity_appears_in("rag", "storage"));
    }

    #[test]
    fn entity_variants_count_as_matches() {
        // RAG ↔ 检索增强 / retrieval augmented（spec 三示例）
        assert!(entity_appears_in("rag", "检索增强生成是一种…"));
        assert!(entity_appears_in("rag", "retrieval-augmented generation"));
        assert!(entity_appears_in("agent", "智能体会根据目标选择工具"));
    }

    // ---- Evidence Role ----

    #[test]
    fn concept_evidence_does_not_count_as_project() {
        let role = classify_evidence_role(
            "Agent 是一种大模型组件，会根据目标判断下一步、选择工具并读取工具结果。",
            None,
        );
        assert_eq!(role, EvidenceRole::Concept);
        let role = classify_evidence_role("参与法律 RAG 项目，负责检索模块开发", None);
        assert_eq!(role, EvidenceRole::Project);
        let role = classify_evidence_role("任意内容", Some("项目经历"));
        assert_eq!(role, EvidenceRole::Project);
    }

    #[test]
    fn existence_requires_project_detection() {
        // CASE 2
        assert!(existence_requires_project_context("我以前有没有做过 Agent 项目？"));
        // 「资料里有没有提到 X」不需要项目语境（是「提到」断言）
        assert!(!existence_requires_project_context(
            "我的文件里有没有提到 Transformer？"
        ));
    }

    // ---- Answerability Gate ----

    #[test]
    fn case_a_rag_question_with_agent_evidence_is_not_answerable() {
        // Phase 4.2 CASE A：RAG 问题 + Agent 证据 → NOT_ANSWERABLE
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let agent_evidence = evidence(
            "Agent 是一种大模型组件，不仅负责生成文本，还会根据目标判断下一步、选择工具并读取工具结果。",
            None,
        );
        let input = AnswerabilityInput {
            question: "我的资料里是怎么介绍 RAG 的？",
            content_query: Some("RAG"),
            plan: &p,
            evidence: &[agent_evidence],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::NotAnswerable);
        assert!(verdict.reason.starts_with("entity_mismatch"));
        assert!(verdict.missing_entities.contains(&"rag".to_owned()));
    }

    #[test]
    fn rag_question_with_rag_evidence_is_answerable() {
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let rag_evidence = evidence(
            "RAG（检索增强生成）通过先检索资料再生成回答的方式缓解大模型幻觉。",
            None,
        );
        let input = AnswerabilityInput {
            question: "我的资料里是怎么介绍 RAG 的？",
            content_query: Some("RAG"),
            plan: &p,
            evidence: &[rag_evidence],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::Answerable);
        assert!(verdict.evidence_entities.contains(&"rag".to_owned()));
    }

    #[test]
    fn case_b_agent_project_existence_needs_project_evidence() {
        // CASE 2：普通 Agent 概念证据不能证明「做过 Agent 项目」
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let concept = evidence("Agent 是一种…会根据目标选择工具。", None);
        let input = AnswerabilityInput {
            question: "我以前有没有做过 Agent 项目？",
            content_query: Some("Agent 项目"),
            plan: &p,
            evidence: &[concept],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::NotAnswerable);
        assert_eq!(verdict.reason, "existence_requires_project_context");

        // 项目经历语境证据 → 通过（实体 agent 也在证据中出现）
        let project = evidence(
            "项目经历：基于 Agent 的智能问答系统，负责编排模块开发。",
            Some("项目经历"),
        );
        let input = AnswerabilityInput {
            question: "我以前有没有做过 Agent 项目？",
            content_query: Some("Agent 项目"),
            plan: &p,
            evidence: &[project],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::Answerable);
    }

    #[test]
    fn transformer_local_no_evidence_text_stays_local() {
        // CASE 3：无证据文案不得包含外部知识
        let text = local_no_evidence_answer(
            "我的文件里有没有提到 Transformer？",
            &["transformer".to_owned()],
            false,
        );
        assert!(text.contains("transformer"));
        assert!(!text.contains("BERT"));
        assert!(!text.contains("通常"));
    }

    #[test]
    fn external_knowledge_markers_detected() {
        assert_eq!(
            find_external_knowledge_marker("Transformer通常用于自然语言处理领域"),
            Some("通常用于")
        );
        assert!(find_external_knowledge_marker("建议联系管理员").is_some());
        assert!(find_external_knowledge_marker("入职满一年后每年享有 5 天带薪年假").is_none());
    }

    #[test]
    fn partial_coverage_when_some_entities_missing() {
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let rag_only = evidence("RAG 通过检索增强缓解幻觉。", None);
        let input = AnswerabilityInput {
            question: "RAG 和 LangGraph 分别是怎么介绍的？",
            content_query: Some("RAG LangGraph"),
            plan: &p,
            evidence: &[rag_only],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::Partial);
        assert!(verdict.reason.starts_with("partial_entity_coverage"));
    }

    #[test]
    fn no_strong_entities_passes_with_lower_confidence() {
        let p = plan(QueryIntent::DocumentQa, QueryOperation::Qa);
        let input = AnswerabilityInput {
            question: "合同里关于付款的条款是什么？",
            content_query: Some("付款 条款"),
            plan: &p,
            evidence: &[evidence("付款方式为分期支付…", None)],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::Answerable);
        assert!(verdict.confidence < 0.9);
    }

    #[test]
    fn claim_subject_mismatch_catches_agent_evidence_rag_claim() {
        // spec 十五示例：Evidence 讲 Agent，Claim 说 RAG → UNSUPPORTED
        let mismatch = claim_subject_mismatch(
            "RAG 是一种根据目标判断下一步并选择工具的能力。",
            &["Agent 根据目标判断下一步、选择工具并读取工具结果。"],
        );
        assert_eq!(mismatch.as_deref(), Some("rag"));
        // 主体一致 → 放行
        assert!(claim_subject_mismatch(
            "RAG 通过先检索再生成缓解幻觉。",
            &["RAG（检索增强生成）先检索资料再生成回答。"],
        )
        .is_none());
        // 负向 claim 跳过（其证据本来就不含被问实体）
        assert!(claim_subject_mismatch(
            "资料中没有找到 LangGraph 相关的项目记录。",
            &["项目经历：基于 RAG 的问答系统。"],
        )
        .is_none());
        // 无实体的纯中文 claim 跳过
        assert!(claim_subject_mismatch("合同要求分期付款。", &["付款方式为分期支付。"]).is_none());
    }

    #[test]
    fn verdict_serializes_for_trace() {
        // trace 字段名稳定（spec 十八）
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let input = AnswerabilityInput {
            question: "我的资料里是怎么介绍 RAG 的？",
            content_query: Some("RAG"),
            plan: &p,
            evidence: &[evidence("Agent 会选择工具…", None)],
        };
        let verdict = evaluate_answerability(&input);
        let json = serde_json::to_value(&verdict).unwrap();
        assert_eq!(json["status"], "not_answerable");
        assert_eq!(json["answer_shape"], "description");
        assert!(json["query_entities"].is_array());
        assert!(json["evidence_roles"].is_array());
    }
}
