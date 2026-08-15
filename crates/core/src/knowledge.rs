use std::collections::HashSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::router::Intent;
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
    let json = strip_code_fence(raw);
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
        .map(|(index, evidence)| {
            // 命中块前后各注入一块相邻文本（已按 token 上限截断），让模型
            // 知道「这段证据在原文中前后是什么」。【上文】/【下文】只作
            // 语境，不得被引用。
            let marker = format!("[S{}]", index + 1);
            let mut block = format!("{marker} 正文：{}", evidence.quote);
            if let Some(before) = evidence
                .context_before
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                block = format!("{marker} 【上文】{before}\n{block}");
            }
            if let Some(after) = evidence
                .context_after
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                block = format!("{block}\n{marker} 【下文】{after}");
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut prompt = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        prompt.push_str(&format!(
            "对话历史（仅作理解前文，不能作为新证据；回答中出现的来源编号只对应当前问题的可用证据）：\n{folded}\n\n"
        ));
    }
    prompt.push_str(&format!(
        "问题：{}\n\n可用证据：\n{}\n\n\
把证据整理成一段通顺、自然的中文回答，直接回答用户的问题。用自己的话改写证据内容，不要照抄原文；回答按逻辑分成 2-8 条 claim（每条一到两句），每条 claim 在 citation_ids 中列出直接支持它的 S 编号。改写时保留关键数字和专有名词。回答中出现的每个事实都必须能从可用证据中找到依据，不得凭空编造或补充外部知识。不得使用不存在的编号，不得把对话历史当作证据。每条证据中的【上文】/【下文】只是命中块在原文中的邻近文本，仅用于帮助你理解正文证据的语境，不得作为引用来源。证据不足时 claims 为空，并在 refusal 中说明“当前资料中未找到足够依据”。只输出符合指定 JSON Schema 的对象，不要输出 Markdown、代码块或解释。\n\n\
参考示例一（证据充足时的润色输出）：\n\
问题：公司年假政策是什么？\n\
可用证据：\n[S1] 入职满一年后每年享有 5 天带薪年假，工龄每增加一年增加 1 天，上限 15 天。\n[S2] 年假需在每年 3 月底前休完，未休部分可顺延至次年 6 月，但最多顺延 5 天。\n\
输出：{{\"claims\":[{{\"text\":\"入职满一年后，员工每年可享受 5 天带薪年假，工龄每增加一年就多 1 天，最多不超过 15 天。\",\"citation_ids\":[\"S1\"]}},{{\"text\":\"年假原则上需在次年 3 月底前用完，未休完的部分最多可顺延 5 天至次年 6 月。\",\"citation_ids\":[\"S1\",\"S2\"]}}],\"refusal\":null}}\n\n\
参考示例二（证据不足时的拒绝输出）：\n\
问题：公司加班补贴标准是多少？\n\
可用证据：\n[S1] 本周三下午三点在会议室三召开部门季度总结会。\n\
输出：{{\"claims\":[],\"refusal\":\"当前资料中未找到足够依据\"}}\n\n\
现在请回答上面的真实问题：",
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
    let cleaned = strip_code_fence(generated);
    let draft = serde_json::from_str::<GroundedAnswerDraft>(cleaned).ok()?;
    if draft.claims.is_empty() || draft.claims.len() > 12 {
        return None;
    }
    // refusal 与 claims 在 schema 中允许共存（refusal 可空）；弱模型常把
    // 「部分不确定」写进 refusal 而非只给 null。只要 claims 非空就以 claims 为准，
    // 否则上面 claims.is_empty() 已按拒绝处理。
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
    // 润色后的回答：多条 claim 按自然段拼接，不带 [S#] 内联标记（引用通过
    // 前端文件标签表达，claims/citations 结构保留给前端做引文详情）。
    result.answer = claims
        .iter()
        .map(|claim| claim.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    result.claims = claims;
    result.used_file_ids = source_files.iter().map(|source| source.file_id).collect();
    result.source_files = source_files;
    result.answer_mode = "generated".into();
    Some(result)
}

/// 剥离 markdown 代码围栏（```json / ```），无围栏时原样返回。
/// 供解析生成模型输出的三处共用（查询理解/引用核验/意图路由）。
fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed)
}

