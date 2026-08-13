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
    let ambiguous_patterns = [
        Regex::new(r"去年|上个?[月周]|前[几些](天|周|月|年)|最近(几天|几周|几月|一年)").unwrap(),
        Regex::new(r"这[个些]|那[个些]|上述|前面提到|之前说的").unwrap(),
    ];
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 200 {
        return false;
    }
    ambiguous_patterns
        .iter()
        .any(|pattern| pattern.is_match(trimmed))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnswerStyle {
    Concise,
    Detailed,
    List,
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
                "翻翻V1只允许开启严格证据模式的资料问答",
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
pub struct GroundedAnswerClaimDraft {
    pub text: String,
    pub citation_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroundedAnswerDraft {
    pub claims: Vec<GroundedAnswerClaimDraft>,
    pub refusal: Option<String>,
}

pub fn grounded_answer_json_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["claims", "refusal"],
        "properties": {
            "claims": {
                "type": "array",
                "maxItems": 12,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "citation_ids"],
                    "properties": {
                        "text": {"type": "string", "minLength": 1, "maxLength": 1000},
                        "citation_ids": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": 6,
                            "uniqueItems": true,
                            "items": {"type": "string", "pattern": "^S[1-9][0-9]*$"}
                        }
                    }
                }
            },
            "refusal": {"type": ["string", "null"], "maxLength": 300}
        }
    })
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

/// Fast, deterministic support check for claims that closely quote their
/// evidence. It is deliberately conservative: numbers must be preserved and a
/// paraphrase with weak lexical overlap falls through to the local entailment
/// verifier instead of being accepted here.
pub fn claim_has_deterministic_support<'a>(
    claim: &str,
    evidence: impl IntoIterator<Item = &'a str>,
) -> bool {
    let claim_normalized = normalize_support_text(claim);
    if claim_normalized.chars().count() < 4 {
        return false;
    }
    let evidence_normalized = evidence
        .into_iter()
        .map(normalize_support_text)
        .collect::<Vec<_>>()
        .join(" ");
    if evidence_normalized.contains(&claim_normalized) {
        return true;
    }
    let claim_numbers = support_numbers(&claim_normalized);
    if claim_numbers
        .iter()
        .any(|number| !evidence_normalized.contains(number))
    {
        return false;
    }
    let claim_tokens = support_tokens(&claim_normalized);
    if claim_tokens.len() < 3 {
        return false;
    }
    let evidence_tokens = support_tokens(&evidence_normalized);
    let matched = claim_tokens.intersection(&evidence_tokens).count();
    matched as f64 / claim_tokens.len() as f64 >= 0.72
}

