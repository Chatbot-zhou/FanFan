//! Document Resolver：把 QueryPlan 的 target（用户所指的目标对象）
//! 解析成 file_id 白名单，供 RetrievalScope 使用。
//!
//! 设计约束（需求文档「八、Document Resolver」）：
//! - 不只按 filename——综合 document_type / document_title / entity /
//!   keywords / session active / recent referenced / filename 多信号打分；
//! - 权重集中配置（[`SIGNAL_WEIGHTS`]），可调参，不散落在分支里；
//! - 高置信度且明显唯一 → 锁定一个文件（Resolved）；
//! - 中置信度或两个非常接近的候选 → MultipleCandidates，保留 top-2/3 进 scope；
//! - 低置信度 → 不错误锁定，退回宽 scope（Unresolved + fallback_reason）。
//!
//! Memory 层（alias / confirmed relation）是下一阶段的信号来源；P0 先接入
//! 会话上下文与文档画像。纯函数、无 IO，画像与文件名由编排层读取后传入。

use std::collections::HashMap;

use uuid::Uuid;

use crate::AskSessionContext;
use crate::ask::document_retrieval::cosine_similarity;
use crate::ask::query_normalize::{meaningful_tokens, strip_target_stop_phrases};
use crate::ask::query_plan::{
    DocumentCandidate, DocumentResolution, QueryIntent, QueryPlan, ResolutionStatus,
};
use crate::contracts::DocumentType;
use crate::knowledge::DocumentProfile;
use crate::profile_builder::type_keywords_for;

/// 综合打分时使用的信号权重（可配置：改这里即可调参，编排层不感知细节）。
/// 每个信号命中 +weight（0..=1 分数直接相加，分数上限≈1.05）。
pub const SIGNAL_WEIGHTS: &[(&str, f32)] = &[
    ("document_type", 0.35),
    ("session_active", 0.30),
    ("document_title", 0.25),
    ("session_referenced", 0.20),
    ("entity_match", 0.20),
    ("keyword_match", 0.15),
    ("semantic", 0.30),
    ("filename", 0.10),
    ("owner_match", 0.05),
    // FIND 定位：content_query/filters 描述串与文件名的「子序列+二元组重合」
    // 命中（仅 DOCUMENT_FIND intent）。命中即 +weight，再按重合度加
    // weight×coverage 的梯度，使「2019年数据库下午真题」这类描述唯一指向
    // 正确文件（同年的其它文件只覆盖部分片段，分数明显落后）。
    ("find_content", 0.40),
    // 非 FIND 的定位类 intent：target.reference/document_name 描述串与文件名
    // 的「子序列+二元组重合」匹配（词序翻转/插字容忍，见 7.6）。命中即
    // +weight，再按重合度加 weight×coverage 梯度。
    ("reference_match", 0.35),
];

/// 语义通道的余弦下限：低于此相似度不贡献语义分（避免 embedding 相近的
/// 无关文件因语义分进入候选池）。语义是补召回信号，不是唯一主排序。
pub const SEMANTIC_MATCH_MIN_COSINE: f32 = 0.24;

/// 达到该分数且与第二名差距 ≥ [`HIGH_MARGIN`] → 锁定单文件。
pub const HIGH_CONFIDENCE_THRESHOLD: f32 = 0.50;
/// 「非常接近」的判定：best 与 second 的分数差小于该值 → 不锁定。
pub const HIGH_MARGIN: f32 = 0.15;
/// 中置信度下限：达到后保留 top-2/3 进 scope，不锁定。
pub const MEDIUM_CONFIDENCE_THRESHOLD: f32 = 0.30;
/// MultipleCandidates 时进入 scope 的最大候选数。
pub const MAX_CANDIDATE_SCOPE: usize = 3;

/// GraduationReferenceResolver（Phase 4.3 CASE 5）：「毕业」类引用的语义
/// 扩展词元。「我毕业时候那个材料」的有意义词元只剩「毕业」，但真实
/// 毕业材料的文件名常不含「毕业」两字（「开题报告书」「学位论文」
/// 「答辩PPT」）——目标含「毕业」时，这些等价词元参与标题/文件名匹配。
/// 「设计」单独过宽（会命中「设计院合同」），只以「毕业设计」组合参与。
const GRADUATION_REFERENCE_MARKER: &str = "毕业";
const GRADUATION_EXPANSION_TERMS: &[&str] = &["毕业", "论文", "答辩", "开题", "学位", "毕业设计"];

/// Document Resolver 的输入：QueryPlan + 会话上下文 + 候选画像 + 文件名。
/// 可选语义通道：`question_vector` + `profile_vectors` 同时提供时，对每个
/// 画像补算问题向量与画像向量的余弦相似度（补召回信号）。两者缺一即跳过
/// 语义通道，退化为纯元数据打分——Fast Path 优先，嵌入缺失不阻断定位。
#[derive(Debug, Clone)]
pub struct ResolverInput<'a> {
    pub plan: &'a QueryPlan,
    pub session: &'a AskSessionContext,
    pub profiles: Vec<DocumentProfile>,
    /// file_id → 文件名（弱信号，只做最后兜底）
    pub file_names: HashMap<Uuid, String>,
    /// 问题的嵌入向量（可选；语义通道需同时提供 profile_vectors）
    pub question_vector: Option<Vec<f32>>,
    /// file_id → 画像向量（可选；语义通道需同时提供 question_vector）
    pub profile_vectors: HashMap<Uuid, Vec<f32>>,
}

impl<'a> ResolverInput<'a> {
    pub fn new(
        plan: &'a QueryPlan,
        session: &'a AskSessionContext,
        profiles: Vec<DocumentProfile>,
        file_names: HashMap<Uuid, String>,
    ) -> Self {
        Self {
            plan,
            session,
            profiles,
            file_names,
            question_vector: None,
            profile_vectors: HashMap::new(),
        }
    }

    /// 追加语义通道向量（builder：不影响 `new` 的既有调用方）。
    pub fn with_vectors(
        mut self,
        question_vector: Option<Vec<f32>>,
        profile_vectors: HashMap<Uuid, Vec<f32>>,
    ) -> Self {
        self.question_vector = question_vector;
        self.profile_vectors = profile_vectors;
        self
    }
}

/// 目标对象是否完全为空（没有任何可定位依据）。
///
/// FIND 例外：FIND 的 content_query 就是用户对文件的描述（「2019年数据库
/// 下午的真题文件」），即使 reference/type/name 全空也具备定位依据。判定
/// 口径与 find_content 信号（见 `score_candidate` 7.5）一致：描述清洗后
/// ≥4 字符才算可定位；过短（如「合同」）不构成区分信号，判空退回宽 scope。
fn target_is_empty(plan: &QueryPlan) -> bool {
    let no_target = plan
        .target
        .reference
        .as_deref()
        .unwrap_or("")
        .trim()
        .is_empty()
        && plan
            .target
            .document_name
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty()
        && plan.target.document_type.is_none()
        && plan
            .target
            .entity_name
            .as_deref()
            .unwrap_or("")
            .trim()
            .is_empty();
    if !no_target {
        return false;
    }
    if plan.intent == QueryIntent::DocumentFind {
        let desc = plan.content_query.as_deref().unwrap_or("").trim();
        return strip_target_stop_phrases(desc).chars().count() < 4;
    }
    true
}

/// 解析目标对象为文件白名单。返回的 `DocumentResolution` 供编排层：
/// - Resolved → scope.file_ids = [唯一文件]；
/// - MultipleCandidates → scope.file_ids = top-2/3；
/// - Unresolved → scope 不设 file_ids（退回宽检索）。
pub fn resolve_documents(input: &ResolverInput<'_>) -> DocumentResolution {
    if target_is_empty(input.plan) {
        return DocumentResolution::unresolved("目标对象为空（reference/document_type/name 均无）");
    }
    if input.profiles.is_empty() {
        return DocumentResolution::unresolved("没有可用文档画像，无法定位目标文件");
    }

    // 模型驱动精确定位：当 LLM Parser 凭语义判定用户「精确点名」了某份文档
    // （给出完整标题/文件名）并把完整标题放进 target.document_name 时，
    // resolver **信任模型**，把候选收敛到名字精确对应的内容族：库内恰好一份
    // → 锁定；多份同名副本 → 全部进 scope（它们是被点名的同一内容族）。
    // 精确匹配覆盖两种层级（见 [`precise_name_equals`]）：
    //   1. 全串相等型：完整标题/文件名（含剥扩展名、副本序号）；
    //   2. 内容族子串型：目标名是真实文件名的连续子串，用于同名内容族的
    //      版本前缀/后缀（如 `人工智能面试宝典` ⊂ `1人工智能面试宝典_V6.6(20250606)`）。
    // 当两层都落空时，**不**立即判「不在库内」：真实文档因年份/措辞与生成
    // 标题存在词序或插字差异（如「数据库系统工程师考试2020年上午真题」vs
    // 「2020年数据库系统工程师考试上午真题」）时，回退到下方的通用多信号
    // 评分（语义/类型/标题/文件名）继续定位，避免把库内真实存在的文档误报
    // 为不存在、进而丢失 Document Recall。通用评分自身有阈值与 margin 约束，
    // 仍不会仅凭一个弱信号就错误锁定；确实与库内任何文档不相干时才 Unresolved。
    if input.plan.target.precise_named_document
        && let Some(target_name) = input.plan.target.document_name.as_deref()
    {
        let target_name = target_name.trim();
        if !target_name.is_empty() {
            let exact: Vec<&DocumentProfile> = input
                .profiles
                .iter()
                .filter(|profile| {
                    let file_name = input
                        .file_names
                        .get(&profile.file_id)
                        .map(String::as_str)
                        .unwrap_or("");
                    precise_name_equals(target_name, profile, file_name)
                })
                .collect();
            if !exact.is_empty() {
                let scope: Vec<Uuid> = exact.iter().map(|profile| profile.file_id).collect();
                let candidates = exact
                    .iter()
                    .map(|profile| {
                        DocumentCandidate::new(
                            profile.file_id,
                            1.0,
                            vec!["precise_named_document".to_owned()],
                        )
                    })
                    .collect();
                return if exact.len() == 1 {
                    DocumentResolution {
                        candidates,
                        resolved_file_ids: scope,
                        confidence: 1.0,
                        status: ResolutionStatus::Resolved,
                        fallback_reason: None,
                    }
                } else {
                    DocumentResolution {
                        candidates,
                        resolved_file_ids: scope,
                        confidence: 1.0,
                        status: ResolutionStatus::MultipleCandidates,
                        fallback_reason: Some(
                            "精确点名命中多份同名副本（同一内容族），一并进 scope".to_owned(),
                        ),
                    }
                };
            }
        }
    }

    // owner_match 是「我的」归属者给所有画像的底分信号（0.05），不算候选
    // 资格：仅靠底分进候选会在真实库（千级画像）里把无关文件拖进 top-3 scope。
    let owner_floor = SIGNAL_WEIGHTS
        .iter()
        .find(|(name, _)| *name == "owner_match")
        .map(|(_, weight)| *weight)
        .unwrap_or(0.0);
    let mut candidates: Vec<DocumentCandidate> = input
        .profiles
        .iter()
        .map(|profile| score_candidate(input, profile))
        .filter(|candidate| candidate.score > owner_floor)
        .collect();
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_id.to_string().cmp(&b.file_id.to_string()))
    });

    let Some(best) = candidates.first() else {
        return DocumentResolution::unresolved(
            "没有任何信号命中（类型/标题/实体/文件名均不匹配），不锁定文件，退回宽 scope",
        );
    };
    if candidates.len() == 1 {
        if best.score >= MEDIUM_CONFIDENCE_THRESHOLD {
            return DocumentResolution {
                candidates: candidates.clone(),
                resolved_file_ids: vec![best.file_id],
                confidence: best.score,
                status: ResolutionStatus::Resolved,
                fallback_reason: None,
            };
        }
        return DocumentResolution::unresolved("唯一候选分数过低，不锁定文件，退回宽 scope");
    }

    let second = &candidates[1];
    // 「明显唯一」的决定性判据是与第二名的差距，而非绝对分：
    // 0.40 vs 0.05（gap 0.35）明显是「我的简历」；0.40 vs 0.38 则是「非常接近」。
    let second_close = best.score - second.score < HIGH_MARGIN;
    if best.score >= MEDIUM_CONFIDENCE_THRESHOLD && !second_close {
        return DocumentResolution {
            candidates: candidates.clone(),
            resolved_file_ids: vec![best.file_id],
            confidence: best.score,
            status: ResolutionStatus::Resolved,
            fallback_reason: None,
        };
    }
    if best.score >= MEDIUM_CONFIDENCE_THRESHOLD {
        // 存在多个非常接近的候选（含高置信度接近的情况）：P0 保留 top-2/3
        // 进 scope（不锁单文件），需要澄清的交互留给下一阶段。
        let scope: Vec<Uuid> = candidates
            .iter()
            .take(MAX_CANDIDATE_SCOPE)
            .map(|candidate| candidate.file_id)
            .collect();
        return DocumentResolution {
            candidates: candidates.clone(),
            resolved_file_ids: scope,
            confidence: best.score,
            status: ResolutionStatus::MultipleCandidates,
            fallback_reason: Some("存在多个接近候选，保留 top-2/3 进 scope".to_owned()),
        };
    }
    DocumentResolution::unresolved("低置信度，不错误锁定文件，退回宽 scope")
}

