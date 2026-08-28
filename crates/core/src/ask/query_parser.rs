//! Query Parser：把 LOCAL（或已解析的 AMBIGUOUS）请求解析成结构化 [`QueryPlan`]。
//!
//! 核心约束：把「目标对象」与「目标对象内部查询的内容」严格拆开——
//! 「我的简历里有没有 LangGraph」必须解析为
//! `target.document_type = resume` + `content_query = "LangGraph"`，
//! 禁止把「我的 简历 LangGraph」整体作为检索关键词。
//!
//! Prompt 与解析器集中在本模块；解析失败由调用方回退（不中断问答）。

use crate::AskMessage;
use crate::ask::query_normalize::strip_target_stop_phrases;
use crate::ask::query_plan::{
    QueryIntent, QueryOperation, QueryPlan, QueryTarget, QuestionShape, SourceIntent,
};
use crate::ask::source_router::{
    SourceRouting, apply_ambiguous_override, apply_capability_override,
};
use crate::contracts::DocumentType;
use crate::knowledge::fold_recent_history;

/// 文档类型的中文名（用于从「我的简历里有没有写 X」构造 scope 引导词）。
fn document_type_cn_name(document_type: DocumentType) -> &'static str {
    match document_type {
        DocumentType::Resume => "简历",
        DocumentType::Contract => "合同",
        DocumentType::Invoice => "发票",
        DocumentType::Paper => "论文",
        DocumentType::ProjectDocument => "项目文档",
        DocumentType::Meeting => "会议纪要",
        DocumentType::LearningMaterial => "课件",
        DocumentType::Certificate => "证书",
        DocumentType::Report => "报告",
        DocumentType::Spreadsheet => "表格",
        DocumentType::Other => "",
    }
}

/// Query Parser 的输出 JSON Schema（llama.cpp 侧约束解码）。
pub fn query_parser_schema() -> serde_json::Value {
    let target_schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "reference", "document_type", "document_name", "precise_named_document",
            "owner", "entity_type", "entity_name"
        ],
        "properties": {
            "reference": {"type": ["string", "null"], "maxLength": 200},
            "document_type": {"type": ["string", "null"]},
            "document_name": {"type": ["string", "null"], "maxLength": 200},
            "precise_named_document": {"type": "boolean"},
            "owner": {"type": ["string", "null"], "maxLength": 50},
            "entity_type": {"type": ["string", "null"], "maxLength": 100},
            "entity_name": {"type": ["string", "null"], "maxLength": 200}
        }
    });
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "source", "intent", "operation", "target",
            "content_query", "filters",
            "question_shape", "requires_project_context",
            "requires_entity_items",
            "requires_document_resolution", "requires_full_document", "confidence"
        ],
        "properties": {
            "source": {"type": "string", "enum": ["local", "general", "ambiguous"]},
            "intent": {
                "type": "string",
                "enum": [
                    "document_find", "document_qa", "document_summary",
                    "library_qa", "multi_document_qa", "compare_documents", "general_chat"
                ]
            },
            "operation": {
                "type": "string",
                "enum": ["find", "qa", "summary", "extract", "compare"]
            },
            "question_shape": {
                "type": "string",
                "enum": [
                    "boolean_existence", "list", "location",
                    "summary", "fact", "description"
                ]
            },
            "requires_project_context": {"type": "boolean"},
            "requires_entity_items": {"type": "boolean"},
            "target": target_schema,
            "secondary_target": {
                "type": ["object", "null"],
                "additionalProperties": false,
                "required": [
                    "reference", "document_type", "document_name", "precise_named_document",
                    "owner", "entity_type", "entity_name"
                ],
                "properties": {
                    "reference": {"type": ["string", "null"], "maxLength": 200},
                    "document_type": {"type": ["string", "null"]},
                    "document_name": {"type": ["string", "null"], "maxLength": 200},
                    "precise_named_document": {"type": "boolean"},
                    "owner": {"type": ["string", "null"], "maxLength": 50},
                    "entity_type": {"type": ["string", "null"], "maxLength": 100},
                    "entity_name": {"type": ["string", "null"], "maxLength": 200}
                }
            },
            "content_query": {"type": ["string", "null"], "maxLength": 300},
            "filters": {
                "type": ["object", "null"],
                "additionalProperties": false,
                "properties": {
                    "time": {"type": ["string", "null"], "maxLength": 50},
                    "file_type": {"type": ["string", "null"], "maxLength": 50},
                    "path": {"type": ["string", "null"], "maxLength": 300}
                }
            },
            "requires_document_resolution": {"type": "boolean"},
            "requires_full_document": {"type": "boolean"},
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        }
    })
}

/// 构建 Query Parser prompt。
/// system 说明角色与目标；user 含历史（仅作理解上文的参考）、
/// 任务拆解说明、示例与当前输入。
pub fn query_parser_prompt(question: &str, history: &[AskMessage]) -> (String, String) {
    let system = "你是翻翻的「本地资料查询解析器」。你的任务是理解：1. 用户想对什么对象操作；2. 用户真正想知道什么；3. 是否明确指定或隐含指定某个文档；4. 是否需要阅读整份文档。不要直接回答问题，不要生成检索结果，只输出规定 JSON。"
        .into();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!(
            "【对话历史】以下是最近 5 条对话的历史记录，仅作理解上文的参考，不是解析对象，严禁复读或引用其中的任何内容：\n{folded}\n\n"
        ));
    }
    user.push_str(&format!(
        r#"【当前输入】只解析下面这一句用户刚刚说的话：
用户：{question}

【任务】把这句话拆成「目标对象」（target）与「目标对象内部查询的内容」（content_query）两个独立字段。

规则：
- 「我的简历里有没有 LangGraph」→ 绝不能把「我的简历 LangGraph」整体作为搜索关键词；必须拆成 target.document_type = "resume"、target.owner = "self"、content_query = "LangGraph"。
- 明确提到某个文档对象（我的简历/那个大模型材料/这份合同）时 requires_document_resolution 必须为 true（先定位文件，再在文件内检索），禁止把它和内容词混成一个检索词。
- 「我的简历里有没有写 X」「我的简历里有没有提到 X」是存在性问句 → operation = "qa"（回答「有/没有」），绝不是 extract。
- 「我以前有没有做过 Agent 项目？」是存在性问句 → operation = "qa"；只有明确要清单（「把项目名称提取出来」「列出所有项目」）才是 extract。
- 「我的资料里是怎么介绍 RAG 的？」「我的文件里有没有提到 Transformer？」→ LIBRARY_QA；content_query 只填真正的内容词（RAG / Transformer），「我的资料里是怎么介绍的」是 scope 引导语，不算内容。
- 明确限定在某个文档对象内部查询（我的**简历**里找 X / 那份**合同**里有没有 Y）→ DOCUMENT_QA（单文档/单类型内检索），target 必须填该对象、requires_document_resolution = true；**禁止**把「我的简历」这种目标限定整句丢进 content_query 或误判为全库 LIBRARY_QA。
- 「我的简历有什么内容」「我的简历主要写了什么」→ DOCUMENT_SUMMARY（整文摘要），不是普通关键词检索；content_query 为 null，requires_full_document = true。
- 「我的简历在哪里」「我电脑里有XX文件吗」「哪个文件是XX」「帮我找一下XX」「XX文件是哪个」→ DOCUMENT_FIND（用户只关心「某个文件在不在 / 是哪个 / 在哪」，不关心文件内容）：target 填目标对象、content_query = null、question_shape = "location"、requires_document_resolution = true。区分标准：关心「文件在不在 / 是哪个 / 在哪」→ find；关心「文件里提到了什么 / 有没有讲 X」→ document_qa / library_qa。
- 「所有文件里哪些提到了 RAG」「我的资料里有没有 LangGraph」→ LIBRARY_QA；target 为空对象，content_query = 实际检索词。
- 「比较我两个简历版本」→ COMPARE_DOCUMENTS；target 填第一个版本（如「简历」+ owner "self"），secondary_target 填第二个版本（如 reference「第二个版本」），requires_document_resolution = true；分不清先后时 target 填「更早/更常见」的那个，secondary_target 填另一个。
- 没有明确文件目标的全库检索 → LIBRARY_QA，requires_document_resolution = false。
- 非比较类请求（document_qa / document_summary / library_qa / multi_document_qa 等）secondary_target 一律填 null。
- source：LOCAL 请求填 "local"；由会话上下文恢复的 AMBIGUOUS 请求填 "ambiguous"。
- intent / operation 必须从枚举值中选：intent ∈ document_find / document_qa / document_summary / library_qa / multi_document_qa / compare_documents / general_chat；operation ∈ find / qa / summary / extract / compare。
- document_type ∈ resume / contract / invoice / paper / project_document / meeting / learning_material / certificate / report / spreadsheet / other，不确定填 null。
- question_shape 判断用户期望的回答形态：有没有/是否…过 → "boolean_existence"；有哪些/列一下 → "list"；在哪/第几页 → "location"；主要写了什么/总结 → "summary"；多少/几号/谁（精确值） → "fact"；其余（是什么/怎么介绍/描述） → "description"。
- requires_project_context：只有「是否做过/参与过某类项目或经历」这类存在性断言才填 true（如「我以前有没有做过 Agent 项目？」→ true），其余一律 false。
- requires_entity_items：只有 EXTRACT 清单的条目必须是「实体/名称形式」（如「有哪些项目」→ 条目是项目名称，不是整段技术描述）时才填 true；条目可以是事实片段/描述（如日期、条款、联系方式）时填 false；非 extract 一律 false。
- 明确说「我的」时 owner 填 "self"，否则 null。
- 用户**精确点名**某份文档时（给出完整标题或文件名，如「周晨博20212P2002《专业实习》课程实习总结报告.docx」）：target.document_name 必须填该完整标题、precise_named_document = true、requires_document_resolution = true；凭指代/类型定位（「我的简历」「那份合同」）时 precise_named_document = false。precise_named_document 只表达「用户精确点名」这一语义判定，不得把它当关键词收集。
- 不确定的字段填 null，不要编造。

【示例】
用户：我的简历里有哪些项目？
输出：{{"source":"local","intent":"document_qa","operation":"extract","target":{{"reference":"我的简历","document_type":"resume","document_name":null,"owner":"self","entity_type":null,"entity_name":null}},"content_query":"项目经历","filters":{{"time":null,"file_type":null,"path":null}},"question_shape":"list","requires_project_context":false,"requires_entity_items":true,"requires_document_resolution":true,"requires_full_document":false,"confidence":0.96}}

用户：我的简历有什么内容？
输出：{{"source":"local","intent":"document_summary","operation":"summary","target":{{"reference":"我的简历","document_type":"resume","document_name":null,"owner":"self","entity_type":null,"entity_name":null}},"content_query":null,"filters":{{"time":null,"file_type":null,"path":null}},"question_shape":"summary","requires_project_context":false,"requires_document_resolution":true,"requires_full_document":true,"confidence":0.95}}

用户：我的资料里有没有提到 LangGraph？
输出：{{"source":"local","intent":"library_qa","operation":"qa","target":{{"reference":null,"document_type":null,"document_name":null,"owner":null,"entity_type":null,"entity_name":null}},"content_query":"LangGraph","filters":{{"time":null,"file_type":null,"path":null}},"question_shape":"boolean_existence","requires_project_context":false,"requires_document_resolution":false,"requires_full_document":false,"confidence":0.9}}

用户：我以前有没有做过 Agent 项目？
输出：{{"source":"local","intent":"document_qa","operation":"qa","target":{{"reference":"我的经历","document_type":null,"document_name":null,"owner":"self","entity_type":null,"entity_name":null}},"content_query":"Agent 项目","filters":{{"time":null,"file_type":null,"path":null}},"question_shape":"boolean_existence","requires_project_context":true,"requires_document_resolution":false,"requires_full_document":false,"confidence":0.88}}

用户：比较我两个简历版本有什么不同？
输出：{{"source":"local","intent":"compare_documents","operation":"compare","target":{{"reference":"我的简历","document_type":"resume","document_name":null,"precise_named_document":false,"owner":"self","entity_type":null,"entity_name":null}},"secondary_target":{{"reference":"第二个版本","document_type":null,"document_name":null,"precise_named_document":false,"owner":null,"entity_type":null,"entity_name":null}},"content_query":"有什么不同","filters":{{"time":null,"file_type":null,"path":null}},"question_shape":"description","requires_project_context":false,"requires_entity_items":false,"requires_document_resolution":true,"requires_full_document":false,"confidence":0.92}}

用户：概括一下周晨博20212P2002《专业实习》课程实习总结报告.docx都在讲什么？
输出：{{"source":"local","intent":"document_summary","operation":"summary","target":{{"reference":null,"document_type":null,"document_name":"周晨博20212P2002《专业实习》课程实习总结报告.docx","precise_named_document":true,"owner":null,"entity_type":null,"entity_name":null}},"content_query":null,"filters":{{"time":null,"file_type":null,"path":null}},"question_shape":"summary","requires_project_context":false,"requires_entity_items":false,"requires_document_resolution":true,"requires_full_document":true,"confidence":0.98}}

用户：我电脑里有数据库系统工程师2020年上午的真题吗
输出：{{"source":"local","intent":"document_find","operation":"find","target":{{"reference":"我的资料","document_type":"learning_material","document_name":"数据库系统工程师2020年上午的真题","precise_named_document":true,"owner":"self","entity_type":null,"entity_name":null}},"content_query":null,"filters":{{"time":"2020","file_type":null,"path":null}},"question_shape":"location","requires_project_context":false,"requires_entity_items":false,"requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}}

用户：找一下人工智能面试宝典这份资料
输出：{{"source":"local","intent":"document_find","operation":"find","target":{{"reference":"我的资料","document_type":"learning_material","document_name":"人工智能面试宝典","precise_named_document":true,"owner":"self","entity_type":null,"entity_name":null}},"content_query":null,"filters":{{"time":null,"file_type":null,"path":null}},"question_shape":"location","requires_project_context":false,"requires_entity_items":false,"requires_document_resolution":true,"requires_full_document":false,"confidence":0.88}}

用户：2019年数据库下午的真题文件是哪个
输出：{{"source":"local","intent":"document_find","operation":"find","target":{{"reference":null,"document_type":"learning_material","document_name":null,"precise_named_document":null,"owner":null,"entity_type":null,"entity_name":null}},"content_query":null,"filters":{{"time":"2019","file_type":"pdf","path":null}},"question_shape":"location","requires_project_context":false,"requires_entity_items":false,"requires_document_resolution":true,"requires_full_document":false,"confidence":0.85}}

只输出符合 JSON Schema 的对象，不要输出 Markdown、代码块或解释。"#,
        question = question.trim()
    ));
    (system, user)
}

/// 静态 scope 引导短语（长短语在前）。「我的资料里是怎么介绍 RAG 的？」中
/// 这段引导不算检索内容，剥离后才是真正的 content_query。
const CONTENT_SCOPE_PREFIXES: &[&str] = &[
    "我的资料里是怎么介绍",
    "我的文件里是怎么介绍",
    "我的知识库里是怎么介绍",
    "我的资料里有没有提到",
    "我的文件里有没有提到",
    "我的知识库里有没有提到",
    "我的资料里有没有讲",
    "我的文件里有没有讲",
    "我的资料里有没有写",
    "我的文件里有没有写",
    "我的资料里怎么介绍",
    "我的文件里怎么介绍",
    "我的资料里介绍了",
    "我的文件里介绍了",
    "我的资料里讲了",
    "我的文件里讲了",
    "我的资料里提到",
    "我的文件里提到",
    "我的资料里写了",
    "我的文件里写了",
    "我的资料里",
    "我的文件里",
    "我的知识库里",
    "我的文档里",
    "资料里",
    "文件里",
    "知识库里",
    "文档里",
    "我以前有没有做过",
    "以前有没有做过",
    "之前有没有做过",
];

/// 剥离 content_query 头部的 scope 引导短语与尾部疑问词：
/// 「我的资料里是怎么介绍 RAG 的？」→「RAG」；「我以前有没有做过 Agent 项目？」
/// →「Agent 项目」。target 相关的动态引导（我的简历里有没有写）同样参与。
/// 剥离失败（剩余为空）返回原句。
pub fn strip_content_scope_prefix(query: &str, plan: &QueryPlan) -> String {
    let mut prefixes: Vec<String> = CONTENT_SCOPE_PREFIXES
        .iter()
        .map(|p| p.to_string())
        .collect();
    // 动态：target 给出的文档对象（reference/document_name/类型中文名）
    for raw in [
        plan.target.reference.as_deref(),
        plan.target.document_name.as_deref(),
        plan.target.document_type.map(document_type_cn_name),
    ]
    .into_iter()
    .flatten()
    {
        let target = raw.trim().trim_start_matches("我的").trim().to_owned();
        if target.is_empty() || target.chars().count() > 12 {
            continue;
        }
        for suffix in [
            "里有没有写",
            "里有没有提到",
            "里有没有讲",
            "里怎么介绍",
            "里有没有",
            "里怎么",
            "里",
        ] {
            prefixes.push(format!("我的{target}{suffix}"));
            prefixes.push(format!("{target}{suffix}"));
        }
    }
    prefixes.sort_by_key(|prefix| std::cmp::Reverse(prefix.chars().count()));

    let trimmed = query.trim();
    for prefix in prefixes {
        if let Some(remainder) = trimmed.strip_prefix(&prefix) {
            let remainder = remainder
                .trim()
                .strip_prefix("关于")
                .unwrap_or(remainder.trim())
                .trim()
                .trim_start_matches(['是', '有', '：', ':', ' '])
                .trim_end_matches(|c: char| {
                    matches!(
                        c,
                        '？' | '?'
                            | '！'
                            | '!'
                            | '。'
                            | '、'
                            | '，'
                            | ' '
                            | '的'
                            | '吗'
                            | '呢'
                            | '啊'
                            | '了'
                    )
                })
                .trim()
                .to_owned();
            if !remainder.is_empty() {
                return remainder;
            }
        }
    }
    trimmed.to_owned()
}

/// 强对比词（1.4 COMPARE 兜底用），匹配时按长度降序保证最长优先
/// （「有何异同」先于「有何」；「有什么区别」先于「区别」）。
const COMPARE_MARKERS: &[&str] = &[
    "有何异同",
    "有什么区别",
    "区别是什么",
    "有什么不同",
    "有何区别",
    "哪个更难",
    "哪个简单",
    "哪个好",
    "哪个更",
    "对比",
    "比较",
    "分别",
];

/// 结构枚举问句整词短语：「某份文档/教材/手册分了几章、讲了哪几章、有哪些章节、
/// 章节目录」这类要的是**单份文档内部章节结构**（大纲标题串），区别于整文内容
/// 摘要（「写了什么内容/总结一下」，要的是逐节内容概览）。与 1.3 summary_markers
/// 中的结构分支、以及 1.5 asks_document_structure 同源：均用「章节/哪几章」整词
/// 短语，不收录裸「章节/大纲/目录」单字（它们在文件定位语境会误伤真正的找文件
/// 问句）。通用语言结构判定，不针对任何具体文件/关键词/case。
const STRUCTURE_ENUMERATION_MARKERS: &[&str] = &[
    "哪些章节", "哪几章", "分了几章", "讲了几章", "讲了哪几章", "分了哪些章",
    "多少章", "章节目录",
];

/// 「容器/装载」类名词：指文档的整体类别或装载单元（手册/讲义/逐字稿/清单…），
/// 本身不是能定位某小节的检索词。当**枚举/计数**问句（几个/哪些/多少）的内容查询
/// 完全由这类词构成（如「整理了几个项目的*逐字稿*」→ content="逐字稿"），说明
/// 用户要的是**通读全文枚举出所有内容项**，而不是拿容器词做 chunk 局部检索——
/// 后者召回必然为空而被门控拒答（no_evidence）。收录常见文档类别词与装载单元词，
/// 作为「纯容器 → 整文概述」兜底（[`finalize_query_plan`] 6.5）的**通用**判定，不针对
/// 任何具体文件/关键词/case。与 answer_gate 的 GENERIC_BIGRAMS 保留同类词的精神
/// 一致：容器词不单独承载主题。
const CONTENT_CONTAINER_NOUNS: &[&str] = &[
    "逐字稿", "手册", "讲义", "教程", "教案", "笔记", "指南", "规范", "标准",
    "报告", "说明", "方案", "计划", "清单", "列表", "目录", "大纲", "章节", "小节",
    "内容", "资料", "文档", "文件", "记录", "汇总", "总结", "材料",
];

/// 判定问句是否是「跨文档对比」：强对比词 + 双文档结构信号**同时**满足。
/// 概念对比（「视图和基本表有什么区别」）两侧是概念名词、不是文档对象，
/// 不满足双文档结构，因此不会误升级（保持 document_qa）。通用语言结构判定，
/// 不针对任何具体文件/关键词/case。
fn has_compare_intent(question: &str) -> bool {
    let has_marker = COMPARE_MARKERS.iter().any(|marker| question.contains(marker));
    has_marker && has_dual_document_targets(question)
}

/// 双文档结构信号：问句里出现两组可作文档区分的单元——两个不同 4 位年份
/// （「2019年和2020年上午的数据库真题」）、上午+下午/上卷+下卷/上册+下册
/// （「2020上午题和下午题」）、或「两份/两个版本/两个文件」这类显式双份。
fn has_dual_document_targets(question: &str) -> bool {
    // 两个不同 4 位年份（如 2019 与 2020）→ 两个版本的同类文件。
    let mut years = std::collections::HashSet::new();
    let bytes = question.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && let Some(year) = question[i..i + 4].parse::<u32>().ok()
            && (1900..=2099).contains(&year)
        {
            years.insert(year);
        }
        i += 1;
    }
    if years.len() >= 2 {
        return true;
    }
    // 时段/册次对比（上/下午、上/下卷、上/下篇、上/下册）。
    if (question.contains("上午") && question.contains("下午"))
        || (question.contains("上卷") && question.contains("下卷"))
        || (question.contains("上篇") && question.contains("下篇"))
        || (question.contains("上册") && question.contains("下册"))
    {
        return true;
    }
    // 显式「两份/两个版本」等双文档表达。
    question.contains("两份")
        || question.contains("两个版本")
        || question.contains("两个文件")
        || question.contains("两个文档")
        || question.contains("两份文件")
        || question.contains("两份资料")
}

