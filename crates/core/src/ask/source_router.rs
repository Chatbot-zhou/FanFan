//! Source Router：信息来源路由（LOCAL / GENERAL / AMBIGUOUS）。
//!
//! 取代旧的 retrieval/chat 二分类。核心语义变化：
//! - 「无法确定 → 闲聊」废弃；「无法确定 → AMBIGUOUS」，由 Context Resolver
//!   结合会话上下文再判断；
//! - LOCAL 与 GENERAL 的判断与「是否检索到证据」完全解耦——LOCAL 请求
//!   即使无证据也绝不转闲聊。
//!
//! Prompt 与解析器集中在本模块，编排层只负责调用。

use serde::{Deserialize, Serialize};

use crate::ask::query_plan::SourceIntent;
use crate::knowledge::fold_recent_history;
use crate::AskMessage;

/// Source Router 的输出。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SourceRouting {
    pub source: SourceIntent,
    pub confidence: f32,
}

/// 输出 JSON Schema：`{"source": "local|general|ambiguous", "confidence": 0.0}`。
/// 供 llama.cpp 侧约束解码（response_format json_schema），解析器不猜字符串。
pub fn source_routing_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["source", "confidence"],
        "properties": {
            "source": {
                "type": "string",
                "enum": ["local", "general", "ambiguous"]
            },
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0}
        }
    })
}

/// 确定性寒暄/助手身份 fast-path（CASE 1：你好/你是谁/你能做什么 必须稳定 GENERAL）。
/// 只覆盖**最明确**的寒暄与助手身份问题；句子含「我的/资料/文件/简历/合同/
/// 项目/论文/材料/记录」等自有资料表达时不走此路径（交给 LLM Router 判断），
/// 避免误伤 LOCAL。命中即 GENERAL——这是 Router 之上的确定性快路径，
/// 不是把 Router 换成白名单。
pub fn fast_path_greeting(question: &str) -> Option<SourceIntent> {
    const LOCAL_MARKERS: &[&str] = &[
        "我的", "资料", "文件", "文档", "简历", "合同", "知识库", "收藏",
        "项目", "论文", "材料", "记录", "笔记", "课件", "报告",
    ];
    const GREETINGS: &[&str] = &[
        "你好", "您好", "哈喽", "嗨", "hello", "hi", "hey", "早上好", "中午好",
        "下午好", "晚上好", "早安", "晚安", "谢谢", "再见", "拜拜", "辛苦了", "感谢",
    ];
    const IDENTITY_PHRASES: &[&str] = &[
        "你是谁", "你叫什么名字", "你叫什么", "你能做什么", "你是做什么的", "你是什么",
        "你有什么功能", "你能干什么", "介绍一下你自己", "你会什么",
        "你有哪些功能", "你是什么助手", "你的名字是什么",
    ];
    let q = question.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    // 带自有资料表达的句子一律不走 fast-path（可能问「你好，我的简历在哪」）
    if LOCAL_MARKERS.iter().any(|marker| q.contains(marker)) {
        return None;
    }
    // 身份短语必须「整问句命中」：短语后只允许标点/语气词。否则
    // 「你是谁提出的」「你能做什么项目吗」这类问题会误走 fast-path——
    // 它们是内容性问题，交给 LLM Router 判断。
    let identity_hit = IDENTITY_PHRASES.iter().any(|phrase| {
        q.strip_prefix(phrase).is_some_and(|tail| {
            tail.chars().all(|c| {
                matches!(c, '？' | '?' | '！' | '!' | '。' | '，' | ' ' | '吗' | '呢' | '呀' | '啊' | '啦')
            })
        })
    });
    if identity_hit {
        return Some(SourceIntent::General);
    }
    // 纯寒暄：整句等于寒暄词，或寒暄词 + 少量语气词（你好呀/嗨喽/hello!)
    let stripped = q.trim_matches(|c: char| !c.is_alphanumeric() && c != '!');
    let greeting_hit = GREETINGS.iter().any(|greeting| {
        if stripped == *greeting {
            return true;
        }
        stripped
            .strip_prefix(greeting)
            .is_some_and(|tail| {
                tail.chars().all(|c| matches!(c, '呀' | '啊' | '哈' | '啦' | '嘛' | '哦' | '哟' | '好'))
                    && tail.chars().count() <= 3
            })
    });
    if greeting_hit {
        return Some(SourceIntent::General);
    }
    None
}

