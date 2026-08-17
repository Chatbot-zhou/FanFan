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

use crate::ask::query_normalize::meaningful_tokens;
use crate::ask::query_plan::{DocumentCandidate, DocumentResolution, QueryPlan, ResolutionStatus};
use crate::contracts::DocumentType;
use crate::knowledge::DocumentProfile;
use crate::profile_builder::type_keywords_for;
use crate::AskSessionContext;

/// 综合打分时使用的信号权重（可配置：改这里即可调参，编排层不感知细节）。
/// 每个信号命中 +weight（0..=1 分数直接相加，分数上限≈1.05）。
pub const SIGNAL_WEIGHTS: &[(&str, f32)] = &[
    ("document_type", 0.35),
    ("session_active", 0.30),
    ("document_title", 0.25),
    ("session_referenced", 0.20),
    ("entity_match", 0.20),
    ("keyword_match", 0.15),
    ("filename", 0.10),
    ("owner_match", 0.05),
];

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
const GRADUATION_EXPANSION_TERMS: &[&str] =
    &["毕业", "论文", "答辩", "开题", "学位", "毕业设计"];

/// Document Resolver 的输入：QueryPlan + 会话上下文 + 候选画像 + 文件名。
#[derive(Debug, Clone)]
pub struct ResolverInput<'a> {
    pub plan: &'a QueryPlan,
    pub session: &'a AskSessionContext,
    pub profiles: Vec<DocumentProfile>,
    /// file_id → 文件名（弱信号，只做最后兜底）
    pub file_names: HashMap<Uuid, String>,
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
        }
    }
}

/// 目标对象是否完全为空（没有任何可定位依据）。
fn target_is_empty(plan: &QueryPlan) -> bool {
    plan.target.reference.as_deref().unwrap_or("").trim().is_empty()
        && plan.target.document_name.as_deref().unwrap_or("").trim().is_empty()
        && plan.target.document_type.is_none()
        && plan.target.entity_name.as_deref().unwrap_or("").trim().is_empty()
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

/// 对单个画像综合打分，返回候选与命中信号列表。
fn score_candidate(input: &ResolverInput<'_>, profile: &DocumentProfile) -> DocumentCandidate {
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
            profile.title.contains(keyword)
                || filename.is_some_and(|name| name.contains(keyword))
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
    let title_tokens = [input.plan.target.document_name.as_deref(), input.plan.target.reference.as_deref()]
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
        && (title_tokens.iter().any(|token| {
            profile.title.contains(token) || token.contains(profile.title.trim())
        }) || target_tokens.iter().any(|token| profile.title.contains(token)))
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
    if input.session.last_referenced_file_ids.contains(&profile.file_id) {
        score += weight("session_referenced");
        signals.push("session_referenced".to_owned());
    }

    // 5. 实体匹配（「周晨」「LangGraph 项目」等实体命中画像 entities）
    if let Some(entity_name) = input.plan.target.entity_name.as_deref() {
        let entity = entity_name.trim();
        if !entity.is_empty()
            && profile.entities.iter().any(|candidate| {
                candidate.contains(entity) || entity.contains(candidate.as_str())
            })
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

    // 8. 归属者：P0 没有归属者元数据；「我的」= 用户自己的文件库，
    //    给所有候选一个微弱信号，保持 owner_match 信号可见。
    if input.plan.target.owner.as_deref() == Some("self") {
        score += weight("owner_match");
        signals.push("owner_match".to_owned());
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
        let guide = profile(Uuid::now_v7(), Some(DocumentType::LearningMaterial), "如何写好简历");
        let actual_resume = profile(Uuid::now_v7(), Some(DocumentType::Resume), "大模型开发工程师-周晨");
        let mut file_names = HashMap::new();
        file_names.insert(guide.file_id, "如何写好简历.pdf".to_owned());
        file_names.insert(actual_resume.file_id, "大模型开发工程师-周晨.pdf".to_owned());

        let plan = resume_plan();
        let session = AskSessionContext::default();
        let input = ResolverInput::new(&plan, &session, vec![guide.clone(), actual_resume.clone()], file_names);
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

        let input = ResolverInput::new(&plan, &session, vec![other.clone(), active.clone()], HashMap::new());
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
        let input = ResolverInput::new(&plan, &session, vec![resume_a.clone(), resume_b.clone()], HashMap::new());
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
        let resume = profile(Uuid::now_v7(), Some(DocumentType::Resume), "大模型开发工程师-周晨");
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
        let mut project =
            profile(Uuid::now_v7(), Some(DocumentType::Other), "LangGraph 多智能体项目");
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
        let only = profile(Uuid::now_v7(), Some(DocumentType::Resume), "大模型开发工程师-周晨");
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
        let profile_md = profile(Uuid::now_v7(), None, "周晨博-大模型开发技术点与项目逐字稿-优化版.md");
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
        assert_eq!(resolution.resolved_file_ids.len(), 2, "两个大模型材料候选进 scope");
        // 无关的乡村振兴文档绝不进候选
        assert!(resolution
            .resolved_file_ids
            .iter()
            .all(|id| !id.to_string().contains("unrelated")));
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
        assert!(resolution
            .resolved_file_ids
            .iter()
            .all(|id| !id.to_string().contains("unrelated")));
    }

    #[test]
    fn graduation_expansion_matches_files_without_graduation_literal() {
        // Phase 4.3 CASE 5（GraduationReferenceResolver）：真实毕业材料常
        // 不含「毕业」两字（开题报告/学位论文/答辩PPT）——「毕业」引用靠
        // 扩展词元命中标题；多候选 → clarification，绝不直接拒绝。
        let proposal = profile(Uuid::now_v7(), Some(DocumentType::Paper), "开题报告书");
        let thesis = profile(Uuid::now_v7(), Some(DocumentType::Paper), "工学学位论文终稿");
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
        assert!(resolution
            .resolved_file_ids
            .iter()
            .all(|id| *id != unrelated_id));
    }

    #[test]
    fn graduation_expansion_does_not_hit_design_documents() {
        // 「设计」单独过宽：设计院合同/课程设计不能被「毕业」引用误命中
        //（扩展表只有「毕业设计」组合，无裸「设计」）
        let design_contract = profile(Uuid::now_v7(), Some(DocumentType::Contract), "设计院战略合作合同");
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
        let guide = profile(Uuid::now_v7(), Some(DocumentType::LearningMaterial), "如何写好简历");
        let actual_resume = profile(Uuid::now_v7(), Some(DocumentType::Resume), "大模型开发工程师-周晨");
        let mut file_names = HashMap::new();
        file_names.insert(guide.file_id, "如何写好简历.pdf".to_owned());
        file_names.insert(actual_resume.file_id, "大模型开发工程师-周晨.pdf".to_owned());
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
    fn weights_are_configured_together() {
        // 可配置信号权重的健康检查：所有信号都在表里且为正权重
        let total: f32 = SIGNAL_WEIGHTS.iter().map(|(_, weight)| *weight).sum();
        assert!(total > HIGH_CONFIDENCE_THRESHOLD, "权重总和应超过高置信度阈值");
        assert!(SIGNAL_WEIGHTS.iter().all(|(_, weight)| *weight > 0.0));
    }
}