/// 精准姓名映射（模型驱动精确定位）：判断「模型点名的标题」是否就是该
/// 文档。两级判定，均为通用机械归一化，不构成针对任何文件的特判：
///   1. **全串相等**（两侧去空白）：剥一次扩展名 + 剥尾部「副本序号」后相等，
///      使「带/不带扩展名」「主名/副本」三种点名都能对齐；
///   2. **内容族子串**：目标名的字母数字归一化形式是真实文件名归一化形式的
///      连续子串（或反过来），用于同名内容族的版本前缀/后缀/DocumentProfile
///      补全，如目标 `人工智能面试宝典` ⊂ 文件名 `1人工智能面试宝典_V6.6(20250606)`。
///      为避免过短的泛化子串（如「报告」）一次性吞掉多个无关文档，子串层只
///      在目标归一化长度 ≥ [`SUFFIX_CORE_MIN`] 时启用。
fn precise_name_equals(target: &str, profile: &DocumentProfile, file_name: &str) -> bool {
    let target = target.trim();
    if target.is_empty() {
        return false;
    }
    let target_norm = normalize_precise_name(target);
    if target_norm.is_empty() {
        return false;
    }
    // 画像标题与文件名各自归一化后，任一与 target 归一化后全串相等即命中。
    let title_norm = normalize_precise_name(&profile.title);
    if !title_norm.is_empty() && title_norm == target_norm {
        return true;
    }
    let stem_norm = normalize_precise_name(file_name);
    if !stem_norm.is_empty() && stem_norm == target_norm {
        return true;
    }
    // 内容族子串层（版本前缀/后缀、补全）：真实文档名/标题的归一化形式**包含**
    // 点名的归一化形式时视为同一内容族。只做正向包含（真实名是点名名的超集，
    // 如 `1人工智能面试宝典_V6.6(20250606)` ⊇ `人工智能面试宝典`），不做反向
    // （点名名包含短文件名）——反向会让短泛化标题（如「SQL」「报告」）被较长
    // 的点名名一次性吞进精确 scope。目标长度低于下限时不启用该层。
    let alnum = |value: &str| {
        value
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect::<String>()
    };
    let target_alnum = alnum(&target_norm);
    if target_alnum.chars().count() < SUFFIX_CORE_MIN {
        return false;
    }
    let title_alnum = alnum(&title_norm);
    if !title_alnum.is_empty() && title_alnum.contains(&target_alnum) {
        return true;
    }
    let stem_alnum = alnum(&stem_norm);
    if !stem_alnum.is_empty() && stem_alnum.contains(&target_alnum) {
        return true;
    }
    // 3. **词序翻转/插字容错**：目标与真实名的字母数字串字符二元组高重合且
    //    关键限定（4 位年份、上下午）一致 → 同一内容族。
    //    「数据库系统工程师考试2020年上午真题」与真实文件名
    //    「2020年数据库系统工程师考试上午真题（参考答案）」全串不等、互不包含
    //    （年份位置不同），只有二元组重合能识别为同一文档；年份/上下午门保证
    //    不把「2020上午」误并到「2020下午」或「2019年」的同类文件。
    if (!title_alnum.is_empty() && reordered_same_family(&target_alnum, &title_alnum))
        || (!stem_alnum.is_empty() && reordered_same_family(&target_alnum, &stem_alnum))
    {
        return true;
    }
    false
}

/// 内容族子串层的最小归一化长度：目标归一化字符数低于该值时不走子串匹配，
/// 防止「报告」「简历」这类短泛化子串把多个无关文档一次性吞进精确 scope。
const SUFFIX_CORE_MIN: usize = 6;

/// 词序翻转内容族层的二元组重合下限。二元组是集合式度量、对词序不敏感——
/// 「数据库系统工程师考试2020年上午真题」与「2020年数据库系统工程师考试上午真题
/// （参考答案）」字符组成几乎一致、只是年份位置不同，重合率≈0.95；而同年份的
/// 下午卷因上下午限定不同会低于该值，配合下方年份/上下午门做最终区分。
///
/// 下限须兼顾**整句子句平移**场景：压缩描述「2020年上午数据库真题」把「上午」
/// 整个从句挪到年份之后（实况文件名是「2020年数据库系统工程师考试上午真题
/// （参考答案）」，主题串夹在年份与会话之间），从句边界二元组（年→上午、
/// 上午→数据库、数据库→真题）随平移丢失，二元组去重后重合率≈0.70。这类描述
/// 仍是有年份+上下午+主题的唯一指代，年份/上下午门已把跨年、跨会话文件排除，
/// 故下限须放到能容纳该平移丢失（≈0.70）的水平才不至于把「不同文件」误判漏
/// 锁。低于 0.68 会开始把明显缺词的泛化描述与同年份同会话的同类文件误并（靠
/// MultipleCandidates/ambiguous 兜底，不误答），故保守取 0.68。
const REORDERED_FAMILY_MIN_COVERAGE: f32 = 0.68;

/// 上下午/册次/篇次会话标记：识别「上午/下午」「上卷/下卷」「上册/下册」
/// 「上篇/下篇」，用于区分同一年份的会话版本（上午真题 vs 下午真题）。
/// [`SessionMarker::None`] 表示文本不含会话限定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionMarker {
    None,
    First,
    Second,
}

/// 提取文本的会话标记（上午→First，下午→Second，无→None）。
fn session_marker(text: &str) -> SessionMarker {
    let first = text.contains("上午")
        || text.contains("上卷")
        || text.contains("上册")
        || text.contains("上篇");
    let second = text.contains("下午")
        || text.contains("下卷")
        || text.contains("下册")
        || text.contains("下篇");
    match (first, second) {
        (true, false) => SessionMarker::First,
        (false, true) => SessionMarker::Second,
        // 同时出现上/下限定时无法把它当作单份文档的可靠身份信号。
        _ => SessionMarker::None,
    }
}

/// 词序翻转/插字容忍的「同一内容族」判定（输入应为字母数字归一化串）。
///
/// 目标串与真实名的字符二元组重合 ≥ [`REORDERED_FAMILY_MIN_COVERAGE`]，且
/// 关键限定一致时才视为同一文档：
/// - **年份门**：两侧都含 4 位年份且不一致 → 不同文件（「2020年真题」与
///   「2019年真题」二元组覆盖率基本打平，年份一致是唯一区分信号）；
/// - **上下午门**：目标限定上午而真实名是下午（或反之）→ 不同文件。
/// 目标无年份/无会话限定时不约束（只靠重合率与对方的一致性判断），避免
/// 「真题」这类无限定描述把所有同类文件误并。通用机械归一化，不针对任何文件。
fn reordered_same_family(target: &str, real_name: &str) -> bool {
    if target.chars().count() < SUFFIX_CORE_MIN || real_name.chars().count() < SUFFIX_CORE_MIN {
        return false;
    }
    if bigram_coverage(target, real_name) < REORDERED_FAMILY_MIN_COVERAGE {
        return false;
    }
    match (extract_year(target), extract_year(real_name)) {
        (Some(target_year), Some(real_year)) if target_year != real_year => return false,
        _ => {}
    }
    let target_session = session_marker(target);
    let real_session = session_marker(real_name);
    !(target_session != SessionMarker::None
        && real_session != SessionMarker::None
        && target_session != real_session)
}