/// 判断问句是否含「≥4 字」的主题描述（资料枚举定位用）。
///
/// 与「1.6 资料枚举定位问句 → DOCUMENT_FIND」共用同一组资料集合宾语词
/// （资料/文件/真题…）与枚举/存在定位词（有哪些/哪几年/多少份…）：去掉这些
/// 词后若仍留有 ≥4 字内容，说明用户在定位**某类**具体资料，而非空泛地问
/// 「有哪些资料」。纯通用语言结构判定，不针对任何具体文件/关键词/case。
fn subject_desc_length(question: &str) -> bool {
    const NOUNS: &[&str] = &[
        "资料", "文件", "真题", "手册", "教材", "教程", "文档", "题库", "试卷",
    ];
    const ENUMERATE: &[&str] = &[
        "有哪些", "哪些文件", "哪些资料", "哪几份", "有哪几", "哪几年", "多少份",
        "查得到", "能查到", "有多少", "有几份", "哪些真题", "哪些手册", "有哪些",
    ];
    let mut remaining = question.to_string();
    for word in NOUNS.iter().chain(ENUMERATE.iter()) {
        remaining = remaining.replace(word, "");
    }
    // 用字符数（非字节）统计，避免中文按 UTF-8 字节翻倍误判。
    remaining.chars().count() >= 4
}

/// 判断问句是否在「枚举资料集合」：枚举词（有哪些/哪些/哪几…）与资料集合名词
/// （资料/文件/真题/手册/教材/教程/文档/题库/试卷）构成**紧邻宾语搭配**。
///
/// 泛泛的「有哪些 XX」若枚举的是正文内容（来源/原因/步骤/原则…）而非资料文件，
/// 其宾语不是资料集合名词，不误判成文件定位；只有枚举宾语确为资料集合名词，或
/// 出现「哪几年/哪几份/多少份/有几份」这类天然指向多份资料文件的精确枚举词，
/// 才判定为文件枚举。纯通用语言结构判定，不针对任何具体文件/关键词/case。
fn has_material_enumerate_collocation(question: &str) -> bool {
    const MATERIALS: &[&str] = &[
        "资料", "文件", "真题", "手册", "教材", "教程", "文档", "题库", "试卷",
    ];
    const ENUMERATE: &[&str] = &["有哪些", "有哪几", "哪些", "哪几份", "哪几年"];
    // 精确「文件集合」枚举词：天然指多份资料文件，命中即视为文件枚举。
    const FILE_PRECISE: &[&str] = &["哪几年", "哪几份", "多少份", "有几份"];

    if FILE_PRECISE.iter().any(|word| question.contains(word)) {
        return true;
    }
    for (idx, _ch) in question.char_indices() {
        for enumerate in ENUMERATE {
            if !question[idx..].starts_with(enumerate) {
                continue;
            }
            let end = idx + enumerate.len();
            // 枚举词前 ≤6 字内是否有资料集合名词（「备考资料有哪些」→「资料」在前）。
            // 必须取**原始顺序**窗口，反转会破坏多字词（「资料」→「料资」）匹配。
            let before_start = question[..idx]
                .char_indices()
                .rev()
                .nth(6)
                .map(|(i, _)| i)
                .unwrap_or(0);
            let before_window = &question[before_start..idx];
            if MATERIALS.iter().any(|mat| before_window.contains(mat)) {
                return true;
            }
            // 枚举词后 ≤8 字内是否有资料集合名词（「有哪些资料」→「资料」在后）。
            let after_head: String = question[end..].chars().take(8).collect();
            if MATERIALS.iter().any(|mat| after_head.contains(mat)) {
                return true;
            }
            break; // 该位置仅匹配首个枚举词，避免同一位置重复扫描。
        }
    }
    false
}