/// 将时间升序的会话历史折叠为「用户：/翻翻：」文本行。
/// 取最近 user_limit 条 user 消息与最近 assistant_limit 条 assistant 消息，
/// 按原时间顺序交错输出；单条空白折叠 + 400 字截断（与 chat_prompt /
/// generation_prompt 现状风格一致）。空历史或某角色不足时只输出存在的一侧。
pub fn fold_recent_history(
    history: &[AskMessage],
    user_limit: usize,
    assistant_limit: usize,
) -> String {
    let mut user_indices = Vec::new();
    let mut assistant_indices = Vec::new();
    for (index, message) in history.iter().enumerate() {
        if message.role == "user" {
            user_indices.push(index);
        } else {
            assistant_indices.push(index);
        }
    }
    let user_from = user_indices.len().saturating_sub(user_limit);
    let assistant_from = assistant_indices.len().saturating_sub(assistant_limit);
    let mut kept = vec![false; history.len()];
    for &index in user_indices[user_from..].iter().chain(&assistant_indices[assistant_from..]) {
        kept[index] = true;
    }
    history
        .iter()
        .enumerate()
        .filter(|(index, _)| kept[*index])
        .map(|(_, message)| {
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
        .join("\n")
}

/// 构建 LLM 意图直路由 prompt（替代 few-shot 语义路由 + 仲裁）。
/// system 说明角色与 JSON 输出约束；user 含两分支行为定义、最近 5+5 对话
/// 历史（`fold_recent_history`）与当前问题。只输出 JSON，不输出其他内容。
pub fn intent_routing_prompt(question: &str, history: &[AskMessage]) -> (String, String) {
    let system =
        "你是对话意图判断助手。只输出JSON，不要输出其他任何文字、解释或 markdown 标记。".into();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!("最近的对话（用于判断当前问题是否延续上文）：\n{folded}\n\n"));
    }
    user.push_str(&format!(
        r#"判断用户这句话是想「检索本地资料库」还是「闲聊」。

检索资料（retrieval）：想找文档、查数据、问制度流程、需要引用本地材料作答，或对之前检索过的资料继续追问。判断为检索时，将执行：检索本地资料库 → 按用户问题精排（Rerank）→ 结合检索结果与对话历史给出带引用的回答。例如：
- 公司的报销流程是什么
- 归航计划的时间安排是怎样的
- 去年的财务报表数据
- 那报销金额的上限是多少

闲聊（chat）：寒暄、谈感受、与资料无关的日常对话。判断为闲聊时，直接自然对话回复，不检索。例如：
- 你好啊
- 今天天气怎么样
- 给我讲个笑话
- 你最近怎么样

只凭以上两类定义和这条消息本身判断，不要犹豫。如果无法确定，或更接近寒暄/延续闲聊，输出 {{"intent":"chat"}}。宁可闲聊也绝不误检索。

用户说：{question}

只输出：{{"intent":"retrieval"}} 或 {{"intent":"chat"}}"#,
        question = question.trim()
    ));
    (system, user)
}

/// 解析路由输出为意图；解析失败或意图非法返回 None
/// （route_question 兜底默认走 Chat，不再静默落 RAG）。
/// 兼容两种格式：完整版 prompt 的 JSON（{"intent":"retrieval"|"chat"}）与
/// 0.6B 迷你版的两字输出（「检索」「闲聊」，允许前后空白/标点/引号噪声）。
pub fn parse_intent_verdict(raw: &str) -> Option<Intent> {
    let cleaned = strip_code_fence(raw).trim();
    if let Ok(verdict) = serde_json::from_str::<serde_json::Value>(cleaned) {
        if let Some(intent) = verdict.get("intent").and_then(|value| value.as_str()) {
            return match intent {
                "retrieval" => Some(Intent::Retrieval),
                "chat" => Some(Intent::Chat),
                _ => None,
            };
        }
    }
    // 词级格式：取最后一次出现的候选（recency——模型判的答案通常在末尾）。
    // 约束行「只输出：检索 或 闲聊」若被复读，「闲聊」靠后 → 偏 Chat 安全方向。
    let retrieval_pos = cleaned.rfind("检索");
    let chat_pos = cleaned.rfind("闲聊");
    match (retrieval_pos, chat_pos) {
        (Some(retrieval), Some(chat)) if retrieval > chat => Some(Intent::Retrieval),
        (Some(_), Some(_)) => Some(Intent::Chat),
        (Some(_), None) => Some(Intent::Retrieval),
        (None, Some(_)) => Some(Intent::Chat),
        (None, None) => None,
    }
}

