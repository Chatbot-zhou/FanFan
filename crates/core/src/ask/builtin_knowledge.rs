//! builtin_knowledge：内置技术词条知识库（Phase 4.3 第二部分）。
//!
//! 背景：本地小模型（DeepSeek-R1-4B / Qwen 2B 级）对 LangGraph、RAG、
//! Transformer 等技术概念极易生成错误解释（实测把 LangGraph 解释成
//! GNN、把 RAG 说成 Recurrent Architecture）。这些概念有稳定、客观、
//! 不随时间变化的定义，直接内置词条比让小模型自由发挥可靠得多。
//!
//! 调用优先级（仅 GENERAL 链路）：
//! `普通技术问题 → builtin_knowledge → 未命中 → LLM 自由生成`。
//! builtin_knowledge **不参与**用户资料检索（LOCAL 链路绝不查它），
//! 与个人资料完全隔离。

use serde::{Deserialize, Serialize};

/// 内置词条的磁盘形态（builtin_knowledge.json）。
#[derive(Debug, Clone, Deserialize)]
struct BuiltinEntryRaw {
    /// 触发词条的别名/变体（小写；键本身也是触发词）。
    #[serde(default)]
    terms: Vec<String>,
    category: String,
    answer: String,
}

/// 查询命中的内置词条（对外输出形态）。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BuiltinKnowledgeHit {
    /// 命中的词条键名（如 "langgraph"）。
    pub key: String,
    pub category: String,
    pub answer: String,
}

/// 加载内置词条表。JSON 内嵌于二进制（include_str!），随应用分发，
/// 不产生运行期文件依赖；解析失败属于构建期契约错误，直接 panic。
fn load_entries() -> Vec<(String, BuiltinEntryRaw)> {
    const RAW: &str = include_str!("builtin_knowledge.json");
    let parsed: std::collections::BTreeMap<String, BuiltinEntryRaw> =
        serde_json::from_str(RAW).expect("builtin_knowledge.json 必须是合法 JSON");
    parsed.into_iter().collect()
}

/// 查询内置知识库：问题中出现词条名或其变体（大小写不敏感的 ASCII
/// 折叠 + 原样中文匹配）即命中，返回对应词条。多个词条命中时取
/// 问题中出现位置最靠前**的词条（用户通常先说主题再补充问句，
/// 如「LangGraph 和 RAG 的区别」优先解释 LangGraph 并在词条内已
/// 互相覆盖关联概念）。
///
/// 与 LOCAL 的隔离由调用方保证：仅在 GENERAL 分支调用本函数。
pub fn lookup_builtin_knowledge(question: &str) -> Option<BuiltinKnowledgeHit> {
    let q = question.trim();
    if q.is_empty() {
        return None;
    }
    let folded = q.to_lowercase();
    let mut best: Option<(usize, BuiltinKnowledgeHit)> = None;
    for (key, entry) in load_entries() {
        let mut candidates: Vec<&str> = vec![key.as_str()];
        candidates.extend(entry.terms.iter().map(String::as_str));
        for term in candidates {
            if term.is_empty() {
                continue;
            }
            if let Some(position) = folded.find(term) {
                let better = best
                    .as_ref()
                    .map(|(pos, _)| position < *pos)
                    .unwrap_or(true);
                if better {
                    best = Some((
                        position,
                        BuiltinKnowledgeHit {
                            key: key.clone(),
                            category: entry.category.clone(),
                            answer: entry.answer.clone(),
                        },
                    ));
                }
                break;
            }
        }
    }
    best.map(|(_, hit)| hit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hits_langgraph_and_rag_by_name_or_alias() {
        // CASE E：LangGraph 解释必须命中内置词条，不交给小模型自由生成
        let hit = lookup_builtin_knowledge("帮我解释 LangGraph").expect("hit");
        assert_eq!(hit.key, "langgraph");
        assert!(hit.answer.contains("LangChain"));
        assert!(hit.answer.contains("图"));

        let hit = lookup_builtin_knowledge("RAG 是什么？").expect("hit");
        assert_eq!(hit.key, "rag");
        assert!(hit.answer.contains("检索增强"));

        // 中文别名
        let hit = lookup_builtin_knowledge("什么是检索增强生成").expect("hit");
        assert_eq!(hit.key, "rag");
    }

    #[test]
    fn earliest_term_in_question_wins() {
        // 「Transformer 和 RAG 的区别」→ Transformer 在前，优先解释 Transformer
        let hit = lookup_builtin_knowledge("Transformer 和 RAG 的区别").expect("hit");
        assert_eq!(hit.key, "transformer");
    }

    #[test]
    fn never_hits_plain_chat_or_personal_questions() {
        // 寒暄 / 天气 / 闲聊不命中（交给正常 chat 流程）
        assert!(lookup_builtin_knowledge("你好").is_none());
        assert!(lookup_builtin_knowledge("今天天气如何").is_none());
        assert!(lookup_builtin_knowledge("你是谁").is_none());
    }

    #[test]
    fn case_insensitive_ascii_matching() {
        let hit = lookup_builtin_knowledge("什么是 LORA 微调").expect("hit");
        assert_eq!(hit.key, "lora");
    }

    #[test]
    fn all_entries_have_required_fields() {
        for (key, entry) in load_entries() {
            assert!(!entry.answer.is_empty(), "{key} 缺答案");
            assert_eq!(entry.category, "technical", "{key} 分类应为 technical");
            assert!(
                entry.answer.chars().count() >= 50,
                "{key} 答案过短，不构成可用解释"
            );
        }
    }
}
