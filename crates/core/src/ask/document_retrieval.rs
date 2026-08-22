//! 文档级召回（MULTI_DOCUMENT_QA / COMPARE_DOCUMENTS 的前置）。
//!
//! 两级召回（spec 十一.5 / 十二）：
//!   1. Document-level recall —— 先用轻量元数据信号把全库画像粗筛成小候选集，
//!      再只对候选集取文档级向量精排（避免对全库逐文件做向量比对）。
//!   2. Chunk-level retrieval —— 候选文档集确定后，在其内部做 chunk 检索
//!      （由调用方在候选集 scope 内进行）。
//!
//! 保留 fallback：document recall 无结果时，调用方回落到 wider chunk
//! retrieval（不让所有 chunk 永远直接参与竞争，但也绝不因此空手而归）。
//!
//! 本模块全部为确定性纯函数（不含 LLM 调用），单测直接覆盖打分与融合语义。
//! 召回是增益层：任何信号缺失都不 panic、不报错，最多不命中。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::knowledge::DocumentProfile;

/// 文档级召回候选集上限（排序后取前 N）。
pub const DOCUMENT_RECALL_TOP_N: usize = 8;
/// 通过召回的分数门槛（低于此分数不进候选集）。
pub const DOCUMENT_RECALL_MIN_SCORE: f32 = 0.25;
/// metadata 预筛后取向量的候选上限（向量精排只在这批里做）。
pub const DOCUMENT_RECALL_VECTOR_CANDIDATES: usize = 24;
/// 融合权重：元数据命中为主、向量为辅（0.55 / 0.45）。
pub const DOCUMENT_RECALL_METADATA_WEIGHT: f32 = 0.55;
pub const DOCUMENT_RECALL_VECTOR_WEIGHT: f32 = 0.45;
/// 仅元数据命中（向量缺失或余弦 ≤ 0）时的折算系数。
pub const DOCUMENT_RECALL_METADATA_ONLY_WEIGHT: f32 = 0.8;
/// 参与子串匹配的最短信号长度（短于它全是噪音，如单字）。
pub const DOCUMENT_RECALL_MIN_SIGNAL_LEN: usize = 2;

/// 单个文档的召回结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentCandidateMatch {
    pub file_id: Uuid,
    /// 融合后分数（0.0..=1.0），排序键。
    pub score: f32,
    /// 命中的信号标签（title_match / keyword_match / type_match /
    /// section_title_match / entity_match / summary_match / vector_match），
    /// 供 trace 与前端展示召回依据。
    pub signals: Vec<String>,
}

/// 元数据信号得分：问题文本与画像各字段的子串匹配。
/// 返回 (分数, 命中的信号标签)。每种信号类最多计一次命中；
/// 全部未命中返回 0 分（该画像不进入候选）。
pub fn score_document_metadata(
    question: &str,
    profile: &DocumentProfile,
    file_name: &str,
) -> (f32, Vec<String>) {
    let question = question.trim();
    if question.is_empty() {
        return (0.0, Vec::new());
    }
    let mut score: f32 = 0.0;
    let mut signals = Vec::new();

    // 标题（文件名 + 画像标题）——最强信号。
    // 文件名先剥扩展名；双向匹配覆盖两种常见问法：
    //   a) 问题点名完整标题（"给我找一下述职报告" ⊇ "述职报告"）；
    //   b) 问题本身就是短标题（"述职报告" ⊆ "2025 年度述职报告"）。
    let title_hit = title_matches(question, file_name, &profile.title);
    if title_hit {
        score += 0.45;
        signals.push("title_match".to_owned());
    }
    // 类型（中文展示名 + 英文变体名）。
    let type_hit = [
        profile
            .document_type
            .map(|t| t.display_name())
            .unwrap_or(""),
        profile.document_type.map(|t| t.as_str()).unwrap_or(""),
    ]
    .iter()
    .any(|candidate| signal_in_question(candidate, question));
    if type_hit {
        score += 0.35;
        signals.push("type_match".to_owned());
    }
    // 关键词。
    let keyword_hit = profile
        .keywords
        .iter()
        .any(|keyword| signal_in_question(keyword, question));
    if keyword_hit {
        score += 0.30;
        signals.push("keyword_match".to_owned());
    }
    // 章节标题。
    let section_hit = profile
        .section_titles
        .iter()
        .any(|title| signal_in_question(title, question));
    if section_hit {
        score += 0.25;
        signals.push("section_title_match".to_owned());
    }
    // 实体。
    let entity_hit = profile
        .entities
        .iter()
        .any(|entity| signal_in_question(entity, question));
    if entity_hit {
        score += 0.20;
        signals.push("entity_match".to_owned());
    }
    // 摘要（最弱）。
    let summary_hit = signal_in_question(&profile.summary, question);
    if summary_hit {
        score += 0.15;
        signals.push("summary_match".to_owned());
    }

    // 单画像得分封顶 0.9，避免多个弱信号叠加出荒谬高分。
    (score.min(0.9), signals)
}