/// 0.6B 迷你版意图路由 prompt：两字词级输出（续写强项）+ 词表信号 + 错误样例。
/// 与完整版 [`intent_routing_prompt`] 并存；route_question 按模型档位选择。
/// 设计：0.6B 不擅长长 prompt 全局阅读与 JSON 结构化输出，但擅长词级续写与
/// 示例复读——所以判断标准浓缩为词表引导两句，示例直接给出实测失败句的答案。
pub fn intent_routing_prompt_mini(question: &str, history: &[AskMessage]) -> (String, String) {
    let system = "你是意图判断助手。只输出两个字：检索 或 闲聊。不要输出其他任何文字、解释或 markdown 标记。".into();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!("最近的对话：\n{folded}\n\n"));
    }
    user.push_str(&format!(
        r#"判断用户这句话要不要查本地资料库：
- 想查本地文档、资料、制度、流程、数据、合同、方案 → 检索
- 其他一切（寒暄、闲聊、知识问答、天气、自我介绍、心情） → 闲聊

例子：
- 公司的报销流程是什么 → 检索
- 去年的财务报表数据 → 检索
- 今天天气怎么样 → 闲聊
- 你是什么模型 → 闲聊

用户说：{question}

只输出：检索 或 闲聊。无法确定时输出闲聊。宁可闲聊也绝不误检索。"#,
        question = question.trim()
    ));
    (system, user)
}

/// 构建追问改写 prompt（极简，0.6B 友好）：历史折叠 5+5 段（非空时），
/// 输出约束为行式——每行一个可直接检索的中文问题；已明确单一则原样输出。
/// 0.6B 对行式续写比 JSON 数组格式容错更高。
pub fn query_rewrite_prompt(question: &str, history: &[AskMessage]) -> (String, String) {
    let system = "你是问题改写助手。只输出问题，不要解释或多余文字。".into();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!("最近的对话：\n{folded}\n\n"));
    }
    user.push_str(&format!(
        r#"把当前问题改写成可以直接查资料的中文问题。
如果它包含多个问题，每行写一个。如果它引用了上文（如"那金额上限呢"），补全成完整问题。
如果已经清楚明确，原样写一遍。
只输出问题行，不要解释。

当前问题：{question}"#,
        question = question.trim()
    ));
    (system, user)
}

/// 解析改写输出为检索问题列表：按行拆分、去空/去重、单行 ≤500 字、上限 3 个。
/// 解析失败或输出为空返回空 Vec（调用方回退用户原始问题）。
pub fn parse_rewritten_queries(raw: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut queries = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        let line = line
            .trim_start_matches(|ch| matches!(ch, '-' | '•' | '·' | '#' | ' ' | '\t'))
            .trim_start_matches(|ch: char| ch.is_ascii_digit())
            .trim_start_matches(|ch| matches!(ch, '.' | '、' | ')' | '）' | ' '));
        if line.is_empty() || !seen.insert(line.to_owned()) {
            continue;
        }
        queries.push(line.chars().take(500).collect());
        if queries.len() >= 3 {
            break;
        }
    }
    queries
}

/// 闲聊 persona 的补全 prompt（纯补全非 JSON）：人设 + 对话历史折叠 + 当前问题。
/// 历史格式与 generation_prompt 一致（「用户：/翻翻：」，每条约 400 字），
/// 最多折叠 5 轮（5 条用户 + 5 条模型回复）。
pub fn chat_prompt(request: &AskRequest, history: &[AskMessage]) -> (String, String) {
    let system =
        "你是翻翻，用户本地资料库中的中文智能助手。当前是闲聊场景，不涉及资料检索，直接自然对话即可。回答简短、自然、友好。".to_owned();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!("对话历史：\n{folded}\n\n"));
    }
    user.push_str(&format!("用户：{}", request.question.trim()));
    (system, user)
}