/// 归一化精确点名标题：去两侧空白 → 剥一次扩展名 → 剥一次尾部「副本序号」
/// （`_1`/`_12`）。「报告.docx」→「报告」；「报告_1.docx」→「报告」；
/// 「a.b.docx」→「a.b」。不做相似度/子串启发，只做确定性的全串级归一。
fn normalize_precise_name(name: &str) -> String {
    let mut s = name.trim().to_owned();
    s = strip_extension_like(&s).to_owned();
    s = strip_copy_suffix(&s);
    s
}

/// 剥文件名尾部副本序号：`报告_1` → `报告`（序号为纯 ASCII 数字、前面非空
/// 时才剥）。`A_项目` 不剥（下划线后非纯数字）。只剥一次。
fn strip_copy_suffix(name: &str) -> String {
    match name.rsplit_once('_') {
        Some((stem, suffix))
            if !stem.is_empty()
                && !suffix.is_empty()
                && suffix.bytes().all(|b| b.is_ascii_digit()) =>
        {
            stem.to_owned()
        }
        _ => name.to_owned(),
    }
}

/// 剥离扩展名："报告.docx" → "报告"（"a.b.docx" → "a.b"；无扩展名原样返回）。
fn strip_extension_like(file_name: &str) -> &str {
    match file_name.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty() && extension.chars().all(|ch| !ch.is_whitespace()) =>
        {
            stem
        }
        _ => file_name,
    }
}

/// `needle` 是否按序（允许跳字符）出现在 `haystack` 中。
///
/// FIND 定位专用：用户描述「2019年数据库下午真题」与真实文件名
/// 「2019年上半年数据库系统工程师考试下午真题」之间存在插词（上半年/系统
/// 工程师考试），全串子串匹配会失败，子序列匹配容忍这些插入词，是「年份+
/// 上下午+主题」这类描述的通用定位手段。`needle` 为空返回 false。
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut needle_chars = needle.chars();
    let mut pending = needle_chars.next();
    for ch in haystack.chars() {
        if let Some(need) = pending {
            if need == ch {
                pending = needle_chars.next();
            }
        }
        if pending.is_none() {
            return true;
        }
    }
    pending.is_none()
}

/// 描述串与文件名的相邻字符二元组重合率（0..=1）：衡量描述串的字符序列在
/// 文件名里保留了多大比例。描述为空返回 0。子序列已覆盖「插词」场景，二元组
/// 重合度是它的梯度分量（描述覆盖越完整、加分越多，正确文件与同年的其它
/// 文件由此拉开差距）。
fn bigram_coverage(needle: &str, haystack: &str) -> f32 {
    let needle_chars = needle.chars().collect::<Vec<_>>();
    if needle_chars.len() < 2 || haystack.is_empty() {
        return 0.0;
    }
    let needle_bigrams: std::collections::HashSet<(char, char)> =
        needle_chars.windows(2).map(|w| (w[0], w[1])).collect();
    let haystack_bigrams: std::collections::HashSet<(char, char)> = haystack
        .chars()
        .collect::<Vec<_>>()
        .windows(2)
        .map(|w| (w[0], w[1]))
        .collect();
    let hit = needle_bigrams
        .iter()
        .filter(|bigram| haystack_bigrams.contains(bigram))
        .count();
    hit as f32 / needle_bigrams.len() as f32
}

/// 从任意文本中提取「4 位年份」（如 "2020-09-01" → 2020，"2019" → 2019）。
///
/// FIND 定位专用：时间过滤器可能是完整日期或年份，描述串与文件名都含年份时
/// 需要归一成 4 位整数做一致性比较。只取 `1900..=2099` 的合理区间，避免
/// 「1234」这类纯数字串误判；字节级扫描保证不会切开多字节汉字。提取不到返回
/// None（调用方据此不做年份判定）。
fn extract_year(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    let mut best = None;
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            if let Some(year) = text[i..i + 4].parse::<u32>().ok() {
                if (1900..=2099).contains(&year) {
                    best = Some(year);
                    break;
                }
            }
        }
        i += 1;
    }
    best
}

/// DocumentProfile 的摘要由正文头部生成；短文件名缺失年份/卷次时，摘要开头
/// 往往仍保留封面标题。只读取一个有界前缀作为身份补充，避免把正文后部提到的
/// 其它版本误当成当前文件身份。
const PROFILE_IDENTITY_SUMMARY_CHARS: usize = 160;

fn profile_identity_summary(profile: &DocumentProfile) -> String {
    profile
        .summary
        .chars()
        .take(PROFILE_IDENTITY_SUMMARY_CHARS)
        .collect()
}

fn merge_session_markers(markers: impl IntoIterator<Item = SessionMarker>) -> SessionMarker {
    let mut merged = SessionMarker::None;
    for marker in markers {
        if marker == SessionMarker::None {
            continue;
        }
        if merged != SessionMarker::None && merged != marker {
            return SessionMarker::None;
        }
        merged = marker;
    }
    merged
}

fn unambiguous_year<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<u32> {
    let mut found = None;
    for text in texts {
        let bytes = text.as_bytes();
        let mut index = 0;
        while index + 4 <= bytes.len() {
            if bytes[index..index + 4]
                .iter()
                .all(|byte| byte.is_ascii_digit())
                && let Ok(year) = text[index..index + 4].parse::<u32>()
                && (1900..=2099).contains(&year)
            {
                if found.is_some_and(|existing| existing != year) {
                    return None;
                }
                found = Some(year);
                index += 4;
                continue;
            }
            index += 1;
        }
    }
    found
}