/// 是否「整库主题/类别分布占比」问句。用于把「你收的这些资料，偏数据库方向的
/// 多一些，还是偏大模型方向的多一些呀」「你收的资料里，考试类的跟开发手册类的，
/// 大概是个几比几的占比呀」这类**整库**占比问句归入库概览（LibraryOverview 只读
/// 主题分布，不触发正文检索），避免 0.6B/2B 把它误判成 document_qa/content-QA 后
/// 落入宽 scope RAG 返回无关题目碎片。
///
/// 判定为高精度的通用语言结构，不针对任何具体文件/主题/case，且**必须同时满足**：
///   1. 非指向某一份具体文档正文（排除「这份/那本/那段…」定指单文档所指，这类是
///      文档内容 QA 而非整库分布）；
///   2. 整库收藏措辞（资料/收的/收着/手头/这边/库里/知识库/整份…），排除「这篇
///      特定文本内容」误解——只含「论文/手册」而不含整库措辞的不会命中；
///   3. 分布/占比比较词（占比/比例/几比几/哪种多/多一些/更多/…，或「偏…方向…」句式）。
fn has_theme_distribution_intent(question: &str) -> bool {
    let question = question.trim();
    if question.is_empty() {
        return false;
    }
    // 1) 指向某一份具体文档正文（定指单文档所指），不是整库分布 → 直接排除。
    const DEFINITE_DOC: &[&str] = &[
        "这份", "那份", "这篇", "那篇", "这本", "那本", "该文档", "那段", "那一份",
        "那份文件", "那本手册", "你说的那个", "刚说的那个",
    ];
    if DEFINITE_DOC.iter().any(|word| question.contains(word)) {
        return false;
    }
    // 2) 整库收藏措辞：只有真正指向「整个知识库/你收的资料」这类集合才谈得上分布。
    //    口语里用户常用「手上/手头/这边…这批/这些材料」指代整库集合，不止「资料/收的」。
    const COLLECTION: &[&str] = &[
        "资料", "收的", "收着", "收集", "收藏", "手头", "手上", "这边", "库里", "知识库",
        "整份", "全部文件", "这批", "这批材料",
    ];
    if !COLLECTION.iter().any(|word| question.contains(word)) {
        return false;
    }
    // 3) 分布/占比比较词（含「偏…方向…」句式）。
    //    口语里同一分布语义常用口语连词「的」把方向与多寡连起来（「哪些方向的
    //    多呀」），也常用「轻重/多少/谁多谁少」指方向间占比；此外用户会直接问
    //    「哪一类占得最多/最少」「占比最高/最低」这类**极值**比较，故词表同时
    //    覆盖占比、多寡、偏方向、以及「最多/最少/最高/最低」极值措辞，避免这些
    //    自然口语变体漏判又重新落回 document_qa。通用语言结构判定，不针对任何
    //    文件/主题/case。
    const DISTRIBUTION: &[&str] = &[
        "占比", "所占比例", "比例", "几比几", "几几开", "半对半", "对半分", "占多", "占得最多",
        "占得最少", "占的比重", "哪种多", "哪类多", "哪一类多", "哪类比较多", "偏多", "多还是少",
        "多一些", "更多", "最多", "最少", "占比最高", "占比最低", "比例最高", "比例最低", "哪边多",
        "哪个方向多", "哪些方向多", "哪个方向的多", "哪些方向的多", "哪个方向的", "哪些方向的",
        "占大头", "轻重",
    ];
    if DISTRIBUTION.iter().any(|word| question.contains(word)) {
        return true;
    }
    question.contains("偏") && question.contains("方向")
}

/// 提取对比问句的文档描述前缀：第一个强对比词**之前**的部分，再去掉尾随的
/// 「考察重点/考哪些/侧重/主要内容」等问句残余与疑问语气词。结果保留年份、
/// 上午下午与主题，供 Document Resolver 多信号定位两侧文件。
fn compare_document_reference(question: &str) -> String {
    let mut cut = question.len();
    for marker in COMPARE_MARKERS {
        if let Some(index) = question.find(marker) {
            cut = cut.min(index);
        }
    }
    let mut prefix = question[..cut].trim().to_owned();
    for tail in [
        "考察重点",
        "考查重点",
        "考试重点",
        "都考哪些",
        "考哪些内容",
        "侧重考",
        "分别侧重",
        "主要内容",
        "重点内容",
        "讲了什么",
        "有哪些内容",
    ] {
        if let Some(stripped) = prefix.strip_suffix(tail) {
            prefix = stripped.trim().to_owned();
        }
    }
    prefix
        .trim_end_matches(|c: char| matches!(c, '？' | '?' | '！' | '!' | '。' | '、' | '，' | ' ' | '和' | '与'))
        .trim()
        .to_owned()
}

/// 把对比问句的**成对文件描述**拆成两份独立的文件引用，供 Resolver 分别定位两侧。
///
/// 仅当问句里出现可构成「两份文件」的结构时拆分，否则返回 `(原串, None)`（语义上
/// 不是对比两侧，交给 Resolver 的 candidates top-2 处理）。拆分的两种通用形态：
///
/// 1. **两个不同 4 位年份**（「2019年和2020年上午的数据库真题」）：以第二个年份为界，
///    公共后缀（如「年上午的数据库真题」）同时补到两侧 → 左「2019年上午的数据库真题」、
///    右「2020年上午的数据库真题」；
/// 2. **同一年份的上/下午（或上/下卷等时段）**（「数据库系统工程师2020上午题和下午题」）：
///    以分隔符（和/与/、）前面的时段号切分 → 左「…2020上午题」、右「…2020下午题」。
///
/// 通用语言结构判定（年份成对 / 时段成对），不针对任何具体文件/关键词/case。拆分后
/// 每侧是独立、可单独定位的文件引用，避免「成对引用」作为单一 target 时被 Resolver
/// 模糊加权到泛化文档（如「2023年备考知识点」这种含主题但不含具体年份的文件）。
fn split_compare_references(reference: &str) -> (String, Option<String>) {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return (reference.to_owned(), None);
    }
    // 形态 1：两个不同 4 位年份。取最早与最晚出现的两个不同年份。
    let bytes = trimmed.as_bytes();
    let mut years = Vec::<(usize, u32)>::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && let Some(year) = trimmed[i..i + 4].parse::<u32>().ok()
            && (1900..=2099).contains(&year)
        {
            years.push((i, year));
            i += 4;
        } else {
            i += 1;
        }
    }
    years.dedup_by(|a, b| a.1 == b.1);
    if years.len() >= 2 {
        // 以第二个不同年份的起始位置切分：两侧共享「公共头 + 各自年份 + 公共尾」，
        // 年份之间的连接词（和/与）自然被丢弃。head 取第一个年份之前的前缀，
        // tail 取第二个年份之后的后缀（都是本文件里的字符串切片，多字节字符安全）。
        let (p1, _year1) = years[0];
        let (p2, _year2) = years[1];
        let head = &trimmed[..p1];
        let tail = &trimmed[p2 + 4..];
        let left = format!("{}{}{}", head, &trimmed[p1..p1 + 4], tail);
        let right = format!("{}{}{}", head, &trimmed[p2..p2 + 4], tail);
        return (left, Some(right));
    }
    // 形态 2：上/下午（或上/下卷、上/下篇、上/下册）成对。取分隔符（和/与/、）
    // 前后两个时段号分别构两侧。
    const SESSION_PAIRS: [(&str, &str); 4] = [
        ("上午", "下午"),
        ("上卷", "下卷"),
        ("上篇", "下篇"),
        ("上册", "下册"),
    ];
    for (first, second) in SESSION_PAIRS {
        if !trimmed.contains(first) || !trimmed.contains(second) {
            continue;
        }
        // 分隔符取位于 first 之后、second 之前的「和/与/、」。
        let first_at = trimmed.find(first).unwrap();
        let second_at = trimmed.find(second).unwrap();
        let split = [',', '和', '与', '、']
            .iter()
            .filter_map(|conn| trimmed[first_at..].find(*conn).map(|p| p + first_at))
            .filter(|&p| p > first_at && p < second_at)
            .min()
            .unwrap_or(second_at);
        let left = trimmed[..split].trim().to_owned();
        if left.is_empty() {
            continue;
        }
        let right = left.replacen(first, second, 1);
        return (left, Some(right));
    }
    (reference.to_owned(), None)
}

/// 「问指定文档内容」问句的**文件描述边界标记**：这些标记把问句切成
/// 「文件描述」+「内容问句」两段。覆盖存在性（里有没有/中有没有/中是否
/// 有…）与内容引用（里讲了/里提到/里涉及/里包含…）。刻意**不**收录裸
/// 「里有」/「里有哪」——「数据库里有哪几种索引类型」这类教科书概念题
/// 的「里有哪」不是文档边界，收录会把概念名词误当文件描述、错误锁定
/// 文件。通用语言结构判定，不针对任何具体文件/关键词/case。
const FILE_CONTENT_BOUNDARY_MARKERS: &[&str] = &[
    "里面有没有",
    "里有没有",
    "里有提到",
    "里有涉及",
    "里讲了",
    "里讲到",
    "里提到",
    "里涉及",
    "里包含",
    "里包括",
    "里写了",
    "里面有提到",
    "里面讲了",
    "里面提到",
    "中有没有",
    "中是否有",
    "内有没有",
    "内提到",
];

