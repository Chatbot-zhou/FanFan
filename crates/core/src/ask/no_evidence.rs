//! NO_EVIDENCE 六分类根因（Phase 4.1 spec 十二）：每次「当前资料中未找到
//! 足够依据」的拒绝都必须能回答「为什么」——目标没解析出来 / 文档召回为空 /
//! chunk 检索为空 / 查询级门槛拒绝 / rerank 拒绝 / 真无证据。
//!
//! 挂在 [`AnswerResult::no_evidence_reason`] 上，前端文案不变，trace 与
//! 诊断里可见。分类在拒绝发生的各层（storage 检索 / app_data 编排）就地写入。

use serde::{Deserialize, Serialize};

/// 一次 NO_EVIDENCE 拒绝的根因（拒绝路径才有值；非拒绝路径为 None）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NoEvidenceReason {
    /// 目标对象未解析：Document Resolver 无候选/无信号（无画像、无命中），
    /// 检索退回宽 scope 后仍无证据——根因在定位层，不在检索层。
    TargetNotResolved,
    /// 文档级召回为空：全库资料请求的 document recall 未定位到任何相关文档。
    DocumentRecallEmpty,
    /// chunk 检索为空：FTS+语义融合后没有任何候选块。
    ChunkRetrievalEmpty,
    /// 查询级门槛拒绝：`query_has_relevant_evidence` 判该查询与知识库无关
    ///（语义 top-1 虚高的乱码/无关查询在门槛处被拦下）。
    QueryGateRejected,
    /// rerank 后 top-1 分数低于阈值：候选块存在但都与用户原始问题无关。
    RerankRejected,
    /// Answerability Gate 拒绝（Phase 4.2 spec 二）：候选块与问题中的关键
    /// 实体不一致（CASE A：RAG 问题 + Agent 证据），或存在性断言缺少所需
    /// 语境证据（概念证据不能证明「做过项目」）。相似度分数足够也不放行。
    AnswerabilityRejected,
    /// 其余情况（兜底）：检索有候选、过了门槛，但最终没有可生成/可引用的
    /// 证据（候选全部未过相关门槛、块加载失败、生成侧拒答等）。
    TrueNoEvidence,
}

impl NoEvidenceReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TargetNotResolved => "TARGET_NOT_RESOLVED",
            Self::DocumentRecallEmpty => "DOCUMENT_RECALL_EMPTY",
            Self::ChunkRetrievalEmpty => "CHUNK_RETRIEVAL_EMPTY",
            Self::QueryGateRejected => "QUERY_GATE_REJECTED",
            Self::RerankRejected => "RERANK_REJECTED",
            Self::AnswerabilityRejected => "ANSWERABILITY_REJECTED",
            Self::TrueNoEvidence => "TRUE_NO_EVIDENCE",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_reasons_have_stable_names() {
        for (reason, expected) in [
            (NoEvidenceReason::TargetNotResolved, "TARGET_NOT_RESOLVED"),
            (
                NoEvidenceReason::DocumentRecallEmpty,
                "DOCUMENT_RECALL_EMPTY",
            ),
            (
                NoEvidenceReason::ChunkRetrievalEmpty,
                "CHUNK_RETRIEVAL_EMPTY",
            ),
            (NoEvidenceReason::QueryGateRejected, "QUERY_GATE_REJECTED"),
            (NoEvidenceReason::RerankRejected, "RERANK_REJECTED"),
            (
                NoEvidenceReason::AnswerabilityRejected,
                "ANSWERABILITY_REJECTED",
            ),
            (NoEvidenceReason::TrueNoEvidence, "TRUE_NO_EVIDENCE"),
        ] {
            assert_eq!(reason.as_str(), expected);
        }
    }

    #[test]
    fn serde_round_trips_uppercase_snake() {
        let json = serde_json::to_string(&NoEvidenceReason::QueryGateRejected).unwrap();
        assert_eq!(json, "\"QUERY_GATE_REJECTED\"");
        let parsed: NoEvidenceReason = serde_json::from_str("\"RERANK_REJECTED\"").expect("parses");
        assert_eq!(parsed, NoEvidenceReason::RerankRejected);
    }
}