/// 明确目标描述与候选画像是否存在确定性的身份冲突。
///
/// 文件名/画像标题优先；仅当它们缺失对应限定时，才用摘要头部补齐。两侧都明确
/// 给出年份或卷次且不一致时才判冲突；任一侧缺失或摘要同时出现上下卷时保持未知，
/// 继续走原有多信号评分。该门只收窄带身份限定的目标 scope；普通概念问答没有
/// 年份/卷次限定，不受影响。
fn target_identity_conflicts(input: &ResolverInput<'_>, profile: &DocumentProfile) -> bool {
    let target = [
        input.plan.target.document_name.as_deref(),
        input.plan.target.reference.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
        .filter(|value| !value.trim().is_empty())
        .max_by_key(|value| value.chars().count());
    let Some(target) = target else {
        return false;
    };
    let file_name = input
        .file_names
        .get(&profile.file_id)
        .map(String::as_str)
        .unwrap_or("");
    let summary = profile_identity_summary(profile);

    let target_year = unambiguous_year([target]);
    let candidate_year = unambiguous_year([profile.title.as_str(), file_name, summary.as_str()]);
    if matches!(
        (target_year, candidate_year),
        (Some(target_year), Some(candidate_year)) if target_year != candidate_year
    ) {
        return true;
    }

    let candidate_session = merge_session_markers([
        session_marker(&profile.title),
        session_marker(file_name),
        session_marker(&summary),
    ]);
    let target_session = session_marker(target);
    target_session != SessionMarker::None
        && candidate_session != SessionMarker::None
        && target_session != candidate_session
}

/// 对单个画像综合打分，返回候选与命中信号列表。
fn score_candidate(input: &ResolverInput<'_>, profile: &DocumentProfile) -> DocumentCandidate {
    if target_identity_conflicts(input, profile) {
        return DocumentCandidate::new(profile.file_id, 0.0, vec!["identity_conflict".to_owned()]);
    }
    let mut score = 0.0;
    let mut signals = Vec::new();

    let weight = |key: &str| -> f32 {
        SIGNAL_WEIGHTS
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, weight)| *weight)
            .unwrap_or(0.0)
    };

    // 1. 文档类型（正文语义信号，优先于文件名；文件名里没有「简历」也可靠它命中）
    if let (Some(expected), Some(actual)) = (input.plan.target.document_type, profile.document_type)
    {
        if expected == actual {
            score += weight("document_type");
            signals.push("document_type".to_owned());
        }
    } else if let Some(expected) = input.plan.target.document_type
        && profile.document_type.is_none()
    {
        // 1b. 分类器未运行（document_type IS NULL）时的类型等价回退：
        // 用分类器同款 TYPE_KEYWORDS 在 title/filename 上的确定性命中代替
        // 类型信号（「简历」命中 title/filename ≈ 简历类型证据）。只在该
        // 画像确实无类型时才生效，已有类型的画像不受影响。
        let filename = input.file_names.get(&profile.file_id).map(String::as_str);
        let type_keyword_hit = type_keywords_for(expected).iter().any(|keyword| {
            profile.title.contains(keyword) || filename.is_some_and(|name| name.contains(keyword))
        });
        if type_keyword_hit {
            score += weight("document_type");
            signals.push("document_type_fallback".to_owned());
        }
    }

    // 2. 会话当前激活文件（上轮锁定的文件，指代恢复的最强依据之一）
    if input.session.active_file_id == Some(profile.file_id) {
        score += weight("session_active");
        signals.push("session_active".to_owned());
    }

    // 3. 文档标题（用户给的名字与画像标题互含即命中）。
    //    分类器未运行时（document_type IS NULL）追加「有意义词元」命中：
    //    「我那个大模型的材料」→ 词元「大模型」⊂ 标题即命中（整串互含对
    //    指代式短语永远失败，token 化是唯一可行路径）。
    let title_tokens = [
        input.plan.target.document_name.as_deref(),
        input.plan.target.reference.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|token| !token.trim().is_empty())
    .collect::<Vec<_>>();
    let target_tokens = if profile.document_type.is_none() {
        title_tokens
            .iter()
            .flat_map(|token| meaningful_tokens(token))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    // GraduationReferenceResolver（CASE 5）：目标含「毕业」→ 毕业类等价
    // 词元（论文/答辩/开题/学位…）参与标题匹配。独立于 target_tokens 的
    // 类型条件：已分类为 Paper 的「学位论文」同样要被「毕业」引用命中。
    let graduation_reference = title_tokens
        .iter()
        .any(|token| token.contains(GRADUATION_REFERENCE_MARKER));
    if !title_tokens.is_empty()
        && (title_tokens
            .iter()
            .any(|token| profile.title.contains(token) || token.contains(profile.title.trim()))
            || target_tokens
                .iter()
                .any(|token| profile.title.contains(token)))
    {
        score += weight("document_title");
        signals.push("document_title".to_owned());
    } else if graduation_reference
        && GRADUATION_EXPANSION_TERMS
            .iter()
            .any(|term| profile.title.contains(term))
    {
        score += weight("document_title");
        signals.push("graduation_reference".to_owned());
    }

    // 4. 最近引用过的文件（recent usage，弱于 active）
    if input
        .session
        .last_referenced_file_ids
        .contains(&profile.file_id)
    {
        score += weight("session_referenced");
        signals.push("session_referenced".to_owned());
    }

    // 5. 实体匹配（「周晨」「LangGraph 项目」等实体命中画像 entities）
    if let Some(entity_name) = input.plan.target.entity_name.as_deref() {
        let entity = entity_name.trim();
        if !entity.is_empty()
            && profile
                .entities
                .iter()
                .any(|candidate| candidate.contains(entity) || entity.contains(candidate.as_str()))
        {
            score += weight("entity_match");
            signals.push("entity_match".to_owned());
        }
    }

    // 6. 关键词命中（reference/name 的令牌与画像 keywords 双向互含）
    if !title_tokens.is_empty()
        && title_tokens.iter().any(|token| {
            profile
                .keywords
                .iter()
                .any(|keyword| keyword.contains(token) || token.contains(keyword))
        })
    {
        score += weight("keyword_match");
        signals.push("keyword_match".to_owned());
    }

    // 7. 文件名（最弱信号：只做兜底，绝不单独决定）
    if let Some(filename) = input.file_names.get(&profile.file_id) {
        if title_tokens.iter().any(|token| filename.contains(token))
            || target_tokens.iter().any(|token| filename.contains(token))
        {
            score += weight("filename");
            signals.push("filename".to_owned());
        } else if graduation_reference
            && GRADUATION_EXPANSION_TERMS
                .iter()
                .any(|term| filename.contains(term))
        {
            score += weight("filename");
            signals.push("graduation_filename".to_owned());
        }
    }

    // 7.5 FIND 定位：content_query/filters 描述串与文件名的「子序列+二元组
    //     重合」匹配。仅 DOCUMENT_FIND 生效——FIND 里 content_query 就是用户
    //     对文件的描述（「2019年数据库下午的真题文件」），必须参与定位；
    //     其它 intent 的 content_query 是正文问题，绝不用来定位文件（见
    //     target_lock_does_not_depend_on_content_query_hits 的约束）。
    //     子序列匹配容忍真实文件名里的插词（上半年/系统工程师考试），二元组
    //     重合度提供梯度，使覆盖完整描述的文件显著领先只覆盖部分片段的文件。
    if input.plan.intent == QueryIntent::DocumentFind {
        let mut find_desc = input
            .plan
            .content_query
            .clone()
            .unwrap_or_default()
            .trim()
            .to_owned();
        // 时间过滤器年份归一：日期（2020-09-01）或年份（2019）都只提取
        // 「4 位年份」追加到描述串，且描述里已有同年时跳过——把日期整体
        // 追加会把 2020-09-01 这类噪音混入描述、稀释二元组重合度。
        if let Some(time) = input.plan.filters.time.as_deref() {
            if let Some(year) = extract_year(time) {
                let year = year.to_string();
                if !find_desc.contains(&year) {
                    find_desc.push_str(&year);
                }
            }
        }
        let find_desc = strip_target_stop_phrases(&find_desc);
        let filename = input
            .file_names
            .get(&profile.file_id)
            .map(String::as_str)
            .unwrap_or("");
        // 描述过短（清洗后 <4 字符，如「合同」）时不参与：短描述对所有同类
        // 文件一视同仁，起不到区分作用，反而可能让无关文件的字符恰好按序
        // 命中而误加信号。
        if find_desc.chars().count() >= 4 && !filename.is_empty() {
            let matched = is_subsequence(&find_desc, filename);
            let coverage = bigram_coverage(&find_desc, filename);
            // 年份一致性：描述串与文件名**都**含 4 位年份时要求一致。二元组
            // 重合是集合式度量、对词序不敏感，「2020年」与「2021年」两份文件
            // 覆盖率基本打平（都含 20/02/年上），子序列又因年份位置差异（用户
            // 把年份放中间 vs 文件名放开头）失配——年份一致是唯一能区分「同
            // 一年的版本」与「不同年份的其它文件」的通用信号。描述没给年份时
            // 不靠年份判定（false），避免「真题」这类无年份描述把所有年份的
            // 同类文件一锅端升级进 scope。
            let year_consistent = match (extract_year(&find_desc), extract_year(filename)) {
                (Some(desc_year), Some(file_year)) => desc_year == file_year,
                _ => false,
            };
            // 上下午/册次一致性：描述串与文件名都带会话限定（上午/下午、上/下卷、
            // 上/下册）时要求一致。二元组重合是集合式度量、对词序不敏感，
            // 「2020年…上午真题」与「2020年…下午真题」覆盖率几乎打平（只差
            // 一个上/下字），子序列又因「上午」的「上」不在下午文件名里而失配——
            // 会话一致是唯一能区分用户指的上/下午版本的通用信号。描述没给会话
            // 限定时不约束（None），避免「真题」这类无限定描述误伤。
            let session_consistent = match (session_marker(&find_desc), session_marker(filename)) {
                (SessionMarker::None, _) | (_, SessionMarker::None) => true,
                (desc_session, file_session) => desc_session == file_session,
            };
            if matched {
                // 子序列命中天然携带会话一致性（「上午」的「上」不在下午文件名
                // 里，子序列必然失配），无需额外门。
                let find_weight = weight("find_content");
                score += find_weight + find_weight * coverage;
                signals.push("find_content".to_owned());
            } else if coverage >= 0.6 && year_consistent && session_consistent {
                // 词序翻转但二元组高重合、**年份一致且上下午一致**：视为命中——
                // 描述与文件是同一份文件、只是年份/词序位置不同（「数据库系统
                // 工程师2020年上午真题」vs「2020年数据库系统工程师考试上午真题」）。
                // 任一门冲突（不同年份 / 上午对下午）都不得靠覆盖率锁定，降级为
                // 下方部分分（补召回不锁定）。
                let find_weight = weight("find_content");
                score += find_weight + find_weight * coverage;
                signals.push("find_content".to_owned());
            } else if coverage >= 0.6 {
                // 未形成完整子序列且年份冲突/无年份：给部分分（补召回不锁定），
                // 避免真实存在的文件因词序差异在打分里被完全丢掉，但绝不靠它
                // 锁定——年份冲突说明不是用户指的那一年。
                score += weight("find_content") * 0.6 * coverage;
                signals.push("find_content_partial".to_owned());
            }
        }
    }

    // 7.6 参考串定位：target.reference/document_name 描述串与文件名的「子序列
    //     +二元组重合」匹配（非 FIND 的定位类 intent 通用）。
    //     与 7.5 的分工：7.5 只服务 DOCUMENT_FIND（那里 content_query 就是用户
    //     对文件的描述）；这里是用户对目标文件的描述（reference/document_name），
    //     如「数据库系统工程师考试2020年上午真题」vs 真实文件名
    //     「2020年数据库系统工程师考试上午真题（参考答案）」——词序不同（年份
    //     位置）、存在插字，全串互含匹配失败，只有子序列+二元组重合能识别为
    //     同一文档，且年份/上下午一致性是区分「同一年版本」与「其它年份/另一
    //     会话文件」的通用信号（见 [`reordered_same_family`]）。描述过短（清洗
    //     后 <6 字符）时可能只是概念名或内容词（「ACID」「事务特性」），不参与
    //     定位，避免把无关文件的字符恰好按序命中而误加信号。
    if input.plan.intent != QueryIntent::DocumentFind {
        let reference = input
            .plan
            .target
            .reference
            .as_deref()
            .unwrap_or("")
            .trim();
        let doc_name = input
            .plan
            .target
            .document_name
            .as_deref()
            .unwrap_or("")
            .trim();
        // 取更完整的描述串（parser 不稳定时有时填 reference、有时填 document_name）。
        let reference_desc = if reference.chars().count() >= doc_name.chars().count() {
            reference
        } else {
            doc_name
        };
        let reference_desc = strip_target_stop_phrases(reference_desc);
        let filename = input
            .file_names
            .get(&profile.file_id)
            .map(String::as_str)
            .unwrap_or("");
        if reference_desc.chars().count() >= 6 && !filename.is_empty() {
            let matched = is_subsequence(&reference_desc, filename);
            let coverage = bigram_coverage(&reference_desc, filename);
            if matched {
                let reference_weight = weight("reference_match");
                score += reference_weight + reference_weight * coverage;
                signals.push("reference_match".to_owned());
            } else if reordered_same_family(&reference_desc, filename) {
                // 词序翻转但字符组成几乎一致且年份/上下午一致：同一文档。
                let reference_weight = weight("reference_match");
                score += reference_weight + reference_weight * coverage;
                signals.push("reference_match".to_owned());
            } else if coverage >= 0.6 {
                // 未形成完整子序列且关键限定冲突/缺失：给部分分（补召回不锁定）。
                score += weight("reference_match") * 0.6 * coverage;
                signals.push("reference_match_partial".to_owned());
            }
        }
    }

    // 8. 归属者：P0 没有归属者元数据；「我的」= 用户自己的文件库，
    //    给所有候选一个微弱信号，保持 owner_match 信号可见。
    if input.plan.target.owner.as_deref() == Some("self") {
        score += weight("owner_match");
        signals.push("owner_match".to_owned());
    }

    // 9. 语义通道（补召回，不设 metadata 门槛）：问题向量与画像向量余弦
    //    ≥ SEMANTIC_MATCH_MIN_COSINE 即贡献语义分。文件名/标题里没有「简历」
    //    （如 final_v3.pdf）但正文语言与「我的简历」相近的正确文件，靠本
    //    通道进入候选池，交由 scope 判定收敛——语义是补召回，不是主排序。
    if let (Some(question_vector), Some(profile_vector)) = (
        input.question_vector.as_deref(),
        input
            .profile_vectors
            .get(&profile.file_id)
            .map(|v| v.as_slice()),
    ) {
        let cosine = cosine_similarity(question_vector, profile_vector);
        if cosine >= SEMANTIC_MATCH_MIN_COSINE {
            score += weight("semantic") * cosine;
            signals.push("semantic_match".to_owned());
        }
    }

    DocumentCandidate::new(profile.file_id, score.min(1.0), signals)
}

/// 便捷函数：按文档类型检索候选（编排层查询画像时的过滤器）。
pub fn profile_document_type(profile: &DocumentProfile) -> Option<DocumentType> {
    profile.document_type
}