/// 解析后确定性修正（在 LLM 输出之上叠加的「明显逻辑规则」，不调阈值）：
/// - target 非空 → 强制 requires_document_resolution=true（目标对象必须经
///   Document Resolver 定位，禁止拿「我的简历」整句去全库检索）；
/// - find/qa/summary/compare 四类意图都有一组**通用问句词 + 目标对象**的
///   对称兜底：定位词结尾 → DocumentFind；强内容词且非定位结尾 → DocumentQa；
///   摘要词 + 单文档目标 + 无独立内容查询 → DocumentSummary
///   （requires_full_document）；强对比词 + 双文档结构 → CompareDocuments；
/// - content_query 剥掉 scope 引导（我的资料里…）与尾随疑问词；
/// - 回声防护：content_query/target 与历史任一用户问题相同 → 视为解析失败
///   （0.6B 复读历史问题时返回 None，调用方走确定性回退）。
///
/// 注意：兜底只覆盖「问句词 + 目标对象」这类高置信、与文档无关的形态判定，
/// 不针对任何具体文件/关键词/case；真正的语义理解仍交给 LLM Parser（Prompt
/// 已覆盖），这里仅在模型在 find/qa/summary/compare 之间漂移时做确定性收敛。
pub fn finalize_query_plan(
    mut plan: QueryPlan,
    question: &str,
    history: &[AskMessage],
) -> Option<QueryPlan> {
    // 0. 文件名描述恢复（在 target 判定**之前**）：parser（0.6B/2B）对
    //    「X里有没有提到Y／X里有没有涉及Z的内容」这类**问指定文档内容**的
    //    问句不稳定，可能丢掉点名文件，也可能只保留目标描述的一小部分。用
    //    [`FILE_CONTENT_BOUNDARY_MARKERS`] 里**最早出现**的标记把问句切成两段，
    //    标记之前的前缀就是用户指的文件描述，补回 target.reference 交
    //    Document Resolver 定位。条件是：
    //    - 前缀清洗后非空（纯「我的资料」这类泛指剥光后为空，保持现状）；
    //    - 当前 reference/document_name 都为空，或前缀比两者都更长/更完整。
    //      只补 reference、不覆盖 document_name，避免把模型给出的完整精确标题
    //      退化成残缺前缀。
    //    通用语言结构判定，不针对任何具体文件/关键词/case。
    let question_trim = question.trim();
    let boundary = FILE_CONTENT_BOUNDARY_MARKERS
        .iter()
        .filter_map(|marker| question_trim.find(marker).map(|pos| (marker, pos)))
        .min_by_key(|(_, pos)| *pos);
    if let Some((_marker, pos)) = boundary {
        let prefix = question_trim[..pos].trim().to_owned();
        let document_name = plan
            .target
            .document_name
            .as_deref()
            .map(str::trim)
            .unwrap_or("");
        let current_ref = plan
            .target
            .reference
            .as_deref()
            .map(str::trim)
            .unwrap_or("");
        let cleaned_prefix = strip_target_stop_phrases(&prefix);
        let current_ref = strip_target_stop_phrases(current_ref);
        let document_name = strip_target_stop_phrases(document_name);
        let current_target_len = current_ref
            .chars()
            .count()
            .max(document_name.chars().count());
        let more_complete = cleaned_prefix.chars().count() > current_target_len;
        if !cleaned_prefix.is_empty() && (current_target_len == 0 || more_complete) {
            plan.target.reference = Some(prefix);
        }
    }

    // 1. target 非空 → 强制走 Document Resolver。诊断报告 P0-1：白名单式判定
    //    （resolver_intents）会让「intent=library_qa 但 target 明确指到简历」时
    //    丢失目标对象——target 里显式的文档信息必须永远被尊重，不依赖模型把
    //    intent 恰好判成白名单内的类型。目标对象明确时：
    //    - requires_document_resolution 一律置真；
    //    - LibraryQa（全库泛指）若同时带 target 限定 → 归一化为 DocumentQa
    //      （单文档/单类型内检索），避免「在简历里找 RAG 项目」走全库检索。
    let target_non_empty = plan
        .target
        .reference
        .as_deref()
        .is_some_and(|v| !v.trim().is_empty())
        || plan
            .target
            .document_name
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty())
        || plan.target.document_type.is_some()
        || plan
            .target
            .entity_name
            .as_deref()
            .is_some_and(|v| !v.trim().is_empty());
    if target_non_empty {
        plan.requires_document_resolution = true;
        if plan.intent == QueryIntent::LibraryQa {
            plan.intent = QueryIntent::DocumentQa;
        }
    }
    // 1.1 文件定位问句 → DocumentFind（通用、文本锚定的稳定兜底）。schema 已把
    // location 形态定义为「在哪 / 第几页 / 是哪个 / 哪份」等定位语义，但 0.6B 模型
    // 对这些问句的 question_shape/intent 输出不稳定，故在此用**原始问题文本**的
    // 结尾标记做确定性兜底：以「在哪里 / 在哪 / 是哪份 / 哪一份 / 哪个文件 / 是
    // 哪一个 / 是哪个 / 在不在 / 找一下」等结尾的问句，本质是在问「某个文件在
    // 哪 / 是哪一份 / 有没有这份文件」，应走 find（文件定位）而非抽取式 QA。
    // 结束标记是高精度信号——正文问答通常不会以这些定位词收尾（「主键候选键
    // 的区别是什么」不以之结尾）；这是对模型在 find/qa 之间漂移的通用兜底，
    // 不针对任何具体文件/关键词/case。
    let location_enders: &[&str] = &[
        "是哪个文件", "哪一份", "在哪里", "是哪一个", "哪个文件", "是哪份", "哪份",
        "是哪个", "哪一个", "在哪", "在不在", "找一下文件", "找一下",
        // 找文件问句的常见变体结尾（以「文件里/资料」收尾的定位语义）。光靠模型
        // question_shape 不稳定，0.6B/2B 常把「XX在哪份文件里／是哪份资料」误判成
        // document_qa 而走 rag 空检索；以「哪份文件里/哪个文件里/哪份资料」等收尾
        // 的问句本质是定位「具体哪一份文件/资料」→ 应走 find（文件定位）。这些是
        // 纯定位语义的高精度结尾标记，正文问答不以「哪份文件里/哪份资料」收尾。
        // 「帮我（把…）翻出来／找出来／拿出来」的检索/定位家族结尾。0.6B/2B 常把
        // 这类明明在找某份资料/文件的问句（「还存了份X帮我翻出来」「把Y找出来」）
        // 解析成 document_qa，拿对象描述做 chunk 检索 → 命中知识集锦等旁支部而非
        // 目标文件。以「翻/找/拿…出来（一下）」收尾本质是「定位并取回某个文件/
        // 资料对象」，与 locate 语义同源，应收敛为 find（文件定位）。纯定位语义的
        // 高精度结尾标记，正文问答不以「…翻出来/找出来」收尾。
        // 通用语言结构判定，不针对任何具体文件/关键词/case。
        "哪份文件里", "哪个文件里", "哪份资料", "是哪份资料", "是哪一份资料",
        "翻出来", "找出来", "拿出来", "帮我翻出来", "帮我找出来", "帮我拿出来",
        "帮我翻一下", "帮我拿一下", "翻一下", "拿一下",
    ];
    let question_trimmed = question.trim_matches(|c: char| {
        matches!(
            c,
            '？' | '?' | '！' | '!' | '。' | '、' | '，' | ' ' | '的' | '吗' | '呢' | '啊'
        )
    });
    // 文本归一化（去疑问/语气尾缀），供 1.1~1.3 的兜底判定与回声防护共用。
    let normalized = |text: &str| -> String {
        text.trim_matches(|c: char| {
            matches!(
                c,
                '？' | '?' | '！' | '!' | '。' | '、' | '，' | ' ' | '的' | '吗' | '呢' | '啊'
            )
        })
        .trim()
        .to_owned()
    };
    let ends_with_location = location_enders
        .iter()
        .any(|marker| question_trimmed.ends_with(marker));
    // 只要以定位词结尾即归 find，不依赖模型 intent 输出。0.6B/2B 对这类问句的
    // intent 输出不稳定（DocumentQa / LibraryQa 均可能出现），但结尾定位词是
    // 高精度语义信号——「XX在哪份文件里／是哪份资料」本质是定位文件，与其 intent
    // 初判无关。1.2/1.3 兜底已显式排除 ends_with_location，故无条件化不会与之冲突。
    if ends_with_location {
        plan.intent = QueryIntent::DocumentFind;
    }
    // 1.2 文档内容列举问句 → DocumentQa（与 1.1 对称的向下兜底）。模型会偶尔把
    // 「X里主要收录了哪些方面的Y／X里有没有涉及Z的内容」这类**问文档内容**的
    // 问句误判成 find（文件定位）。判定依据两条都满足才修正：
    //   1. 模型当前判为 find；
    //   2. 原始问句以强「内容词」提问（哪些/什么/区别/包含/涉及/讲了/主要内容
    //      等，表示要的是文档里的具体内容），**且不以定位词结尾**。
    // 定位词是 find 的最强信号，只要问题以定位词收尾（哪怕同时提到内容）仍走
    // find（如「包含XX真题的那份文件是哪个」）；不以定位词结尾却带强内容词时，
    // 它要的是「文档内容」而非「文件在哪」，应归 qa。这是通用内容/定位信号
    // 的不对称兜底，不针对任何具体文件/关键词/case。
    let content_qa_markers: &[&str] = &[
        "哪些方面", "收录了哪些", "包括哪些", "包含哪些", "涉及哪些", "有哪些",
        "什么内容", "主要内容", "讲了什么", "些什么", "哪些", "什么", "区别",
        "分别", "包含", "包括", "涉及", "讲了", "提到", "写了",
        // 文档「模块/板块/部分」划分问句（「大致分哪几个模块／分几大块／有几大部分」）
        // 问的是文档内部的构成结构，本质是内容问答/大纲，不是定位文件本身。0.6B/2B
        // 常把这类问句误判成 find（拿文档名当文件定位），此处收敛回 qa。词条取
        // 具体「结构划分」短语而非裸「哪几个」，避免把「哪几个文件里存了X」这类真实
        // 文件定位问句误伤。通用语言结构判定，不针对任何具体文件/关键词/case。
        "哪几个模块", "分了哪几个", "分成哪几个", "分哪几个", "几个模块", "几大模块",
        "几大块", "几部分", "几个部分", "分为哪几个", "分成了几个", "分几块",
    ];
    let asks_document_content = content_qa_markers
        .iter()
        .any(|marker| question_trimmed.contains(marker));
    if plan.intent == QueryIntent::DocumentFind && asks_document_content && !ends_with_location
    {
        plan.intent = QueryIntent::DocumentQa;
    }
    // 1.2.1 结构枚举问句兜底（在内容列举 1.2 之后、整文摘要 1.3 之前）。
    // 0.6B/2B 对「某本文档/教材讲了哪几章、分了哪些章节」这类**单文档内部结构
    // 枚举**问句不稳定：常因 target 解析为空而不满足 1.3 的 target_non_empty
    // 前提，进而落回 document_qa 拿「哪几章」做 chunk 检索 → 召回为空被门控拒答
    // （如「教材讲了哪几章」no_evidence）。结构枚举整词短语
    // （哪些章节/哪几章/分了几章/讲了哪几章…，与 1.3 summary_markers 的结构分支、
    // 1.6 asks_document_structure 同源）是问「单份文档内部章节目录」的高精度信号，
    // 命中即**独立于 target 是否解析成功**升级为 document_summary 并标记
    // structure_enumeration=true，确保走 get_outline（章节标题串）。守卫与 1.6 find
    // 枚举覆盖一致（非定位词结尾 + 内容检索类意图），避免把真正的文件定位问句改成
    // 大纲。通用语言结构判定，不针对任何具体文件/关键词/case。
    let asks_document_structure_for_summary = STRUCTURE_ENUMERATION_MARKERS
        .iter()
        .any(|word| question_trimmed.contains(word));
    if !ends_with_location
        && asks_document_structure_for_summary
        && !has_compare_intent(question_trimmed)
        && matches!(
            plan.intent,
            QueryIntent::DocumentQa | QueryIntent::MultiDocumentQa | QueryIntent::DocumentSummary
        )
    {
        plan.intent = QueryIntent::DocumentSummary;
        plan.operation = QueryOperation::Summary;
        plan.content_query = None;
        plan.requires_full_document = true;
        plan.structure_enumeration = true;
    }
    // 1.3 整文摘要问句 → DOCUMENT_SUMMARY（与 find/qa 同类的确定性兜底）。0.6B/2B
    // 模型对「总结一下X的主要内容」这类问句不稳定，实测常解析成 document_qa 且把
    // 文档名当 content_query（r31：content="2023年数据库系统工程师备考知识点集锦"），
    // 结果拿文档标题做 chunk 检索 → 召回为空 → 被门控拒答。判定依据**三条同时满足**：
    //   1. 原始问句含摘要意图词（总结/概括/归纳/主要内容/主要写了/讲了什么/有什么
    //      内容/整体/概述/回顾…），表示要的是整篇文档的概览；
    //   2. 目标对象是具体文档（target 非空），摘要必须锁定在单文档上；
    //   3. content_query 为空，或等于目标指代/文档名——即没有真正的独立内容查询，
    //      问的就是整篇文档本身。
    // 满足 → 升级为 document_summary：requires_full_document=true、content_query 置
    // null（生产侧走整文分层摘要管线，禁止只拿 top-chunk 生成）。若问句同时带独立
    // 内容词（「总结一下X里关于数据备份的内容」，content_query≠target）则保持
    // document_qa 走局部检索。通用问句词 + 目标对象判定，不针对任何具体文件/case。
    let summary_markers: &[&str] = &[
        "总结", "概括", "归纳", "主要内容", "主要写了", "讲了什么", "写了什么",
        "写了哪些内容", "有什么内容", "整体", "概述", "回顾一下", "介绍了哪些",
        "都讲了哪些", "涵盖哪些", "重点是什么",
        // 覆盖/综述问句（问「某份材料整体覆盖/考了哪些内容/有哪些题型」，是整文概述，
        // 不是锁定单个内容的局部检索）：真题「都考了哪些类型的题目」、手册「涵盖哪些
        // 章节」。语义是「这份材料的整体范围/构成」，应收敛为 document_summary。通用
        // 语言结构判定，不针对任何具体文件/关键词/case。
        "都考了哪些", "考了哪些", "有哪些题型", "题型分布", "是什么题型",
        // 章节/大纲列举问句（问「某份教材/手册分了几章、讲了哪几章、有哪些章节」），
        // 要的是整篇文档的章节目录/大纲，不是锁定单个内容做局部检索。0.6B 模型会把
        // 「讲了哪几章／分了哪些章节」误判成 document_qa，拿章节问法做 chunk 检索 →
        // 召回为空被门控拒答（如「教材讲了哪几章」no_evidence）。问答以「哪几章/哪些
        // 章节/分了哪些…」收尾是整文结构列举，应收敛为 document_summary（生产走整文
        // 大纲/分层摘要管线）。通用语言结构判定，不针对任何具体文件/关键词/case。
        "哪几章", "哪些章节", "分了几章", "讲了几章", "分了哪些章", "多少章",
    ];
    let asks_whole_document = summary_markers
        .iter()
        .any(|marker| question_trimmed.contains(marker));
    if asks_whole_document
        && target_non_empty
        && plan.intent != QueryIntent::DocumentFind
        && !ends_with_location
    {
        let content_eq_target = plan
            .content_query
            .as_deref()
            .map(normalized)
            .map(|content| {
                let content = content.trim().trim_start_matches("我的").trim().to_owned();
                let reference = plan
                    .target
                    .reference
                    .as_deref()
                    .map(normalized)
                    .map(|value| value.trim().trim_start_matches("我的").trim().to_owned())
                    .unwrap_or_default();
                let document_name = plan
                    .target
                    .document_name
                    .as_deref()
                    .map(normalized)
                    .unwrap_or_default();
                !content.is_empty()
                    && (content == reference || content == document_name || content == question_trimmed)
            })
            .unwrap_or(true);
        if plan.content_query.is_none() || content_eq_target {
            plan.intent = QueryIntent::DocumentSummary;
            plan.operation = QueryOperation::Summary;
            plan.content_query = None;
            plan.requires_full_document = true;
            // 区分「结构枚举大纲」与「整文内容摘要」：结构枚举问句（要章节标题串）
            // 才走 get_outline；整文内容摘要（写了什么/总结一下）回落 legacy 摘要
            // 管线，避免被大纲工具接管后丢失内容概览。用结构枚举整词短语判定，
            // 通用语言结构，不针对任何具体文件/关键词/case。
            plan.structure_enumeration = STRUCTURE_ENUMERATION_MARKERS
                .iter()
                .any(|word| question_trimmed.contains(word));
        }
    }
    // 1.4 跨文档对比问句 → COMPARE_DOCUMENTS（与 1.1~1.3 同类的确定性兜底）。
    // 0.6B/2B 模型对「X和Y有何异同/有什么区别/分别…」这类**跨文档对比**问句
    // 极不稳定，实测会把 target/content 解析成别的问句（相互串台），落进
    // document_qa 后回答退化为单文件局部检索、丢失对比结构。判定依据两条
    // **同时满足**：强对比词（[`COMPARE_MARKERS`]）+ 双文档结构信号
    // （[`has_dual_document_targets`]，两个年份/上午下午/两份）。概念对比
    // （「视图和基本表有什么区别」）两侧是概念不是文档，不满足后者，保持
    // document_qa。升级后 target.reference 与 content_query 取对比词**之前**
    // 的文档描述前缀（年份/上午下午/主题都留在里面，供 Resolver 多信号定位
    // 两侧），由 Resolver 的 scope/candidates top-2 落到比较管线
    // （生产 run_compare_answer 口径）。
    if has_compare_intent(question_trimmed)
        && matches!(
            plan.intent,
            QueryIntent::DocumentQa
                | QueryIntent::LibraryQa
                | QueryIntent::MultiDocumentQa
                | QueryIntent::CompareDocuments
        )
    {
        plan.intent = QueryIntent::CompareDocuments;
        plan.operation = QueryOperation::Compare;
        plan.requires_document_resolution = true;
        plan.requires_full_document = false;
        let paired = compare_document_reference(question_trimmed);
        if !paired.is_empty() {
            // 「成对文件描述」拆成两侧独立引用：target 指向第一侧，secondary_target
            // 指向第二侧，供 Resolver / compare 管线分别定位（避免「2019年和2020年
            // 上午的数据库真题」作为单一 target 被模糊加权到无关版本）。content_query
            // 仍保留完整前缀，供两侧文件内检索取词。拆不出成对结构时保持原样，交给
            // Resolver 的 candidates top-2。
            let (left, right) = split_compare_references(&paired);
            plan.target.reference = Some(left);
            plan.content_query = Some(paired);
            // 拆分后的 target.reference 就是本侧**唯一权威描述**：清除 parser 可能
            // 灌进 document_name 的整句污染（如 0.6B/2B 把「数据库系统工程师2020
            // 上午题和下午题考察重点有什么区别」整句塞进 document_name），并关闭
            // 精确点名标志，确保 Resolver 的 reference_desc 采用干净的 `left` 而非
            // 更长的整句（reference_desc 取 reference/document_name 较长者）。
            plan.target.document_name = None;
            plan.target.precise_named_document = false;
            if let Some(side_b) = right {
                plan.secondary_target = Some(QueryTarget {
                    reference: Some(side_b),
                    document_type: plan.target.document_type,
                    owner: plan.target.owner.clone(),
                    ..QueryTarget::default()
                });
            }
        }
    }
    // 1.5 纯库概览归一化（在 find/summary/compare 等更具体意图判定**之后**，作为
    // 最后兜底）。当问题没有具体文档/实体对象，也没有实质性内容词，而是问
    // 「知识库/我的资料（整体）有哪些、收集了什么」这类**库级概览**时，归一为
    // library_qa + content_query=null。这样 `tool_for_plan` 才能把它映射到
    // LibraryOverview（走库概览，只读画像计数，不触发底层检索），避免误走
    // document_qa/rag_search 去检索无正文实体的问题而产出 no_evidence。通用语言
    // 结构判定，刻意不针对任何具体文件/关键词/case：
    //   - 无具体对象：target 为空或仅为库级泛指（知识库/我的资料…），且无
    //     document_type/document_name/entity 限定；
    //   - 无实质内容词：content_query 为 None，或规范化后极短（≤2 字的库级名词，
    //     如「资料」「内容」）——真正的实体/内容（体检报告、RAG、数据库系统工程师）
    //     一定更长，不会被误砍。
    let library_level_targets: &[&str] = &[
        "我的知识库", "知识库", "我的资料", "资料库", "我的文库", "我的文档", "我的库",
    ];
    let overview_ref = plan
        .target
        .reference
        .as_deref()
        .map(|s| s.trim())
        .unwrap_or("");
    // 库级泛指信号：问题文本**强库级**指代/收集动作（知识库、我的资料、收集了…），
    // 用于 reference 为空时排除纯空泛问句（「有哪些问题」无库级信号不误归）。刻意
    // **不**把宽泛的「资料/文库/文档」单字当作充分信号——「2023 年的数据库系统
    // 工程师备考资料有哪些」是定位具体真题文件（find），不是库概览；只有真正指向
    // 整个「知识库/我的资料」或「收集/收录了（全部）资料」的才归概览。通用语言
    // 结构判定，不针对任何具体文件/关键词/case。
    let question_overview_signal = [
        "知识库", "我的资料", "资料库", "我的文库", "我的文档", "我的库", "收集了", "收录了",
    ]
    .iter()
    .any(|word| question_trimmed.contains(word));
    let is_library_level_target = if overview_ref.is_empty() {
        question_overview_signal
    } else {
        library_level_targets
            .iter()
            .any(|word| overview_ref.contains(word))
    };
    // 口语化全库盘点：问句在盘点**整库**的资料数量/方向（「你这边收着多少份
    // 资料，都是些啥方向的」「一共几份资料」）。这类问句无具体文件、也无具体
    // 正文内容词，判定其全库枚举语义（盘点量 + 方向），归库概览，避免落入 1.6
    // 的「枚举定位某类资料文件」而误判 document_find。
    // 纯通用语言结构判定——绝不依赖任何具体文件/关键词/case。
    // target 是否构成**可信的内容定位限定**：只有真正由用户说出的限定才算。
    // 0.6B/2B 对整库盘点问句常凭空填充 dtype/entity_name（如既不盘点也不聊简历
    // 的问句被填上「我的简历」或「Other」）。真实的目标限定必然来自用户原话、
    // 能以子串在问句原文中找到，或是用户点到的**具体**文档类型；原文里找不到
    // 的实体/名称、以及文档类型的通用兜底 Other（模型无法归类时的默认值），
    // 都是幻觉/非限定，不能用来否定库概览判定。此处以「问句文本可溯源」为准，
    // 而非盲信模型解析出的 target 字段。
    let has_specific_type = plan.target.document_type.map_or(false, |ty| {
        !matches!(ty, DocumentType::Other)
    });
    let name_in_question = plan
        .target
        .document_name
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .is_some_and(|v| question_trimmed.contains(v.trim()));
    let entity_in_question = plan
        .target
        .entity_name
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .is_some_and(|v| question_trimmed.contains(v.trim()));
    let target_unqualified = !has_specific_type && !name_in_question && !entity_in_question;
    let positional_refs: &[&str] = &[
        "你这边", "我这边", "我这", "我这里", "这边", "你那", "你那儿", "你手上",
        "你手头", "我手头",
    ];
    // 库级盘点问句是否**面向用户自己的整库**：看位置词是否出现在用户原话里。
    // 不依赖模型解析的 reference（0.6B/2B 对「你手头收的资料」常解析成具体引用、
    // 实体或空白，逐次漂移），以问题文本为准更稳定。通用语言结构判定，不针对任何
    // 具体文件/关键词/case。
    let is_positional_ref = overview_ref.is_empty()
        || positional_refs
            .iter()
            .any(|w| overview_ref.contains(w) || question_trimmed.contains(w));
    // 全库盘点措辞：命中「资料集合量 + 盘点」这一类语言结构。为通用、高精度，
    // 用**量词 + 集合名词的双重条件**（数量盘点「多少份/几份」；方向盘点需配合
    // 「啥方向/什么方向/哪些方向」这类方向词），避免把正文里偶然出现的「几分/多
    // 少…」误判。不做任何具体文件/主题特判。
    let counts_library_collection = ["多少份", "几份", "共几份", "一共几份", "总共几份"]
        .iter()
        .any(|w| question_trimmed.contains(w));
    let asks_library_direction = ["啥方向", "什么方向", "哪些方向", "都有哪些方向", "分别是什么"]
        .iter()
        .any(|w| question_trimmed.contains(w));
    // 类别盘点：问整库资料「拢共能分哪几类/怎么分类」这类**分类**盘点，同样归
    // 库概览（只读按类型计数，不锁定具体文件）。用整词短语避免正文偶然出现的裸
    // 「几类」误判；文档内部的「哪几个模块/哪几章」等结构枚举词不在此列，仍走
    // 文档内容/大纲。通用语言结构判定，不针对任何具体文件/关键词/case。
    let asks_library_category = [
        "分哪几类", "能分哪几类", "分为哪几类", "分几类", "怎么分类", "如何分类", "有哪些类",
    ]
    .iter()
    .any(|w| question_trimmed.contains(w));
    let asks_whole_library_inventory = target_unqualified
        && is_positional_ref
        && (counts_library_collection || asks_library_direction || asks_library_category);
    let is_library_level = (is_library_level_target || asks_whole_library_inventory)
        && target_unqualified;
    // 库概览语义须由**问题文本**二次确认，不能依赖模型对 content_query 的长度判定
    // （0.6B/2B 对同一存在性问句重跑会给不定长 content）。库概览必须是「枚举库里
    // 有什么」：命中概览枚举词（有哪些/收集了什么）且**不是**存在性命中词（有没有/
    // 里有…吗）——存在性（「我的资料里有体检报告吗」）要的是具体内容的 yes/no，
    // 归 rag_search 而非概览。通用语言结构判定，不针对任何具体文件/关键词/case。
    let asks_overview = [
        "有哪些", "哪些资料", "收集了", "收录了", "有什么", "几份", "多少份", "都有什么",
        // 类别盘点：问「资料拢共能分哪几类/有哪些类/怎么分类」这类整库**分类**盘点。
        // 用整词短语（哪几类/分哪几类…）避免正文偶然出现的「几类」误判；库级判定
        // 仍由 is_library_level 把关（须指向整库而非具体资料文件），不针对任何具体
        // 文件/主题/case。
        "哪几类", "分哪几类", "能分哪几类", "有哪些类", "有几类", "怎么分类",
    ]
    .iter()
    .any(|word| question_trimmed.contains(word));
    let existence_hit = ["有没有", "中有没有", "里有没有", "内有没有", "是否存在"]
        .iter()
        .any(|word| question_trimmed.contains(word));
    if is_library_level
        && asks_overview
        && !existence_hit
        && !ends_with_location
        && !has_compare_intent(question_trimmed)
    {
        plan.intent = QueryIntent::LibraryQa;
        plan.operation = QueryOperation::Qa;
        plan.content_query = None;
        plan.requires_document_resolution = false;
        plan.requires_full_document = false;
    }
    // 1.5b 整库主题/类别分布占比问句 → 库概览（LibraryQa）。1.5 的库概览判定依赖
    // `target_unqualified`（0.6B/2B 常给整库盘点问句凭空补幻影 document_type，使
    // target_unqualified 变 false 而卡住 is_library_level）；而「你收的整库资料偏
    // 某方向多还是偏另一方向多」「考试类跟开发手册类几比几」这类**整库占比分布**
    // 问句，本身只能指整库、不指特定文件正文，用 `has_theme_distribution_intent`
    // 这一通用语言结构独立判定即可兜住，不受幻影 type 影响。归一为 library_qa +
    // content_query=None，由 LibraryOverview 只读主题分布计数（不触发正文检索），
    // 避免误走 document_qa / 宽 scope RAG 返回无关题目碎片。通用判定，不针对任何
    // 具体文件/主题/case。
    if !ends_with_location
        && !has_compare_intent(question_trimmed)
        && has_theme_distribution_intent(question_trimmed)
        && matches!(
            plan.intent,
            QueryIntent::DocumentQa
                | QueryIntent::LibraryQa
                | QueryIntent::MultiDocumentQa
        )
    {
        plan.intent = QueryIntent::LibraryQa;
        plan.operation = QueryOperation::Qa;
        plan.content_query = None;
        plan.requires_document_resolution = false;
        plan.requires_full_document = false;
    }
    // 1.6 资料枚举定位问句 → DOCUMENT_FIND（在 1.1~1.5 之后作为最终覆盖层）。
    // 0.6B/2B 对「按主题/年份枚举一**组**资料文件」的问句（`2023 年的数据库系统
    // 工程师备考资料有哪些`、`上午真题大概能查到哪几年`)不稳定，常解析成
    // library_qa / multi_document_qa 而误走 rag 空检索 → no_evidence。它们问的
    // 是「库里存在哪些**资料文件/真题**」这样的文件定位/枚举，不是资料正文里
    // 讲了什么内容。判定**全部满足**才归 find：
    //   1. 当前是内容检索类意图（document/qa/multi-doc），非 find/summary/compare；
    //   2. 枚举词与「资料集合」名词构成**紧邻宾语搭配**（备考资料+有哪些、哪些+
    //      真题、真题+能查到哪几年），而不是泛泛「有哪些」枚举正文内容
    //      （「手册里说大模型幻觉有哪些来源」的宾语是「来源」非资料，不误归）；
    //   3. 非库级概览（1.5 已把「知识库/我的资料整体概览」归 library_qa；其
    //      is_library_level 为真，此处跳过，避免把库概览误判成文件枚举）；
    //   4. **不是**问资料正文内容（含「内容/题型/知识点/讲了…」等进入正文的词，
    //      由 1.3 document_summary 或 document_qa 接管，不落 find）；
    //   5. 去除宾语词与枚举词后仍留有主题描述（≥4 字），说明要定位**某类**具体
    //      资料，而非空泛枚举。
    // 通用语言结构判定，不针对任何具体文件/关键词/case。归一后 content_query 保留
    // 用户的资料描述，供 Document Resolver 多信号定位。
    let find_in_body_markers: &[&str] = &[
        "内容", "题型", "知识点", "讲了", "讲的", "涵盖了", "包括哪些类型", "归纳",
    ];
    let asks_in_body = find_in_body_markers
        .iter()
        .any(|word| question_trimmed.contains(word));
    // 整文书目/结构枚举信号：问的是「分了哪些章节/讲了哪几章」这样的**单份文档内部
    // 结构**枚举，本质是整文大纲（document_summary / get_outline），不是库里枚举
    // **多份资料文件**。此类问句若文档标题恰含资料集合名词（如「大模型应用开发
    // 手册…哪些章节」的「手册」）会被 1.6 的紧邻宾语搭配误判成 file-find——问的是
    // 文档内部章节而非定位「另一份文件」，落 find 反而扩大 scope（返回文件而非其
    // 结构）。只用「结构枚举」整词短语（哪些章节/哪几章…，均与 1.3 summary_markers
    // 的结构分支同源）作信号，命中即**排除**在 1.6 的文件枚举覆盖之外；不用裸
    // 「章节/大纲/目录」单字——它们在文件定位语境（「哪些资料有章节考题」）下会
    // 误伤真正的找文件问句。通用语言结构判定，不针对任何具体文件/关键词/case。
    let asks_document_structure = [
        "哪些章节", "哪几章", "分了几章", "讲了几章", "讲了哪几章", "分了哪些章",
        "多少章", "章节目录",
    ]
    .iter()
    .any(|word| question_trimmed.contains(word));
    if !ends_with_location
        && !asks_document_structure
        && !has_compare_intent(question_trimmed)
        && !is_library_level
        && matches!(
            plan.intent,
            QueryIntent::DocumentQa
                | QueryIntent::LibraryQa
                | QueryIntent::MultiDocumentQa
        )
        && has_material_enumerate_collocation(question_trimmed)
        && !asks_in_body
        && subject_desc_length(question_trimmed)
    {
        plan.intent = QueryIntent::DocumentFind;
        plan.operation = QueryOperation::Find;
        plan.requires_document_resolution = true;
        plan.requires_full_document = false;
    }
    // 2. 回声防护（在 scope 剥离**之前**）：解析结果等于历史任一用户问题
    // → 解析失败。必须先判——剥离会把复读句拆成残片（「我的资料里有没有
    // 提到 RAG」→「没有提到 RAG」），残片对比永不相等，防护就失效了。
    let content = plan
        .content_query
        .as_deref()
        .map(normalized)
        .unwrap_or_default();
    let reference = plan
        .target
        .reference
        .as_deref()
        .map(normalized)
        .unwrap_or_default();
    let echoes_history = history.iter().any(|message| {
        if message.role != "user" {
            return false;
        }
        let prior = normalized(&message.content);
        !prior.is_empty() && (content == prior || reference == prior)
    });
    if echoes_history {
        return None;
    }
    // 5. content_query 剥 scope 引导（在目标分离之后，target 动态引导才可用）
    if let Some(content_query) = plan.content_query.clone() {
        let stripped = strip_content_scope_prefix(&content_query, &plan);
        plan.content_query = Some(stripped);
    }
    // 6. content_query 过短裸实体恢复：某些存在性问句（boolean_existence）下，
    //    LLM 常把检索词压成不含中文的过短裸实体（如「SCI 论文投稿相关的内容吗」
    //    → "SCI"）。以纯缩略词做语义检索，向量落点在模型先验方向上，召回不足，
    //    被相关性门槛滤掉后误判「无证据→拒答」（错误否定）。此时从原始问句用
    //    同一条 scope 剥壳逻辑恢复语义更丰富的短语作为检索词（仍是同一文档范围，
    //    不扩大 scope），恢复「证据存在但提问挖不出」的召回。通用结构判定，不针对
    //    任何具体文件/关键词/case。
    if let Some(thin) = plan.content_query.as_deref() {
        if is_bare_ascii_fragment(thin) {
            let recovered = strip_content_scope_prefix(question, &plan);
            if !recovered.is_empty() && recovered != thin {
                plan.content_query = Some(recovered);
            }
        }
    }
    // 6.5 文档内「枚举/计数 + 纯容器内容词」→ 整文概述。0.6B/2B 对
    //   「X 里整理了几个项目的逐字稿／X 里有哪几份清单」这类枚举/计数问句，
    //   常把检索词压成容器/装载类名词（逐字稿/手册/清单…），拿它做 chunk 检索
    //   召回必然为空 → 被门控拒答（no_evidence）。这类词指向的是「整篇文档要被
    //   枚举出的内容项」，不是某小节的定位词；用户真实要的是通读全文、列出所有
    //   项目/条目。命中即升级为 document_summary（整文内容摘要／按需大纲），由
    //   摘要管线枚举全文，而不是拿容器词做局部检索。判定**全部满足**才触发：
    //   1. 当前仍是内容检索类意图（document_qa / multi_document_qa）；
    //   2. 问句是枚举/计数形态（几个/哪些/多少/有哪些；或模型 question_shape 为
    //      list / fact）；
    //   3. content_query 剥壳后为**纯容器词**（无实质主题）；
    //   4. 已锁定具体目标对象（避免把库级泛指误升级）；
    //   5. 非定位结尾、非跨文档对比（防御性守卫，与 1.6 一致）。
    //   通用语言结构判定，不针对任何具体文件/关键词/case。
    let enumeration_count_shape = plan.question_shape == QuestionShape::List
        || plan.question_shape == QuestionShape::Fact
        || ["几个", "多少个", "有哪些", "哪些", "有多少", "多少份"]
            .iter()
            .any(|word| question_trimmed.contains(word));
    let content_pure_container = plan
        .content_query
        .as_deref()
        .map(is_pure_container_query)
        .unwrap_or(false);
    if enumeration_count_shape
        && content_pure_container
        && target_non_empty
        && !ends_with_location
        && !has_compare_intent(question_trimmed)
        && matches!(
            plan.intent,
            QueryIntent::DocumentQa | QueryIntent::MultiDocumentQa
        )
    {
        plan.intent = QueryIntent::DocumentSummary;
        plan.operation = QueryOperation::Summary;
        plan.content_query = None;
        plan.requires_document_resolution = true;
        plan.requires_full_document = true;
        // 保持 structure_enumeration=false：这是整文内容枚举，不是「章节标题串」，
        // 由 legacy 摘要管线通读全文列出内容项（详见 tool_for_plan 注释）。
    }
    // 7. 确定性歧义同步（省略/指代、裸祈使、会话记忆恢复 → Ambiguous）：
    //    source_router 对这类「需结合会话上下文澄清 / 恢复所指」的问句已判
    //    Ambiguous；这里在规划层把同一判定同步到 plan.source，使 Agent
    //    Planner 不会对上下文相关题越权取底层工具（应回落既有 Context
    //    Resolver / Legacy 澄清）。只依据确定性信号（`apply_ambiguous_override`），
    //    不叠加模型对小模型不稳定的 source 标签。通用语言结构判定，不针对
    //    任何具体文件/关键词/case。
    if plan.source != SourceIntent::Ambiguous {
        let mut ambiguity_routing = SourceRouting {
            source: SourceIntent::Local,
            confidence: 0.0,
        };
        apply_ambiguous_override(question, &mut ambiguity_routing);
        if ambiguity_routing.source == SourceIntent::Ambiguous {
            plan.source = SourceIntent::Ambiguous;
        }
    }
    // 8. 确定性能力询问同步（source_router 误判 local → general）：与第 7 段同
    //    源，只依据确定性信号（`apply_capability_override`），不叠加模型对小模型
    //    不稳定的 source 标签。「你能把……比一比/分析一下吗」类能力询问从 local
    //    拉回 general，编排层自然介绍本地资料助手能力，不进入文档检索（对应
    //    source_router 判断原则 9，避免 hit 真实评测能力询问被当 document_qa）。
    //    通用语言结构判定，不针对任何具体文件/关键词/case。
    if plan.source != SourceIntent::Ambiguous {
        let mut capability_routing = SourceRouting {
            source: plan.source,
            confidence: 0.0,
        };
        apply_capability_override(question, &mut capability_routing);
        plan.source = capability_routing.source;
    }
    Some(plan)
}

