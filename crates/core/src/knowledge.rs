use std::collections::HashSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppError, EvidenceRef, ScopeFilter, SearchSession};

/// 查询意图：从自然语言中提取的结构化检索参数
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryIntent {
    #[serde(default)]
    pub rewritten_query: String,
    pub time_hint: Option<TimeHint>,
    #[serde(default)]
    pub extension_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimeHint {
    pub from: String,
    pub to: String,
}

/// 构建查询改写 prompt。只输出 JSON，不输出其他内容。
pub fn query_understanding_prompt(query: &str, today: &str) -> (String, String) {
    let system =
        "你是查询理解助手。只输出JSON，不要输出其他任何文字、解释或 markdown 标记。".into();
    let user = format!(
        r#"将自然语言问题转换为结构化检索参数。只输出JSON：
{{"rewritten_query":"核心关键词","time_hint":null或{{"from":"YYYY-MM-DD","to":"YYYY-MM-DD"}},"extension_hints":["docx","pdf"]}}
- rewritten_query: 去除口语词（"那个""在哪里""帮我找"），保留核心检索关键词
- time_hint: 解析时间指代（"去年""上个月""2025年"），以当前日期 {today} 为基准，仅当明确时才设置
- extension_hints: 用户提到的文件格式（"word文档"→docx,"excel表格"→xlsx,"ppt"→pptx,"pdf"→pdf）
若查询已经是关键词则照写 rewritten_query，未提及的字段用 null 或空数组。

查询：{query}"#,
        today = today,
        query = query
    );
    (system, user)
}

/// 解析生成模型返回的 JSON 为 QueryIntent，失败时用原始查询回退
pub fn parse_query_intent(raw: &str, fallback_query: &str) -> QueryIntent {
    let trimmed = raw.trim();
    // 去除可能的 markdown 代码块包裹
    let json = trimmed
        .strip_prefix("```json")
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .or_else(|| {
            trimmed
                .strip_prefix("```")
                .and_then(|s| s.strip_suffix("```"))
                .map(str::trim)
        })
        .unwrap_or(trimmed);
    let mut intent: QueryIntent = serde_json::from_str(json).unwrap_or_else(|_| QueryIntent {
        rewritten_query: String::new(),
        time_hint: None,
        extension_hints: Vec::new(),
    });
    if intent.rewritten_query.trim().is_empty() {
        intent.rewritten_query = fallback_query.to_owned();
    }
    // 校验时间格式基本合法
    if let Some(ref hint) = intent.time_hint
        && (hint.from.len() != 10 || hint.to.len() != 10)
    {
        intent.time_hint = None;
    }
    intent
}

/// 判断查询是否需要自然语言理解
pub fn is_natural_language_query(query: &str) -> bool {
    let nl_patterns = [
        Regex::new(r"去[年天]|上个?[月周]|前[几些]|最[近新]|这[个些]|那[个些]").unwrap(),
        Regex::new(r"在哪里|找一?[下个]|帮我|有没有|什么是|如何|怎么|什么时[候间]").unwrap(),
        Regex::new(r"[，。！？,\.!\?]").unwrap(),
    ];
    let trimmed = query.trim();
    // 纯关键词通常短且无标点
    if trimmed.chars().count() <= 4 && !nl_patterns[2].is_match(trimmed) {
        return false;
    }
    nl_patterns.iter().any(|p| p.is_match(trimmed))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStyle {
    Concise,
    Detailed,
    List,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AskMode {
    #[default]
    Rag,
    EvidenceExtracts,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskRequest {
    pub question: String,
    pub session_id: Option<Uuid>,
    pub scope: ScopeFilter,
    pub answer_style: AnswerStyle,
    pub retrieval_limit: u32,
    pub max_source_files: u32,
    pub strict_evidence: bool,
    #[serde(default)]
    pub mode: AskMode,
    #[serde(default)]
    pub allow_degraded_extractive: bool,
}

impl AskRequest {
    pub fn validate(&self) -> Result<(), AppError> {
        let length = self.question.trim().chars().count();
        if !(2..=2_000).contains(&length) {
            return Err(AppError::new(
                "ASK_QUESTION_INVALID",
                "问题长度需要在2到2000个字符之间",
                false,
            ));
        }
        if !self.strict_evidence {
            return Err(AppError::new(
                "ASK_STRICT_EVIDENCE_REQUIRED",
                "拾忆V1只允许开启严格证据模式的资料问答",
                false,
            ));
        }
        if !(1..=30).contains(&self.retrieval_limit) || !(1..=12).contains(&self.max_source_files) {
            return Err(AppError::new(
                "ASK_LIMIT_INVALID",
                "检索块数量需要在1到30之间，来源文件数量需要在1到12之间",
                false,
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GroundingStatus {
    Grounded,
    Partial,
    Insufficient,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupportStatus {
    Supported,
    Partial,
    Unsupported,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerClaim {
    pub claim_id: Uuid,
    pub text: String,
    pub support_status: SupportStatus,
    pub citations: Vec<EvidenceRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerSourceFile {
    pub file_id: Uuid,
    pub display_name: String,
    #[serde(
        rename(serialize = "display_path"),
        alias = "display_path",
        serialize_with = "crate::serialize_display_path"
    )]
    pub canonical_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AnswerResult {
    pub session_id: Uuid,
    pub message_id: Uuid,
    pub answer: String,
    pub grounding_status: GroundingStatus,
    pub insufficient_evidence: bool,
    pub claims: Vec<AnswerClaim>,
    pub source_files: Vec<AnswerSourceFile>,
    pub used_file_ids: Vec<Uuid>,
    pub elapsed_ms: u64,
    pub answer_mode: String,
    #[serde(default)]
    pub retrieval_channels: Vec<String>,
    #[serde(default)]
    pub index_coverage: f64,
    #[serde(default)]
    pub degradation_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RagReadiness {
    pub ready: bool,
    pub generation_ready: bool,
    pub embedding_ready: bool,
    pub vision_ready: bool,
    pub semantic_index_coverage: f64,
    pub scope_index_coverage: f64,
    pub image_index_coverage: f64,
    pub pending_image_assets: u64,
    #[serde(skip_serializing)]
    pub degradation_level: String,
    pub background_notice: Option<String>,
    pub blockers: Vec<AppError>,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskSession {
    pub session_id: Uuid,
    pub scope: ScopeFilter,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskMessage {
    pub message_id: Uuid,
    pub session_id: Uuid,
    pub role: String,
    pub content: String,
    pub answer: Option<AnswerResult>,
    pub error: Option<AppError>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskSessionSummary {
    pub session_id: Uuid,
    pub title: String,
    pub scope: ScopeFilter,
    pub message_count: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_error: Option<AppError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskSessionPage {
    pub items: Vec<AskSessionSummary>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AskMessagePage {
    pub items: Vec<AskMessage>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

pub fn assemble_extractive_answer(
    request: &AskRequest,
    session: &SearchSession,
    evidence: Vec<(EvidenceRef, AnswerSourceFile)>,
    started_at: Instant,
) -> AnswerResult {
    let session_id = request.session_id.unwrap_or_else(Uuid::now_v7);
    if evidence.is_empty() {
        return AnswerResult {
            session_id,
            message_id: Uuid::now_v7(),
            answer:
                "当前资料中未找到足够依据。你可以换一种说法、扩大检索范围，或等待相关资料完成索引。"
                    .into(),
            grounding_status: GroundingStatus::Insufficient,
            insufficient_evidence: true,
            claims: Vec::new(),
            source_files: Vec::new(),
            used_file_ids: Vec::new(),
            elapsed_ms: started_at.elapsed().as_millis() as u64,
            answer_mode: "extractive".into(),
            retrieval_channels: vec!["filename".into(), "fts".into()],
            index_coverage: 0.0,
            degradation_reason: None,
        };
    }

    let mut seen_files = HashSet::new();
    let mut source_files = Vec::new();
    let mut claims = Vec::new();
    for (evidence, source) in evidence.into_iter().take(request.retrieval_limit as usize) {
        if !seen_files.contains(&source.file_id)
            && seen_files.len() >= request.max_source_files as usize
        {
            continue;
        }
        if seen_files.insert(source.file_id) {
            source_files.push(source);
        }
        claims.push(AnswerClaim {
            claim_id: Uuid::now_v7(),
            text: compact_quote(&evidence.quote, 260),
            support_status: SupportStatus::Supported,
            citations: vec![evidence],
        });
    }

    let lead = match request.answer_style {
        AnswerStyle::Concise => "在你的本地资料中找到这些直接依据：",
        AnswerStyle::Detailed => {
            "我先按相关性整理了本地资料中的直接依据。以下内容均可回到原文核对："
        }
        AnswerStyle::List => "本地资料中的相关依据：",
    };
    let body = claims
        .iter()
        .enumerate()
        .map(|(index, claim)| format!("{}. {}", index + 1, claim.text))
        .collect::<Vec<_>>()
        .join("\n");
    AnswerResult {
        session_id,
        message_id: Uuid::now_v7(),
        answer: format!("{lead}\n{body}"),
        grounding_status: GroundingStatus::Grounded,
        insufficient_evidence: false,
        used_file_ids: source_files.iter().map(|source| source.file_id).collect(),
        source_files,
        claims,
        elapsed_ms: started_at.elapsed().as_millis() as u64 + session.elapsed_ms,
        answer_mode: "extractive".into(),
        retrieval_channels: vec!["filename".into(), "fts".into()],
        index_coverage: 0.0,
        degradation_reason: None,
    }
}

fn compact_quote(text: &str, limit: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= limit {
        return normalized;
    }
    format!("{}…", normalized.chars().take(limit).collect::<String>())
}

pub fn generation_prompt(
    request: &AskRequest,
    extractive: &AnswerResult,
    history: &[AskMessage],
) -> String {
    let sources = extractive
        .claims
        .iter()
        .flat_map(|claim| claim.citations.iter())
        .enumerate()
        .map(|(index, evidence)| format!("[S{}] {}", index + 1, evidence.quote))
        .collect::<Vec<_>>()
        .join("\n");
    let mut prompt = String::new();
    if !history.is_empty() {
        let lines = history
            .iter()
            .map(|message| {
                let who = if message.role == "user" {
                    "用户"
                } else {
                    "拾忆"
                };
                let content = message
                    .content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{who}：{}", content.chars().take(400).collect::<String>())
            })
            .collect::<Vec<_>>()
            .join("\n");
        prompt.push_str(&format!(
            "对话历史（仅作理解前文，不能作为新证据；回答中出现的来源编号只对应当前问题的可用证据）：\n{lines}\n\n"
        ));
    }
    prompt.push_str(&format!(
        "问题：{}\n\n可用证据：\n{}\n\n只根据以上证据回答。每个包含事实的Markdown段落、列表项和表格数据行都必须在行末包含一个或多个来源编号，例如[S1]；不得使用不存在的编号。标题和表格分隔行可以不带引用；证据不足时只回复‘当前资料中未找到足够依据’。\n回答格式要求：使用Markdown组织内容；回答中涉及的关键概念或术语用**加粗**标出（每段不超过5处）；来源编号必须位于该事实行末尾，表格行可在编号后保留末尾竖线。不要输出代码块。",
        request.question.trim(),
        sources
    ));
    prompt
}

/// 严格证据校验中允许出现的非事实行：空行、Markdown 标题、分隔线和表格分隔行。
fn is_decorative_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') || matches!(trimmed, "---" | "***" | "___") {
        return true;
    }
    if trimmed.starts_with('|') && trimmed.ends_with('|') {
        let cells = trimmed
            .trim_matches('|')
            .split('|')
            .map(str::trim)
            .collect::<Vec<_>>();
        return !cells.is_empty()
            && cells.iter().all(|cell| {
                let value = cell.trim_matches(':');
                value.len() >= 3 && value.chars().all(|character| character == '-')
            });
    }
    false
}

pub fn apply_grounded_generation(
    extractive: &AnswerResult,
    generated: &str,
) -> Option<AnswerResult> {
    if extractive.insufficient_evidence || generated.trim().is_empty() {
        return None;
    }
    let evidence = extractive
        .claims
        .iter()
        .flat_map(|claim| claim.citations.iter().cloned())
        .collect::<Vec<_>>();
    let marker = Regex::new(r"\[S(\d+)\]").expect("static citation pattern");
    let mut claims = Vec::new();
    let lines = generated.lines().collect::<Vec<_>>();
    for (line_index, line) in lines.iter().enumerate() {
        let sentence = line.trim();
        if is_decorative_line(sentence) {
            continue;
        }
        if sentence.starts_with('|')
            && sentence.ends_with('|')
            && lines
                .iter()
                .skip(line_index + 1)
                .find(|candidate| !candidate.trim().is_empty())
                .is_some_and(|candidate| is_decorative_line(candidate))
        {
            continue;
        }
        if sentence.starts_with("```") {
            return None;
        }
        let last_marker = marker.find_iter(sentence).last()?;
        let suffix = sentence[last_marker.end()..].trim();
        if !suffix
            .chars()
            .all(|character| matches!(character, '。' | '！' | '？' | '.' | '!' | '?' | '|'))
        {
            return None;
        }
        let mut citations = Vec::new();
        for capture in marker.captures_iter(sentence) {
            let index = capture.get(1)?.as_str().parse::<usize>().ok()?;
            let selected = evidence.get(index.checked_sub(1)?)?.clone();
            if !citations
                .iter()
                .any(|item: &EvidenceRef| item.evidence_id == selected.evidence_id)
            {
                citations.push(selected);
            }
        }
        if citations.is_empty() {
            return None;
        }
        claims.push(AnswerClaim {
            claim_id: Uuid::now_v7(),
            text: sentence.to_owned(),
            support_status: SupportStatus::Supported,
            citations,
        });
    }
    if claims.is_empty() {
        return None;
    }
    let used = claims
        .iter()
        .flat_map(|claim| claim.citations.iter().map(|citation| citation.file_id))
        .collect::<HashSet<_>>();
    let source_files = extractive
        .source_files
        .iter()
        .filter(|source| used.contains(&source.file_id))
        .cloned()
        .collect::<Vec<_>>();
    let mut result = extractive.clone();
    result.answer = generated.trim().to_owned();
    result.claims = claims;
    result.used_file_ids = source_files.iter().map(|source| source.file_id).collect();
    result.source_files = source_files;
    result.answer_mode = "generated".into();
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Availability, SearchChannelState, SearchChannels};

    fn request() -> AskRequest {
        AskRequest {
            question: "项目如何优化召回率？".into(),
            session_id: None,
            scope: ScopeFilter {
                root_ids: vec![],
                collection_ids: vec![],
                file_ids: vec![],
                extensions: vec![],
                modified_from: None,
                modified_to: None,
                availability: Availability::Present,
            },
            answer_style: AnswerStyle::Concise,
            retrieval_limit: 12,
            max_source_files: 8,
            strict_evidence: true,
            mode: AskMode::Rag,
            allow_degraded_extractive: false,
        }
    }

    #[test]
    fn strict_evidence_is_mandatory() {
        let mut value = request();
        value.strict_evidence = false;
        assert_eq!(
            value.validate().unwrap_err().code,
            "ASK_STRICT_EVIDENCE_REQUIRED"
        );
    }

    #[test]
    fn no_evidence_produces_an_explicit_refusal() {
        let result = assemble_extractive_answer(
            &request(),
            &SearchSession {
                search_id: Uuid::now_v7(),
                status: "completed".into(),
                channels: SearchChannels {
                    filename: SearchChannelState::Completed,
                    fulltext: SearchChannelState::Completed,
                    semantic: SearchChannelState::Unavailable,
                },
                results: vec![],
                next_cursor: None,
                elapsed_ms: 1,
            },
            vec![],
            Instant::now(),
        );
        assert!(result.insufficient_evidence);
        assert_eq!(result.grounding_status, GroundingStatus::Insufficient);
        assert!(result.answer.contains("未找到足够依据"));
    }

    #[test]
    fn generated_answer_requires_a_valid_marker_on_every_sentence() {
        let mut base = assemble_extractive_answer(
            &request(),
            &SearchSession {
                search_id: Uuid::now_v7(),
                status: "completed".into(),
                channels: SearchChannels {
                    filename: SearchChannelState::Completed,
                    fulltext: SearchChannelState::Completed,
                    semantic: SearchChannelState::Unavailable,
                },
                results: vec![],
                next_cursor: None,
                elapsed_ms: 1,
            },
            vec![],
            Instant::now(),
        );
        base.insufficient_evidence = false;
        base.grounding_status = GroundingStatus::Grounded;
        base.claims = vec![AnswerClaim {
            claim_id: Uuid::now_v7(),
            text: "依据".into(),
            support_status: SupportStatus::Supported,
            citations: vec![EvidenceRef {
                evidence_id: Uuid::now_v7(),
                file_id: Uuid::now_v7(),
                revision_id: Uuid::now_v7(),
                node_id: Uuid::now_v7(),
                chunk_id: Uuid::now_v7(),
                image_asset_id: None,
                quote: "采用混合召回".into(),
                locator: Default::default(),
                retrieval_score: 1.0,
            }],
        }];
        base.source_files = vec![AnswerSourceFile {
            file_id: base.claims[0].citations[0].file_id,
            display_name: "资料.md".into(),
            canonical_path: "D:\\资料.md".into(),
        }];
        assert!(apply_grounded_generation(&base, "项目采用混合召回。[S1]").is_some());
        assert!(apply_grounded_generation(
            &base,
            "## 结论\n\n- 项目采用混合召回。[S1]\n\n| 项目 | 说明 |\n| --- | --- |\n| 检索 | 采用混合召回。[S1] |"
        )
        .is_some());
        assert!(apply_grounded_generation(&base, "项目采用混合召回。没有引用。").is_none());
        assert!(apply_grounded_generation(&base, "- 缺少引用").is_none());
        assert!(apply_grounded_generation(&base, "不存在的来源。[S2]").is_none());
    }
}
