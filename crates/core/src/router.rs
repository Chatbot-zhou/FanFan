//! 意图路由：共享路由类型与纯寒暄白名单。
//!
//! 路由决策主体是生成模型的 LLM 直路由（见 [`crate::knowledge::intent_routing_prompt`]），
//! 本模块只提供路由决策共享类型（[`Intent`]/[`RouteDecision`]）与确定性分流
//! （`is_pure_greeting` 白名单、`truncate_text` 文本截断）。

use serde::{Deserialize, Serialize};

/// 路由意图
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// 用户想检索本地资料库
    Retrieval,
    /// 闲聊/寒暄/与资料无关，直接对话
    Chat,
    /// 语义路由拿不准，需要生成模型仲裁
    Ambiguous,
}

/// 路由决策结果
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteDecision {
    /// 最终决策（Ambiguous 表示需要仲裁）
    pub intent: Intent,
    /// 语义路由原始 argmax 类别（仅 Retrieval | Chat）
    pub top_category: Intent,
    /// 最高余弦相似度
    pub top_score: f32,
    /// top_score 与次类别最高分的差值
    pub margin: f32,
}

/// 路由文本截断长度，超长按字符截断
pub const ROUTE_TEXT_LIMIT: usize = 500;

/// 纯寒暄白名单：整句精确匹配（允许结尾标点），≤8 字直接判定为闲聊。
/// 必须在语义路由之前检查——寒暄句的 BGE 向量落在模型先验方向上，
/// 检索示例反而更近（实测「你好」vs chat 0.466 < 0.55 进不了 Chat），
/// 交给仲裁又会被 0.6B 的检索偏置带偏。白名单只在「整句就是寒暄」
/// 时命中，不会误伤「你好，查一下报销流程」这类带真实诉求的问句。
pub fn is_pure_greeting(query: &str) -> bool {
    let trimmed = query.trim();
    let mut chars = 0;
    let mut core_end = trimmed.len();
    for (index, ch) in trimmed.char_indices() {
        match ch {
            '!' | '！' | '.' | '。' | '?' | '？' | '～' | '~' | '…' | ' ' => {}
            _ => {
                chars += 1;
                core_end = index + ch.len_utf8();
            }
        }
    }
    if chars == 0 || chars > 8 {
        return false;
    }
    matches!(
        trimmed[..core_end].to_ascii_lowercase().as_str(),
        "你好" | "您好" | "嗨" | "哈喽" | "hello" | "hi" | "在吗" | "在干嘛"
            | "干嘛呢" | "吃了吗" | "谢谢" | "谢谢你" | "谢谢您" | "多谢" | "感谢"
            | "再见" | "拜拜" | "晚安" | "早上好" | "下午好" | "晚上好" | "没事了"
            | "好的" | "好呀" | "嗯嗯" | "在的" | "在呢" | "有啥事" | "辛苦啦"
            | "辛苦了"
    )
}

/// 截断超长文本（按字节上限，零分配），保证路由输入可控
pub fn truncate_text(text: &str) -> &str {
    if text.chars().count() > ROUTE_TEXT_LIMIT {
        let mut end = ROUTE_TEXT_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_whitelist_hits() {
        for query in [
            "你好",
            "您好！",
            "嗨",
            "哈喽",
            "hello",
            "Hi",
            "在吗",
            "在干嘛？",
            "吃了吗",
            "谢谢",
            "谢谢你",
            "晚安～",
            "早上好",
            "辛苦啦",
            "好的！",
            " 你好 ",
            "在吗？？？",
        ] {
            assert!(is_pure_greeting(query), "白名单应命中: {query:?}");
        }
    }

    #[test]
    fn greeting_whitelist_misses() {
        for query in [
            "你好，帮我查一下报销流程",
            "你好请问归航计划的时间安排",
            "今天天气怎么样",
            "你叫什么名字",
            "请介绍一下这份合同",
            "报销金额的上限是多少",
            "你好吗我很好",
            "在吗，有个资料要你找",
            "谢谢你的帮助",
            "好的谢谢你们团队的方案",
        ] {
            assert!(!is_pure_greeting(query), "白名单不应命中: {query:?}");
        }
    }

    #[test]
    fn truncate_surrogate_safe() {
        assert_eq!(truncate_text("短文本"), "短文本");
        // 中文 3 字节/字符：600 字 = 1800 字节 → 截到 ≤500 字节（498 字节 = 166 字）
        let long = "字".repeat(600);
        let cut = truncate_text(&long);
        assert!(cut.len() <= ROUTE_TEXT_LIMIT);
        assert_eq!(cut.chars().count(), 166);
        // ASCII 1 字节/字符：恰好截到 500 字节
        let ascii = "a".repeat(600);
        assert_eq!(truncate_text(&ascii).chars().count(), ROUTE_TEXT_LIMIT);
    }
}