/// content_query 是否是无中文语义的过短裸实体：不含任何表意汉字、整体字符数
/// 很少。此类检索词（纯 ASCII 缩略词）语义单薄，独立做向量检索易召回不足；
/// 一旦命中就在 5.6 用原问句剥壳恢复更丰富的短语。纯 ASCII 但较长的多词
/// 描述（如 "Machine Learning"）已自带语义，不触发。
fn is_bare_ascii_fragment(query: &str) -> bool {
    const MAX_FRAGMENT_CHARS: usize = 12;
    let has_cjk = query
        .chars()
        .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
    let too_short = query.chars().count() <= MAX_FRAGMENT_CHARS;
    !has_cjk && too_short
}

/// 判定 content_query 是否**只剩容器/装载类名词**（无实质主题内容）。
/// 反复剥掉尾部/整体命中的容器词（含「的/X/X着」等粘着后缀），余下为空即视为
/// 纯容器——如「逐字稿/手册」剥光为空。通用语言结构判定，不针对任何具体文件/关键词/case。
fn is_pure_container_query(query: &str) -> bool {
    let mut remainder = query.trim();
    loop {
        let mut removed = false;
        for noun in CONTENT_CONTAINER_NOUNS {
            if remainder == *noun {
                remainder = "";
                removed = true;
                break;
            }
            if let Some(stripped) = remainder.strip_suffix(*noun) {
                remainder = stripped.trim_end_matches(|c: char| {
                    matches!(c, '的' | '之' | '和' | '与' | '及' | '中' | '着')
                });
                removed = true;
                break;
            }
        }
        if !removed {
            break;
        }
    }
    remainder.is_empty()
}

