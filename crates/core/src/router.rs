//! 意图路由：用轻量 BERT（复用 Embedding 模型）做 few-shot 语义路由。
//!
//! 每条意图（闲聊/检索）维护一组中文示例句（few-shot shots），提问向量与
//! 示例向量做余弦相似度，取最高分类别；置信度或区分度不足时返回
//! [`Intent::Ambiguous`]，由调用方交给生成模型仲裁。

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

/// top_score 低于此值 → 仲裁
pub const ROUTE_CONFIDENCE_THRESHOLD: f32 = 0.55;
/// margin 低于此值（两类别分不出）→ 仲裁
pub const ROUTE_MARGIN_THRESHOLD: f32 = 0.08;
/// 路由文本（问题/示例句）截断长度，超长按字符截断
pub const ROUTE_TEXT_LIMIT: usize = 500;

/// 闲聊类示例句（few-shot shots）
pub fn chat_examples() -> &'static [&'static str] {
    &[
        "你好啊",
        "今天天气怎么样",
        "你叫什么名字",
        "你吃饭了吗",
        "给我讲个笑话吧",
        "我好无聊啊",
        "我们随便聊聊天吧",
        "谢谢你帮我",
        "你觉得这部电影怎么样",
        "周末有什么好玩的推荐",
        "我今天心情不太好",
        "你在干嘛呢",
    ]
}

/// 检索类示例句（few-shot shots）
pub fn retrieval_examples() -> &'static [&'static str] {
    &[
        "公司的报销流程是什么",
        "归航计划的整体时间安排是怎样的",
        "这个合同的有效期到什么时候",
        "上个月的会议纪要在哪里",
        "新员工入职需要准备哪些材料",
        "产品的技术参数表有吗",
        "员工手册里关于请假的规定",
        "去年的财务报表数据",
        "项目验收标准是什么",
        "帮我找一下客户名单",
        "整理一下发票清单",
        "报销金额的上限是多少",
    ]
}

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

/// 余弦相似度。空向量或维度不一致返回 0.0；输入会被防御性归一化，
/// 对已 L2 归一化的向量（worker 输出）无影响。
pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.is_empty() || right.is_empty() || left.len() != right.len() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_l = 0.0_f64;
    let mut norm_r = 0.0_f64;
    for (l, r) in left.iter().zip(right.iter()) {
        dot += f64::from(*l) * f64::from(*r);
        norm_l += f64::from(*l) * f64::from(*l);
        norm_r += f64::from(*r) * f64::from(*r);
    }
    if norm_l <= 1e-12 || norm_r <= 1e-12 {
        return 0.0;
    }
    (dot / (norm_l * norm_r).sqrt()) as f32
}

