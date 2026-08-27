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

use crate::AskMessage;
use crate::ask::query_plan::SourceIntent;
use crate::knowledge::fold_recent_history;

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

/// PersonalReferenceDetector（安全网角色）：识别「明确指向用户自有资料」
/// 的表达（我的/我的简历/毕业时候……）。它**不再**抢在 LLM Router 之前
/// 强制 LOCAL——路由/意图判断已全部交给模型语义理解（AI 优先）。此处仅
/// 作为 run_chat_answer 的最终幻觉保护闸：当路由/解析链路把个人问题误判
/// 为闲聊且无来源证据时，用固定 NO_EVIDENCE 拒绝文案兜底，绝不自由生成
/// 幻觉（RAG 定义错误 / 通用简历模板）。
///
/// 命中返回首个匹配的标记短语（供 trace 展示），未命中返回 None。
/// 只匹配「明确指向用户自有资料」的表达；纯技术问题绝不命中。
pub fn personal_reference_hit(question: &str) -> Option<&'static str> {
    const PERSONAL_MARKERS: &[&str] = &[
        "我的",
        "我之前",
        "我的资料",
        "我的文件",
        "我的文档",
        "我的简历",
        "我的项目",
        "我的论文",
        "我的材料",
        "我的笔记",
        "我的记录",
        "我的合同",
        "我的收藏",
        "我以前",
        "我做过",
        "我写过",
        "我毕业",
        "毕业时候",
        "那个材料",
        "那份材料",
        "那篇论文",
        "那份文件",
    ];
    let q = question.trim();
    if q.is_empty() {
        return None;
    }
    PERSONAL_MARKERS
        .iter()
        .copied()
        .find(|marker| q.contains(marker))
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

local：用户的问题明确或高度可能需要读取用户自己的文件、文档、资料库、历史资料或当前正在查看的文件才能回答；也包括领域/概念/知识类问题（如数据库、大模型、技术名词解释、考试知识点等）——这类问题应优先到用户资料库检索相关文档并以引用作答。
general：问题只需要普通聊天即可回答，与用户本地资料内容无关。包括日常寒暄、闲聊、询问助手当前状态/心情/感受、询问本应用或助手的使用方法（怎么问资料、能做什么、怎么导入文件、文件导入后进入什么流程）、点名要助手操作或调侃等与资料内容无关的话题。注意：如果一句话语义残缺、既看不出要查资料、也看不出明确的闲聊/咨询意图（例如没头没尾的祈使句），不要默认 general——应判 ambiguous 请用户澄清。
ambiguous：存在「这个、那个、里面、刚才的、之前的、第二个、它」等指代，或者仅凭当前一句无法确定是否需要本地资料，需要结合当前会话上下文判断。

判断原则（核心判别：这个问题是否需要读取用户的本地资料/文件库才能回答）：
1. 出现「我的」「我之前」「我的文件」「我的资料」「我的简历」「我的合同」「我的项目」「之前那份」「这个文件」等表达时，判断 local。
2. 用户询问某份文档中的内容，判断 local。
3. 用户询问自己过去记录、工作、学习、项目、文件中的内容，判断 local。
4. 用户用「我们的」「咱们的」指代自己的数据、资料、文档库、目录里的内容，判断 local。
5. 用户询问公司/团队/部门内部制度、报销、请假、流程、规定、申请、政策等可能保存在用户授权文档中的内容，判断 local（应优先在用户资料中查找，而不是直接当通用常识回答）。
6. 用户询问本地文件库自身的情况（有哪些文件、文件关系、内容对比、总结、相似文件），判断 local。
7. 领域/概念/知识类问题——数据库（事务、ACID、索引、范式、视图、隔离级别、分库分表等）、大模型（LLM、RAG、Agent、提示词工程、微调等）、以及任何需要解释技术名词或考察知识点的问题——一律判断 local：应优先到用户资料库检索相关文档并用引用回答，而不是直接当作通用常识；只有在用户资料中没有相关证据时才退化为通用知识。
8. 纯寒暄、日常闲聊、问候、询问助手当前状态/心情/感受、询问本应用或助手的使用方法（怎么问资料、能做什么、怎么导入文件、文件导入后接下来干嘛）等与用户资料内容无关的话题，判断 general。
9. 「你是谁」「你能做什么」「介绍一下你自己」等身份/能力问题，判断 general。
10. 只有真正存在指代不清、或语义残缺无法判断是否涉及本地资料时，才判断 ambiguous（交由澄清处理，不硬猜 local 或 general）。