/// PersonalReferenceDetector（Phase 4.3 CASE 1）：Router 前置的确定性
/// 自有资料表达检测。LLM Router 在推理模型（DeepSeek-R1 / Qwen3.5 等
/// thinking 模型）上会先输出长思维链，48 token 截断后 JSON 解析必然失败
/// （trace 实测 routing_raw 全是「好，我现在需要判断……」），parse_failed
/// 的兜底曾直接进 Chat 自由生成——「我的资料里是怎么介绍 RAG 的？」被
/// 回答成递归架构幻觉。本检测在 LLM Router **之前**命中即强制 LOCAL，
/// 彻底绕开小模型的路由不可靠性；与 fast_path_greeting（General 方向）
/// 互为镜像，都是 Router 之上的确定性快路径。
///
/// 命中返回首个匹配的标记短语（供 trace 展示），未命中返回 None。
/// 只匹配「明确指向用户自有资料」的表达；纯技术问题绝不命中。
pub fn personal_reference_hit(question: &str) -> Option<&'static str> {
    const PERSONAL_MARKERS: &[&str] = &[
        "我的", "我之前", "我的资料", "我的文件", "我的文档", "我的简历",
        "我的项目", "我的论文", "我的材料", "我的笔记", "我的记录", "我的合同",
        "我的收藏", "我以前", "我做过", "我写过", "我毕业", "毕业时候",
        "那个材料", "那份材料", "那篇论文", "那份文件",
    ];
    let q = question.trim();
    if q.is_empty() {
        return None;
    }
    PERSONAL_MARKERS.iter().copied().find(|marker| q.contains(marker))
}

/// 存在性查询检测（Phase 4.3 CASE 2/3）：「有没有做过 / 是否做过 / 之前
/// 做过 / 有没有提到」等句式统一为 existence 意图。这类问句的期望答案
/// 是「有/没有 + 文件依据」，绝不能让 LLM 展开概念解释（「Agent 是什么」
/// 「自动驾驶 Agent」都是泛知识污染）。规则判定不依赖 LLM。
pub fn existence_query_hit(question: &str) -> bool {
    const EXISTENCE_PATTERNS: &[&str] = &[
        "有没有做过", "是否做过", "之前做过", "以前做过", "有没有项目",
        "有没有提到", "有没有提及", "有没有写", "有没有说", "有没有涉及",
        "有没有出现", "有没有包含", "有没有关于", "是否提到", "是否包含",
        "是否出现", "写过没有", "做过没有", "有没有相关",
    ];
    let q = question.trim();
    !q.is_empty() && EXISTENCE_PATTERNS.iter().any(|pattern| q.contains(pattern))
}