fn normalize_support_text(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() || is_han_character(character) {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn support_numbers(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .filter(|token| token.chars().any(|character| character.is_ascii_digit()))
        .map(str::to_owned)
        .collect()
}

fn support_tokens(value: &str) -> HashSet<String> {
    let mut tokens = HashSet::new();
    let mut han_run = Vec::new();
    let mut latin = String::new();
    let flush_han = |run: &mut Vec<char>, output: &mut HashSet<String>| {
        if run.len() == 1 {
            output.insert(run[0].to_string());
        } else {
            for pair in run.windows(2) {
                output.insert(pair.iter().collect());
            }
        }
        run.clear();
    };
    let flush_latin = |run: &mut String, output: &mut HashSet<String>| {
        if run.chars().count() >= 2 {
            output.insert(std::mem::take(run));
        } else {
            run.clear();
        }
    };
    for character in value.chars() {
        if is_han_character(character) {
            flush_latin(&mut latin, &mut tokens);
            han_run.push(character);
        } else if character.is_ascii_alphanumeric() || character == '.' {
            flush_han(&mut han_run, &mut tokens);
            latin.push(character);
        } else {
            flush_han(&mut han_run, &mut tokens);
            flush_latin(&mut latin, &mut tokens);
        }
    }
    flush_han(&mut han_run, &mut tokens);
    flush_latin(&mut latin, &mut tokens);
    tokens
}

fn is_han_character(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
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
                    "翻翻"
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
        "问题：{}\n\n可用证据：\n{}\n\n只根据以上证据回答。把每个可独立核验的事实写成一条claim，并在citation_ids中列出直接支持它的S编号。不得使用不存在的编号，不得把对话历史当作证据，不得补充外部知识。只输出符合指定JSON Schema的对象，不要输出Markdown、代码块或解释。证据不足时claims为空，并在refusal中说明‘当前资料中未找到足够依据’。",
        request.question.trim(),
        sources
    ));
    prompt
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
    let cleaned = generated
        .trim()
        .strip_prefix("```json")
        .or_else(|| generated.trim().strip_prefix("```"))
        .unwrap_or(generated.trim())
        .strip_suffix("```")
        .unwrap_or(generated.trim())
        .trim();
    let draft = serde_json::from_str::<GroundedAnswerDraft>(cleaned).ok()?;
    if draft.claims.is_empty()
        || draft.claims.len() > 12
        || draft
            .refusal
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        return None;
    }
    let mut claims = Vec::new();
    for draft_claim in draft.claims {
        let text = draft_claim.text.trim();
        if text.is_empty() || text.chars().count() > 1000 || text.contains("[S") {
            return None;
        }
        if draft_claim.citation_ids.is_empty() || draft_claim.citation_ids.len() > 6 {
            return None;
        }
        let mut citations = Vec::new();
        for citation_id in &draft_claim.citation_ids {
            let index = citation_id.strip_prefix('S')?.parse::<usize>().ok()?;
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
            text: text.to_owned(),
            support_status: SupportStatus::Supported,
            citations,
        });
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
    result.answer = claims
        .iter()
        .map(|claim| {
            let markers = claim
                .citations
                .iter()
                .filter_map(|citation| {
                    evidence
                        .iter()
                        .position(|item| item.evidence_id == citation.evidence_id)
                        .map(|index| format!("[S{}]", index + 1))
                })
                .collect::<Vec<_>>()
                .join("");
            format!("- {} {}", claim.text, markers)
        })
        .collect::<Vec<_>>()
        .join("\n");
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
    fn generated_answer_requires_schema_valid_claims_and_existing_citations() {
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
        assert!(
            apply_grounded_generation(
                &base,
                r#"{"claims":[{"text":"项目采用混合召回。","citation_ids":["S1"]}],"refusal":null}"#
            )
            .is_some()
        );
        assert!(apply_grounded_generation(&base, "项目采用混合召回。[S1]").is_none());
        assert!(
            apply_grounded_generation(
                &base,
                r#"{"claims":[{"text":"缺少引用","citation_ids":[]}],"refusal":null}"#
            )
            .is_none()
        );
        assert!(
            apply_grounded_generation(
                &base,
                r#"{"claims":[{"text":"不存在的来源","citation_ids":["S2"]}],"refusal":null}"#
            )
            .is_none()
        );
        assert!(
            apply_grounded_generation(
                &base,
                r#"{"claims":[{"text":"项目采用混合召回。","citation_ids":["S1"]}],"refusal":"当前资料中未找到足够依据"}"#
            )
            .is_none(),
            "a response cannot contain factual claims and a refusal at the same time"
        );
        assert_eq!(grounded_answer_json_schema()["additionalProperties"], false);
    }

    #[test]
    fn deterministic_claim_support_preserves_numbers_and_requires_strong_overlap() {
        assert!(claim_has_deterministic_support(
            "项目采用混合召回，目标召回率为90%。",
            ["项目采用混合召回，目标召回率为90%。"]
        ));
        assert!(!claim_has_deterministic_support(
            "项目采用混合召回，目标召回率为80%。",
            ["项目采用混合召回，目标召回率为90%。"]
        ));
        assert!(!claim_has_deterministic_support(
            "这个项目表现非常优秀。",
            ["项目采用混合召回，并通过引用核验。"]
        ));
    }
}