/// 第 1 级粗筛（metadata 打分）：返回按分数降序、截断到
/// `DOCUMENT_RECALL_VECTOR_CANDIDATES` 的 (分数, file_id, 信号) 列表。
/// 调用方据此批量取向量——全库画像可能数千，只对粗筛集查库。
pub fn preselect_document_profiles(
    question: &str,
    profiles: &[(DocumentProfile, String)],
) -> Vec<(f32, Uuid, Vec<String>)> {
    let mut preselected: Vec<(f32, Uuid, Vec<String>)> = profiles
        .iter()
        .filter_map(|(profile, name)| {
            let (metadata_score, signals) = score_document_metadata(question, profile, name);
            (metadata_score > 0.0).then_some((metadata_score, profile.file_id, signals))
        })
        .collect();
    preselected.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    preselected.truncate(DOCUMENT_RECALL_VECTOR_CANDIDATES);
    preselected
}

/// 两级召回精排：metadata 预筛 → 候选集向量精排 → 融合排序 → 截断。
///
/// - `profiles`：全库画像（含文件名）。
/// - `vectors`：候选画像的文档级向量（key = file_id）；缺失的画像走
///   metadata-only 折算，不会 panic。
/// - `question_vector`：问题的嵌入向量；为 None 时所有候选降级为
///   metadata-only。
/// - 返回按分数降序、截断到 `DOCUMENT_RECALL_TOP_N` 且分数不低于
///   `DOCUMENT_RECALL_MIN_SCORE` 的候选。
pub fn rank_document_candidates(
    question: &str,
    question_vector: Option<&[f32]>,
    profiles: &[(DocumentProfile, String)],
    vectors: &HashMap<Uuid, Vec<f32>>,
) -> Vec<DocumentCandidateMatch> {
    let preselected = preselect_document_profiles(question, profiles);

    // 第 2 级：向量精排 + 融合。向量只在预筛集内部参与（元数据零命中的
    // 画像不进入候选，文档级召回不做纯向量硬拉——那会让无关文档因
    // embedding 相近污染 scope；无命中的 wider fallback 由 chunk 级接管）。
    let mut ranked: Vec<DocumentCandidateMatch> = preselected
        .into_iter()
        .map(|(metadata_score, file_id, mut signals)| {
            let vector_score = question_vector
                .zip(vectors.get(&file_id))
                .map(|(q, p)| cosine_similarity(q, p))
                .unwrap_or(0.0);
            let score = if vector_score > 0.0 {
                DOCUMENT_RECALL_METADATA_WEIGHT * metadata_score
                    + DOCUMENT_RECALL_VECTOR_WEIGHT * vector_score
            } else {
                DOCUMENT_RECALL_METADATA_ONLY_WEIGHT * metadata_score
            };
            if vector_score > 0.0 {
                signals.push("vector_match".to_owned());
            }
            DocumentCandidateMatch {
                file_id,
                score,
                signals,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    ranked.truncate(DOCUMENT_RECALL_TOP_N);
    ranked.retain(|candidate| candidate.score >= DOCUMENT_RECALL_MIN_SCORE);
    ranked
}

// ===========================================================================
// 并行召回（Parallel Recall）+ RRF 融合
// ===========================================================================
// 需求四的核心：文件级召回应「并行召回 + 融合」，**禁止先 metadata 过滤、
// 再在剩余文件里做 embedding**——那会永久丢掉 filename/metadata 无法识别、
// 但正文向量与 query 相近的正确文件（如「我的简历 → final_v3.pdf」，文件名
// 里没有「简历」）。因此这里提供独立于既有 `rank_document_candidates` 的
// 三通道并行召回：
//   A. 精确/标题通道（precise filename/title）
//   B. 元数据通道（文档类型/实体/会话上下文/关键词/摘要）
//   C. 纯语义通道（profile_vector 余弦，**不设 metadata 门槛**）
// 三个通道各自产出有序候选，再用 RRF（Reciprocal Rank Fusion）融合成最终
// 排序。所有函数均为确定性纯函数（不含 LLM 调用）。
//
// 说明：既有 `rank_document_candidates` 仍保留（向后兼容，场景化测试与部分
// 调用方依赖其加权和打分）；新的并行召回由调用方显式选用。

/// 纯语义通道：问题向量与每个画像 profile_vector 的余弦相似度下限。
/// 低于此相似度不进语义候选（补召回也要守住基本相关性，避免 embedding
/// 相近的无关文档污染 scope）。取低阈值偏召回，交由 RRF 融合做最终排序。
pub const SEMANTIC_RECALL_MIN_SIMILARITY: f32 = 0.24;
/// 纯语义通道返回的最大候选数（全库也可能数千画像，语义候选截断到该上限）。
pub const SEMANTIC_RECALL_MAX_CANDIDATES: usize = 40;
/// RRF 融合常数 k（出现在每个列表的分数分母中，k 越大越平滑、惩罚靠后名次）。
pub const RRF_K: f32 = 60.0;
/// 并行召回最终返回的候选上限（融合后截断）。
pub const PARALLEL_RECALL_TOP_N: usize = 8;

/// 纯语义召回通道：对每个「有 profile_vector」的画像计算余弦相似度，
/// 高于 [`SEMANTIC_RECALL_MIN_SIMILARITY`] 即进候选，**不要求任何元数据命中**
/// ——文件名/标题表达不了真实用途、但正文向量与 query 语义相近的文件靠
/// 本通道召回。无向量的画像不参与。返回按相似度降序的 (file_id, cosine)。
pub fn semantic_document_recall(
    question_vector: &[f32],
    profiles: &[(DocumentProfile, String)],
    vectors: &HashMap<Uuid, Vec<f32>>,
) -> Vec<(Uuid, f32)> {
    let mut scored: Vec<(Uuid, f32)> = profiles
        .iter()
        .filter_map(|(profile, _)| {
            let cosine = vectors
                .get(&profile.file_id)
                .map(|v| cosine_similarity(question_vector, v))
                .unwrap_or(0.0);
            (cosine >= SEMANTIC_RECALL_MIN_SIMILARITY).then_some((profile.file_id, cosine))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
    });
    scored.truncate(SEMANTIC_RECALL_MAX_CANDIDATES);
    scored
}

/// 精确/标题通道 + 元数据通道即既有 metadata 打分（`score_document_metadata`
/// 已同时覆盖 title/type/entity/keyword/summary，A 与 B 合并为一列表）。
/// 返回按分数降序的候选，供 RRF 融合使用（只用于排序，分数本身不进融合）。
pub fn metadata_ranked_ids(question: &str, profiles: &[(DocumentProfile, String)]) -> Vec<Uuid> {
    let mut preselected = preselect_document_profiles(question, profiles);
    preselected.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.to_string().cmp(&b.1.to_string()))
    });
    preselected
        .into_iter()
        .map(|(_, file_id, _)| file_id)
        .collect()
}