示例：
- 「你好」→ general
- 「你是谁？」→ general（问助手身份，不是问资料）
- 「你能做什么？」→ general
- 「今天心情怎么样」「你忙吗」「最近怎么样」→ general（日常闲聊/询问状态）
- 「我该怎么开始问资料」「怎么导入文件」「文件已经导入好了，接下来干嘛」→ general（询问翻翻的使用流程，不是某份文件内容）
- 「你是谁写的」「这个功能是谁做的」→ general（问事物来源，不需要资料）
- 「事务的ACID特性是什么」「B+树和B树有什么区别」「SQL注入怎么避免」→ local（领域概念问题，优先查用户资料库）
- 「RAG的核心思想是什么」「AI Agent和大语言模型有什么关系」「提示词工程为什么重要」→ local（大模型概念，优先查用户资料库）
- 「SCI论文智能辅助投稿系统这个产品主要解决什么问题」→ local（点名本地产品文档，应检索其需求说明书）
- 「报销流程和请假规定是什么」→ local（公司内部制度，优先在用户文档中查找）
- 「我们的数据里有没有提到……」「哪几份文件内容差不多」→ local（问用户自己的数据/文件库）
- 「我的简历里有 LangGraph 吗」→ local（问自己资料的内容）

总原则：一切以「这个问题是否需要用你用户的本地资料/文件库才能回答」为准。需要或很可能需要用户文件、或者属于领域/概念/知识类问题（即便通用常识也可能覆盖）的判断 local——优先到用户资料中求证；确实与用户资料无关的寒暄、身份能力、翻翻产品用法、纯操作类判断 general；仅凭当前一句无法判断、或存在指代/语义残缺时判断 ambiguous（交由澄清），不去硬猜。

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
/// 思维链包裹的合法判定；提取失败才返回 None（由编排层重试 LLM，仍失败
/// 则走诚实澄清兜底——不猜意图、不进自由闲聊）。
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

/// 确定性澄清兜底：把少数高精度「语义残缺 / 指代本会话」问句强制归为 ambiguous。
///
/// 动机：0.6B/2B 模型对两类问句的来源判断不稳定——①会话记忆恢复类（「上次咱们
/// 聊到哪了」「刚才说到哪」）被 prompt 规则 4「咱们→local」误伤成 local；②缺失
/// 所指的祈使句（「帮我调一下这个参数」）被当成 general/操作。二者都不是「要读某
/// 份本地文档的内容」，而是「需要结合当前会话/上下文澄清」，强行 local 会导致无
/// 证据检索被拒、强行 general 会错答。
///
/// 判定是高精度的通用模式（非针对任何文件/关键词/case）：
/// - 会话记忆恢复：同时出现「时间指代（上次/刚才/上回/之前…）」与「对话动词
///   （聊到/说到/讨论到/谈到…）」→ 恢复上次会话，需上下文，不再按文件检索。
/// - 缺失所指祈使句：以动作/把字开头、句含「这个/那个」等指示代词、且**不含**
///   任何文档/资料指称词（文件/资料/文档/简历/报告/表格及常见扩展名）→ 没有
///   指向任何具体对象的裸命令，请用户澄清所指。
///
/// 只有命中且当前 LLM 判定不是 ambiguous 时才改写；LLM 已经判 ambiguous 的不动。
/// 若问题带明确文档对象，命中第二类会被文档词挡住，不会误伤真实资料提问。
pub fn apply_ambiguous_override(question: &str, routing: &mut SourceRouting) {
    if routing.source == SourceIntent::Ambiguous {
        return;
    }
    let q = question.trim();
    if q.is_empty() {
        return;
    }
    // 会话记忆恢复：上次/刚才 + 聊到/说到 等组合。
    let time_refs = &["上次", "刚才", "上回", "之前", "刚刚"];
    let talk_verbs = &["聊到", "说到", "讨论到", "谈到", "聊到哪", "说到哪", "讲到"];
    let is_memory_resume = time_refs
        .iter()
        .any(|marker| q.contains(marker))
        && talk_verbs.iter().any(|marker| q.contains(marker));

    // 缺失所指祈使句：动作/把字开头 + 指示代词 + 无文档对象词。
    const ACTION_STARTERS: &[&str] = &["帮我", "请帮我", "给我", "帮我调", "帮我改", "帮我处理", "请把", "帮我把", "把"];
    const DEICTIC_WORDS: &[&str] = &["这个", "那个"];
    const DOCUMENT_WORDS: &[&str] = &[
        "文件", "资料", "文档", "简历", "报告", "表格", "合同", "论文", "幻灯片",
        ".pdf", ".md", ".doc", ".docx", ".txt", ".ppt", ".pptx", ".xlsx",
    ];
    let is_deictic_command = ACTION_STARTERS.iter().any(|marker| q.starts_with(marker))
        && DEICTIC_WORDS.iter().any(|marker| q.contains(marker))
        && !DOCUMENT_WORDS.iter().any(|marker| q.contains(marker));

    // 缺上下文省略/指代：过去时指代追认 / 裸指示代词语义追认 / 极简裸话题省略。
    let is_ellipsis_reference = ellipsis_reference_hint(q).is_some();

    if is_memory_resume || is_deictic_command || is_ellipsis_reference {
        routing.source = SourceIntent::Ambiguous;
        routing.confidence = (routing.confidence * 0.5 + 0.5).clamp(0.0, 0.7);
    }
}