/// 构建 Source Router prompt。
/// system 说明角色与输出约束；user 含最近 5+5 折叠历史（仅作上下文）、
/// 三类来源定义、判断原则与输出格式。
pub fn source_router_prompt(question: &str, history: &[AskMessage]) -> (String, String) {
    let system = "你是翻翻的「信息来源路由器」。你的任务不是回答问题，而是判断回答用户当前问题是否需要使用用户的本地资料。只输出符合 JSON Schema 的对象，不要解释。"
        .into();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!(
            "【对话历史】以下是最近 5 条对话的历史记录（用于判断当前问题是否延续上文），不是用户现在说的话：\n{folded}\n\n"
        ));
    }
    user.push_str(&format!(
        r#"【当前问题】下面这一句才是用户刚刚说的最新一句话，请只根据这一句判断信息来源：
用户说：{question}

分类只有三种：

local：用户的问题明确或高度可能需要读取用户自己的文件、文档、资料库、历史资料或当前正在查看的文件才能回答。
general：问题可以作为普通聊天或通用知识问题回答，不需要读取用户本地资料。
ambiguous：存在「这个、那个、里面、刚才的、之前的、第二个、它」等指代，或者仅凭当前一句无法确定是否需要本地资料，需要结合当前会话上下文判断。

判断原则：
1. 出现「我的」「我之前」「我的文件」「我的资料」「我的简历」「我的合同」「我的项目」「之前那份」「这个文件」等表达时，优先判断 local。
2. 用户询问某份文档中的内容，判断 local。
3. 用户询问自己过去记录、工作、学习、项目、文件中的内容，判断 local。
4. 通用知识问题判断 general。
5. 纯寒暄判断 general；「你是谁」「你能做什么」「你叫什么」「介绍一下你自己」等询问助手身份或能力的问题也是 general（与本地资料无关，直接回答即可）。
6. 指代不清时判断 ambiguous，禁止为了安全直接判断 general。

示例：
- 「你好」→ general
- 「你是谁？」→ general（问助手身份，不是问资料）
- 「你能做什么？」→ general
- 「你是谁写的」「这个功能是谁做的」→ general（问事物来源，不需要资料）
- 「我的简历里有 LangGraph 吗」→ local（问自己资料的内容）

无法确定时输出 ambiguous，不要直接猜 general。只有明确是通用知识或寒暄时才输出 general。

只输出：{{"source":"local"|"general"|"ambiguous","confidence":0.0到1.0之间的数字}}"#,
        question = question.trim()
    ));
    (system, user)
}

/// 解析 Source Router 输出；解析失败或 source 非法返回 None。
/// 大小写宽容（schema 约束输出小写，这里兜底 LLM 不守约束的情况）。
///
/// Phase 4.3 增强：推理模型（DeepSeek-R1 / Qwen3.5）常在 JSON 前输出
/// 思维链文本（「好，我现在需要判断……{"source":"local",...}」）。整段
/// `from_str` 失败时降级为**提取首个平衡的 JSON 对象**再解析，救回被
/// 思维链包裹的合法判定；提取失败才返回 None（parse_failed 兜底逻辑
/// 由编排层的 PersonalReferenceDetector 与 existence 规则接管）。
pub fn parse_source_routing(raw: &str) -> Option<SourceRouting> {
    let parse_value = |text: &str| -> Option<serde_json::Value> {
        let cleaned = text
            .trim()
            .strip_prefix("```json")
            .or_else(|| text.trim().strip_prefix("```"))
            .and_then(|s| s.strip_suffix("```"))
            .map(str::trim)
            .unwrap_or(text.trim());
        serde_json::from_str::<serde_json::Value>(cleaned).ok()
    };
    let value = parse_value(raw).or_else(|| extract_first_json_object(raw))?;
    let source = SourceIntent::parse_lenient(value.get("source")?.as_str()?)?;
    let confidence = value
        .get("confidence")
        .and_then(|value| value.as_f64())
        .map(|value| value.clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0);
    Some(SourceRouting { source, confidence })
}