/// 解析 Query Parser 输出；解析失败或必填字段非法返回 None（调用方回退）。
/// 大小写与噪声宽容：schema 约束输出小写，这里兜底 LLM 不守约束的情况。
pub fn parse_query_plan(raw: &str) -> Option<QueryPlan> {
    let cleaned = raw
        .trim()
        .strip_prefix("```json")
        .or_else(|| raw.trim().strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(raw.trim());
    let value = serde_json::from_str::<serde_json::Value>(cleaned).ok()?;
    parse_query_plan_lenient(&value)
}

/// 逐字段宽容解析：枚举大小写归一化，缺失可选字段用默认值。
fn parse_query_plan_lenient(value: &serde_json::Value) -> Option<QueryPlan> {
    let source = SourceIntent::parse_lenient(value.get("source")?.as_str()?)?;
    let intent = QueryIntent::parse_lenient(value.get("intent")?.as_str()?)?;
    let operation = QueryOperation::parse_lenient(value.get("operation")?.as_str()?)?;

    let mut plan = QueryPlan {
        source,
        intent,
        operation,
        ..QueryPlan::default()
    };

    if let Some(target) = value.get("target").and_then(|value| value.as_object()) {
        plan.target.reference = target
            .get("reference")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        plan.target.document_type = target
            .get("document_type")
            .and_then(|value| value.as_str())
            .and_then(DocumentType::parse_lenient);
        plan.target.document_name = target
            .get("document_name")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        plan.target.precise_named_document = target
            .get("precise_named_document")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        plan.target.owner = target
            .get("owner")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        plan.target.entity_type = target
            .get("entity_type")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        plan.target.entity_name = target
            .get("entity_name")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
    }

    // COMPARE_DOCUMENTS 的第二个目标：整对象为空（全 null/缺失）视为 None。
    if let Some(target) = value
        .get("secondary_target")
        .and_then(|value| value.as_object())
    {
        let secondary = QueryTarget {
            reference: target
                .get("reference")
                .and_then(|value| value.as_str())
                .map(str::to_owned),
            document_type: target
                .get("document_type")
                .and_then(|value| value.as_str())
                .and_then(DocumentType::parse_lenient),
            document_name: target
                .get("document_name")
                .and_then(|value| value.as_str())
                .map(str::to_owned),
            precise_named_document: target
                .get("precise_named_document")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            owner: target
                .get("owner")
                .and_then(|value| value.as_str())
                .map(str::to_owned),
            entity_type: target
                .get("entity_type")
                .and_then(|value| value.as_str())
                .map(str::to_owned),
            entity_name: target
                .get("entity_name")
                .and_then(|value| value.as_str())
                .map(str::to_owned),
        };
        let is_empty = secondary.reference.is_none()
            && secondary.document_type.is_none()
            && secondary.document_name.is_none()
            && !secondary.precise_named_document
            && secondary.owner.is_none()
            && secondary.entity_type.is_none()
            && secondary.entity_name.is_none();
        if !is_empty {
            plan.secondary_target = Some(secondary);
        }
    }

    plan.content_query = value
        .get("content_query")
        .and_then(|value| value.as_str())
        .filter(|query| !query.trim().is_empty())
        .map(|query| query.trim().to_owned());

    if let Some(filters) = value.get("filters").and_then(|value| value.as_object()) {
        plan.filters.time = filters
            .get("time")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        plan.filters.file_type = filters
            .get("file_type")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
        plan.filters.path = filters
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_owned);
    }

    plan.requires_document_resolution = value
        .get("requires_document_resolution")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    plan.requires_full_document = value
        .get("requires_full_document")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    plan.question_shape = value
        .get("question_shape")
        .and_then(|value| value.as_str())
        .and_then(QuestionShape::parse_lenient)
        .unwrap_or(QuestionShape::Description);
    plan.requires_project_context = value
        .get("requires_project_context")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    plan.requires_entity_items = value
        .get("requires_entity_items")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    plan.confidence = value
        .get("confidence")
        .and_then(|value| value.as_f64())
        .map(|value| value.clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0);

    Some(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resume_projects_into_target_and_content_query() {
        // CASE 1 / 8：目标对象与内容查询严格分离
        let raw = r#"{"source":"local","intent":"document_qa","operation":"extract",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"项目经历","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.96}"#;
        let plan = parse_query_plan(raw).expect("valid plan parses");
        assert_eq!(plan.source, SourceIntent::Local);
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
        assert_eq!(plan.operation, QueryOperation::Extract);
        assert_eq!(plan.target.document_type, Some(DocumentType::Resume));
        assert_eq!(plan.target.owner.as_deref(), Some("self"));
        assert_eq!(plan.content_query.as_deref(), Some("项目经历"));
        assert!(plan.requires_document_resolution);
        assert!(!plan.requires_full_document);
        assert!((plan.confidence - 0.96).abs() < 1e-6);
        // LLM 语义判断：项目清单的条目必须是实体/名称形式
        let raw = r#"{"source":"local","intent":"document_qa","operation":"extract",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"项目经历","filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"list","requires_project_context":false,"requires_entity_items":true,
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.96}"#;
        let plan = parse_query_plan(raw).expect("valid plan parses");
        assert_eq!(plan.question_shape, QuestionShape::List);
        assert!(plan.requires_entity_items);
    }

    #[test]
    fn parses_summary_intent() {
        // CASE 7：DOCUMENT_SUMMARY 必须 requires_full_document，content_query 为 null
        let raw = r#"{"source":"local","intent":"document_summary","operation":"summary",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":null,"filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":true,"confidence":0.95}"#;
        let plan = parse_query_plan(raw).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentSummary);
        assert_eq!(plan.operation, QueryOperation::Summary);
        assert!(plan.requires_full_document);
        assert_eq!(plan.content_query, None);
    }

    #[test]
    fn parses_library_qa_without_document_target() {
        // CASE 3 / 4 变体：全库检索不锁定单文件
        let raw = r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"LangGraph","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":false,"requires_full_document":false,"confidence":0.9}"#;
        let plan = parse_query_plan(raw).unwrap();
        assert_eq!(plan.intent, QueryIntent::LibraryQa);
        assert_eq!(plan.operation, QueryOperation::Qa);
        assert_eq!(plan.content_query.as_deref(), Some("LangGraph"));
        assert!(!plan.requires_document_resolution);
    }

    #[test]
    fn parses_my_materials_into_library_qa() {
        // CASE 3：「我的资料」类请求 → library_qa，全库检索不锁单文件
        let raw = r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"有哪些项目","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":false,"requires_full_document":false,"confidence":0.93}"#;
        let plan = parse_query_plan(raw).unwrap();
        assert_eq!(plan.intent, QueryIntent::LibraryQa);
        assert_eq!(plan.operation, QueryOperation::Qa);
        assert_eq!(plan.content_query.as_deref(), Some("有哪些项目"));
        // 全库检索：target 无锁定对象，不需要 Document Resolver
        assert!(!plan.requires_document_resolution);
        assert_eq!(plan.target.document_type, None);
        assert_eq!(plan.target.owner.as_deref(), Some("self"));
    }

    #[test]
    fn parses_langgraph_project_into_entity_target_and_content_query() {
        // CASE 6：LangGraph 查询的目标与内容严格分离——「LangGraph 项目」进
        // target（entity），「架构设计」才是 content_query；绝不把整句当查询词。
        let raw = r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":"self","entity_type":"project","entity_name":"LangGraph 项目"},
            "content_query":"架构设计","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.91}"#;
        let plan = parse_query_plan(raw).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
        assert_eq!(plan.target.entity_name.as_deref(), Some("LangGraph 项目"));
        assert_eq!(plan.content_query.as_deref(), Some("架构设计"));
        assert!(
            !plan
                .content_query
                .as_deref()
                .unwrap_or("")
                .contains("LangGraph")
        );
        assert!(plan.requires_document_resolution);
    }

    #[test]
    fn tolerant_of_case_fences_and_missing_optionals() {
        // 大写变体 + code fence 兜底
        let raw = "```json\n{\"source\":\"LOCAL\",\"intent\":\"DOCUMENT_QA\",\"operation\":\"extract\",\
            \"target\":{\"document_type\":\"RESUME\"},\
            \"content_query\":\"项目经历\",\"requires_document_resolution\":true,\"confidence\":0.8}\n```";
        let plan = parse_query_plan(raw).expect("tolerant parse");
        assert_eq!(plan.source, SourceIntent::Local);
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
        assert_eq!(plan.target.document_type, Some(DocumentType::Resume));
        assert_eq!(plan.content_query.as_deref(), Some("项目经历"));
        // 缺失的可选字段用默认值
        assert_eq!(plan.target.reference, None);
        assert_eq!(plan.target.owner, None);
        assert_eq!(plan.filters.time, None);
    }

    #[test]
    fn rejects_invalid_plans() {
        assert!(parse_query_plan(r#"{"intent":"document_qa"}"#).is_none()); // 缺 source
        assert!(parse_query_plan(r#"{"source":"local","intent":"chat"}"#).is_none()); // 非法 intent
        assert!(parse_query_plan("").is_none());
        assert!(parse_query_plan("not json").is_none());
    }

    #[test]
    fn timeout_or_garbage_output_falls_back_deterministically() {
        // 边界 (10)：Query Parser 超时/垃圾输出 → parse 返回 None，编排层
        // unwrap_or_default() 用确定性默认计划继续，绝不崩溃也不猜测目标。
        for raw in ["", "not json", "{}", "```\n\n```"] {
            assert!(
                parse_query_plan(raw).is_none(),
                "超时/垃圾输出解析失败: {raw:?}"
            );
        }
        let fallback = QueryPlan::default();
        assert_eq!(fallback.intent, QueryIntent::DocumentQa);
        assert_eq!(fallback.source, SourceIntent::Ambiguous);
        assert_eq!(fallback.operation, QueryOperation::Qa);
        assert!(
            !fallback.requires_document_resolution,
            "默认计划不锁定任何文档目标"
        );
        assert!(fallback.target.reference.is_none());
        assert_eq!(fallback.content_query, None);
    }

    #[test]
    fn parses_compare_documents_into_dual_targets() {
        // CASE：比较两个简历版本 → primary + secondary_target 分离
        let raw = r#"{"source":"local","intent":"compare_documents","operation":"compare",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "secondary_target":{"reference":"第二个版本","document_type":null,"document_name":null,
                                "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"有什么不同","filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.92}"#;
        let plan = parse_query_plan(raw).expect("compare plan parses");
        assert_eq!(plan.intent, QueryIntent::CompareDocuments);
        assert_eq!(plan.operation, QueryOperation::Compare);
        assert_eq!(plan.target.document_type, Some(DocumentType::Resume));
        let secondary = plan.secondary_target.expect("secondary target present");
        assert_eq!(secondary.reference.as_deref(), Some("第二个版本"));
        assert_eq!(secondary.document_type, None);
    }

    #[test]
    fn finalize_dual_year_question_upgrades_to_compare() {
        // r35：两个不同年份 → 跨文档对比。模型把 target/content 解析成串台问句
        // （document_qa + 垃圾 target）时，确定性兜底升级为 compare_documents，
        // 并把 target.reference / content_query 重置为对比词之前的文档描述前缀。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"数据库系统工程师2020上午题和下午题考察重点有什么区别",
                      "document_type":null,"document_name":null,"owner":null,"entity_type":null,"entity_name":null},
            "content_query":"数据库系统工程师",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"description","requires_document_resolution":true,"requires_full_document":false,"confidence":0.7}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "2019年和2020年上午的数据库真题分别侧重考哪些内容，有何异同",
            &[],
        )
        .unwrap();
        assert_eq!(plan.intent, QueryIntent::CompareDocuments);
        assert_eq!(plan.operation, QueryOperation::Compare);
        assert!(plan.requires_document_resolution);
        assert!(!plan.requires_full_document);
        // 文档描述前缀拆成两侧独立引用：target.reference 指向第一侧，
        // secondary_target.reference 指向第二侧；content_query 保留完整前缀。
        assert_eq!(
            plan.target.reference.as_deref(),
            Some("2019年上午的数据库真题")
        );
        assert_eq!(
            plan.content_query.as_deref(),
            Some("2019年和2020年上午的数据库真题")
        );
        let secondary = plan.secondary_target.as_ref().expect("dual target present");
        assert_eq!(
            secondary.reference.as_deref(),
            Some("2020年上午的数据库真题")
        );
    }

    #[test]
    fn finalize_session_pair_question_upgrades_to_compare() {
        // r36：上午+下午 → 跨文档对比。前缀剥离「考察重点」等问句残余。
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"ER图",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"description","requires_document_resolution":false,"requires_full_document":false,"confidence":0.7}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "数据库系统工程师2020上午题和下午题考察重点有什么区别",
            &[],
        )
        .unwrap();
        assert_eq!(plan.intent, QueryIntent::CompareDocuments);
        assert_eq!(plan.operation, QueryOperation::Compare);
        // 上/下午拆成两侧独立引用：target.reference 指向上午一侧，
        // secondary_target 指向下午一侧。
        assert_eq!(
            plan.target.reference.as_deref(),
            Some("数据库系统工程师2020上午题")
        );
        let secondary = plan.secondary_target.as_ref().expect("dual target present");
        assert_eq!(
            secondary.reference.as_deref(),
            Some("数据库系统工程师2020下午题")
        );
    }

    #[test]
    fn finalize_concept_comparison_stays_document_qa() {
        // r23：概念对比（视图 vs 基本表）两侧不是文档对象 → 不满足双文档结构，
        // 保持 document_qa，绝不误升级为 compare_documents。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"实务","document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"视图和基本表",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"description","requires_document_resolution":true,"requires_full_document":false,"confidence":0.8}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "实务里视图和基本表有什么区别", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
        assert_eq!(plan.operation, QueryOperation::Qa);
    }

    #[test]
    fn finalize_compare_question_does_not_touch_find() {
        // 定位问句即便带对比词/双年份也不升级为 compare（find 是定位优先）。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_find","operation":"find",
            "target":{"reference":"我的资料","document_type":null,"document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":null,
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"location","requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "哪一份是2019年的真题，哪一份是2020年的？",
            &[],
        )
        .unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentFind);
    }

    #[test]
    fn secondary_target_null_or_empty_means_none() {
        // 非比较请求的 secondary_target 为 null → 不设第二个目标
        let raw = r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"document_type":"resume","reference":"我的简历"},
            "secondary_target":null,
            "content_query":"项目经历","requires_document_resolution":true,"confidence":0.9}"#;
        let plan = parse_query_plan(raw).expect("parses");
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
        assert!(plan.secondary_target.is_none());
        // 全空对象也视为 None（模型输出的空壳）
        let empty = raw.replace("\"secondary_target\":null", r#""secondary_target":{"reference":null,"document_type":null,"document_name":null,"owner":null,"entity_type":null,"entity_name":null}"#);
        let plan = parse_query_plan(&empty).expect("parses");
        assert!(plan.secondary_target.is_none());
    }

    #[test]
    fn parses_precise_named_document_flag() {
        // 模型驱动精确定位：解析器把「完整标题+精确点名」读入 document_name 与
        // precise_named_document。
        let raw = r#"{"source":"local","intent":"document_summary","operation":"summary",
            "target":{"reference":null,"document_type":null,
                      "document_name":"周晨博20212P2002《专业实习》课程实习总结报告.docx",
                      "precise_named_document":true,"owner":null,"entity_type":null,"entity_name":null},
            "content_query":null,"filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"summary","requires_project_context":false,"requires_entity_items":false,
            "requires_document_resolution":true,"requires_full_document":true,"confidence":0.98}"#;
        let plan = parse_query_plan(raw).expect("parses");
        assert_eq!(
            plan.target.document_name.as_deref(),
            Some("周晨博20212P2002《专业实习》课程实习总结报告.docx")
        );
        assert!(plan.target.precise_named_document);
        assert!(plan.requires_document_resolution);
        // 缺省（model 未给该字段）→ 默认 false，兼容旧主体输出
        let without = raw.replace("\"precise_named_document\":true,", "");
        let plan = parse_query_plan(&without).expect("parses without flag");
        assert!(!plan.target.precise_named_document);
    }

    #[test]
    fn parses_document_find_intent() {
        let raw = r#"{"source":"local","intent":"document_find","operation":"find",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":null,"filters":{"time":null,"file_type":null,"path":null},
            "requires_document_resolution":true,"requires_full_document":false,"confidence":0.8}"#;
        let plan = parse_query_plan(raw).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentFind);
        assert_eq!(plan.operation, QueryOperation::Find);
        assert!(plan.requires_document_resolution);
    }

    #[test]
    fn schema_has_required_fields_and_enum_lists() {
        let schema = query_parser_schema();
        let required: Vec<_> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(required.contains(&"source"));
        assert!(required.contains(&"target"));
        assert!(required.contains(&"content_query"));
        let intents: Vec<_> = schema["properties"]["intent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(intents.contains(&"document_summary"));
        assert!(intents.contains(&"library_qa"));
    }

    #[test]
    fn prompt_forbids_concatenating_target_into_query() {
        let (system, user) = query_parser_prompt("我的简历里有没有 LangGraph？", &[]);
        assert!(system.contains("本地资料查询解析器"));
        // 规则明确禁止把目标拼回检索词
        assert!(user.contains("绝不能把"));
        assert!(user.contains("LangGraph"));
        // 文档类型词表在 prompt 里（约束 16：enum 统一，模型输出可被 schema 约束）
        assert!(user.contains("resume"));
        // 示例覆盖拆分、摘要、全库三种形态
        assert!(user.contains("document_summary"));
        assert!(user.contains("library_qa"));
    }

    #[test]
    fn prompt_has_new_phase2_rules() {
        // Phase 4.1 新增规则：存在性问句 → qa；scope 引导语不算内容；
        // 提到明确文档对象必须 requires_document_resolution=true
        let (_, user) = query_parser_prompt("我的资料里是怎么介绍 RAG 的？", &[]);
        assert!(user.contains("存在性问句"));
        assert!(user.contains("operation = \"qa\""));
        assert!(user.contains("scope 引导语"));
        assert!(user.contains("requires_document_resolution 必须为 true"));
    }

    #[test]
    fn finalize_strips_scope_preamble_from_library_qa() {
        // CASE 2：模型把整句当 content_query → 确定性剥掉「我的资料里是怎么介绍的」
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"我的资料里是怎么介绍 RAG 的？","requires_document_resolution":false,"confidence":0.8}"#)
            .unwrap();
        let question = "我的资料里是怎么介绍 RAG 的？";
        let plan = finalize_query_plan(plan, question, &[]).unwrap();
        assert_eq!(plan.content_query.as_deref(), Some("RAG"));
        assert_eq!(plan.intent, QueryIntent::LibraryQa);
    }

    #[test]
    fn finalize_strips_file_scope_and_keeps_library_qa() {
        // CASE 3：我的文件里有没有提到 Transformer？ → content_query=Transformer
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"我的资料里有没有提到 Transformer？","requires_document_resolution":false,"confidence":0.8}"#)
            .unwrap();
        let question = "我的文件里有没有提到 Transformer？";
        let plan = finalize_query_plan(plan, question, &[]).unwrap();
        assert_eq!(plan.content_query.as_deref(), Some("Transformer"));
        assert_eq!(plan.operation, QueryOperation::Qa);
    }

    #[test]
    fn finalize_forces_document_resolution_when_target_present() {
        // CASE 6：模型把 requires_document_resolution 置 false 但 target 有简历
        // → 强制 true（目标对象必须经 Document Resolver 定位）
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"我的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"我的简历里有没有写 LangGraph","requires_document_resolution":false,"confidence":0.7}"#)
            .unwrap();
        let question = "我的简历里有没有写 LangGraph？";
        let plan = finalize_query_plan(plan, question, &[]).unwrap();
        assert!(plan.requires_document_resolution);
        // 存在性问句 → qa；content_query 剥掉「我的简历里有没有写」
        assert_eq!(plan.operation, QueryOperation::Qa);
        assert_eq!(plan.content_query.as_deref(), Some("LangGraph"));
    }

    #[test]
    fn finalize_rejects_plan_echoing_history_question() {
        // 回声防护：0.6B 复读历史里的用户问题 → 视为解析失败（None）。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"我的资料里有没有提到 RAG","requires_document_resolution":false,"confidence":0.7}"#)
            .unwrap();
        let history = vec![AskMessage {
            message_id: uuid::Uuid::now_v7(),
            session_id: uuid::Uuid::now_v7(),
            role: "user".to_owned(),
            content: "我的资料里有没有提到 RAG".to_owned(),
            answer: None,
            error: None,
            created_at: chrono::Utc::now(),
        }];
        let question = "帮我讲讲 RAG 的检索流程";
        let result = finalize_query_plan(plan, question, &history);
        assert!(result.is_none(), "复读历史问题的解析必须判失败");
    }

    #[test]
    fn finalize_keeps_plan_when_history_unrelated() {
        // 历史无关时不误伤：正常解析结果保留
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"我的资料里有没有提到 Transformer？","requires_document_resolution":false,"confidence":0.8}"#)
            .unwrap();
        let history = vec![AskMessage {
            message_id: uuid::Uuid::now_v7(),
            session_id: uuid::Uuid::now_v7(),
            role: "user".to_owned(),
            content: "你好".to_owned(),
            answer: None,
            error: None,
            created_at: chrono::Utc::now(),
        }];
        let plan =
            finalize_query_plan(plan, "我的文件里有没有提到 Transformer？", &history).unwrap();
        assert_eq!(plan.content_query.as_deref(), Some("Transformer"));
    }

    #[test]
    fn finalize_file_location_question_promotes_to_find() {
        // 「XX文件在哪里」→ shape=location + content_query 复读文档名 → DocumentFind
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":null,"document_type":"learning_material",
                      "document_name":"SCI论文智能辅助投稿系统的需求说明书",
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"SCI论文智能辅助投稿系统的需求说明书",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"location","requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "该系统的需求说明书在哪里", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentFind);
    }

    #[test]
    fn finalize_keeps_content_location_as_document_qa() {
        // 正文定位子问题（「主键题在第几页」content_query 是正文关键词，≠ 文档名）
        // 不应被升级为 find；且 shape=location 但 content_query 非复读文档名时保持 QA。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":null,"document_type":"learning_material",
                      "document_name":"2020年数据库系统工程师考试上午真题（参考答案）",
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"主键和候选键",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"location","requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "主键和候选键的题在第几页", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
    }

    #[test]
    fn finalize_content_enumeration_question_keeps_as_document_qa() {
        // 「X里主要收录了哪些方面的Y」是文档内容列举，不以定位词结尾、带强内容词
        // （哪些）。即使模型误判为 find，也应被确定性兜底归回 qa。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_find","operation":"find",
            "target":{"reference":null,"document_type":"learning_material",
                      "document_name":"人工智能面试宝典",
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"人工智能面试宝典",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"list","requires_document_resolution":true,"requires_full_document":false,"confidence":0.8}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "人工智能面试宝典里主要收录了哪些方面的面试题", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
    }

    #[test]
    fn finalize_find_stays_when_location_ender_present_despite_content_words() {
        // 「包含XX真题的那份文件是哪个」以定位词结尾 → 即便带内容词（包含）仍走
        // find，不被内容词向下修正误伤。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_find","operation":"find",
            "target":{"reference":null,"document_type":"learning_material",
                      "document_name":"2020年数据库系统工程师考试上午真题",
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"2020年数据库系统工程师考试上午真题",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"location","requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "包含2020年真题的那份文件是哪个", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentFind);
    }

    #[test]
    fn finalize_recovers_named_file_reference_when_parser_drops_it() {
        // r37 真实口径：parser 把「2023年数据库系统工程师备考知识点里有没有提到
        // ER图和关系模型」解析成无目标 library_qa（reference/document_name 全空）。
        // 文件描述恢复兜底用「里有没有」之前的前缀补回 target.reference，并因
        // target 非空强制 requires_document_resolution、LibraryQa→DocumentQa。
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"ER图",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"boolean_existence","requires_document_resolution":false,
            "requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan =
            finalize_query_plan(plan, "2023年数据库系统工程师备考知识点里有没有提到ER图和关系模型", &[])
                .unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
        assert!(plan.requires_document_resolution);
        assert_eq!(
            plan.target.reference.as_deref(),
            Some("2023年数据库系统工程师备考知识点")
        );
    }

    #[test]
    fn finalize_prefers_complete_question_prefix_over_partial_document_name() {
        // 模型只保留目标描述的一小部分时，原问题边界前的完整前缀应补进
        // reference；document_name 原样保留，交给 resolver 选择更完整的描述。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":"上午试卷",
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"容灾设计",
            "filters":{"time":"2031年","file_type":null,"path":null},
            "question_shape":"boolean_existence","requires_document_resolution":true,
            "requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "2031年的云平台认证上午试卷里有没有涉及容灾设计的内容",
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.target.reference.as_deref(),
            Some("2031年的云平台认证上午试卷")
        );
        assert_eq!(plan.target.document_name.as_deref(), Some("上午试卷"));
    }

    #[test]
    fn finalize_recovery_does_not_overwrite_precise_name_or_concept_questions() {
        // 模型已给出精确点名标题（document_name）时绝不覆盖；教科书概念题
        // 「数据库里有哪几种索引类型」不含边界标记（裸「里有哪」不在表里），
        // 不得把概念名词误当文件描述。
        let precise = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"2023年数据库系统工程师备考知识点集锦","document_type":null,
                      "document_name":"2023年数据库系统工程师备考知识点集锦","owner":null,"entity_type":null,"entity_name":null},
            "content_query":"ER图",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"boolean_existence","requires_document_resolution":true,
            "requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let precise = finalize_query_plan(precise, "2023年数据库系统工程师备考知识点里有没有提到ER图", &[]).unwrap();
        assert_eq!(
            precise.target.reference.as_deref(),
            Some("2023年数据库系统工程师备考知识点集锦")
        );

        let concept = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"数据库索引",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"list","requires_document_resolution":false,
            "requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let concept = finalize_query_plan(concept, "数据库里有哪几种索引类型", &[]).unwrap();
        assert!(
            concept.target.reference.is_none(),
            "概念题不得误设 reference: {:?}",
            concept.target.reference
        );
        assert!(
            !concept.requires_document_resolution,
            "概念题不得被强制锁定文件"
        );
    }

    #[test]
    fn finalize_summary_question_upgrades_to_document_summary() {
        // 「总结一下X的主要内容」模型误判为 document_qa 且把文档名当 content_query
        // （r31 实测）→ 摘要词 + 单文档目标 + content_query 等于目标 → 升级
        // DocumentSummary：requires_full_document=true、content_query=null。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"summary",
            "target":{"reference":"2023年数据库系统工程师备考知识点集锦","document_type":null,
                      "document_name":null,"owner":null,"entity_type":null,"entity_name":null},
            "content_query":"2023年数据库系统工程师备考知识点集锦",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"summary","requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "总结一下2023年数据库系统工程师备考知识点集锦的主要内容",
            &[],
        )
        .unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentSummary);
        assert!(plan.requires_full_document);
        assert_eq!(plan.content_query, None);
    }

    #[test]
    fn finalize_summary_with_independent_content_stays_document_qa() {
        // 「总结一下X里关于数据备份的内容」content_query 是独立内容词（≠ 目标）→
        // 是局部内容问答而非整文摘要，保持 document_qa。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"数据库系统工程师备考资料","document_type":null,
                      "document_name":null,"owner":null,"entity_type":null,"entity_name":null},
            "content_query":"数据备份和恢复机制",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"description","requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "总结一下数据库系统工程师备考资料里关于数据备份和恢复机制的内容",
            &[],
        )
        .unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentQa);
        assert!(!plan.requires_full_document);
    }

    #[test]
    fn finalize_find_stays_find_despite_summary_words() {
        // 定位问句「找一下XX的总结文件是哪个」即便带「总结」也不升级为摘要。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_find","operation":"find",
            "target":{"reference":null,"document_type":"learning_material",
                      "document_name":"2023年备考总结文件",
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"2023年备考总结文件",
            "filters":{"time":null,"file_type":null,"path":null},
            "question_shape":"location","requires_document_resolution":true,"requires_full_document":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "找一下2023年备考总结文件是哪个", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentFind);
    }

    #[test]
    fn strip_scope_handles_target_dynamic_prefixes() {
        // 动态引导：target=我的简历 时「我的简历里有没有写 LangGraph」→ LangGraph
        let mut plan = QueryPlan::default();
        plan.target.document_type = Some(DocumentType::Resume);
        plan.target.reference = Some("我的简历".to_owned());
        assert_eq!(
            strip_content_scope_prefix("我的简历里有没有写 LangGraph", &plan),
            "LangGraph"
        );
        // 类型中文名引导同样生效（即使 reference 为空）
        let mut plan2 = QueryPlan::default();
        plan2.target.document_type = Some(DocumentType::Resume);
        assert_eq!(
            strip_content_scope_prefix("我的简历里有没有提到 LangGraph", &plan2),
            "LangGraph"
        );
    }

    #[test]
    fn finalize_recognizes_pure_library_overview() {
        // 库概览「我的知识库都有哪些资料？」：强库级指代 + 概览枚举词 + 非存在性。
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":"知识库","document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":null,"requires_document_resolution":false,"confidence":0.8}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "我的知识库都有哪些资料？", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::LibraryQa);
        assert!(!plan.requires_document_resolution);
        assert_eq!(plan.content_query, None);
    }

    #[test]
    fn finalize_location_ender_forces_find_even_if_model_says_qa() {
        // 找文件「…在哪份文件里？」结尾定位词无条件归 find，不依赖模型 intent。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"周晨博的简历","document_type":"resume","document_name":null,
                      "owner":"self","entity_type":null,"entity_name":null},
            "content_query":"周晨博的简历内容","requires_document_resolution":true,"confidence":0.8}"#)
            .unwrap();
        let plan = finalize_query_plan(plan, "周晨博的简历在哪份文件里？", &[]).unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentFind);
    }

    #[test]
    fn finalize_material_enumeration_maps_to_find() {
        // 资料枚举「2023年的…备考资料有哪些？」归 find（定位一组资料文件）。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"2023年的数据库系统工程师备考资料","document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"2023年的数据库系统工程师备考资料","requires_document_resolution":true,"confidence":0.8}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "2023年的数据库系统工程师备考资料有哪些？",
            &[],
        )
        .unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentFind);
        assert!(plan.requires_document_resolution);
    }

    #[test]
    fn finalize_structure_enumeration_not_hijacked_to_find() {
        // 结构枚举「手册…都分了哪些章节？」问的是文档内部章节（大纲），即便文档
        // 标题含资料集合名词「手册」也可能命中 1.6 的紧邻宾语搭配，不得被误判成
        // file-find（会扩大 scope 返回文件而非其章节）。应升级为 DocumentSummary
        //（structure_enumeration，生产走 get_outline 输出章节标题串）。
        let plan = parse_query_plan(r#"{"source":"local","intent":"document_qa","operation":"qa",
            "target":{"reference":"大模型应用开发手册","document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"都分了哪些章节","requires_document_resolution":true,"confidence":0.8}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "大模型应用开发手册都分了哪些章节？",
            &[],
        )
        .unwrap();
        assert_eq!(plan.intent, QueryIntent::DocumentSummary);
        assert_eq!(plan.operation, QueryOperation::Summary);
        assert!(plan.structure_enumeration);
        assert_ne!(plan.intent, QueryIntent::DocumentFind);
    }

    #[test]
    fn finalize_recovers_rich_content_query_from_bare_ascii_fragment() {
        // 存在性问句「我的资料里有 SCI 论文投稿相关的内容吗？」：LLM 把检索词压成
        // 纯 ASCII 裸实体 "SCI"，语义单薄会导致召回不足、证据被滤掉后误拒答。
        // 应从原问句用同一条 scope 剥壳逻辑恢复更丰富的检索短语（仍是同一库范围）。
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"SCI","requires_document_resolution":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "我的资料里有 SCI 论文投稿相关的内容吗？",
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.content_query.as_deref(),
            Some("SCI 论文投稿相关的内容")
        );
    }

    #[test]
    fn finalize_keeps_rich_cjk_content_query_unchanged() {
        // 已含中文语义的 content_query（如「大模型项目」）不触发过短恢复，避免
        // 用带噪音的原问句覆盖用户已表达清楚的检索词（如「大模型项目相关的内容」）。
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"大模型项目","requires_document_resolution":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "我的资料里有没有大模型项目相关的内容？",
            &[],
        )
        .unwrap();
        assert_eq!(plan.content_query.as_deref(), Some("大模型项目"));
    }

    #[test]
    fn finalize_keeps_long_ascii_description_unchanged() {
        // 纯 ASCII 但较长的多词描述（自带语义）不触发恢复。
        let plan = parse_query_plan(r#"{"source":"local","intent":"library_qa","operation":"qa",
            "target":{"reference":null,"document_type":null,"document_name":null,
                      "owner":null,"entity_type":null,"entity_name":null},
            "content_query":"Machine Learning","requires_document_resolution":false,"confidence":0.9}"#)
            .unwrap();
        let plan = finalize_query_plan(
            plan,
            "我的资料里有没有 Machine Learning 相关的内容？",
            &[],
        )
        .unwrap();
        assert_eq!(plan.content_query.as_deref(), Some("Machine Learning"));
    }
}