/// 0.6B 专用的闲聊 prompt。0.6B 指令跟随弱、在裸 system+user 下只会复读
/// system 里的自我介绍或套「你好！…？」模板复读用户问题（日志实测），但
/// 示例复读是它的强项。因此：system 只留一句不诱发自我介绍的话；user 给
/// 5+5 历史后接 4 条「用户：/翻翻：」示例（直接对应实测失败句与寒暄高频
/// 场景），示例紧邻输出位置，最后以「用户：{q}\n翻翻：」结尾引导续写。
pub fn chat_prompt_mini(question: &str, history: &[AskMessage]) -> (String, String) {
    let system = "你是翻翻，一个中文助手。直接回答用户的问题，不要重复用户说的话，不要说「你好！我是翻翻」。"
        .to_owned();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!("对话历史：\n{folded}\n\n"));
    }
    user.push_str(
        "示例：\n\
         用户：你好\n\
         翻翻：你好呀，有什么想聊的？\n\
         用户：你是谁\n\
         翻翻：我是翻翻，一个中文助手，平时可以帮你查查资料聊聊天。\n\
         用户：今天天气怎么样\n\
         翻翻：这我还真不知道，我没有联网看天气的能力，你打开手机天气App看看更准。\n\
         用户：你别学我说话\n\
         翻翻：好好好，不学你了。你有别的事想问我吗？\n\n",
    );
    user.push_str(&format!("用户：{}\n翻翻：", question.trim()));
    (system, user)
}

/// 剥离文本中的 [S\d+] 引用标记（如 "[S1]"），保留正文
fn strip_citation_markers(text: &str) -> String {
    let marker = Regex::new(r"\[S\d+\]").unwrap();
    marker.replace_all(text, "").trim().to_owned()
}

/// 引用核验失败时，从生成模型的原始输出中宽松提取可展示文本。
/// 能解析为 grounded draft：claims 非空则逐条 "- 正文"（剥离 [S\d+] 标记），
/// 否则用 refusal；解析失败返回原文（可能是纯补全输出）。统一截断到 4000 字符。
pub fn extract_unverified_text(generated: &str) -> String {
    let raw = strip_code_fence(generated);
    let text = match serde_json::from_str::<GroundedAnswerDraft>(raw) {
        Ok(draft) if !draft.claims.is_empty() => draft
            .claims
            .iter()
            .map(|claim| {
                let body = strip_citation_markers(claim.text.trim());
                format!("- {body}")
            })
            .filter(|line| line.trim().len() > 2)
            .collect::<Vec<_>>()
            .join("\n"),
        Ok(draft) => draft.refusal.unwrap_or_default(),
        Err(_) => raw.to_owned(),
    };
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    text.chars().take(4000).collect()
}

