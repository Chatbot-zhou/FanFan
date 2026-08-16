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
5. 纯寒暄判断 general。
6. 指代不清时判断 ambiguous，禁止为了安全直接判断 general。

无法确定时输出 ambiguous，不要直接猜 general。只有明确是通用知识或寒暄时才输出 general。

只输出：{{"source":"local"|"general"|"ambiguous","confidence":0.0到1.0之间的数字}}"#,
        question = question.trim()
    ));
    (system, user)
}

/// 解析 Source Router 输出；解析失败或 source 非法返回 None。
/// 大小写宽容（schema 约束输出小写，这里兜底 LLM 不守约束的情况）。
pub fn parse_source_routing(raw: &str) -> Option<SourceRouting> {
    let cleaned = raw
        .trim()
        .strip_prefix("```json")
        .or_else(|| raw.trim().strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(raw.trim());
    let value = serde_json::from_str::<serde_json::Value>(cleaned).ok()?;
    let source = SourceIntent::parse_lenient(value.get("source")?.as_str()?)?;
    let confidence = value
        .get("confidence")
        .and_then(|value| value.as_f64())
        .map(|value| value.clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0);
    Some(SourceRouting { source, confidence })
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
        // 技术名词不在 LOCAL 标记里：LangGraph 单独出现不会触发 local
        assert!(!user.contains("LangGraph"));
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
    fn schema_enforces_three_sources() {
        let schema = source_routing_schema();
        let sources = schema["properties"]["source"]["enum"].as_array().unwrap();
        let values: Vec<_> = sources.iter().map(|value| value.as_str().unwrap()).collect();
        assert_eq!(values, vec!["local", "general", "ambiguous"]);
        assert_eq!(schema["required"][0], "source");
        assert_eq!(schema["required"][1], "confidence");
    }
}
