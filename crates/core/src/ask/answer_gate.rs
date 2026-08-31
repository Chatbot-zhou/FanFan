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

use crate::ask::query_plan::{QueryIntent, QueryOperation, QueryPlan, QuestionShape};

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

/// 从 QueryPlan 推导回答语义形态（AI 优先）：intent / operation /
/// question_shape 全部来自 LLM QueryParser 的语义解析输出，此处只做
/// 「结构化意图 → 回答形态」的映射，不再用问题关键词表（有哪些/多少/
/// 在哪/是什么…）硬编码猜测形态。
pub fn classify_answer_shape(_question: &str, plan: &QueryPlan) -> AnswerShape {
    // 结构性意图优先（来自 LLM Parser 的 intent / operation）
    match plan.intent {
        QueryIntent::CompareDocuments => return AnswerShape::Compare,
        QueryIntent::DocumentFind => return AnswerShape::Location,
        QueryIntent::DocumentSummary => return AnswerShape::Summary,
        _ => {}
    }
    if plan.operation == QueryOperation::Extract {
        return AnswerShape::Extract;
    }
    // 其余形态由 LLM Parser 的 question_shape 语义判断决定
    match plan.question_shape {
        QuestionShape::BooleanExistence => AnswerShape::BooleanExistence,
        QuestionShape::List => AnswerShape::List,
        QuestionShape::Location => AnswerShape::Location,
        QuestionShape::Summary => AnswerShape::Summary,
        QuestionShape::Fact => AnswerShape::FactLookup,
        QuestionShape::Description => AnswerShape::Description,
    }
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
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "what", "which", "who", "whose",
    "how", "why", "when", "where", "of", "in", "on", "at", "for", "to", "and", "or", "not", "no",
    "do", "does", "did", "have", "has", "had", "my", "me", "i", "you", "your", "it", "its", "this",
    "that", "these", "those", "with", "about", "there", "here", "can", "could", "should", "would",
];