/// 元数据通道的候选（含信号），供 trace 展示「按哪些依据找到」。
pub fn metadata_ranked_candidates(
    question: &str,
    profiles: &[(DocumentProfile, String)],
) -> Vec<DocumentCandidateMatch> {
    let mut preselected = preselect_document_profiles(question, profiles);
    preselected.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.to_string().cmp(&b.1.to_string()))
    });
    preselected
        .into_iter()
        .map(|(score, file_id, signals)| DocumentCandidateMatch {
            file_id,
            score: score.min(1.0),
            signals,
        })
        .collect()
}

/// RRF 融合：把多个有序候选列表（每项 file_id）融合为一个有序列表。
/// 名次越靠前分数越高；同一 file_id 出现在越多列表、越靠前，总分越高。
/// 返回按分数降序的 (Uuid, rrf_score)（未归一化）。
pub fn rrf_fuse(ranked_lists: &[Vec<Uuid>], k: f32) -> Vec<(Uuid, f32)> {
    use std::collections::HashMap;
    let mut score: HashMap<Uuid, f32> = HashMap::new();
    for list in ranked_lists {
        for (rank, file_id) in list.iter().enumerate() {
            *score.entry(*file_id).or_insert(0.0) += 1.0 / (k + rank as f32 + 1.0);
        }
    }
    let mut fused: Vec<(Uuid, f32)> = score.into_iter().collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
    });
    fused
}

