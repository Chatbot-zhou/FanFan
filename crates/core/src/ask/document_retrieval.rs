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
    let type_hit = [profile.document_type.map(|t| t.display_name()).unwrap_or(""),
            profile.document_type.map(|t| t.as_str()).unwrap_or("")]
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
            DocumentCandidateMatch { file_id, score, signals }
        })
        .collect();

    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    ranked.truncate(DOCUMENT_RECALL_TOP_N);
    ranked.retain(|candidate| candidate.score >= DOCUMENT_RECALL_MIN_SCORE);
    ranked
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
        Some((stem, extension)) if !stem.is_empty() && extension.chars().all(|c| !c.is_whitespace()) => {
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
        let (score, signals) = score_document_metadata("给我找一下我的述职报告", &profile, "述职报告.docx");
        assert!(score >= 0.45, "title signal must fire, got {score}");
        assert!(signals.contains(&"title_match".to_owned()));
    }

    #[test]
    fn metadata_type_hit_via_chinese_label() {
        let mut profile = mk_profile(2, "张三个人简历", vec![]);
        profile.document_type = Some(DocumentType::Resume);
        let (score, signals) = score_document_metadata("我的简历在哪里", &profile, "resume.pdf");
        assert!(score >= 0.35, "type signal must fire via 中文名, got {score}");
        assert!(signals.contains(&"type_match".to_owned()));
    }

    #[test]
    fn metadata_keyword_hit() {
        let profile = mk_profile(3, "会议纪要 2026-01", vec!["RAG", "向量检索"]);
        let (score, signals) = score_document_metadata("哪些资料提到向量检索", &profile, "meeting.txt");
        assert!(score >= 0.30);
        assert!(signals.contains(&"keyword_match".to_owned()));
    }

    #[test]
    fn metadata_title_hit_via_shared_phrase() {
        // 标题比问题长且只共享核心片段："述职报告备份" ↔ "找一下述职报告"。
        let profile = mk_profile(9, "述职报告备份", vec![]);
        let (score, signals) = score_document_metadata("找一下述职报告", &profile, "backup.docx");
        assert!(score >= 0.45, "shared-phrase title match must fire, got {score}");
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
            (mk_profile(1, "述职报告", vec![]), "述职报告.docx".to_owned()),
            (mk_profile(2, "述职报告备份", vec![]), "述职报告备份.docx".to_owned()),
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
        profiles.push((mk_profile(101, "述职报告", vec![]), "述职报告.docx".to_owned()));
        let ranked = rank_document_candidates("找一下述职报告", None, &profiles, &HashMap::new());
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].file_id, Uuid::from_u128(101));
    }

    #[test]
    fn ranking_caps_at_top_n() {
        let profiles: Vec<(DocumentProfile, String)> = (1..=20u32)
            .map(|id| (mk_profile(id, "述职报告", vec![]), "述职报告.docx".to_owned()))
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
        assert!((value - 1.0).abs() < 1e-5, "collinear vectors -> 1.0, got {value}");
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