/// 省略/指代问句的通用高精度信号（兜底第三类）。
///
/// 动机：小模型常把「缺前文的省略/指代问句」误判为 local（当资料检索，
/// 无证据被拒→拒答/错答）或 general（当闲聊，编造上文没有的内容）。这类
/// 句子是「指向上文话题/资料」，脱离当前会话无法独立定位，应判 AMBIGUOUS
/// 交由 Context Resolver 处理——有会话上下文就恢复目标文件继续，无上下文
/// 就诚实澄清。只收通用句式，不针对任何具体文件/关键词/case。
///
/// 三类各是高精度、覆盖面窄的信号：
///   A. 过去时指代追认：过去时间限定（之前/刚才/上回…）与指示限定（那个/
///      那份/这个/这份…）共现，整句指向历史会话提到的话题或资料（如
///      「之前那个大模型项目呢」「刚才那份资料里有没有讲怎么部署」）。脱离
///      上下文无法定位所指对象。
///   B. 裸指示代词语义追认：整句只靠「就那个/那个呢/这个呢/就是它」等裸
///      指代指向对象（如「对，就那个。区别在哪？」）。带实体限定语的
///      「<实体>那个<名词>」（如「SCI论文那个产品规划书」）不命中——它有
///      独立定位信息，属于找文件，不属于缺上下文。
///   C. 极简裸话题省略：极短全中文名词短语 + 句末「呢」（如「项目经历呢？」）
///      是省略谓语的追认问，无主语/动作/明确对象，脱离上文无法确定所指。
fn ellipsis_reference_hint(question: &str) -> Option<&'static str> {
    let q = question.trim();

    // A. 过去时指代追认：过去词 + 指示限定词共现。
    const PAST_WORDS: &[&str] = &["之前", "刚才", "上回", "上周", "那次", "上次", "刚刚", "昨天"];
    const DEICTIC_LIMITER: &[&str] = &["那个", "那份", "这个", "这份", "几个", "些"];
    if PAST_WORDS
        .iter()
        .any(|marker| q.contains(marker))
        && DEICTIC_LIMITER.iter().any(|marker| q.contains(marker))
    {
        return Some("past_deictic");
    }

    // B. 裸指示代词语义追认：只含一个无实体限定的裸指代短语。
    const BARE_DEICTIC: &[&str] = &[
        "就那个", "就是那个", "这个呢", "那个呢", "就是它", "那个的是", "就这个",
    ];
    if BARE_DEICTIC.iter().any(|marker| q.contains(marker)) {
        return Some("bare_deictic");
    }

    // C. 极简裸话题省略：全中文名词短语 + 句末「呢」，剔除疑问词/人称代词/闲聊词。
    //    先剥句末标点（。？!！）再判「呢」，避免「项目经历呢？」带问号时剥不掉。
    const ELLIPSIS_PROHIBITED: &[&str] = &[
        "什么", "怎么", "为什么", "多少", "谁", "哪", "几", "你", "我", "他", "她", "它",
        "你们", "我们", "他们", "她们", "它们", "咱", "嗯", "好", "对",
    ];
    let body = q.trim_end_matches(['。', '？', '!', '！', ' ']);
    if let Some(rest) = body.strip_suffix('呢') {
        let stripped = rest.trim();
        let len_ok = (2..=7).contains(&stripped.chars().count());
        let all_cjk = stripped
            .chars()
            .all(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        let prohibited = ELLIPSIS_PROHIBITED.iter().any(|marker| stripped.contains(marker));
        if len_ok && all_cjk && !prohibited {
            return Some("bare_ellipsis");
        }
    }

    None
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
            b'}' if depth > 0 => {
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
        // None，编排层重试 LLM，仍失败则走诚实澄清兜底（不猜意图）；
        // 唯一合法的 general 来自模型明确输出 source=general。
        for raw in [
            "",
            "not json",
            "{\"source\":\"general\"",
            "timeout: no output",
        ] {
            assert!(
                parse_source_routing(raw).is_none(),
                "超时/垃圾不得解析成功: {raw:?}"
            );
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
        let history = vec![
            message("user", "看看我的简历"),
            message("assistant", "好的"),
        ];
        let (system, user) = source_router_prompt("第二个项目是什么", &history);
        assert!(system.contains("信息来源路由器"));
        for keyword in ["local", "general", "ambiguous"] {
            assert!(user.contains(keyword), "缺少分类定义 {keyword}");
        }
        // 旧的「无法确定 → chat」语义必须不存在
        assert!(!user.contains("宁可闲聊"));
        // 新语义：无法确定 → ambiguous（交由澄清）
        assert!(user.contains("判断 ambiguous"));
        // 历史折叠段 + 当前问题段
        assert!(user.contains("【对话历史】"));
        assert!(user.contains("用户说：第二个项目是什么"));
        // 输出约束含 confidence
        assert!(user.contains("confidence"));
    }

    #[test]
    fn prompt_routes_tech_concept_questions_to_local() {
        // 决策：领域/概念/知识类问题优先搜用户本地资料，不直接当通用常识。
        // prompt 必须：
        // - 明确「领域/概念/知识类问题一律 local」；
        // - 不因问题里出现技术名词就把问题判 local 的唯一依据是技术名词，而是
        //   依赖「概念类问题走 local」这一类别级语义（LOCAL 标记词表仍只有
        //   「我的/资料/文件/简历/合同」等自有文件表达，不含技术名词）。
        let (_, user) = source_router_prompt("Transformer 和 RNN 的区别是什么？", &[]);
        // 概念/知识类问题 → local（第7条类别级语义）
        assert!(user.contains("领域/概念/知识类问题"));
        assert!(user.contains("优先到用户资料库检索相关文档"));
        // 纯寒暄 / 产品用法 → general（第8条仍必须存在）
        assert!(user.contains("纯寒暄"));
        // LOCAL 标记词表覆盖「我的资料」等表达
        assert!(user.contains("我的资料"));
        assert!(user.contains("我的简历"));
        // 技术名词不在 LOCAL 标记里：LangGraph 只作为「本地问题示例」的
        // 内容词出现（「我的简历里有 LangGraph 吗」→ local，判定依据是
        // 「我的简历」而非技术名词）
        assert!(user.contains("我的简历里有 LangGraph 吗"));
        assert!(
            user.lines()
                .all(|line| { !line.contains("LangGraph") || line.contains("我的") })
        );
        // 「无法确定 / 语义残缺」只能走 ambiguous 澄清，不得硬猜
        assert!(user.contains("语义残缺"));
        assert!(user.contains("指代不清"));
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
        let values: Vec<_> = sources
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(values, vec!["local", "general", "ambiguous"]);
        assert_eq!(schema["required"][0], "source");
        assert_eq!(schema["required"][1], "confidence");
    }

    #[test]
    fn ambiguous_override_catches_memory_resume_and_deictic_command() {
        // 会话记忆恢复：上次 + 聊到 → ambiguous
        let mut r = parse_source_routing(r#"{"source":"local","confidence":0.9}"#).unwrap();
        apply_ambiguous_override("上次咱们聊到哪了？帮我想起来", &mut r);
        assert_eq!(r.source, SourceIntent::Ambiguous);

        // 缺失所指祈使句：帮我 + 这个 + 无文档词 → ambiguous
        let mut r = parse_source_routing(r#"{"source":"general","confidence":0.8}"#).unwrap();
        apply_ambiguous_override("帮我调一下这个参数", &mut r);
        assert_eq!(r.source, SourceIntent::Ambiguous);
    }

    #[test]
    fn ambiguous_override_keeps_document_bearing_questions_intact() {
        // 带明确文档对象的把字句：把那个文件删掉 → 保留原路由，不拉成 ambiguous
        let mut r = parse_source_routing(r#"{"source":"local","confidence":0.9}"#).unwrap();
        apply_ambiguous_override("把那个文件删掉", &mut r);
        assert_eq!(r.source, SourceIntent::Local);

        // 普通闲聊不带指示代词：帮我介绍一下自己 → 不命中
        let mut r = parse_source_routing(r#"{"source":"general","confidence":0.9}"#).unwrap();
        apply_ambiguous_override("帮我介绍一下你自己", &mut r);
        assert_eq!(r.source, SourceIntent::General);

        // 已 ambiguous 的不再改写
        let mut r = parse_source_routing(r#"{"source":"ambiguous","confidence":0.5}"#).unwrap();
        apply_ambiguous_override("上次咱们聊到哪了", &mut r);
        assert_eq!(r.source, SourceIntent::Ambiguous);
    }

    #[test]
    fn ambiguous_override_is_noop_on_clear_content_questions() {
        // 明确资料正文问句不被误伤（无 上次+聊到、无 帮我+这个）
        let mut r = parse_source_routing(r#"{"source":"local","confidence":0.9}"#).unwrap();
        apply_ambiguous_override("事务的ACID特性分别是什么意思", &mut r);
        assert_eq!(r.source, SourceIntent::Local);
        // 普通询问不带指示代词即便以泛指开头
        let mut r = parse_source_routing(r#"{"source":"local","confidence":0.9}"#).unwrap();
        apply_ambiguous_override("帮我查一下分库分表的原理", &mut r);
        assert_eq!(r.source, SourceIntent::Local);
    }

    #[test]
    fn ambiguous_override_catches_ellipsis_and_reference_questions() {
        // 缺前文的省略/指代问句：小模型常误判为 local（当资料检索无证据拒答）
        // 或 general（当闲聊编造）。应统一拉回 ambiguous，交由 Context Resolver
        // 决定「有上下文→恢复，无上下文→澄清」。对应真实评测 [21][22][23][25]。
        for question in [
            "之前那个大模型项目呢？",       // 过去时指代追认
            "刚才那份资料里有没有讲怎么部署？", // 过去时指代追认
            "对，就那个。区别在哪？",       // 裸指示代词语义追认
            "项目经历呢？",               // 极简裸话题省略
        ] {
            let mut r = parse_source_routing(r#"{"source":"local","confidence":0.9}"#).unwrap();
            apply_ambiguous_override(question, &mut r);
            assert_eq!(
                r.source,
                SourceIntent::Ambiguous,
                "{question} 应按省略/指代缺上下文判 ambiguous"
            );
        }
    }

    #[test]
    fn ambiguous_override_keeps_explicit_local_questions_intact() {
        // 带明确实体定位 / 明确内容词的资料问句，不得被新「省略/指代识别」误伤。
        for question in [
            "SCI 论文那个产品规划书是哪个文件？",   // 实体限定语「<实体>那个<名词>」属找文件
            "大模型应用开发手册都分了哪些章节？",    // 明确文档名 + 结构问
            "我的资料里有没有大模型项目相关的内容？", // 明确内容词
            "资料里有 2024 年数据库系统工程师真题吗？", // 明确主题 + 年份
            "归航计划的负责人是谁？",              // 明确实体查询（非省略）
        ] {
            let mut r = parse_source_routing(r#"{"source":"local","confidence":0.9}"#).unwrap();
            apply_ambiguous_override(question, &mut r);
            assert_eq!(
                r.source,
                SourceIntent::Local,
                "{question} 含明确对象，不应被拉成 ambiguous"
            );
        }
    }

    #[test]
    fn personal_reference_detector_hits_case_a_through_d() {
        // Phase 4.3 CASE A/B/C/D：自有资料表达必须在 LLM Router 之前命中，
        // 强制 LOCAL（trace 实测思维链截断 → parse_failed → chat 幻觉）
        for question in [
            "我的资料里是怎么介绍 RAG 的？",      // CASE A
            "我的简历主要写了什么？",             // CASE D
            "我的文件里有没有提到 Transformer？", // CASE C（我的 + 存在性双命中）
            "我以前有没有做过 Agent 项目？",      // CASE B
            "我毕业时候那个材料在哪",             // CASE 5
            "那个材料讲了什么",                   // 指代式自有资料
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