#[cfg(test)]
mod tests {
    use crate::ask::query_plan::{QueryIntent, QueryOperation, QueryTarget, SourceIntent};

    use super::*;

    fn profile(file_id: Uuid, document_type: Option<DocumentType>, title: &str) -> DocumentProfile {
        DocumentProfile {
            file_id,
            revision_id: Uuid::now_v7(),
            title: title.to_owned(),
            summary: String::new(),
            keywords: Vec::new(),
            entities: Vec::new(),
            document_type,
            type_confidence: None,
            section_titles: Vec::new(),
            representative_text_hash: None,
            updated_at: chrono::Utc::now(),
        }
    }

    fn resume_plan() -> QueryPlan {
        QueryPlan {
            source: SourceIntent::Local,
            intent: QueryIntent::DocumentQa,
            operation: QueryOperation::Extract,
            target: QueryTarget {
                reference: Some("我的简历".to_owned()),
                document_type: Some(DocumentType::Resume),
                document_name: None,
                precise_named_document: false,
                owner: Some("self".to_owned()),
                entity_type: None,
                entity_name: None,
            },
            content_query: Some("项目经历".to_owned()),
            requires_document_resolution: true,
            ..QueryPlan::default()
        }
    }

    #[test]
    fn resume_wins_over_how_to_write_resume_by_type_not_filename() {
        // CASE 1 的核心：正文类型信号优先于文件名。
        // 「如何写好简历.pdf」文件名含「简历」但类型不是简历；
        // 「大模型开发工程师-周晨.pdf」文件名不含「简历」但 document_type = resume。
        let guide = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "如何写好简历",
        );
        let actual_resume = profile(
            Uuid::now_v7(),
            Some(DocumentType::Resume),
            "大模型开发工程师-周晨",
        );
        let mut file_names = HashMap::new();
        file_names.insert(guide.file_id, "如何写好简历.pdf".to_owned());
        file_names.insert(
            actual_resume.file_id,
            "大模型开发工程师-周晨.pdf".to_owned(),
        );

