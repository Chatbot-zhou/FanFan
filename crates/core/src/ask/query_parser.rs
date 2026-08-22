//! Query Parser：把 LOCAL（或已解析的 AMBIGUOUS）请求解析成结构化 [`QueryPlan`]。
//!
//! 核心约束：把「目标对象」与「目标对象内部查询的内容」严格拆开——
//! 「我的简历里有没有 LangGraph」必须解析为
//! `target.document_type = resume` + `content_query = "LangGraph"`，
//! 禁止把「我的 简历 LangGraph」整体作为检索关键词。
//!
//! Prompt 与解析器集中在本模块；解析失败由调用方回退（不中断问答）。

use crate::AskMessage;
use crate::ask::query_plan::{
    QueryIntent, QueryOperation, QueryPlan, QueryTarget, QuestionShape, SourceIntent,
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
- 「我的简历在哪里」→ DOCUMENT_FIND；content_query 为 null。
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

/// 解析后确定性修正（在 LLM 输出之上叠加的「明显逻辑规则」，不调阈值）：
/// - target 非空 → 强制 requires_document_resolution=true（目标对象必须经
///   Document Resolver 定位，禁止拿「我的简历」整句去全库检索）；
/// - content_query 剥掉 scope 引导（我的资料里…）与尾随疑问词；
/// - 回声防护：content_query/target 与历史任一用户问题相同 → 视为解析失败
///   （0.6B 复读历史问题时返回 None，调用方走确定性回退）。
///
/// 注意：意图/操作/形态类判断（FIND、存在性问句、摘要问句）一律由 LLM
/// Parser 自己完成（Prompt 已覆盖），此处**不再**叠加规则覆盖——避免用
/// 硬编码规则取代模型的语义理解。
pub fn finalize_query_plan(
    mut plan: QueryPlan,
    _question: &str,
    history: &[AskMessage],
) -> Option<QueryPlan> {
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
    // 2. 回声防护（在 scope 剥离**之前**）：解析结果等于历史任一用户问题
    // → 解析失败。必须先判——剥离会把复读句拆成残片（「我的资料里有没有
    // 提到 RAG」→「没有提到 RAG」），残片对比永不相等，防护就失效了。
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
    Some(plan)
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
}