/// 实体等价变体表（通用机制，非针对单一问题的硬编码）：常见技术缩写 ↔
/// 中英文全称。命中任一变体即视为「证据中出现了该实体」。表保持保守——
/// 只收真实通行的展开，避免把缩写错误扩大成别的概念。
const ENTITY_VARIANTS: &[(&str, &[&str])] = &[
    (
        "rag",
        &[
            "检索增强",
            "retrieval augmented",
            "retrieval-augmented",
            "retrieval augmented generation",
        ],
    ),
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
    if let Some((_, variants)) = ENTITY_VARIANTS.iter().find(|(key, _)| *key == entity) {
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
        let has_cjk = form_lower
            .chars()
            .any(|ch| ('\u{4e00}'..='\u{9fff}').contains(&ch));
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
// 中文内容词一致性（通用机制，非针对某文件/关键词）
// ============================================================

/// 判定 CJK 基本区 / 扩展 A / 兼容表意字符。
fn is_han_char(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
    )
}

/// 单字填充位：任一 bigram 含其中任一字符即视为「句式词」丢弃。
/// 目的是让「主题性」内容词（如 火星/移民/事务/索引）与句式填充
/// （我的/里面/有没有/提到）分离；只做通用过滤，允许个别词回归。
const FRAME_CHARS: &[char] = &[
    '我', '你', '他', '她', '它', '的', '了', '呢', '啊', '哦', '这', '那', '哪', '什', '么', '怎',
    '何', '为', '是', '没', '否', '提', '讲', '问', '看', '找', '里', '中', '会', '能', '要', '想',
    '吧', '请', '已', '被', '到', '有',
];

/// 通用高频泛化词（二元）：拆掉后不参与「证据是否含查询主题」判定。
/// 只收真正高频率、偏虚的名词/动词；具体领域词（事务/模型/视图）绝不在此列。
///
/// 除句式泛化词外，还收一类「容器/框架名词」（报告/条款/说明/章节…）：
/// 这类词本身不是主题，只是把主题内容装起来的外壳（如「体检报告」的
/// 「报告」、「付款条款」的「条款」）。它们在不同文档里极常见，若让它们
/// 单独参与「证据含主题词」判定，会让「体检报告」仅凭一个碰巧同现的
/// 「报告」就误判存在（CASE：[26]「体检报告」误命中「健康状态报告」）。
/// 因此把它们一并过滤——只有真正的主题词（体检/付款/事务…）才计入。
const GENERIC_BIGRAMS: &[&str] = &[
    "计划", "内容", "方面", "部分", "情况", "东西", "问题", "相关", "时候", "文件", "资料", "文档",
    "系统", "模块", "功能", "概念", "原理", "知识", "基础", "重点", "作用", "区别", "联系", "比较",
    "解释", "介绍", "讲述", "提到", "包括", "包含", "涉及", "关于", "报告", "条款", "说明", "描述",
    "章节", "小节",
];

/// 从查询（问题 + 内容查询）抽取「主题性」中文内容词。
/// 复用与 FTS 索引用同一套 Han 连续段 bigram 概念：
///   1. 提取连续汉字段；
///   2. 对每段按窗口 2 生成 bigram；
///   3. 丢弃含句式单字（FRAME_CHARS）或落在通用泛化词（GENERIC_BIGRAMS）
///      的 bigram，保留主题性内容词；
///   4. 去重并截断。
/// 该结果用于「证据完全不含查询主题词 → 回答与主题无关」的拒答门控，
/// 只做通用过滤，不对任何具体文件/关键词做特判。
fn extract_chinese_content_terms(text: &str) -> Vec<String> {
    const SEGMENT_MIN: usize = 2;
    const MAX_TERMS: usize = 6;
    let mut terms: Vec<String> = Vec::new();
    let mut han_run: Vec<char> = Vec::new();
    for character in text.chars() {
        if is_han_char(character) {
            han_run.push(character);
        } else {
            push_chinese_bigrams(&mut terms, &han_run, SEGMENT_MIN);
            han_run.clear();
        }
    }
    push_chinese_bigrams(&mut terms, &han_run, SEGMENT_MIN);
    let mut seen = std::collections::HashSet::new();
    terms.retain(|term| seen.insert(term.clone()));
    terms.truncate(MAX_TERMS);
    terms
}

/// 把单个连续汉字段切成 bigram，过滤句式/泛化词后加入集合。
fn push_chinese_bigrams(terms: &mut Vec<String>, run: &[char], min_len: usize) {
    if run.len() < min_len {
        return;
    }
    for pair in run.windows(2) {
        let bigram: String = pair.iter().collect();
        if bigram.chars().any(|ch| FRAME_CHARS.contains(&ch)) {
            continue;
        }
        if GENERIC_BIGRAMS.iter().any(|generic| generic == &bigram) {
            continue;
        }
        if !terms.contains(&bigram) {
            terms.push(bigram);
        }
    }
}

/// 提取主题中的「复合」连续汉字段（长度 ≥ `MIN_COMPOSITE_LEN`）。
///
/// 复合词（如「量子计算」「火星移民计划」「机器学习」）常由「罕见核心词 +
/// 常见半词」构成。单独用 2 字 bigram 判定时，「量子计算」中的「计算」、
/// 「机器学习」中的「学习」等常见半词单独落地会**带偏整个主题**（把
/// 无关证据误判为该主题存在，生成侧据此杜撰）。本函数把这类复合字段整体
/// 保留，供存在性/事实类问句做更严格的落地判定。只做通用句式判定，不针对
/// 任何具体文件/关键词/case。
fn extract_composite_han_fields(text: &str) -> Vec<String> {
    const MIN_COMPOSITE_LEN: usize = 4;
    let mut fields: Vec<String> = Vec::new();
    let mut han_run: Vec<char> = Vec::new();
    for character in text.chars() {
        if is_han_char(character) {
            han_run.push(character);
        } else {
            if han_run.len() >= MIN_COMPOSITE_LEN {
                fields.push(han_run.iter().collect());
            }
            han_run.clear();
        }
    }
    if han_run.len() >= MIN_COMPOSITE_LEN {
        fields.push(han_run.iter().collect());
    }
    fields
}

/// 统计复合字段在（已小写的）证据中命中的不同 bigram 个数（去重）。
/// 「量子计算」在证据只有「计算」→ 命中数 1；「量子计算」完整落地 →
/// 命中数 ≥2。
fn count_field_bigram_hits(field: &str, evidence_lower: &str) -> usize {
    let chars: Vec<char> = field.chars().collect();
    let mut hits: Vec<String> = Vec::new();
    if chars.len() < 2 {
        return 0;
    }
    for pair in chars.windows(2) {
        let bigram: String = pair.iter().collect();
        if evidence_lower.contains(&bigram) && !hits.contains(&bigram) {
            hits.push(bigram);
        }
    }
    hits.len()
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
    "项目经历",
    "项目经验",
    "项目名称",
    "项目背景",
    "项目简介",
    "负责",
    "参与开发",
    "参与设计",
    "主导",
    "职责",
    "成果",
    "交付",
    "系统设计",
    "架构设计",
    "落地",
    "上线",
];
/// 概念解释标记。
const CONCEPT_MARKERS: &[&str] = &[
    "是一种",
    "是 一种",
    "指的是",
    "是指",
    "定义为",
    "概念",
    "定义",
    "原理",
    "会根据",
    "可以用来",
    "被用来",
];
/// 人物信息标记。
const PERSON_MARKERS: &[&str] = &[
    "姓名",
    "联系方式",
    "电话",
    "邮箱",
    "个人简介",
    "性别",
    "出生",
];
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
/// 判定依据来自 LLM QueryParser 输出的 requires_project_context 字段——
/// 模型语义判断「问题是否断言了做过/参与过某类项目经历」，不再用关键词
/// 表（有没有 + 项目 + 做过…）硬编码猜测。
pub fn existence_requires_project_context(question: &str, plan: &QueryPlan) -> bool {
    let _ = question;
    plan.requires_project_context && plan.question_shape == QuestionShape::BooleanExistence
}

/// 中文主题门控的失败详情：机器可读原因码 + 判定缺哪些主题信息。
struct ChineseThemeFail {
    /// 机器可读原因码（year_mismatch:<年份> / entity_mismatch:<词>）
    reason: String,
    /// 判定为缺失的主题信息（年份或未命中的主题词），用于问题描述与 trace。
    missing: Vec<String>,
}

/// 从主题文本提取第一个显式年份（4 位 19xx / 20xx），如「2024 年真题」→ "2024"。
/// 只匹配 ASCII 数字年份；用于存在类问题里「某年真题/记录是否存在」的强事实门控。
fn specified_year(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 4 {
        return None;
    }
    for i in 0..=chars.len() - 4 {
        let first = chars[i];
        if first != '1' && first != '2' {
            continue;
        }
        if chars[i + 1].is_ascii_digit()
            && chars[i + 2].is_ascii_digit()
            && chars[i + 3].is_ascii_digit()
        {
            let year: String = chars[i..i + 4].iter().collect();
            if year.starts_with("19") || year.starts_with("20") {
                return Some(year);
            }
        }
    }
    None
}

/// 规则 6 核心判定：中文内容词一致性 + 强事实年份落地（防编造）。
///
/// 对纯中文 / 无 ASCII 强实体命中的查询，证据与问题主题不一致时必须拒答，
/// 否则生成模型会拿无关证据编造主题。两路通用判定：
///
///  1. 年份强事实（存在/事实类回答）：主题里显式指定的年份必须在证据中
///     落地。例：「2024 年数据库系统工程师真题」证据却只到 2012 年 → 拒答。
///  2. 主题词落地：必须至少有一个「主题性」中文内容词在证据中出现。容器/
///     框架词（报告/条款/章节…）已在 `GENERIC_BIGRAMS` 过滤，不会因一个
///     碰巧同现的「报告」就误判「体检报告」存在。
///
/// 返回 `Some(fail)` 表示应拒答；`None` 表示放行。只做通用判定，不做任何
/// 模型调用，且不对具体文件/关键词做特判。
fn check_chinese_theme_gate(
    question: &str,
    content_query: Option<&str>,
    evidence_lower: &str,
    answer_shape: AnswerShape,
) -> Option<ChineseThemeFail> {
    let theme_source = content_query
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| question.trim());
    let theme_terms = extract_chinese_content_terms(theme_source);

    // 1. 强事实年份门控：存在性/事实类问题里显式年份必须落到证据。
    if matches!(
        answer_shape,
        AnswerShape::BooleanExistence | AnswerShape::FactLookup
    ) {
        if let Some(year) = specified_year(theme_source) {
            if !evidence_lower.contains(&year) {
                return Some(ChineseThemeFail {
                    reason: format!("year_mismatch:{year}"),
                    missing: vec![year],
                });
            }
        }

        // 1b. 复合词落地（存在/事实类问句的防偏带强门控）：长度 ≥4 的连续
        //     汉字段（复合词，如「量子计算」「机器学习」）若「未完整落地 且
        //     其内部不同 bigram 命中数 <2」，则证据只覆盖了该词的某个常见
        //     半词（如「计算」），不足以证明整个复合主题存在。此时必须拒答，
        //     否则生成模型会拿只含半词的无关证据杜撰复合主题内容（CASE：
        //     「量子计算」被数据库真题里的「计算」带偏，杜撰 CISC/分组计算）。
        //     只对复合词收紧；短主题词（≤3 字）维持下方 bigram 宽松判定，
        //     避免误伤「事务/隔离/范式」等正常术语。
        let composite_fields = extract_composite_han_fields(theme_source);
        for field in &composite_fields {
            if !evidence_lower.contains(field) && count_field_bigram_hits(field, evidence_lower) < 2
            {
                return Some(ChineseThemeFail {
                    reason: format!("composite_mismatch:{}", field),
                    missing: vec![field.clone()],
                });
            }
        }
    }

    // 2. 主题词落地：至少一个主题性中文内容词出现在证据里。
    if !theme_terms.is_empty() && !theme_terms.iter().any(|term| evidence_lower.contains(term)) {
        return Some(ChineseThemeFail {
            reason: format!("entity_mismatch:{}", theme_terms.join(",")),
            missing: theme_terms,
        });
    }

    None
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
/// 6. 中文内容词一致性：对无 ASCII 强实体命中的查询，证据完全不含任何
///    主题性中文内容词 → NOT_ANSWERABLE(entity_mismatch)（防编造）。
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

    // 规则 2：实体明显不一致 → 拒答（CASE A）。
    // 「英文实体全 miss」只有在问题不含可用中文主题锚点时，才是「证据与主题
    // 无关」的可靠信号。中英混合问题里，英文词常只是概念的译名原文（如
    // Read Committed / Repeatable Read），中文证据以译文（读已提交/可重复读）
    // 出现，英文词全 miss 不代表主题不一致。此时不在此拒答，交由后续规则 4
    // （Partial，允许继续）与规则 6（中文内容词门控）兜底，避免因译名差异误拒。
    if !query_entities.is_empty() && matched.is_empty() {
        let theme_source = input
            .content_query
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| input.question.trim());
        let has_chinese_theme = !extract_chinese_content_terms(theme_source).is_empty();
        if !has_chinese_theme {
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
        // 含中文主题词：不在此提前拒答，落到规则 4/6 做中文一致性兜底。
    }

    // 规则 3：项目存在性断言需要 PROJECT 语境证据（spec 五）
    let requires_project = existence_requires_project_context(input.question, input.plan);
    if requires_project && !evidence_roles.contains(&EvidenceRole::Project) {
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

    // 规则 6：中文内容词一致性（防编造）——详见 `check_chinese_theme_gate`。
    // 仅在无 ASCII 强实体命中（matched 为空）时生效；有英文锚点交给既有规则。
    if matched.is_empty() {
        if let Some(fail) = check_chinese_theme_gate(
            input.question,
            input.content_query,
            &evidence_lower,
            answer_shape,
        ) {
            return AnswerabilityVerdict {
                status: AnswerabilityStatus::NotAnswerable,
                confidence: 0.1,
                reason: fail.reason,
                answer_shape,
                query_entities,
                evidence_entities: matched,
                missing_entities: fail.missing,
                evidence_roles,
            };
        }
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

/// LOCAL 生成的系统提示词（严格证据约束）。
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
    "一般来说",
    "通常来说",
    "根据一般知识",
    "根据常识",
    "根据通用知识",
    "建议联系管理员",
    "联系资料库管理员",
    "联系管理员",
    "最新版本可能有",
    "通常用于",
    "通常会",
    "一般会",
    "你可以参考",
    "你可以查阅",
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
因此暂时无法确认。"
            .to_owned();
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
        "没有找到",
        "未找到",
        "未提及",
        "没有提到",
        "未出现",
        "不足以",
        "无法确认",
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
            question_shape: QuestionShape::Description,
            requires_project_context: false,
            requires_entity_items: false,
            target: QueryTarget::default(),
            secondary_target: None,
            content_query: None,
            filters: Default::default(),
            requires_document_resolution: true,
            requires_full_document: false,
            structure_enumeration: false,
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
        // 存在性形态来自 LLM Parser 的 question_shape（不再是问题关键词判定）
        let mut p = plan(QueryIntent::DocumentQa, QueryOperation::Qa);
        p.question_shape = QuestionShape::BooleanExistence;
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
        // 形态由 LLM Parser 的 question_shape 决定（不再是关键词标记）
        let mut list_plan = qa.clone();
        list_plan.question_shape = QuestionShape::List;
        assert_eq!(
            classify_answer_shape("我的简历里面有哪些项目", &list_plan),
            AnswerShape::List
        );
        let mut fact_plan = qa.clone();
        fact_plan.question_shape = QuestionShape::Fact;
        assert_eq!(
            classify_answer_shape("合同金额是多少", &fact_plan),
            AnswerShape::FactLookup
        );
        assert_eq!(
            classify_answer_shape(
                "我的简历主要写了什么？",
                &plan(QueryIntent::DocumentSummary, QueryOperation::Summary)
            ),
            AnswerShape::Summary
        );
        assert_eq!(
            classify_answer_shape(
                "随便问点什么",
                &plan(QueryIntent::CompareDocuments, QueryOperation::Compare)
            ),
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
        // 依据 LLM Parser 的 requires_project_context + question_shape
        let mut project = plan(QueryIntent::DocumentQa, QueryOperation::Qa);
        project.question_shape = QuestionShape::BooleanExistence;
        project.requires_project_context = true;
        assert!(existence_requires_project_context(
            "我以前有没有做过 Agent 项目？",
            &project
        ));
        // 「资料里有没有提到 X」LLM 不判需要项目语境 → false
        let mut mention = plan(QueryIntent::DocumentQa, QueryOperation::Qa);
        mention.question_shape = QuestionShape::BooleanExistence;
        assert!(!existence_requires_project_context(
            "我的文件里有没有提到 Transformer？",
            &mention
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
        // LLM Parser 输出：存在性断言 + 需要项目语境证据
        let mut p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        p.question_shape = QuestionShape::BooleanExistence;
        p.requires_project_context = true;
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
    fn chinese_content_terms_partition_frame_and_topic() {
        // 通用抽取：句式填充被拆走，主题性中文词保留。
        let terms = extract_chinese_content_terms("我的资料里有没有提到火星移民");
        assert!(terms.contains(&"火星".to_owned()));
        assert!(terms.contains(&"移民".to_owned()));
        // 句式填充字不残留为内容词（如「到火」这类跨词噪音）
        assert!(!terms.iter().any(|t| t.contains('到')));
        // 数据库概念词保留
        let terms = extract_chinese_content_terms("事务的隔离级别有哪些");
        assert!(terms.contains(&"事务".to_owned()));
        assert!(terms.contains(&"隔离".to_owned()));
        // 泛化词被丢弃（不参与判定）：计划
        assert!(!extract_chinese_content_terms("火星移民计划").contains(&"计划".to_owned()));
    }

    #[test]
    fn chinese_no_evidence_topic_is_not_answerable() {
        // 防编造核心：问题主题（火星/移民）在证据里完全不存在 → 必须拒答，
        // 否则生成模型会拿无关证据杜撰该主题的内容。
        let mut p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        p.question_shape = QuestionShape::BooleanExistence;
        let unrelated = evidence(
            "某巴士维修连锁公司开发信息系统，包含通用信息查询、药品管理等功能模块。",
            None,
        );
        let input = AnswerabilityInput {
            question: "我的资料里有没有提到火星移民计划",
            content_query: Some("火星移民计划"),
            plan: &p,
            evidence: &[unrelated],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::NotAnswerable);
        // m18 复合词门控：整词「火星移民计划」完全无证据 → 直接 composite_mismatch
        assert!(verdict.reason.starts_with("composite_mismatch"));
        assert!(verdict.missing_entities.contains(&"火星移民计划".to_owned()));
    }

    #[test]
    fn chinese_on_topic_evidence_is_answerable() {
        // 证据含查询中文主题词 → 放行（不过度拦截）
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let on_topic = evidence("事务具有原子性、一致性、隔离性和持久性。", None);
        let input = AnswerabilityInput {
            question: "事务的隔离级别是什么意思",
            content_query: Some("事务 隔离"),
            plan: &p,
            evidence: &[on_topic],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::Answerable);
    }

    #[test]
    fn chinese_gate_yields_to_ascii_anchor() {
        // 查询有 ASCII 锚点命中（RAG）时，中文门控不重复拦截
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let ascii = evidence("RAG 通过先检索再生成缓解幻觉，检索增强生成是核心。", None);
        let input = AnswerabilityInput {
            question: "RAG的核心思想是什么",
            content_query: Some("RAG 核心"),
            plan: &p,
            evidence: &[ascii],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::Answerable);
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
        assert!(
            claim_subject_mismatch(
                "RAG 通过先检索再生成缓解幻觉。",
                &["RAG（检索增强生成）先检索资料再生成回答。"],
            )
            .is_none()
        );
        // 负向 claim 跳过（其证据本来就不含被问实体）
        assert!(
            claim_subject_mismatch(
                "资料中没有找到 LangGraph 相关的项目记录。",
                &["项目经历：基于 RAG 的问答系统。"],
            )
            .is_none()
        );
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

    #[test]
    fn container_word_does_not_ground_specific_artifact() {
        // [26] 错误肯定：问「体检报告」，证据只有「健康状态报告 / 个人成绩报告」。
        // 「报告」是容器词，已在 GENERIC_BIGRAMS 过滤，不得凭它单独判定「体检报告」存在。
        let mut p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        p.question_shape = QuestionShape::BooleanExistence;
        let input = AnswerabilityInput {
            question: "我的资料里有我的体检报告吗？",
            content_query: Some("体检报告"),
            plan: &p,
            evidence: &[evidence("健康状态报告 个人成绩报告", None)],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::NotAnswerable);
        // m18 复合词门控：整词「体检报告」未落地且半词「报告」是容器词 → composite_mismatch
        assert!(verdict.reason.starts_with("composite_mismatch"));
        // 真实「体检报告」文本应放行（主题词「体检」落地）
        let genuine = evidence("体检报告：体温、血压、血常规等检查结果。", None);
        let input = AnswerabilityInput {
            question: "我的资料里有我的体检报告吗？",
            content_query: Some("体检报告"),
            plan: &p,
            evidence: &[genuine],
        };
        assert_eq!(
            evaluate_answerability(&input).status,
            AnswerabilityStatus::Answerable
        );
    }

    #[test]
    fn explicit_year_in_existence_query_must_land_in_evidence() {
        // [28] 错误肯定：问「2024 年数据库系统工程师真题」，证据只有 2012 年真题，
        // 年份未落地 → 存在性命中不得成立。
        let mut p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        p.question_shape = QuestionShape::BooleanExistence;
        let input = AnswerabilityInput {
            question: "资料里有 2024 年数据库系统工程师真题吗？",
            content_query: Some("2024年数据库系统工程师真题"),
            plan: &p,
            evidence: &[evidence("2012年上半年数据库系统工程师考试上午真题（参考答案）", None)],
        };
        let verdict = evaluate_answerability(&input);
        assert_eq!(verdict.status, AnswerabilityStatus::NotAnswerable);
        assert!(verdict.reason.starts_with("year_mismatch:2024"));

        // 年份确实落地 → 放行
        let on_year = evidence("2024年数据库系统工程师真题（参考答案）", None);
        let input = AnswerabilityInput {
            question: "资料里有 2024 年数据库系统工程师真题吗？",
            content_query: Some("2024年数据库系统工程师真题"),
            plan: &p,
            evidence: &[on_year],
        };
        assert_eq!(
            evaluate_answerability(&input).status,
            AnswerabilityStatus::Answerable
        );
    }

    #[test]
    fn ascii_anchor_keeps_existence_question_answerable() {
        // [18] 回归防线：问「SCI 论文投稿内容有没有」有 ASCII 强实体 "SCI"
        // 落在证据里 → 走既有实体规则放行，中文门控不得误伤（即便证据没逐字
        // 命中「相关的内容」）。
        let p = plan(QueryIntent::LibraryQa, QueryOperation::Qa);
        let input = AnswerabilityInput {
            question: "我的资料里有 SCI 论文投稿相关的内容吗？",
            content_query: Some("SCI 论文投稿相关的内容"),
            plan: &p,
            evidence: &[evidence("SCI 论文智能辅助投稿系统，面向科研作者的投稿准备与合规辅助平台", None)],
        };
        assert_eq!(
            evaluate_answerability(&input).status,
            AnswerabilityStatus::Answerable
        );
    }
}