/// 三通道并行召回的完整结果：各通道候选 + RRF 融合后的 top-N。供调用方取
/// scope，也供 trace 展示「每个通道召回什么、融合后谁排前」——据此能明确
/// 归因 Planner / Resolver / Semantic Recall / Scope 各自的成败。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelDocumentRecall {
    /// A/B 元数据通道候选（标题/类型/实体/上下文/关键词），按分数降序。
    pub metadata_candidates: Vec<DocumentCandidateMatch>,
    /// C 纯语义通道候选（仅 file_id，无 metadata 门槛）。
    pub semantic_candidates: Vec<(Uuid, f32)>,
    /// RRF 融合后的 top-N 候选（归一化到 0..=1，信号为命中来源并集）。
    pub fused: Vec<DocumentCandidateMatch>,
    /// 语义通道是否生效（question_vector 存在且至少一个画像有向量）。
    pub semantic_enabled: bool,
}

/// 主入口：A(精确/标题)+B(元数据) 与 C(语义) 并行召回 → RRF 融合 → top-N。
///
/// - 语义通道要求 `question_vector` 非空；否则退化为 metadata-only（与既有
///   `rank_document_candidates` 行为一致，Fast Path 优先、模型/嵌入缺失不报错）。
/// - metadata 候选即使为空也**不短路**：只要语义能召回，文件仍进入融合，
///   这才是真正的「并行不预筛」。若两通道皆空返回空集。
pub fn parallel_document_recall(
    question: &str,
    question_vector: Option<&[f32]>,
    profiles: &[(DocumentProfile, String)],
    vectors: &HashMap<Uuid, Vec<f32>>,
) -> ParallelDocumentRecall {
    let metadata_candidates = metadata_ranked_candidates(question, profiles);
    let semantic_enabled = question_vector.is_some() && vectors.values().any(|v| !v.is_empty());
    let semantic_candidates = match question_vector {
        Some(q) if semantic_enabled => semantic_document_recall(q, profiles, vectors),
        _ => Vec::new(),
    };

    // 三个有序列表：A/B(metadata)→RRF；C(semantic)→RRF。
    let metadata_ids: Vec<Uuid> = metadata_candidates.iter().map(|c| c.file_id).collect();
    let semantic_ids: Vec<Uuid> = semantic_candidates.iter().map(|(fid, _)| *fid).collect();
    let mut lists: Vec<Vec<Uuid>> = Vec::with_capacity(2);
    if !metadata_ids.is_empty() {
        lists.push(metadata_ids);
    }
    if !semantic_ids.is_empty() {
        lists.push(semantic_ids);
    }
    let fused_raw = if lists.is_empty() {
        Vec::new()
    } else {
        rrf_fuse(&lists, RRF_K)
    };

    // 归一化 RRF 分数，并把每个候选的命中来源信号合并到 meta 信号（语义命中
    // 补 semantic_match），截断到 top-N。
    let meta_by_id: HashMap<Uuid, DocumentCandidateMatch> = metadata_candidates
        .into_iter()
        .map(|c| (c.file_id, c))
        .collect();
    let max_score = fused_raw
        .iter()
        .map(|(_, score)| *score)
        .fold(0.0f32, f32::max)
        .max(1e-6);
    let fused: Vec<DocumentCandidateMatch> = fused_raw
        .into_iter()
        .map(|(file_id, score)| {
            let mut match_candidate =
                meta_by_id
                    .get(&file_id)
                    .cloned()
                    .unwrap_or(DocumentCandidateMatch {
                        file_id,
                        score: 0.0,
                        signals: Vec::new(),
                    });
            let semantic = semantic_candidates
                .iter()
                .find(|(fid, _)| *fid == file_id)
                .map(|(_, cosine)| *cosine as f64);
            if semantic.is_some()
                && !match_candidate
                    .signals
                    .iter()
                    .any(|s| s == "semantic_match")
            {
                match_candidate.signals.push("semantic_match".to_owned());
            }
            match_candidate.score = (score / max_score).min(1.0);
            match_candidate
        })
        .collect::<Vec<_>>();
    let fused = fused.into_iter().take(PARALLEL_RECALL_TOP_N).collect();

    ParallelDocumentRecall {
        metadata_candidates: meta_by_id.into_values().collect(),
        semantic_candidates,
        fused,
        semantic_enabled,
    }
}

