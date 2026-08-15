//! 意图路由：共享路由类型与文本截断。
//!
//! 路由决策全部由生成模型的 LLM 直路由给出（见 [`crate::knowledge::intent_routing_prompt`]），
//! 本模块只提供路由决策共享类型（[`Intent`]/[`RouteDecision`]）与 `truncate_text` 文本截断。

use serde::{Deserialize, Serialize};

/// 路由意图
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// 用户想检索本地资料库
    Retrieval,
    /// 闲聊/寒暄/与资料无关，直接对话
    Chat,
}

/// 路由决策结果（LLM 直路由输出，无仲裁路径）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteDecision {
    pub intent: Intent,
    /// 与 intent 相同，保留供追踪记录
    pub top_category: Intent,
    pub top_score: f32,
    pub margin: f32,
}

/// 路由文本截断长度，超长按字符截断
pub const ROUTE_TEXT_LIMIT: usize = 500;

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