/// few-shot 语义路由：每类取与示例向量的最高余弦，再按置信度/区分度判定。
///
/// - top_score < [`ROUTE_CONFIDENCE_THRESHOLD`] 或 margin <
///   [`ROUTE_MARGIN_THRESHOLD`] → `intent = Ambiguous`（调用方仲裁）
/// - 否则取 argmax 类别。某类示例向量为空时该类最高分按 0.0 计。
pub fn route_query(
    question_vector: &[f32],
    chat_vectors: &[Vec<f32>],
    retrieval_vectors: &[Vec<f32>],
) -> RouteDecision {
    let chat_score = chat_vectors
        .iter()
        .map(|v| cosine_similarity(question_vector, v))
        .fold(0.0_f32, f32::max);
    let retrieval_score = retrieval_vectors
        .iter()
        .map(|v| cosine_similarity(question_vector, v))
        .fold(0.0_f32, f32::max);
    let (top_category, top_score, second_score) = if chat_score >= retrieval_score {
        (Intent::Chat, chat_score, retrieval_score)
    } else {
        (Intent::Retrieval, retrieval_score, chat_score)
    };
    let margin = top_score - second_score;
    let intent = if top_score < ROUTE_CONFIDENCE_THRESHOLD || margin < ROUTE_MARGIN_THRESHOLD {
        Intent::Ambiguous
    } else {
        top_category
    };
    RouteDecision {
        intent,
        top_category,
        top_score,
        margin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(x: f32, y: f32) -> Vec<f32> {
        let norm = (x * x + y * y).sqrt();
        vec![x / norm, y / norm]
    }

    #[test]
    fn cosine_four_states() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) - -1.0).abs() < 1e-6);
        assert_eq!(cosine_similarity(&[], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        // 非单位向量：归一化后与单位向量方向相同 → 1.0
        assert!((cosine_similarity(&[2.0, 0.0], &[3.0, 0.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn route_high_confidence() {
        let chat = vec![unit(1.0, 0.0)];
        let retrieval = vec![unit(0.0, 1.0)];
        let d = route_query(&unit(1.0, 0.0), &chat, &retrieval);
        assert_eq!(d.intent, Intent::Chat);
        assert_eq!(d.top_category, Intent::Chat);
        assert!((d.top_score - 1.0).abs() < 1e-6);
        assert!((d.margin - 1.0).abs() < 1e-6);

        let d = route_query(&unit(0.0, 1.0), &chat, &retrieval);
        assert_eq!(d.intent, Intent::Retrieval);
        assert_eq!(d.top_category, Intent::Retrieval);

        // 强偏向聊天但含检索成分 → 高置信 Chat
        let d = route_query(&unit(0.98, 0.14), &chat, &retrieval);
        assert_eq!(d.intent, Intent::Chat);
        assert!(d.margin > 0.8);
    }

    #[test]
    fn route_ambiguous_on_margin() {
        let chat = vec![unit(1.0, 0.0)];
        let retrieval = vec![unit(0.0, 1.0)];
        // 两类别等距 → margin≈0 → 仲裁
        let d = route_query(&unit(1.0, 1.0), &chat, &retrieval);
        assert_eq!(d.intent, Intent::Ambiguous);
        assert!((d.top_score - 0.70710678).abs() < 1e-5);
        assert!(d.margin < 1e-5);
    }

    #[test]
    fn route_ambiguous_on_low_confidence() {
        let chat = vec![unit(1.0, 0.0)];
        let retrieval = vec![unit(0.0, 1.0)];
        // 与所有示例句负相似 → 各类最高分被 0.0 下限钳住 → margin=0 → 仲裁
        let d = route_query(&unit(-1.0, -1.0), &chat, &retrieval);
        assert_eq!(d.intent, Intent::Ambiguous);
        assert_eq!(d.top_score, 0.0);
        assert_eq!(d.margin, 0.0);
    }

    #[test]
    fn route_margin_boundary() {
        // 余弦取 0.919 / 0.921，margin 为 0.081 / 0.079，两侧都远离阈值 0.08（f32 误差 ~1e-6）
        let chat = vec![unit(1.0, 0.0)];
        let above = (1.0_f32 - 0.919 * 0.919).sqrt();
        let d = route_query(&unit(1.0, 0.0), &chat, &vec![unit(0.919, above)]);
        assert!((d.margin - 0.081).abs() < 1e-4);
        assert_eq!(d.intent, Intent::Chat);

        let below = (1.0_f32 - 0.921 * 0.921).sqrt();
        let d = route_query(&unit(1.0, 0.0), &chat, &vec![unit(0.921, below)]);
        assert!((d.margin - 0.079).abs() < 1e-4);
        assert_eq!(d.intent, Intent::Ambiguous);
    }

    #[test]
    fn route_empty_category_falls_through() {
        let chat = vec![unit(1.0, 0.0)];
        let d = route_query(&unit(1.0, 0.0), &chat, &[]);
        assert_eq!(d.intent, Intent::Chat);
        assert_eq!(d.top_category, Intent::Chat);
        assert!((d.margin - 1.0).abs() < 1e-6);
    }

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