/// 纯向量召回（metadata 预筛之外的兜底路径已由调用方接管 wider chunk
/// retrieval，此函数只用于场景化测试与 trace 辅助）。
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a <= f32::EPSILON || norm_b <= f32::EPSILON {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// 标题信号：文件名（剥扩展名）或画像标题。
/// 三级匹配，覆盖三种问法：
///   a) 问题点名完整标题（"给我找一下述职报告" ⊇ "述职报告"）；
///   b) 问题本身就是短标题（"述职报告" ⊆ "2025 年度述职报告"）；
///   c) 问题指称标题的核心片段（"找一下述职报告" 与 "述职报告备份"
///      共享连续片段 "述职报告"）。
/// 短于最小信号长度的信号一律忽略（单字全噪音）。
fn title_matches(question: &str, file_name: &str, title: &str) -> bool {
    let name_signal = strip_extension(file_name);
    [name_signal.as_str(), title].iter().any(|signal| {
        let signal = signal.trim();
        if signal.chars().count() < DOCUMENT_RECALL_MIN_SIGNAL_LEN {
            return false;
        }
        if question.contains(signal) || signal.contains(question) {
            return true;
        }
        shared_phrase(question, signal)
    })
}

/// 公共连续片段：question 的任一 2..=8 字连续子串出现在 signal 中即命中。
/// 覆盖「标题比问题长且只共享核心片段」的场景；宁误报不漏报——
/// 文档级召回是粗筛，向量精排在后兜底。
fn shared_phrase(question: &str, signal: &str) -> bool {
    let question_chars: Vec<char> = question.chars().collect();
    if question_chars.len() < DOCUMENT_RECALL_MIN_SIGNAL_LEN {
        return false;
    }
    let max_len = question_chars.len().min(8);
    for len in (DOCUMENT_RECALL_MIN_SIGNAL_LEN..=max_len).rev() {
        for start in 0..=question_chars.len() - len {
            let phrase: String = question_chars[start..start + len].iter().collect();
            if signal.contains(&phrase) {
                return true;
            }
        }
    }
    false
}

/// 剥离扩展名："述职报告.docx" → "述职报告"（"a.b.docx" → "a.b"）。
fn strip_extension(file_name: &str) -> String {
    match file_name.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty() && extension.chars().all(|c| !c.is_whitespace()) =>
        {
            stem.to_owned()
        }
        _ => file_name.to_owned(),
    }
}