/// 引用核验失败后的降级答案：保留正文展示，标记未通过核验。
/// 清空 claims/sources/used_file_ids，grounding_status=Insufficient，
/// answer_mode="unverified"，degradation_reason 给出可见文案。
/// retrieval_channels 与 index_coverage 沿用 base（语义检索确实发生过）。
pub fn unverified_answer(
    base: &AnswerResult,
    session_id: Uuid,
    answer_text: String,
    elapsed_ms: u64,
) -> AnswerResult {
    let mut result = base.clone();
    result.session_id = session_id;
    result.message_id = Uuid::now_v7();
    result.answer = answer_text;
    result.grounding_status = GroundingStatus::Insufficient;
    result.insufficient_evidence = false;
    result.claims = Vec::new();
    result.source_files = Vec::new();
    result.used_file_ids = Vec::new();
    result.answer_mode = "unverified".into();
    result.elapsed_ms = elapsed_ms;
    result.degradation_reason = Some("本次生成结果未通过核验，置信度较低".to_owned());
    result
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
                context_before: None,
                context_after: None,
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
        // refusal 与 claims 在 schema 中允许共存：弱模型常把「部分不确定」
        // 写进 refusal。claims 非空时以 claims 为准，refusal 不构成拒绝。
        let coexistence = apply_grounded_generation(
            &base,
            r#"{"claims":[{"text":"项目采用混合召回。","citation_ids":["S1"]}],"refusal":"当前资料中未找到足够依据"}"#,
        )
        .expect("claims with refusal should resolve to claims");
        assert_eq!(coexistence.claims.len(), 1);
        assert_eq!(coexistence.answer_mode, "generated");
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

    fn message(role: &str, content: &str) -> AskMessage {
        AskMessage {
            message_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            role: role.into(),
            content: content.into(),
            answer: None,
            error: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn intent_verdict_parsing() {
        assert_eq!(
            parse_intent_verdict(r#"{"intent":"retrieval"}"#),
            Some(Intent::Retrieval)
        );
        assert_eq!(
            parse_intent_verdict("```json\n{\"intent\":\"chat\"}\n```"),
            Some(Intent::Chat)
        );
        assert_eq!(
            parse_intent_verdict("```\n{\"intent\":\"retrieval\"}\n```"),
            Some(Intent::Retrieval)
        );
        assert_eq!(parse_intent_verdict("随便说点什么"), None);
        assert_eq!(parse_intent_verdict(r#"{"intent":"ambiguous"}"#), None);
        assert_eq!(parse_intent_verdict(r#"{"other":1}"#), None);
        assert_eq!(parse_intent_verdict(""), None);
        // 0.6B 迷你版的两字输出（含噪声容错）
        assert_eq!(parse_intent_verdict("闲聊"), Some(Intent::Chat));
        assert_eq!(parse_intent_verdict("检索"), Some(Intent::Retrieval));
        assert_eq!(parse_intent_verdict("答案是：闲聊。"), Some(Intent::Chat));
        assert_eq!(parse_intent_verdict("\"检索\""), Some(Intent::Retrieval));
        // 约束行被复读（"检索"在前"闲聊"在后）→ 取最后出现的 → 偏 Chat 安全
        assert_eq!(
            parse_intent_verdict("只输出：检索 或 闲聊"),
            Some(Intent::Chat)
        );
    }

    #[test]
    fn intent_routing_prompt_mini_guides_by_vocabulary_and_failure_examples() {
        let history = vec![message("user", "你好"), message("assistant", "你好！")];
        let (system, user) = intent_routing_prompt_mini("报销流程是什么", &history);
        // 词级输出约束
        assert!(system.contains("只输出两个字"));
        assert!(user.contains("检索 或 闲聊"));
        // 词表信号引导
        assert!(user.contains("文档、资料、制度、流程、数据、合同、方案"));
        assert!(user.contains("其他一切"));
        // 实测失败句直接进示例（chat 示例靠后贴近输出位置）
        assert!(user.contains("今天天气怎么样 → 闲聊"));
        assert!(user.contains("你是什么模型 → 闲聊"));
        assert!(user.contains("公司的报销流程是什么 → 检索"));
        // 5+5 历史段 + 当前问题 + 兜底句
        assert!(user.contains("最近的对话"));
        assert!(user.contains("用户：你好"));
        assert!(user.contains("翻翻：你好！"));
        assert!(user.contains("用户说：报销流程是什么"));
        assert!(user.contains("宁可闲聊也绝不误检索"));
    }

    #[test]
    fn intent_routing_prompt_mini_empty_history_has_no_history_section() {
        let (_, user) = intent_routing_prompt_mini("今天天气怎么样", &[]);
        assert!(!user.contains("最近的对话"));
        assert!(user.contains("用户说：今天天气怎么样"));
    }

    #[test]
    fn query_rewrite_prompt_injects_history_and_instructs_line_output() {
        let history = vec![message("user", "报销流程是什么"), message("assistant", "报销分三步。")];
        let (system, user) = query_rewrite_prompt("那金额上限呢", &history);
        assert!(system.contains("只输出问题"));
        assert!(user.contains("最近的对话"));
        assert!(user.contains("每行写一个"));
        assert!(user.contains("那金额上限呢"));
        assert!(user.contains("补全成完整问题"));
        // 无历史时不带历史段
        let (_, user2) = query_rewrite_prompt("报销流程是什么", &[]);
        assert!(!user2.contains("最近的对话"));
        assert!(user2.contains("原样写一遍"));
    }

    #[test]
    fn parse_rewritten_queries_splits_lines_and_dedupes() {
        assert_eq!(
            parse_rewritten_queries("公司的报销流程是什么\n请假规定有哪些"),
            vec!["公司的报销流程是什么".to_owned(), "请假规定有哪些".to_owned()]
        );
        // 空行/重复/列表符号剥离
        assert_eq!(
            parse_rewritten_queries("\n1、报销流程\n- 报销流程\n  \n"),
            vec!["报销流程".to_owned()]
        );
        // 上限 3 个
        assert_eq!(parse_rewritten_queries("a\nb\nc\nd").len(), 3);
        // 空输入/乱格式 → 空 Vec（回退原始问题）
        assert_eq!(parse_rewritten_queries(""), Vec::<String>::new());
        assert_eq!(parse_rewritten_queries("   \n \n"), Vec::<String>::new());
        // markdown 符号行剥净后为空
        assert_eq!(parse_rewritten_queries("---\n###"), Vec::<String>::new());
        assert_eq!(parse_rewritten_queries("## 标题行"), vec!["标题行".to_owned()]);
    }

    #[test]
    fn parse_rewritten_queries_truncates_long_lines() {
        let long = "长".repeat(600);
        let queries = parse_rewritten_queries(&long);
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].chars().count(), 500);
    }

    #[test]
    fn chat_prompt_folds_history_and_question() {
        let history = vec![message("user", "  你好  呀 "), message("assistant", "你好！")];
        let (system, user) = chat_prompt(&request(), &history);
        assert!(system.contains("翻翻"));
        assert!(user.contains("用户：你好 呀"));
        assert!(user.contains("翻翻：你好！"));
        assert!(user.contains("用户：项目如何优化召回率？"));
        // 无历史时不出现历史段
        let (_, user2) = chat_prompt(&request(), &[]);
        assert!(!user2.contains("对话历史"));
        assert!(user2.starts_with("用户："));
    }

    #[test]
    fn chat_prompt_mini_has_examples_and_continuation_tail() {
        let history = vec![message("user", "你好"), message("assistant", "你好呀")];
        let (system, user) = chat_prompt_mini("今天天气怎么样", &history);
        // system 不再有会被 0.6B 转述成自我介绍的长角色设定
        assert!(!system.contains("本地资料库"));
        assert!(!system.contains("中文智能助手"));
        assert!(system.contains("不要重复用户说的话"));
        // 5+5 历史段保留
        assert!(user.contains("对话历史："));
        assert!(user.contains("翻翻：你好呀"));
        // 4 条示例直接对应实测失败句（你是谁/天气/学我说话）与寒暄
        assert!(user.contains("用户：你是谁"));
        assert!(user.contains("用户：今天天气怎么样"));
        assert!(user.contains("用户：你别学我说话"));
        assert!(user.contains("用户：你好"));
        // 示例位于输出位置之前，结尾是「用户：{q}\n翻翻：」续写引导
        let tail = user
            .split("翻翻：好好好")
            .last()
            .expect("末尾示例存在");
        assert!(tail.contains("用户：今天天气怎么样\n翻翻："), "输出引导紧邻示例");
        assert!(!user.contains("你好！我是翻翻"), "示例不含被复读的模板开场白");
    }

    #[test]
    fn chat_prompt_mini_empty_history_skips_history_section() {
        let (_, user) = chat_prompt_mini("你是谁", &[]);
        assert!(!user.contains("对话历史"));
        assert!(user.contains("用户：你是谁\n翻翻："));
    }

    #[test]
    fn chat_prompt_folds_at_most_five_rounds() {
        // 8 轮（16 条消息）→ 只折叠最近 5 轮（5 用户 + 5 翻翻）
        let mut history = Vec::new();
        for round in 1..=8 {
            history.push(message("user", &format!("第{round}轮问题")));
            history.push(message("assistant", &format!("第{round}轮回答")));
        }
        let (_, user) = chat_prompt(&request(), &history);
        // 折叠段 5 用户 + 5 翻翻；末尾另有 chat_prompt 追加的一行当前提问
        let user_lines = user.lines().filter(|line| line.starts_with("用户：")).count();
        let assistant_lines = user.lines().filter(|line| line.starts_with("翻翻：")).count();
        assert_eq!(assistant_lines, 5, "最多折叠 5 条翻翻回复");
        assert_eq!(user_lines, 6, "5 条历史用户消息 + 1 行当前提问");
        assert!(user.contains("用户：第4轮问题"), "最近 5 轮包含第 4 轮");
        assert!(!user.contains("第1轮"), "早于最近 5 轮的历史被丢弃");
        assert!(!user.contains("第2轮"));
        assert!(!user.contains("第3轮"));
    }

    #[test]
    fn fold_recent_history_caps_and_interleaves_by_role() {
        // 10 轮（20 条）→ 只保留最近 5 用户 + 5 翻翻，保持时间升序交错
        let mut history = Vec::new();
        for round in 1..=10 {
            history.push(message("user", &format!("u{round}")));
            history.push(message("assistant", &format!("a{round}")));
        }
        let folded = fold_recent_history(&history, 5, 5);
        let lines = folded.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 10);
        // 只保留最近 5 轮（u6/a6 … u10/a10），时间升序交错；逐行全等断言
        // （不能用 contains("u1")——"u10" 含子串 "u1"）
        let expected = (6..=10)
            .flat_map(|round| vec![format!("用户：u{round}"), format!("翻翻：a{round}")])
            .collect::<Vec<_>>();
        assert_eq!(lines, expected);
    }

    #[test]
    fn fold_recent_history_handles_missing_roles() {
        // 空历史
        assert_eq!(fold_recent_history(&[], 5, 5), "");
        // 只有 user 消息：保留最近 5 条
        let users = (1..=7)
            .map(|round| message("user", &format!("u{round}")))
            .collect::<Vec<_>>();
        let folded = fold_recent_history(&users, 5, 5);
        assert_eq!(folded.lines().count(), 5);
        assert!(folded.contains("用户：u3"));
        assert!(!folded.contains("u1"));
        assert!(!folded.contains("u2"));
        // 只有 assistant 消息：assistant_limit 生效
        let assistants = (1..=3)
            .map(|round| message("assistant", &format!("a{round}")))
            .collect::<Vec<_>>();
        let folded = fold_recent_history(&assistants, 5, 5);
        assert_eq!(folded.lines().count(), 3);
        assert!(folded.contains("翻翻：a3"));
        // user_limit=0 → 只输出 assistant 侧
        let history = vec![message("user", "u1"), message("assistant", "a1")];
        let folded = fold_recent_history(&history, 0, 5);
        assert_eq!(folded, "翻翻：a1");
    }

    #[test]
    fn fold_recent_history_truncates_long_lines() {
        let long = "字".repeat(600);
        let history = vec![message("user", &long)];
        let folded = fold_recent_history(&history, 5, 5);
        let line = folded.lines().next().expect("folded line");
        assert!(line.starts_with("用户："));
        // 400 字截断 + 角色标签（“用户：”3 个字符）
        assert_eq!(line.chars().count(), 3 + 400);
    }

    #[test]
    fn intent_routing_prompt_specifies_branches_and_history() {
        let history = vec![message("user", "你好"), message("assistant", "你好！")];
        let (system, user) = intent_routing_prompt("报销流程是什么", &history);
        assert!(system.contains("只输出JSON"));
        // 两分支行为定义
        assert!(user.contains("检索资料（retrieval）"));
        assert!(user.contains("检索本地资料库"));
        assert!(user.contains("Rerank"));
        assert!(user.contains("闲聊（chat）"));
        assert!(user.contains("直接自然对话回复，不检索"));
        // 5+5 历史段（角色标注）
        assert!(user.contains("最近的对话"));
        assert!(user.contains("用户：你好"));
        assert!(user.contains("翻翻：你好！"));
        // JSON 输出约束与兜底句
        assert!(user.contains(r#"{"intent":"retrieval"}"#));
        assert!(user.contains(r#"{"intent":"chat"}"#));
        assert!(user.contains("宁可闲聊也绝不误检索"));
        assert!(user.contains("用户说：报销流程是什么"));
    }

    #[test]
    fn intent_routing_prompt_empty_history_has_no_history_section() {
        let (_, user) = intent_routing_prompt("今天天气怎么样", &[]);
        // 历史段头部只在有历史时出现（「延续上文」为该段独有的标记）
        assert!(!user.contains("延续上文"));
        assert!(!user.contains("最近的对话"));
        assert!(user.contains("用户说：今天天气怎么样"));
    }

    #[test]
    fn generation_prompt_injects_neighbor_context_around_quotes() {
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
                context_before: Some("先介绍背景".into()),
                context_after: Some("再讲结论".into()),
                locator: Default::default(),
                retrieval_score: 1.0,
            }],
        }];
        let prompt = generation_prompt(&request(), &base, &[]);
        assert!(prompt.contains("[S1] 【上文】先介绍背景"));
        assert!(prompt.contains("[S1] 正文：采用混合召回"));
        assert!(prompt.contains("[S1] 【下文】再讲结论"));
        // 上下文只作语境：prompt 明确告知模型不得把【上文】/【下文】当引用来源
        assert!(prompt.contains("不得作为引用来源"));
        // 无邻居时不出现上下文行（指令文本中的【上文】/【下文】字样除外）
        let mut bare = base.clone();
        bare.claims[0].citations[0].context_before = None;
        bare.claims[0].citations[0].context_after = None;
        let prompt = generation_prompt(&request(), &bare, &[]);
        assert!(!prompt.contains("[S1] 【上文】"));
        assert!(!prompt.contains("[S1] 【下文】"));
        assert!(prompt.contains("[S1] 正文：采用混合召回"));
    }

    #[test]
    fn extract_unverified_handles_all_inputs() {
        let draft = r#"{"claims":[{"text":"报销上限是5000元","citation_ids":["S1"]}],"refusal":null}"#;
        assert_eq!(extract_unverified_text(draft), "- 报销上限是5000元");
        // 带 [S\d+] 标记的正文被剥离
        let dirty = r#"{"claims":[{"text":"规定如下[S1][S2]","citation_ids":["S1"]}],"refusal":null}"#;
        assert_eq!(extract_unverified_text(dirty), "- 规定如下");
        // 多条 claims 逐条 "- " 拼接
        let multi = r#"{"claims":[{"text":"第一点","citation_ids":["S1"]},{"text":"第二点","citation_ids":["S2"]}],"refusal":null}"#;
        assert_eq!(extract_unverified_text(multi), "- 第一点\n- 第二点");
        // refusal 兜底
        let refusal = r#"{"claims":[],"refusal":"资料里没找到"}"#;
        assert_eq!(extract_unverified_text(refusal), "资料里没找到");
        // 非 JSON → 原文
        let raw = "直接回答：报销上限是5000元";
        assert_eq!(extract_unverified_text(raw), raw);
        // 围栏包裹
        let fenced = format!("```json\n{draft}\n```");
        assert_eq!(extract_unverified_text(&fenced), "- 报销上限是5000元");
        // 超长截断到 4000 字符
        let long = format!(
            r#"{{"claims":[{{"text":"{}","citation_ids":["S1"]}}],"refusal":null}}"#,
            "长".repeat(5000)
        );
        assert_eq!(extract_unverified_text(&long).chars().count(), 4000);
    }

    #[test]
    fn unverified_answer_degrades_cleanly() {
        let mut base = assemble_extractive_answer(
            &request(),
            &SearchSession {
                search_id: Uuid::now_v7(),
                status: "completed".into(),
                channels: SearchChannels {
                    filename: SearchChannelState::Completed,
                    fulltext: SearchChannelState::Completed,
                    semantic: SearchChannelState::Completed,
                },
                results: vec![],
                next_cursor: None,
                elapsed_ms: 1,
            },
            vec![],
            Instant::now(),
        );
        base.insufficient_evidence = false;
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
                quote: "依据原文".into(),
                context_before: None,
                context_after: None,
                locator: crate::SourceLocator {
                    kind: crate::SourceKind::Pdf,
                    page_no: Some(1),
                    slide_no: None,
                    sheet_name: None,
                    cell_range: None,
                    paragraph_no: None,
                    line_start: None,
                    line_end: None,
                    shape_no: None,
                    bbox: None,
                    heading_path: Vec::new(),
                },
                retrieval_score: 0.9,
            }],
        }];
        let session_id = Uuid::now_v7();
        let degraded = unverified_answer(&base, session_id, "未核验的答案正文".into(), 1234);
        assert_eq!(degraded.answer, "未核验的答案正文");
        assert_eq!(degraded.session_id, session_id);
        assert_ne!(degraded.message_id, base.message_id);
        assert!(degraded.claims.is_empty());
        assert!(degraded.source_files.is_empty());
        assert!(degraded.used_file_ids.is_empty());
        assert_eq!(degraded.grounding_status, GroundingStatus::Insufficient);
        assert!(!degraded.insufficient_evidence);
        assert_eq!(degraded.answer_mode, "unverified");
        assert_eq!(degraded.elapsed_ms, 1234);
        assert_eq!(
            degraded.degradation_reason.as_deref(),
            Some("本次生成结果未通过核验，置信度较低")
        );
        // 检索通道信息保留
        assert_eq!(degraded.retrieval_channels, base.retrieval_channels);
        assert_eq!(degraded.index_coverage, base.index_coverage);
    }
}