/// 从混合文本中提取首个「花括号平衡」的 JSON 对象子串并解析。
/// 思维链中出现的 `{` 若未闭合会被跳过，继续向后找；只接受能完整
/// 解析为合法 JSON 的片段，避免把正文里的孤立花括号当成 JSON。
fn extract_first_json_object(raw: &str) -> Option<serde_json::Value> {
    let bytes = raw.as_bytes();
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                    if depth == 0
                        && let Some(begin) = start
                        && let Ok(value) =
                            serde_json::from_str::<serde_json::Value>(&raw[begin..=index])
                    {
                        return Some(value);
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> AskMessage {
        AskMessage {
            message_id: uuid::Uuid::now_v7(),
            session_id: uuid::Uuid::now_v7(),
            role: role.to_owned(),
            content: content.to_owned(),
            answer: None,
            error: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn parses_valid_json_with_all_sources() {
        for (source, expected) in [
            ("local", SourceIntent::Local),
            ("general", SourceIntent::General),
            ("ambiguous", SourceIntent::Ambiguous),
        ] {
            let raw = format!(r#"{{"source":"{source}","confidence":0.9}}"#);
            let routing = parse_source_routing(&raw).expect("valid routing parses");
            assert_eq!(routing.source, expected);
            assert!((routing.confidence - 0.9).abs() < 1e-6);
        }
    }

    #[test]
    fn confidence_is_clamped_and_defaulted() {
        // 越界钳制
        let routing = parse_source_routing(r#"{"source":"local","confidence":3.0}"#).unwrap();
        assert_eq!(routing.confidence, 1.0);
        let routing = parse_source_routing(r#"{"source":"local","confidence":-1.0}"#).unwrap();
        assert_eq!(routing.confidence, 0.0);
        // 缺失 confidence → 默认 0.0
        let routing = parse_source_routing(r#"{"source":"local"}"#).unwrap();
        assert_eq!(routing.confidence, 0.0);
    }

    #[test]
    fn tolerant_of_case_and_code_fences() {
        // schema 约束输出小写，LLM 不守时解析器兜底
        assert_eq!(
            parse_source_routing(r#"{"source":"LOCAL","confidence":0.8}"#)
                .unwrap()
                .source,
            SourceIntent::Local
        );
        let fenced = "```json\n{\"source\":\"ambiguous\",\"confidence\":0.6}\n```";
        assert_eq!(
            parse_source_routing(fenced).unwrap().source,
            SourceIntent::Ambiguous
        );
    }

    #[test]
    fn rejects_invalid_or_empty_output() {
        assert!(parse_source_routing(r#"{"source":"chat"}"#).is_none());
        assert!(parse_source_routing(r#"{"intent":"local"}"#).is_none());
        assert!(parse_source_routing("").is_none());
        assert!(parse_source_routing("not json").is_none());
    }

    #[test]
    fn timeout_or_garbage_never_maps_to_general() {
        // 边界 (9)：Router 超时/垃圾输出绝不静默当 GENERAL——解析失败返回
        // None，编排层走「routing_parse_failed → chat」显式 trace 回退或直接
        // 上抛错误；唯一合法的 general 来自模型明确输出 source=general。
        for raw in ["", "not json", "{\"source\":\"general\"", "timeout: no output"] {
            assert!(parse_source_routing(raw).is_none(), "超时/垃圾不得解析成功: {raw:?}");
        }
        // 明确输出 general 才可能当 chat
        assert_eq!(
            parse_source_routing(r#"{"source":"general","confidence":0.9}"#)
                .unwrap()
                .source,
            SourceIntent::General
        );
    }

    #[test]
    fn prompt_defines_three_sources_and_ambiguous_default() {
        let history = vec![message("user", "看看我的简历"), message("assistant", "好的")];
        let (system, user) = source_router_prompt("第二个项目是什么", &history);
        assert!(system.contains("信息来源路由器"));
        for keyword in ["local", "general", "ambiguous"] {
            assert!(user.contains(keyword), "缺少分类定义 {keyword}");
        }
        // 旧的「无法确定 → chat」语义必须不存在
        assert!(!user.contains("宁可闲聊"));
        // 新语义：无法确定 → ambiguous
        assert!(user.contains("无法确定时输出 ambiguous"));
        // 历史折叠段 + 当前问题段
        assert!(user.contains("【对话历史】"));
        assert!(user.contains("用户说：第二个项目是什么"));
        // 输出约束含 confidence
        assert!(user.contains("confidence"));
    }

    #[test]
    fn prompt_routes_tech_general_knowledge_to_general() {
        // CASE 2：通用技术问题（Transformer）→ general。prompt 必须：
        // - 明确「通用知识问题判断 general」；
        // - 不因问题里出现技术名词/项目名就把问题判 local（LOCAL 标记词表
        //   只有「我的/资料/文件/简历/合同」等自有文件表达，不含技术名词）。
        let (_, user) = source_router_prompt("Transformer 和 RNN 的区别是什么？", &[]);
        assert!(user.contains("通用知识问题判断 general"));
        assert!(user.contains("纯寒暄判断 general"));
        // LOCAL 标记词表覆盖「我的资料」等表达（CASE 3 的来源判断依据）
        assert!(user.contains("我的资料"));
        assert!(user.contains("我的简历"));
        // 技术名词不在 LOCAL 标记里：LangGraph 只作为「本地问题示例」的
        // 内容词出现（「我的简历里有 LangGraph 吗」→ local，判定依据是
        // 「我的简历」而非技术名词）
        assert!(user.contains("我的简历里有 LangGraph 吗"));
        assert!(user.lines().all(|line| {
            !line.contains("LangGraph") || line.contains("我的")
        }));
        // 「无法确定」只能走 ambiguous，禁止猜 general（CASE 2 的另一半语义）
        assert!(user.contains("禁止为了安全直接判断 general"));
    }

    #[test]
    fn parses_general_verdict_for_tech_question() {
        // CASE 2 的确定性部分：模型输出 general 判定时解析正确
        let routing =
            parse_source_routing(r#"{"source":"general","confidence":0.95}"#).expect("parses");
        assert_eq!(routing.source, SourceIntent::General);
        assert!((routing.confidence - 0.95).abs() < 1e-6);
    }

    #[test]
    fn fast_path_routes_greetings_to_general() {
        // CASE 1：寒暄必须稳定 GENERAL（不依赖 0.6B 模型分类）
        for question in [
            "你好",
            "您好",
            "你好呀",
            "嗨",
            "hello",
            "早上好",
            "谢谢",
            "拜拜",
            "哈喽！",
        ] {
            assert_eq!(
                fast_path_greeting(question),
                Some(SourceIntent::General),
                "{question} 应走寒暄 fast-path"
            );
        }
    }

    #[test]
    fn fast_path_routes_identity_questions_to_general() {
        // CASE 1：助手身份问题（你是谁？你能做什么？）必须 GENERAL
        for question in [
            "你是谁？",
            "你是谁",
            "你叫什么名字",
            "你能做什么？",
            "你是做什么的",
            "介绍一下你自己",
            "你有什么功能",
            "你会什么",
        ] {
            assert_eq!(
                fast_path_greeting(question),
                Some(SourceIntent::General),
                "{question} 应走身份 fast-path"
            );
        }
    }

    #[test]
    fn fast_path_never_swallows_local_questions() {
        // 带自有资料表达的句子绝不走 fast-path（误伤 LOCAL 是更严重的错误）
        for question in [
            "你好，我的简历在哪",
            "你是谁，我的资料呢",
            "你能做什么项目吗",
            "你好，帮我找一下文件",
            "我的资料里有没有提到你",
        ] {
            assert_eq!(
                fast_path_greeting(question),
                None,
                "{question} 含自有资料表达，不得走 fast-path"
            );
        }
    }

    #[test]
    fn fast_path_does_not_catch_tech_questions() {
        // 技术问题与普通疑问句不误命中
        for question in [
            "Transformer 是什么",
            "RAG 和微调的区别",
            "帮我解释 LangGraph",
            "你是谁提出的",
        ] {
            assert_eq!(fast_path_greeting(question), None, "{question} 不应命中 fast-path");
        }
    }

    #[test]
    fn prompt_routes_identity_questions_to_general() {
        // CASE 1 的 prompt 强化：助手身份问题明确 general
        let (_, user) = source_router_prompt("你是谁？", &[]);
        assert!(user.contains("你是谁"));
        assert!(user.contains("你能做什么"));
        assert!(user.contains("问助手身份，不是问资料"));
    }

    #[test]
    fn schema_enforces_three_sources() {
        let schema = source_routing_schema();
        let sources = schema["properties"]["source"]["enum"].as_array().unwrap();
        let values: Vec<_> = sources.iter().map(|value| value.as_str().unwrap()).collect();
        assert_eq!(values, vec!["local", "general", "ambiguous"]);
        assert_eq!(schema["required"][0], "source");
        assert_eq!(schema["required"][1], "confidence");
    }

    #[test]
    fn personal_reference_detector_hits_case_a_through_d() {
        // Phase 4.3 CASE A/B/C/D：自有资料表达必须在 LLM Router 之前命中，
        // 强制 LOCAL（trace 实测思维链截断 → parse_failed → chat 幻觉）
        for question in [
            "我的资料里是怎么介绍 RAG 的？",   // CASE A
            "我的简历主要写了什么？",           // CASE D
            "我的文件里有没有提到 Transformer？", // CASE C（我的 + 存在性双命中）
            "我以前有没有做过 Agent 项目？",     // CASE B
            "我毕业时候那个材料在哪",           // CASE 5
            "那个材料讲了什么",                 // 指代式自有资料
        ] {
            assert!(
                personal_reference_hit(question).is_some(),
                "{question} 应命中 PersonalReferenceDetector"
            );
        }
    }

    #[test]
    fn personal_reference_detector_never_hits_tech_or_chat() {
        // 纯技术问题 / 寒暄绝不命中（否则 GENERAL 会被误伤为 LOCAL）
        for question in [
            "LangGraph 是什么",
            "RAG 和微调的区别",
            "Transformer 原理",
            "你好",
            "你是谁",
            "什么是检索增强生成",
        ] {
            assert!(
                personal_reference_hit(question).is_none(),
                "{question} 不应命中 PersonalReferenceDetector"
            );
        }
    }

    #[test]
    fn existence_query_hit_covers_case_b_and_c() {
        // Phase 4.3 CASE B/C：存在性问句规则命中（有/没有 + 文件依据格式）
        for question in [
            "我以前有没有做过 Agent 项目？",
            "是否做过大模型项目",
            "之前有没有提到 LangGraph",
            "有没有写过论文",
            "有没有相关的项目记录",
            "我的文件里有没有提到 Transformer？",
        ] {
            assert!(existence_query_hit(question), "{question} 应命中存在性问句");
        }
        // 清单式 / 概念式 / 寒暄不是存在性问句
        for question in [
            "我的简历里有哪些项目",
            "把项目名称提取出来",
            "LangGraph 是什么",
            "你好",
        ] {
            assert!(
                !existence_query_hit(question),
                "{question} 不是存在性问句"
            );
        }
    }

    #[test]
    fn parse_source_routing_extracts_json_from_thinking_preamble() {
        // Phase 4.3 CASE 1 根因：DeepSeek-R1/Qwen3.5 输出思维链前缀 +
        // 合法 JSON。整段 from_str 失败时必须提取首个平衡 JSON 对象救回。
        let raw = "好，我现在需要判断这个问题的信息来源。用户提到了自己的资料，\
                   所以应该是本地资料查询。{\"source\":\"local\",\"confidence\":0.9}";
        let routing = parse_source_routing(raw).expect("思维链前缀后的 JSON 必须被提取");
        assert_eq!(routing.source, SourceIntent::Local);
        assert!((routing.confidence - 0.9).abs() < 1e-6);

        // 孤立未闭合的 { 不干扰：跳过后继续找平衡对象
        let raw_mixed = "思考 { 这个不算 } 然后 {\"source\":\"general\",\"confidence\":0.8} 结尾";
        let routing = parse_source_routing(raw_mixed).expect("跳过干扰花括号后提取");
        assert_eq!(routing.source, SourceIntent::General);

        // 纯思维链无 JSON → None（由确定性规则接管）
        assert!(parse_source_routing("用户在问资料，我需要判断一下来源。").is_none());
    }
}