        let plan = resume_plan();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![guide.clone(), actual_resume.clone()],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![actual_resume.file_id]);
        let best = &resolution.candidates[0];
        assert!(best.signals.iter().any(|s| s == "document_type"));
        // 学习资料靠 filename 撞词但总分不足以反超
        assert!(!best.signals.iter().any(|s| s == "filename"));
    }

    #[test]
    fn session_active_file_wins_over_weaker_type_match() {
        // 指代场景：会话已锁定 active_file_id，同一类型的其他文件不该抢
        let active = profile(Uuid::now_v7(), Some(DocumentType::Contract), "房屋租赁合同");
        let other = profile(Uuid::now_v7(), Some(DocumentType::Contract), "劳动合同");
        let session = AskSessionContext {
            active_file_id: Some(active.file_id),
            ..AskSessionContext::default()
        };
        let mut plan = resume_plan();
        plan.target.document_type = Some(DocumentType::Contract);
        plan.target.reference = Some("那份合同".to_owned());

        let input = ResolverInput::new(
            &plan,
            &session,
            vec![other.clone(), active.clone()],
            HashMap::new(),
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![active.file_id]);
        let best = &resolution.candidates[0];
        assert!(best.signals.iter().any(|s| s == "session_active"));
        assert!(best.signals.iter().any(|s| s == "document_type"));
    }

    #[test]
    fn two_very_close_candidates_return_multiple_candidates_scope() {
        // 两份简历几乎一样 → 不锁单文件，top-2 进 scope
        let resume_a = profile(Uuid::now_v7(), Some(DocumentType::Resume), "简历 v1");
        let resume_b = profile(Uuid::now_v7(), Some(DocumentType::Resume), "简历 v2");
        let plan = resume_plan();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![resume_a.clone(), resume_b.clone()],
            HashMap::new(),
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::MultipleCandidates);
        assert_eq!(resolution.resolved_file_ids.len(), 2);
        assert!(resolution.fallback_reason.is_some());
    }

    #[test]
    fn no_signal_hit_returns_unresolved_wide_scope() {
        // 目标与画像完全无关 → 不锁定，退回宽 scope
        let unrelated = profile(Uuid::now_v7(), Some(DocumentType::Invoice), "七月发票");
        let plan = resume_plan();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, vec![unrelated], HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
        assert!(resolution.resolved_file_ids.is_empty());
        assert!(resolution.fallback_reason.is_some());
    }

    #[test]
    fn entity_name_match_is_a_signal() {
        // 「周晨的论文」→ entity_name=周晨 命中画像实体；
        // 实体+关键词双重命中跨过中置信度阈值，明显唯一 → 锁定。
        let mut paper = profile(Uuid::now_v7(), Some(DocumentType::Paper), "多模态检索论文");
        paper.entities = vec!["周晨".to_owned()];
        paper.keywords = vec!["周晨".to_owned()];
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = Some("周晨的论文".to_owned());
        plan.target.entity_name = Some("周晨".to_owned());
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, vec![paper], HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        let signals = &resolution.candidates[0].signals;
        assert!(signals.iter().any(|s| s == "entity_match"));
        assert!(signals.iter().any(|s| s == "keyword_match"));
    }

    #[test]
    fn weak_entity_signal_alone_never_locks() {
        // 低置信度保护：仅实体命中（无第二信号）→ 不锁定，退回宽 scope
        let mut paper = profile(Uuid::now_v7(), Some(DocumentType::Paper), "多模态检索论文");
        paper.entities = vec!["周晨".to_owned()];
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = None;
        plan.target.entity_name = Some("周晨".to_owned());
        plan.target.owner = None;
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, vec![paper], HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
        assert!(resolution.resolved_file_ids.is_empty());
    }

    #[test]
    fn target_lock_does_not_depend_on_content_query_hits() {
        // CASE 5 的前半：目标锁定只看 target（简历），与 content_query 是否
        // 命中画像无关——「简历里有没有身份证号」在画像层面没有任何身份证
        // 关键词，仍必须锁定简历文件；「没有依据」由检索阶段裁决（LOCAL +
        // NO_EVIDENCE 返回固定文案，绝不转闲聊）。
        let resume = profile(
            Uuid::now_v7(),
            Some(DocumentType::Resume),
            "大模型开发工程师-周晨",
        );
        let mut plan = resume_plan();
        plan.content_query = Some("身份证号".to_owned());
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, vec![resume.clone()], HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![resume.file_id]);
    }

    #[test]
    fn langgraph_entity_resolves_to_file_scope() {
        // CASE 6 的后半：target 里的实体（LangGraph 项目）解析为 file_id
        // 白名单，后续检索只在这个 scope 内找「架构设计」。
        let mut project = profile(
            Uuid::now_v7(),
            Some(DocumentType::Other),
            "LangGraph 多智能体项目",
        );
        project.entities = vec!["LangGraph 项目".to_owned()];
        project.keywords = vec!["LangGraph".to_owned()];
        let mut plan = resume_plan();
        plan.target.document_type = None;
        // 解析器对「LangGraph 项目的架构设计」的典型输出：目标短语进
        // reference、实体进 entity_name，两者都是 target（与 content_query
        // 严格分离）；单靠实体+归属者（0.25）不锁定——见
        // weak_entity_signal_alone_never_locks。
        plan.target.reference = Some("LangGraph 项目".to_owned());
        plan.target.entity_name = Some("LangGraph 项目".to_owned());
        plan.content_query = Some("架构设计".to_owned());
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, vec![project.clone()], HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![project.file_id]);
        let best = &resolution.candidates[0];
        assert!(best.signals.iter().any(|s| s == "entity_match"));
        assert!(best.signals.iter().any(|s| s == "keyword_match"));
    }

    #[test]
    fn empty_target_returns_unresolved_without_scoring() {
        let mut plan = resume_plan();
        plan.target = QueryTarget::default();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, Vec::new(), HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
        assert!(resolution.fallback_reason.is_some());
    }

    #[test]
    fn no_profiles_returns_unresolved() {
        let plan = resume_plan();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, Vec::new(), HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
    }

    #[test]
    fn single_medium_candidate_locks_it() {
        // 唯一候选（type 命中）分数中等 → 明显唯一仍锁定
        let only = profile(
            Uuid::now_v7(),
            Some(DocumentType::Resume),
            "大模型开发工程师-周晨",
        );
        let plan = resume_plan();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, vec![only.clone()], HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![only.file_id]);
    }

    #[test]
    fn type_fallback_fires_when_classifier_not_run() {
        // CASE 5：分类器未运行（document_type 全 NULL）时，「我的简历」靠
        // TYPE_KEYWORDS 的「简历」命中 title/filename 拿到类型等价信号
        let unclassified = profile(Uuid::now_v7(), None, "周晨博简历.pdf");
        let mut plan = resume_plan();
        plan.operation = QueryOperation::Qa;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(unclassified.file_id, "周晨博简历.pdf".to_owned());
        let input = ResolverInput::new(&plan, &session, vec![unclassified], file_names);
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        let best = &resolution.candidates[0];
        assert!(
            best.signals.iter().any(|s| s == "document_type_fallback"),
            "未分类画像应走类型等价回退信号: {:?}",
            best.signals
        );
        assert_eq!(resolution.resolved_file_ids.len(), 1);
    }

    #[test]
    fn type_fallback_still_distinguishes_multi_resumes() {
        // CASE 5：多份未分类简历 → 全部拿到类型等价信号 → 多候选澄清，
        // 绝不锁错单文件，也绝不 unresolved 宽检索
        let resume_a = profile(Uuid::now_v7(), None, "周晨博简历.pdf");
        let resume_b = profile(Uuid::now_v7(), None, "周晨博简历英文.docx");
        let resume_c = profile(Uuid::now_v7(), None, "苗宇飞简历.pdf");
        let mut plan = resume_plan();
        plan.operation = QueryOperation::Qa;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&resume_a, &resume_b, &resume_c] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![resume_a, resume_b, resume_c],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(
            resolution.status,
            ResolutionStatus::MultipleCandidates,
            "多份同等可信简历必须澄清而不是猜"
        );
        assert_eq!(resolution.resolved_file_ids.len(), 3);
    }

    #[test]
    fn token_title_match_resolves_llm_material_target() {
        // CASE 8/9：目标「那个大模型的材料」（无类型信号）→ 词元「大模型」
        // 命中标题/文件名 → 候选生成（多候选由编排层澄清）
        let profile_md = profile(
            Uuid::now_v7(),
            None,
            "周晨博-大模型开发技术点与项目逐字稿-优化版.md",
        );
        let profile_manual = profile(Uuid::now_v7(), None, "大模型应用开发手册.pdf");
        let unrelated = profile(Uuid::now_v7(), None, "乡村振兴项目文档.md");
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = Some("我那个大模型的材料".to_owned());
        plan.target.owner = Some("self".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&profile_md, &profile_manual, &unrelated] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![profile_md, profile_manual, unrelated],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::MultipleCandidates);
        assert_eq!(
            resolution.resolved_file_ids.len(),
            2,
            "两个大模型材料候选进 scope"
        );
        // 无关的乡村振兴文档绝不进候选
        assert!(
            resolution
                .resolved_file_ids
                .iter()
                .all(|id| !id.to_string().contains("unrelated"))
        );
        let best = &resolution.candidates[0];
        assert!(best.signals.iter().any(|s| s == "document_title"));
    }

    #[test]
    fn token_title_match_finds_graduation_material() {
        // CASE 7：FIND 目标「我毕业时候那个材料」→ 词元「毕业」命中
        // 毕业设计/毕业体验类文件名
        let graduation = profile(Uuid::now_v7(), None, "毕业设计（论文）开题报告书.docx");
        let survey = profile(Uuid::now_v7(), None, "毕业生调查表(毕业生).pdf");
        let unrelated = profile(Uuid::now_v7(), None, "七月发票.pdf");
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = Some("我毕业时候那个材料".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&graduation, &survey, &unrelated] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![graduation, survey, unrelated],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::MultipleCandidates);
        assert_eq!(resolution.resolved_file_ids.len(), 2);
        assert!(
            resolution
                .resolved_file_ids
                .iter()
                .all(|id| !id.to_string().contains("unrelated"))
        );
    }

    #[test]
    fn graduation_expansion_matches_files_without_graduation_literal() {
        // Phase 4.3 CASE 5（GraduationReferenceResolver）：真实毕业材料常
        // 不含「毕业」两字（开题报告/学位论文/答辩PPT）——「毕业」引用靠
        // 扩展词元命中标题；多候选 → clarification，绝不直接拒绝。
        let proposal = profile(Uuid::now_v7(), Some(DocumentType::Paper), "开题报告书");
        let thesis = profile(
            Uuid::now_v7(),
            Some(DocumentType::Paper),
            "工学学位论文终稿",
        );
        let unrelated = profile(Uuid::now_v7(), None, "七月发票.pdf");
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = Some("我毕业时候那个材料".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&proposal, &thesis, &unrelated] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let unrelated_id = unrelated.file_id;
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![proposal, thesis, unrelated],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(
            resolution.status,
            ResolutionStatus::MultipleCandidates,
            "毕业类多候选必须澄清而不是拒绝"
        );
        assert_eq!(resolution.resolved_file_ids.len(), 2);
        let signals = &resolution.candidates[0].signals;
        assert!(
            signals.iter().any(|s| s == "graduation_reference"),
            "扩展命中应记录 graduation_reference 信号: {signals:?}"
        );
        // 无关文件不进候选
        assert!(
            resolution
                .resolved_file_ids
                .iter()
                .all(|id| *id != unrelated_id)
        );
    }

    #[test]
    fn graduation_expansion_does_not_hit_design_documents() {
        // 「设计」单独过宽：设计院合同/课程设计不能被「毕业」引用误命中
        //（扩展表只有「毕业设计」组合，无裸「设计」）
        let design_contract = profile(
            Uuid::now_v7(),
            Some(DocumentType::Contract),
            "设计院战略合作合同",
        );
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = Some("我毕业时候那个材料".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(design_contract.file_id, "设计院战略合作合同.pdf".to_owned());
        let input = ResolverInput::new(&plan, &session, vec![design_contract], file_names);
        let resolution = resolve_documents(&input);
        assert_ne!(
            resolution.status,
            ResolutionStatus::Resolved,
            "「设计」裸词不得被毕业引用命中"
        );
    }

    #[test]
    fn classified_profiles_keep_strict_matching() {
        // 已有类型的画像不受 token 回退影响：「如何写好简历」（学习资料）靠
        // filename 撞「简历」词也打不过真正类型=resume 的画像
        let guide = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "如何写好简历",
        );
        let actual_resume = profile(
            Uuid::now_v7(),
            Some(DocumentType::Resume),
            "大模型开发工程师-周晨",
        );
        let mut file_names = HashMap::new();
        file_names.insert(guide.file_id, "如何写好简历.pdf".to_owned());
        file_names.insert(
            actual_resume.file_id,
            "大模型开发工程师-周晨.pdf".to_owned(),
        );
        let plan = resume_plan();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![guide.clone(), actual_resume.clone()],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![actual_resume.file_id]);
        let best = &resolution.candidates[0];
        assert!(best.signals.iter().any(|s| s == "document_type"));
        assert!(!best.signals.iter().any(|s| s == "document_type_fallback"));
    }

    #[test]
    fn find_content_query_locks_year_session_exam_paper() {
        // FIND「2019年数据库下午的真题文件是哪个」：正确文件
        // 「2019年上半年数据库系统工程师考试下午真题」靠 content_query 描述串的
        // 子序列+二元组命中锁定；2023 备考集锦只剩 document_type 底分，绝不抢位。
        let correct = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2019年上半年数据库系统工程师考试下午真题（参考答案）",
        );
        let wrong_a = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2023年数据库系统工程师备考知识点集锦",
        );
        let wrong_b = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2023年数据库系统工程师易混淆知识点",
        );
        let mut plan = resume_plan();
        plan.intent = QueryIntent::DocumentFind;
        plan.target.document_type = Some(DocumentType::LearningMaterial);
        plan.content_query = Some("2019年数据库下午的真题文件".to_owned());
        plan.filters.time = Some("2019".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&correct, &wrong_a, &wrong_b] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(&plan, &session, vec![correct, wrong_a, wrong_b], file_names);
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids.len(), 1);
        let best = &resolution.candidates[0];
        assert!(best.signals.iter().any(|s| s == "find_content"));
    }

    #[test]
    fn find_content_trailing_question_residue_still_locks_unique_file() {
        // 常见功能词干扰：FIND 描述末尾带裸疑问残词（「真题文件是哪个」里的
        // 「哪个」、「真题是哪份」里的「哪份」）。组合停止词只收录了「哪个文件」
        // 没收录裸「哪个/哪些/哪份/哪几」，残词会让子序列匹配在「真题」之后卡在
        // 「哪」上失配、退化成弱一档的二元组覆盖率兜底。停止词表补齐裸疑问词后
        // 描述收敛为「2019年数据库下午真题」，直接命中子序列、唯一锁定正确文件；
        // 同时正确文件与「同年下午 vs 其它年」的竞争文件保持足够差距，不退化。
        let correct = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2019年上半年数据库系统工程师考试下午真题（参考答案）",
        );
        let morning = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2019年上半年数据库系统工程师考试上午真题（参考答案）",
        );
        let other_year = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2020年数据库系统工程师考试下午真题（参考答案）",
        );
        let knowledge = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2023年数据库系统工程师备考知识点集锦",
        );
        let mut plan = resume_plan();
        plan.intent = QueryIntent::DocumentFind;
        plan.target.document_type = None;
        plan.content_query = Some("2019年数据库下午的真题文件是哪个".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&correct, &morning, &other_year, &knowledge] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![correct.clone(), morning, other_year, knowledge],
            file_names,
        );
        let correct_score = score_candidate(&input, &input.profiles[0]);
        assert!(
            correct_score.signals.iter().any(|s| s == "find_content"),
            "清洗掉尾部疑问残词后必须命中完整 find_content: {:?}",
            correct_score.signals
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![correct.file_id]);
    }

    #[test]
    fn find_resolves_from_content_query_when_parser_drops_target() {
        // 真实 parser 输出：FIND 问句（r14「2019年数据库下午的真题文件是哪个」）
        // 模型把文件描述放进 content_query、却把 reference/document_type/name 全
        // 留空。target_is_empty 的 FIND 例外必须让 content_query 参与定位——
        // find_content 信号（7.5）本就按「FIND 的 content_query 就是文件描述」
        // 设计，早退会把它短路成「目标对象为空」。此测试锁定 r14 正确文件。
        let correct = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2019年上半年数据库系统工程师考试下午真题（参考答案）",
        );
        let morning = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2019年上半年数据库系统工程师考试上午真题（参考答案）",
        );
        let other_year = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2020年数据库系统工程师考试下午真题（参考答案）",
        );
        let mut plan = resume_plan();
        plan.intent = QueryIntent::DocumentFind;
        plan.target.reference = None;
        plan.target.document_type = None;
        plan.target.document_name = None;
        plan.target.entity_name = None;
        plan.target.owner = None;
        plan.content_query = Some("2019年数据库下午的真题文件".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&correct, &morning, &other_year] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![correct.clone(), morning, other_year],
            file_names,
        );
        assert!(
            !target_is_empty(&plan),
            "FIND 的 content_query 必须构成定位依据，不得判空"
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![correct.file_id]);
        assert!(
            resolution.candidates[0]
                .signals
                .iter()
                .any(|s| s == "find_content"),
            "正解必须靠 find_content 锁定: {:?}",
            resolution.candidates[0].signals
        );
    }

    #[test]
    fn find_content_query_ignores_word_order_but_high_coverage_gets_partial() {
        // 描述串与文件名词序不同且**年份冲突**（描述「数据库系统工程师考试2020年
        // 上午真题」vs 文件「2021年数据库系统工程师考试上午真题」）：子序列失败、
        // 二元组重合度高但年份不一致 → 部分分（find_content_partial）。部分分是
        // 「补召回」不是「锁定」：分数仅 ~0.23（+owner 底分 0.05）远低于
        // MEDIUM_CONFIDENCE_THRESHOLD，resolver 因此不错误锁定（Unresolved 退回
        // 宽 scope 由检索层收敛），只保证真实存在的文件不会因词序差异在打分里被
        // 完全丢掉——直接断言 score_candidate 的打分：年份冲突文件带
        // find_content_partial 且分数明显高于共享词元少的无关文件。
        //
        // 注意：二元组重合是集合式度量（对词序不敏感），「2020年」与「2021年
        // 上半年」两份文件共享全部内容二元组，覆盖率相同——部分分不承诺区分
        // 年份，只承诺「正确内容词序翻转不丢失」。年份的精确区分由命中子序列的
        // find_content 主路径（结合 filters.time）负责。
        let correct = profile(
            Uuid::now_v7(),
            None,
            "2021年数据库系统工程师考试上午真题（参考答案）",
        );
        let unrelated = profile(
            Uuid::now_v7(),
            None,
            "教师资格考试综合素质真题",
        );
        let mut plan = resume_plan();
        plan.intent = QueryIntent::DocumentFind;
        plan.target.document_type = None;
        plan.content_query = Some("数据库系统工程师考试2020年上午真题".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(correct.file_id, correct.title.clone());
        file_names.insert(unrelated.file_id, unrelated.title.clone());
        let input = ResolverInput::new(&plan, &session, vec![correct, unrelated], file_names);

        let correct_score = score_candidate(&input, &input.profiles[0]);
        let unrelated_score = score_candidate(&input, &input.profiles[1]);
        assert!(
            correct_score
                .signals
                .iter()
                .any(|s| s == "find_content_partial"),
            "词序翻转但高重合的描述必须触发部分分: {:?}",
            correct_score.signals
        );
        assert!(
            !unrelated_score
                .signals
                .iter()
                .any(|s| s == "find_content_partial"),
            "共享词元过少的无关文件不得触发部分分: {:?}",
            unrelated_score.signals
        );
        assert!(
            correct_score.score > unrelated_score.score,
            "正确文件({:.3})应领先无关文件({:.3})",
            correct_score.score,
            unrelated_score.score
        );

        // 低置信度不锁定（部分分仅补召回），退回宽 scope。
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
    }

    #[test]
    fn find_content_session_conflict_does_not_lock_wrong_session() {
        // r10 真实口径：FIND「我电脑里有数据库系统工程师2020年上午的真题吗」，
        // content_query=「数据库系统工程师2020年上午的真题」。上午真题与下午真题
        // 二元组重合度几乎打平（只差上/下字）、年份相同——会话门必须把下午卷
        // 降级为 find_content_partial（补召回不锁定），让上午卷成为唯一锁定目标；
        // 2023 备考集锦年份冲突、内容词元少，分数更低不构成竞争。
        let morning = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2020年数据库系统工程师考试上午真题（参考答案）",
        );
        let afternoon = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2020年数据库系统工程师考试下午真题（参考答案）",
        );
        let knowledge = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2023年数据库系统工程师备考知识点集锦",
        );
        let mut plan = resume_plan();
        plan.intent = QueryIntent::DocumentFind;
        plan.target.document_type = None;
        plan.content_query = Some("数据库系统工程师2020年上午的真题".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in [&morning, &afternoon, &knowledge] {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![morning.clone(), afternoon.clone(), knowledge.clone()],
            file_names,
        );
        let morning_score = score_candidate(&input, &input.profiles[0]);
        let afternoon_score = score_candidate(&input, &input.profiles[1]);
        let knowledge_score = score_candidate(&input, &input.profiles[2]);
        assert!(
            morning_score.signals.iter().any(|s| s == "find_content"),
            "上午卷应命中完整 find_content: {:?}",
            morning_score.signals
        );
        assert!(
            afternoon_score.signals.iter().any(|s| s == "find_content_partial"),
            "下午卷因会话冲突只能部分分: {:?}",
            afternoon_score.signals
        );
        assert!(
            !afternoon_score
                .signals
                .iter()
                .any(|s| s == "find_content"),
            "下午卷绝不得命中完整 find_content: {:?}",
            afternoon_score.signals
        );
        assert!(
            morning_score.score - afternoon_score.score >= HIGH_MARGIN,
            "上午卷({:.3})与下午卷({:.3})差距应足以锁定",
            morning_score.score,
            afternoon_score.score
        );
        assert!(
            afternoon_score.score > knowledge_score.score,
            "下午卷({:.3})仍应领先知识集锦({:.3})（补召回）",
            afternoon_score.score,
            knowledge_score.score
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![morning.file_id]);
        assert!(resolution.candidates[0].signals.iter().any(|s| s == "find_content"));
    }

    #[test]
    fn find_content_query_not_applied_to_document_qa() {
        // 非 FIND intent：content_query 是正文问题，绝不参与目标定位——
        // 「简历里有没有身份证号」的 content_query=身份证号 不影响锁定简历。
        let resume = profile(
            Uuid::now_v7(),
            Some(DocumentType::Resume),
            "大模型开发工程师-周晨",
        );
        let unrelated = profile(
            Uuid::now_v7(),
            Some(DocumentType::LearningMaterial),
            "2019年上半年数据库系统工程师考试下午真题（参考答案）",
        );
        let mut plan = resume_plan();
        plan.intent = QueryIntent::DocumentQa;
        plan.content_query = Some("身份证号".to_owned());
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(resume.file_id, "大模型开发工程师-周晨.pdf".to_owned());
        file_names.insert(
            unrelated.file_id,
            "2019年上半年数据库系统工程师考试下午真题（参考答案）.pdf".to_owned(),
        );
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![resume, unrelated],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids.len(), 1);
        let best = &resolution.candidates[0];
        assert!(
            !best.signals.iter().any(|s| s == "find_content"),
            "DocumentQa 不得触发 find_content: {:?}",
            best.signals
        );
    }

    #[test]
    fn weights_are_configured_together() {
        // 可配置信号权重的健康检查：所有信号都在表里且为正权重
        let total: f32 = SIGNAL_WEIGHTS.iter().map(|(_, weight)| *weight).sum();
        assert!(
            total > HIGH_CONFIDENCE_THRESHOLD,
            "权重总和应超过高置信度阈值"
        );
        assert!(SIGNAL_WEIGHTS.iter().all(|(_, weight)| *weight > 0.0));
    }

    #[test]
    fn precise_named_target_locks_scope_to_exact_title_and_not_valid_similar_docs() {
        // 模型驱动精确定位：用户精确点名《专业实习》报告（3 份同名副本），
        // scope 必须只收敛到该内容族，绝不混入相同作者/相近类型的其它文档
        //（生产实习考核表、实训日志、毕业体验、毕业设计论文）。
        let report_title = "周晨博20212P2002《专业实习》课程实习总结报告.docx";
        let copies = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        let similar = [
            profile(
                Uuid::now_v7(),
                Some(DocumentType::Certificate),
                "周晨博生产实习考核表",
            ),
            profile(Uuid::now_v7(), Some(DocumentType::Report), "周晨博毕业体验"),
            profile(
                Uuid::now_v7(),
                Some(DocumentType::Paper),
                "毕业设计（论文）成绩评定表",
            ),
        ];
        let copies_profiles = copies
            .iter()
            .map(|id| profile(*id, Some(DocumentType::Report), report_title))
            .collect::<Vec<_>>();
        let mut all = copies_profiles.clone();
        all.extend(similar.clone());

        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = None;
        plan.target.document_name = Some(report_title.to_owned());
        plan.target.precise_named_document = true;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        for p in &all {
            file_names.insert(p.file_id, p.title.clone());
        }
        let input = ResolverInput::new(&plan, &session, all, file_names);
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::MultipleCandidates);
        for id in &copies {
            assert!(
                resolution.resolved_file_ids.contains(id),
                "同名副本必须进 scope"
            );
        }
        assert_eq!(
            resolution.resolved_file_ids.len(),
            3,
            "仅 3 份同名副本进 scope"
        );
        assert!(
            resolution
                .resolved_file_ids
                .iter()
                .all(|id| !similar.iter().any(|p| p.file_id == *id)),
            "相近但不同名的文档绝不进精确 scope"
        );
    }

    #[test]
    fn precise_named_target_single_match_locks_one_file() {
        // 模型驱动精确定位：精确点名单份《计算机网络》成绩评定表 → 锁定唯一文件。
        let target_title = "《计算机网络》成绩评定表.docx";
        let target = profile(Uuid::now_v7(), None, target_title);
        let other = profile(Uuid::now_v7(), None, "成绩评定表");
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = None;
        plan.target.document_name = Some(target_title.to_owned());
        plan.target.precise_named_document = true;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(target.file_id, target_title.to_owned());
        file_names.insert(other.file_id, "成绩评定表.pdf".to_owned());
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![target.clone(), other.clone()],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![target.file_id]);
    }

    #[test]
    fn precise_named_target_not_in_library_returns_unresolved_not_wide_scope() {
        // 模型判定的精确点名文档不在库内 → Unresolved，绝不回退到相近文档宽 scope。
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = None;
        plan.target.document_name = Some("不存在的课程报告.docx".to_owned());
        plan.target.precise_named_document = true;
        let session = AskSessionContext::default();
        let unrelated = profile(Uuid::now_v7(), None, "七月发票");
        let input = ResolverInput::new(&plan, &session, vec![unrelated], HashMap::new());
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
        assert!(resolution.resolved_file_ids.is_empty());
    }

    #[test]
    fn precise_named_target_matches_versioned_file_prefix() {
        // 模型精确点名简名，真实文件带版本前缀/后缀：内容族子串层应命中共进 scope。
        let target_name = "人工智能面试宝典";
        let real = Uuid::now_v7();
        let similar = Uuid::now_v7();
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = None;
        plan.target.document_name = Some(target_name.to_owned());
        plan.target.precise_named_document = true;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(real, "1人工智能面试宝典_V6.6(20250606).pdf".to_owned());
        file_names.insert(similar, "人工智能求职笔记.pdf".to_owned());
        let docs = vec![
            profile(real, None, "1人工智能面试宝典_V6.6(20250606).pdf"),
            profile(similar, None, "人工智能求职笔记.pdf"),
        ];
        let input = ResolverInput::new(&plan, &session, docs, file_names);
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![real]);
        assert!(!resolution.resolved_file_ids.contains(&similar));
    }

    #[test]
    fn precise_named_target_missing_name_falls_through_to_signals() {
        // 精确点名落空但库内有同类型强信号文档：回退到通用评分，避免把真实存在
        // 的文档误报为「不在库内」（年份/措辞与生成标题存在词序差异时）。
        let mut plan = resume_plan();
        plan.target.document_type = Some(DocumentType::LearningMaterial);
        plan.target.reference = None;
        plan.target.document_name = Some("数据库系统工程师考试2020年上午真题".to_owned());
        plan.target.precise_named_document = true;
        let session = AskSessionContext::default();
        let real = Uuid::now_v7();
        let docs = vec![profile(
            real,
            Some(DocumentType::LearningMaterial),
            "2020年数据库系统工程师考试上午真题（参考答案）.pdf",
        )];
        let mut file_names = HashMap::new();
        file_names.insert(real, "2020年数据库系统工程师考试上午真题（参考答案）.pdf".to_owned());
        let input = ResolverInput::new(&plan, &session, docs, file_names);
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![real]);
    }

    #[test]
    fn precise_named_reordered_title_wins_over_type_matched_similar_docs() {
        // r15 真实口径：parser 点名的标题与真实文件名词序不同（年份位置），
        // 精确匹配第三层（二元组重合+年份/上下午门）必须把 2020 上午真题锁为
        // 唯一目标；类型信号命中的 2023 知识点与同年下午卷都不得进精确 scope。
        let target_name = "数据库系统工程师考试2020年上午真题";
        let real = Uuid::now_v7();
        let wrong_year = Uuid::now_v7();
        let wrong_session = Uuid::now_v7();
        let similar_type = Uuid::now_v7();
        let mut plan = resume_plan();
        plan.target.document_type = Some(DocumentType::LearningMaterial);
        plan.target.reference = None;
        plan.target.document_name = Some(target_name.to_owned());
        plan.target.precise_named_document = true;
        let session = AskSessionContext::default();
        // 真实库里 2020 上午真题被分类器标成 spreadsheet（类型信号完全不命中）。
        let docs = vec![
            profile(real, Some(DocumentType::Spreadsheet), "2020年数据库系统工程师考试上午真题（参考答案）.pdf"),
            profile(wrong_year, Some(DocumentType::Spreadsheet), "2019年上半年数据库系统工程师考试上午真题（参考答案）.pdf"),
            profile(wrong_session, Some(DocumentType::Spreadsheet), "2020年数据库系统工程师考试下午真题（参考答案）.pdf"),
            profile(similar_type, Some(DocumentType::LearningMaterial), "2023年数据库系统工程师备考知识点集锦.pdf"),
        ];
        let mut file_names = HashMap::new();
        file_names.insert(real, "2020年数据库系统工程师考试上午真题（参考答案）.pdf".to_owned());
        file_names.insert(wrong_year, "2019年上半年数据库系统工程师考试上午真题（参考答案）.pdf".to_owned());
        file_names.insert(wrong_session, "2020年数据库系统工程师考试下午真题（参考答案）.pdf".to_owned());
        file_names.insert(similar_type, "2023年数据库系统工程师备考知识点集锦.pdf".to_owned());
        let input = ResolverInput::new(&plan, &session, docs, file_names);
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![real]);
        // 精确路径命中 → 第一候选带 precise 信号，且绝不含 2019/下午/知识点。
        assert!(
            resolution.candidates[0]
                .signals
                .iter()
                .any(|signal| signal == "precise_named_document")
        );
    }

    #[test]
    fn reference_desc_subsequence_locks_reordered_target_when_precise_false() {
        // r15 的 parser 不稳定口径：目标描述被放进 reference（precise=false），
        // 且与真实文件名词序不同。通用打分的 reference_match 信号（子序列+
        // 二元组重合+年份/上下午门）必须把 2020 上午真题锁为唯一目标。
        let target_reference = "数据库系统工程师考试2020年上午真题";
        let real = Uuid::now_v7();
        let wrong_year = Uuid::now_v7();
        let wrong_session = Uuid::now_v7();
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.document_name = None;
        plan.target.reference = Some(target_reference.to_owned());
        plan.target.precise_named_document = false;
        plan.target.owner = Some("self".to_owned());
        let session = AskSessionContext::default();
        let docs = vec![
            profile(real, None, "2020年数据库系统工程师考试上午真题（参考答案）.pdf"),
            profile(wrong_year, None, "2019年上半年数据库系统工程师考试上午真题（参考答案）.pdf"),
            profile(wrong_session, None, "2020年数据库系统工程师考试下午真题（参考答案）.pdf"),
        ];
        let mut file_names = HashMap::new();
        file_names.insert(real, "2020年数据库系统工程师考试上午真题（参考答案）.pdf".to_owned());
        file_names.insert(wrong_year, "2019年上半年数据库系统工程师考试上午真题（参考答案）.pdf".to_owned());
        file_names.insert(wrong_session, "2020年数据库系统工程师考试下午真题（参考答案）.pdf".to_owned());
        let input = ResolverInput::new(&plan, &session, docs, file_names);
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![real]);
        let best = &resolution.candidates[0];
        assert!(
            best.signals.iter().any(|signal| signal == "reference_match"),
            "应命中 reference_match 信号: {:?}",
            best.signals
        );
        // 错误年份/会话的文件只拿到部分分或 0，不构成锁定竞争。
        assert!(
            resolution
                .candidates
                .iter()
                .any(|candidate| candidate.file_id == real)
        );
    }

    #[test]
    fn concept_reference_does_not_participate_in_locating() {
        // 概念名/内容词放进 reference（如 parser 把「事务的ACID特性」当目标）
        // 时，不得对任何文件名产生定位信号（文件名不含其字符，子序列/二元组
        // 重合均为 0，只可能拿到 owner 底分、进不了候选）。
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.document_name = None;
        plan.target.reference = Some("事务的ACID特性".to_owned());
        plan.target.precise_named_document = false;
        let session = AskSessionContext::default();
        let unrelated = profile(Uuid::now_v7(), None, "2020年数据库系统工程师考试上午真题（参考答案）.pdf");
        let mut file_names = HashMap::new();
        file_names.insert(unrelated.file_id, "2020年数据库系统工程师考试上午真题（参考答案）.pdf".to_owned());
        let input = ResolverInput::new(&plan, &session, vec![unrelated], file_names);
        let resolution = resolve_documents(&input);
        // 没有任何可定位信号 → Unresolved（退回宽 scope 或诚实拒答，不误锁）。
        assert_eq!(resolution.status, ResolutionStatus::Unresolved);
        assert!(resolution.resolved_file_ids.is_empty());
    }

    #[test]
    fn precise_named_target_folds_copy_suffix_into_content_family() {
        // 模型驱动精确定位：用户点名主名（无副本序号）时，`_1` 副本序号是被
        // 点名的同一内容族，应一并进 scope；但绝不会把相近但不同名的文档混入。
        let main_title = "周晨博20212P2002《专业实习》课程实习总结报告.docx";
        let main = profile(Uuid::now_v7(), None, main_title);
        let copy = profile(
            Uuid::now_v7(),
            None,
            "周晨博20212P2002《专业实习》课程实习总结报告_1.docx",
        );
        let similar = profile(Uuid::now_v7(), None, "周晨博生产实习考核表.pdf");
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = None;
        plan.target.document_name = Some(main_title.to_owned());
        plan.target.precise_named_document = true;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(main.file_id, main_title.to_owned());
        file_names.insert(
            copy.file_id,
            "周晨博20212P2002《专业实习》课程实习总结报告_1.docx".to_owned(),
        );
        file_names.insert(similar.file_id, "周晨博生产实习考核表.pdf".to_owned());
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![main.clone(), copy.clone(), similar.clone()],
            file_names,
        );
        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::MultipleCandidates);
        let mut ids = resolution.resolved_file_ids.clone();
        ids.sort();
        let mut expected = vec![main.file_id, copy.file_id];
        expected.sort();
        assert_eq!(ids, expected, "主名 + `_1` 副本同进 scope");
        assert!(
            !resolution.resolved_file_ids.contains(&similar.file_id),
            "相近但不同名的文档绝不进精确 scope"
        );
    }

    #[test]
    fn partial_target_uses_profile_header_to_reject_identity_conflicts() {
        let correct_id = Uuid::now_v7();
        let wrong_session_id = Uuid::now_v7();
        let wrong_year_id = Uuid::now_v7();
        let mut correct = profile(correct_id, None, "2031年上.pdf");
        correct.summary = "2031 年云平台认证考试试题 上午卷 考生须知".to_owned();
        correct.keywords = vec!["2031".to_owned()];
        let mut wrong_session = profile(wrong_session_id, None, "2031年下午卷.pdf");
        wrong_session.summary = "2031 年云平台认证考试试题 下午卷 考生须知".to_owned();
        wrong_session.keywords = vec!["2031".to_owned()];
        let mut wrong_year = profile(wrong_year_id, None, "2030年上午卷.pdf");
        wrong_year.summary = "2030 年云平台认证考试试题 上午卷 考生须知".to_owned();
        wrong_year.keywords = vec!["2030".to_owned()];

        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = Some("2031年的云平台认证上午试卷".to_owned());
        plan.target.document_name = Some("上午试卷".to_owned());
        plan.target.precise_named_document = false;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(correct_id, "2031年上.pdf".to_owned());
        file_names.insert(wrong_session_id, "2031年下午卷.pdf".to_owned());
        file_names.insert(wrong_year_id, "2030年上午卷.pdf".to_owned());
        let input = ResolverInput::new(
            &plan,
            &session,
            vec![correct, wrong_session, wrong_year],
            file_names,
        );

        let resolution = resolve_documents(&input);
        assert_eq!(resolution.status, ResolutionStatus::Resolved);
        assert_eq!(resolution.resolved_file_ids, vec![correct_id]);
        assert!(
            resolution
                .candidates
                .iter()
                .all(|candidate| candidate.file_id != wrong_session_id
                    && candidate.file_id != wrong_year_id),
            "年份或卷次冲突的候选不得进入目标 scope"
        );
    }

    #[test]
    fn ambiguous_profile_header_does_not_create_a_false_identity_conflict() {
        let candidate_id = Uuid::now_v7();
        let mut candidate = profile(candidate_id, None, "认证资料汇编.pdf");
        candidate.summary = "2031 年认证资料，包含上午卷与下午卷的统一说明".to_owned();
        let mut plan = resume_plan();
        plan.target.document_type = None;
        plan.target.reference = Some("2031年的云平台认证上午试卷".to_owned());
        plan.target.document_name = Some("上午试卷".to_owned());
        plan.target.precise_named_document = false;
        let session = AskSessionContext::default();
        let mut file_names = HashMap::new();
        file_names.insert(candidate_id, "认证资料汇编.pdf".to_owned());
        let input = ResolverInput::new(&plan, &session, vec![candidate.clone()], file_names);

        assert!(
            !target_identity_conflicts(&input, &candidate),
            "摘要同时出现两个卷次时必须保持未知，不能硬拒绝候选"
        );
    }
}