/// 信号长度达标（≥ DOCUMENT_RECALL_MIN_SIGNAL_LEN 且非空白）且被问题包含。
fn signal_in_question(signal: &str, question: &str) -> bool {
    let signal = signal.trim();
    if signal.chars().count() < DOCUMENT_RECALL_MIN_SIGNAL_LEN {
        return false;
    }
    question.contains(signal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::DocumentType;
    use crate::knowledge::DocumentProfile;

    fn mk_profile(id: u32, title: &str, keywords: Vec<&str>) -> DocumentProfile {
        DocumentProfile {
            file_id: Uuid::from_u128(id as u128),
            revision_id: Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0000 + id as u128),
            title: title.to_owned(),
            summary: String::new(),
            keywords: keywords.into_iter().map(str::to_owned).collect(),
            entities: Vec::new(),
            document_type: None,
            type_confidence: None,
            section_titles: Vec::new(),
            representative_text_hash: None,
            updated_at: chrono::DateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn metadata_title_hit_scores_high() {
        let profile = mk_profile(1, "2025 年度述职报告", vec![]);
        let (score, signals) =
            score_document_metadata("给我找一下我的述职报告", &profile, "述职报告.docx");
        assert!(score >= 0.45, "title signal must fire, got {score}");
        assert!(signals.contains(&"title_match".to_owned()));
    }

    #[test]
    fn metadata_type_hit_via_chinese_label() {
        let mut profile = mk_profile(2, "张三个人简历", vec![]);
        profile.document_type = Some(DocumentType::Resume);
        let (score, signals) = score_document_metadata("我的简历在哪里", &profile, "resume.pdf");
        assert!(
            score >= 0.35,
            "type signal must fire via 中文名, got {score}"
        );
        assert!(signals.contains(&"type_match".to_owned()));
    }

    #[test]
    fn metadata_keyword_hit() {
        let profile = mk_profile(3, "会议纪要 2026-01", vec!["RAG", "向量检索"]);
        let (score, signals) =
            score_document_metadata("哪些资料提到向量检索", &profile, "meeting.txt");
        assert!(score >= 0.30);
        assert!(signals.contains(&"keyword_match".to_owned()));
    }

    #[test]
    fn metadata_title_hit_via_shared_phrase() {
        // 标题比问题长且只共享核心片段："述职报告备份" ↔ "找一下述职报告"。
        let profile = mk_profile(9, "述职报告备份", vec![]);
        let (score, signals) = score_document_metadata("找一下述职报告", &profile, "backup.docx");
        assert!(
            score >= 0.45,
            "shared-phrase title match must fire, got {score}"
        );
        assert!(signals.contains(&"title_match".to_owned()));
        // 反向验证：标题确实与问题无关时不误报。
        let unrelated = mk_profile(10, "工作计划与排期", vec![]);
        let (score, _) = score_document_metadata("找一下述职报告", &unrelated, "plan.docx");
        assert_eq!(score, 0.0, "unrelated title must not fire: {score}");
    }

    #[test]
    fn metadata_no_hit_scores_zero() {
        let profile = mk_profile(4, "无关文件", vec!["a", "bb"]);
        let (score, signals) = score_document_metadata("我的狗丢了", &profile, "random.txt");
        assert_eq!(score, 0.0);
        assert!(signals.is_empty());
    }

    #[test]
    fn metadata_short_signal_ignored() {
        let profile = mk_profile(5, "x", vec!["a"]);
        let (score, _) = score_document_metadata("a x 都太短", &profile, "x.txt");
        assert_eq!(score, 0.0, "single-char signals must not fire");
    }

    #[test]
    fn metadata_score_capped() {
        let mut profile = mk_profile(6, "述职报告", vec!["述职", "报告", "年终总结"]);
        profile.document_type = Some(DocumentType::Report);
        profile.section_titles = vec!["工作总结".to_owned()];
        profile.entities = vec!["团队".to_owned()];
        profile.summary = "年度述职总结".to_owned();
        let (score, signals) =
            score_document_metadata("团队年度述职报告工作总结", &profile, "述职报告.docx");
        assert!(score <= 0.9, "score must be capped at 0.9, got {score}");
        assert!(signals.len() >= 4, "multiple signals should fire");
    }

    #[test]
    fn ranking_fuses_metadata_and_vector() {
        let profiles = vec![
            (
                mk_profile(1, "述职报告", vec![]),
                "述职报告.docx".to_owned(),
            ),
            (
                mk_profile(2, "述职报告备份", vec![]),
                "述职报告备份.docx".to_owned(),
            ),
        ];
        let mut vectors = HashMap::new();
        // 两画像元数据同分（都是 title_match）；向量分决定排序。
        vectors.insert(profiles[0].0.file_id, vec![1.0, 0.0]);
        vectors.insert(profiles[1].0.file_id, vec![-1.0, 0.0]);
        let question_vector = vec![1.0, 0.0];
        let ranked = rank_document_candidates(
            "找一下述职报告",
            Some(&question_vector),
            &profiles,
            &vectors,
        );
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].file_id, profiles[0].0.file_id);
        assert!(
            ranked[0].score > ranked[1].score,
            "metadata 同分时由向量精排决定先后"
        );
        assert!(ranked[0].signals.contains(&"vector_match".to_owned()));
        assert!(ranked[0].signals.contains(&"title_match".to_owned()));
    }

    #[test]
    fn ranking_filters_below_min_score() {
        let mut profiles = Vec::new();
        // 100 份无命中画像 + 1 份命中画像。
        for id in 1..=100u32 {
            profiles.push((mk_profile(id, "随机文件", vec![]), "f.txt".to_owned()));
        }
        profiles.push((
            mk_profile(101, "述职报告", vec![]),
            "述职报告.docx".to_owned(),
        ));
        let ranked = rank_document_candidates("找一下述职报告", None, &profiles, &HashMap::new());
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].file_id, Uuid::from_u128(101));
    }

    #[test]
    fn ranking_caps_at_top_n() {
        let profiles: Vec<(DocumentProfile, String)> = (1..=20u32)
            .map(|id| {
                (
                    mk_profile(id, "述职报告", vec![]),
                    "述职报告.docx".to_owned(),
                )
            })
            .collect();
        let ranked = rank_document_candidates("找一下述职报告", None, &profiles, &HashMap::new());
        assert!(ranked.len() <= DOCUMENT_RECALL_TOP_N, "must cap at TOP_N");
        // 全部元数据同分：排序稳定即可，只验上限。
        assert_eq!(ranked.len(), DOCUMENT_RECALL_TOP_N);
    }

    #[test]
    fn ranking_empty_profiles_returns_empty() {
        let ranked = rank_document_candidates("随便问", None, &[], &HashMap::new());
        assert!(ranked.is_empty());
    }

    #[test]
    fn cosine_handles_mismatched_and_zero_vectors() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        let value = cosine_similarity(&[1.0, 2.0], &[2.0, 4.0]);
        assert!(
            (value - 1.0).abs() < 1e-5,
            "collinear vectors -> 1.0, got {value}"
        );
    }

    #[test]
    fn vector_without_metadata_signal_is_not_recalled() {
        // 元数据零命中的画像不进入候选集——向量精排只在预筛集内部发生，
        // 文档级召回不做纯向量硬拉（否则无关文档会因 embedding 相近污染 scope）。
        let profile = mk_profile(7, "无关标题", vec![]);
        let profiles = vec![(profile.clone(), "f.txt".to_owned())];
        let mut vectors = HashMap::new();
        vectors.insert(profile.file_id, vec![1.0]);
        let ranked = rank_document_candidates("任意问题", Some(&[1.0]), &profiles, &vectors);
        assert!(ranked.is_empty());
    }
}
