use std::collections::HashSet;
use std::time::Instant;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::router::Intent;
use crate::{AppError, DocumentType, EvidenceRef, ScopeFilter, SearchSession};

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
    /// 澄清选择（Step 7）：用户在 NEED_CLARIFICATION 中选中目标文件后，
    /// 原问题 + 该字段继续问答；锁定 scope、写 USER_SELECTION 记忆。
    #[serde(default)]
    pub clarification_selection: Option<Uuid>,
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

/// 澄清候选（Step 7）：多个非常接近的目标文件，需要用户明确选择一个。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClarificationOption {
    pub file_id: Uuid,
    /// 展示名（文件名）
    pub display_name: String,
    pub document_type: Option<DocumentType>,
    /// Resolver 综合得分（仅展示参考，不是决策依据）
    pub score: f32,
    /// 命中的定位信号
    pub signals: Vec<String>,
}

/// 回答模式（强类型，9 类）：替代散乱字符串标记，前端据此选择渲染方式。
///
/// 序列化为 snake_case；历史数据宽容反序列化（未知/旧标记 → Generated）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerMode {
    /// 普通 RAG 生成（证据核验通过）
    Generated,
    /// 闲聊（GENERAL / 路由失败兜底）
    Chat,
    /// LOCAL 检索无证据 → 固定拒绝文案，绝不转闲聊
    RagRefusal,
    /// 生成内容未通过引用核验的降级展示
    Unverified,
    /// NEED_CLARIFICATION：请用户在候选文件中选择目标
    Clarification,
    /// DOCUMENT_SUMMARY 整文摘要
    Summary,
    /// COMPARE_DOCUMENTS 两侧对比
    Compare,
    /// EXTRACT 结构化条目抽取
    Extract,
    /// DOCUMENT_FIND：返回定位到的文件
    Find,
}

impl AnswerMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AnswerMode::Generated => "generated",
            AnswerMode::Chat => "chat",
            AnswerMode::RagRefusal => "rag_refusal",
            AnswerMode::Unverified => "unverified",
            AnswerMode::Clarification => "clarification",
            AnswerMode::Summary => "summary",
            AnswerMode::Compare => "compare",
            AnswerMode::Extract => "extract",
            AnswerMode::Find => "find",
        }
    }

    /// 宽容解析：snake_case 变体名；历史字符串兼容（旧内部中间态
    /// "extractive" → Generated）。未知返回 None。
    pub fn parse_lenient(input: &str) -> Option<AnswerMode> {
        let normalized = input.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "generated" | "extractive" => Some(AnswerMode::Generated),
            "chat" => Some(AnswerMode::Chat),
            "rag_refusal" => Some(AnswerMode::RagRefusal),
            "unverified" => Some(AnswerMode::Unverified),
            "clarification" => Some(AnswerMode::Clarification),
            "summary" => Some(AnswerMode::Summary),
            "compare" => Some(AnswerMode::Compare),
            "extract" => Some(AnswerMode::Extract),
            "find" => Some(AnswerMode::Find),
            _ => None,
        }
    }
}

impl Serialize for AnswerMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AnswerMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(AnswerMode::parse_lenient(&value).unwrap_or(AnswerMode::Generated))
    }
}

/// NEED_CLARIFICATION 强类型载荷：随 `AnswerResult`（answer_mode = "clarification"）
/// 返回给前端，用户选择后带 `clarification_selection` 继续原问题。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClarificationPayload {
    /// 用户原始引用（如「我的简历」）；选择后据此写 USER_SELECTION 记忆
    pub reference: String,
    pub reason: String,
    pub options: Vec<ClarificationOption>,
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
    pub answer_mode: AnswerMode,
    #[serde(default)]
    pub retrieval_channels: Vec<String>,
    #[serde(default)]
    pub index_coverage: f64,
    #[serde(default)]
    pub degradation_reason: Option<String>,
    /// NO_EVIDENCE 的六分类根因（内部诊断，UI 文案不变；仅拒绝路径非空）。
    /// 见 [`crate::ask::NoEvidenceReason`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_evidence_reason: Option<crate::ask::NoEvidenceReason>,
    /// NEED_CLARIFICATION 载荷（仅 answer_mode = "clarification" 时非空）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clarification: Option<ClarificationPayload>,
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

/// 文档画像（document_profiles 表行，migration 8 建表 / 27 扩展）。
///
/// 现有列（title/summary/keywords/entities/embedding）由 organizing.rs 的
/// 集合建议管线写入；`document_type`/`type_confidence`/`section_titles`/
/// `representative_text_hash` 为 P0 扩展列（migration 27），由后续文档
/// 分类器写入。Document Resolver 读取它把「目标对象」锁定为 file_id。
///
/// 画像只用于**定位**，绝不直接成为 Citation Evidence。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentProfile {
    pub file_id: Uuid,
    pub revision_id: Uuid,
    pub title: String,
    pub summary: String,
    pub keywords: Vec<String>,
    pub entities: Vec<String>,
    #[serde(default)]
    pub document_type: Option<DocumentType>,
    #[serde(default)]
    pub type_confidence: Option<f32>,
    #[serde(default)]
    pub section_titles: Vec<String>,
    #[serde(default)]
    pub representative_text_hash: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// 文档画像批量构建（refresh_document_profiles）的统计结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileRefreshResult {
    /// 本次构建/更新的画像数
    pub profiled_files: u64,
    /// 命中候选但因数据缺失（无 chunk / 无向量）跳过的文件数
    pub skipped_files: u64,
}

/// 会话工作上下文（Session Working Context）。
///
/// 每个 ask session 保存轻量工作上下文，供 AMBIGUOUS 请求的 Context Resolver
/// 恢复目标：上一轮定位/引用的文件、文档类型、收藏集与意图。它不是历史
/// 文本，而是结构化的「用户正在指什么」。
///
/// 只允许 Memory/Document Resolver 用它来**定位**文件，绝不允许它成为
/// Citation Evidence（S# 只能来自原始文件 chunk / image evidence）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AskSessionContext {
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub active_file_id: Option<Uuid>,
    #[serde(default)]
    pub active_file_ids: Vec<Uuid>,
    #[serde(default)]
    pub active_document_type: Option<DocumentType>,
    #[serde(default)]
    pub active_entity_id: Option<Uuid>,
    #[serde(default)]
    pub active_collection_id: Option<Uuid>,
    #[serde(default)]
    pub last_referenced_file_ids: Vec<Uuid>,
    #[serde(default)]
    pub last_intent: Option<String>,
    /// 最近一次 NEED_CLARIFICATION 的原始引用（如「我的简历」），
    /// 用户选择后据此写 USER_SELECTION 别名记忆。
    #[serde(default)]
    pub pending_clarification_reference: Option<String>,
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
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
            // 中间态标记（extractive 候选）；调用方按证据门控覆写
            answer_mode: AnswerMode::Generated,
            retrieval_channels: vec!["filename".into(), "fts".into()],
            index_coverage: 0.0,
            degradation_reason: None,
            no_evidence_reason: None,
            clarification: None,
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
        // 中间态标记（extractive 候选）；调用方按证据门控覆写
        answer_mode: AnswerMode::Generated,
        retrieval_channels: vec!["filename".into(), "fts".into()],
        index_coverage: 0.0,
        degradation_reason: None,
        no_evidence_reason: None,
        clarification: None,
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
    // 结构化分区（手册：标题/分隔符区分不同部分，避免指令与示例混淆）。
    // 输出约束用编号列成要点，正面指令优先；示例放最后紧邻输出位置。
    let mut prompt = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        prompt.push_str(&format!(
            "【对话历史】\n{folded}\n\
             说明：历史仅作理解前文，不能作为新证据；回答中出现的来源编号只对应当前问题的可用证据。\n\n"
        ));
    }
    prompt.push_str(&format!(
        "【问题】\n{}\n\n【可用证据】\n{}\n\n\
【回答要求】\n\
1. 把证据整理成一段通顺、自然的中文回答，直接回答用户的问题；用自己的话改写证据内容，不要照抄原文。\n\
2. 回答按逻辑分成 2-8 条 claim（每条一到两句），每条 claim 在 citation_ids 中列出直接支持它的 S 编号。\n\
3. 改写时保留关键数字和专有名词。\n\
4. 回答中出现的每个事实都必须能从可用证据中找到依据，不得凭空编造或补充外部知识。\n\
5. 不得使用不存在的编号，不得把对话历史当作证据。每条证据中的【上文】/【下文】只是命中块在原文中的邻近文本，仅用于帮助你理解正文证据的语境，不得作为引用来源。\n\
6. 证据不足时 claims 为空，并在 refusal 中说明“当前资料中未找到足够依据”。\n\
7. 只输出符合指定 JSON Schema 的对象，不要输出 Markdown、代码块或解释。\n\n\
【参考示例】\n\
示例一（证据充足时的润色输出）：\n\
问题：公司年假政策是什么？\n\
可用证据：\n[S1] 入职满一年后每年享有 5 天带薪年假，工龄每增加一年增加 1 天，上限 15 天。\n[S2] 年假需在每年 3 月底前休完，未休部分可顺延至次年 6 月，但最多顺延 5 天。\n\
输出：{{\"claims\":[{{\"text\":\"入职满一年后，员工每年可享受 5 天带薪年假，工龄每增加一年就多 1 天，最多不超过 15 天。\",\"citation_ids\":[\"S1\"]}},{{\"text\":\"年假原则上需在次年 3 月底前用完，未休完的部分最多可顺延 5 天至次年 6 月。\",\"citation_ids\":[\"S1\",\"S2\"]}}],\"refusal\":null}}\n\n\
示例二（证据不足时的拒绝输出）：\n\
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
    result.answer_mode = AnswerMode::Generated;
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
    for &index in user_indices[user_from..]
        .iter()
        .chain(&assistant_indices[assistant_from..])
    {
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
        user.push_str(&format!(
            "【对话历史】以下是最近 5 条对话的历史记录（用于判断当前问题是否延续上文），不是用户现在说的话：\n{folded}\n\n"
        ));
    }
    user.push_str(&format!(
        r#"【当前问题】下面这一句才是用户刚刚说的最新一句话，请只根据这一句判断意图：
用户说：{question}

判断用户这句话是想「检索本地资料库」还是「闲聊」。

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

只输出：{{"intent":"retrieval"}} 或 {{"intent":"chat"}}"#,
        question = question.trim()
    ));
    (system, user)
}

/// 解析路由输出为意图；解析失败或意图非法返回 None
/// （route_question 兜底默认走 Chat，不再静默落 RAG）。
pub fn parse_intent_verdict(raw: &str) -> Option<Intent> {
    let cleaned = strip_code_fence(raw).trim();
    serde_json::from_str::<serde_json::Value>(cleaned)
        .ok()?
        .get("intent")
        .and_then(|value| value.as_str())
        .and_then(|intent| match intent {
            "retrieval" => Some(Intent::Retrieval),
            "chat" => Some(Intent::Chat),
            _ => None,
        })
}

/// 构建追问改写 prompt（极简，0.6B 友好）：输入按「history」与「question」
/// 两个字段分开给模型——history 是最近 5+5 段历史（仅作理解上文的参考，
/// 明确标注"不是改写对象、严禁复读"），question 是用户当前刚说的那句
/// （唯一改写对象）。输出约束为行式——每行一个可直接检索的中文问题。
/// 0.6B 对行式续写比 JSON 数组格式容错更高。三类改写情形（指代补全/多问题
/// 拆分/原样）各给一个示例——示例复读是 0.6B 的强项，光讲规则它做不到。
pub fn query_rewrite_prompt(question: &str, history: &[AskMessage]) -> (String, String) {
    let system = "你是问题改写助手。把字段 question 中的用户输入改写成能直接查资料的中文问题，一行一个，只输出问题，不要解释或多余文字。"
        .into();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!(
            "【历史记录】（字段 history，最近 5 条对话，仅作理解上文的参考，不是改写对象，严禁复读或引用其中的任何内容）：\n{folded}\n\n"
        ));
    }
    user.push_str(
        "【规则】（只改写字段 question，history 只是参考）：\n\
         - 一句话包含多个问题 → 每个问题各写一行\n\
         - 问题指代了上文（如「那金额上限呢」）→ 补全成完整问题\n\
         - 问题已经清楚明确 → 原样写一遍\n\
         - 只输出问题行，不要解释\n\n\
         【示例】\n\
         用户：那金额上限呢？\n\
         改写：报销金额的上限是多少？\n\n\
         用户：讲讲报销流程和请假规定\n\
         改写：报销流程是什么？\n\
         请假规定有哪些？\n\n\
         用户：去年的财务报表数据\n\
         改写：去年的财务报表数据\n\n",
    );
    user.push_str(&format!(
        "【当前输入】（字段 question，用户刚刚说的话，只改写这一条，不要复读历史里的任何句子）：\n{}\n\n改写：",
        question.trim()
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
            .trim_start_matches(['-', '•', '·', '#', ' ', '\t'])
            .trim_start_matches(|ch: char| ch.is_ascii_digit())
            .trim_start_matches(['.', '、', ')', '）', ' ']);
        // 0.6B 复读「改写：」前缀时剥掉，只留问题本身
        let line = line
            .strip_prefix("改写：")
            .or_else(|| line.strip_prefix("改写:"))
            .unwrap_or(line)
            .trim();
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

/// 闲聊 persona 的补全 prompt（纯补全非 JSON）：人设 + 回答规则 + 对话历史折叠
/// + 当前问题。历史格式与 generation_prompt 一致（「用户：/翻翻：」，每条约 400
///   字），最多折叠 5 轮（5 条用户 + 5 条模型回复）。
///
/// 回答规则分两支（Phase 4.1 技术问答质量修复，CASE 二）：
/// - 日常闲聊：简短自然、不复读问题、不以「你好！我是翻翻」固定开场；
/// - 知识/技术问题：定义 → 核心机制 → 适用场景/区别 的结构化回答，不编造人名/
///   年份/出处/数字，不牺牲准确性迁就口语化，知识不足时明确报告模型能力限制。
///
/// 两套规则常驻 system prompt，不依赖关键词判定（闲聊与科普本来就无法硬切）。
pub fn chat_prompt(request: &AskRequest, history: &[AskMessage]) -> (String, String) {
    let system = "你是翻翻，用户本地资料库中的中文智能助手。当前是闲聊场景，不涉及资料检索，直接自然对话即可。\
\n\n回答规则：\
\n1. 日常闲聊（寒暄、问候、闲聊话题）：简短、自然、口语化，直接回答，不要重复用户的话，不要以「你好！我是翻翻」之类的固定开场白开头。\
\n2. 知识或技术问题（例如「Transformer 是什么」「RAG 和微调有什么区别」「帮我解释 LangGraph」）：\
\n   - 按「定义 → 核心机制 → 适用场景或区别」的结构直接讲清楚概念本身，先给定义再展开；\
\n   - 可以用类比帮助理解，但不要为了口语化牺牲准确性；\
\n   - 不编造人名、年份、出处、数字、机构等事实细节，不确定的细节宁可不说；\
\n   - 对不确定的专有框架/库/工具（尤其是较新的开源项目），宁可简短说明「我对它不够熟悉，可能存在偏差」，也不要编造定义或机制；\
\n   - 知识超出能力范围时，明确回答「我的知识有限/这部分超出了我的模型能力范围」，不要强行编造。\
\n3. 回答保持简洁，直接回答问题，不要东拉西扯。".to_owned();
    let mut user = String::new();
    let folded = fold_recent_history(history, 5, 5);
    if !folded.is_empty() {
        user.push_str(&format!("【对话历史】\n{folded}\n\n"));
    }
    user.push_str("【当前问题】\n");
    user.push_str(&format!("用户：{}", request.question.trim()));
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
    result.answer_mode = AnswerMode::Unverified;
    result.elapsed_ms = elapsed_ms;
    result.degradation_reason = Some("本次生成结果未通过核验，置信度较低".to_owned());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Availability, SearchChannelState, SearchChannels};

    #[test]
    fn answer_mode_serializes_as_snake_case_and_tolerates_legacy_values() {
        // 序列化：9 个变体 → snake_case
        let serialized = serde_json::to_string(&AnswerMode::Compare).unwrap();
        assert_eq!(serialized, "\"compare\"");
        assert_eq!(
            serde_json::to_string(&AnswerMode::Generated).unwrap(),
            "\"generated\""
        );
        // 历史数据兼容：旧内部标记 "extractive" → Generated
        let legacy: AnswerMode = serde_json::from_str("\"extractive\"").unwrap();
        assert_eq!(legacy, AnswerMode::Generated);
        // 未知值 → Generated（宽容兜底，读旧库不炸）
        let unknown: AnswerMode = serde_json::from_str("\"whatever\"").unwrap();
        assert_eq!(unknown, AnswerMode::Generated);
        // 每个变体的 parse_lenient / as_str 往返
        for mode in [
            AnswerMode::Generated,
            AnswerMode::Chat,
            AnswerMode::RagRefusal,
            AnswerMode::Unverified,
            AnswerMode::Clarification,
            AnswerMode::Summary,
            AnswerMode::Compare,
            AnswerMode::Extract,
            AnswerMode::Find,
        ] {
            assert_eq!(AnswerMode::parse_lenient(mode.as_str()), Some(mode));
        }
        assert_eq!(AnswerMode::parse_lenient("FIND"), Some(AnswerMode::Find));
        assert_eq!(AnswerMode::parse_lenient(""), None);
    }

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
            clarification_selection: None,
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
        assert_eq!(coexistence.answer_mode, AnswerMode::Generated);
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
    }

    #[test]
    fn query_rewrite_prompt_injects_history_and_instructs_line_output() {
        let history = vec![
            message("user", "报销流程是什么"),
            message("assistant", "报销分三步。"),
        ];
        let (system, user) = query_rewrite_prompt("那金额上限呢", &history);
        assert!(system.contains("只输出问题"));
        assert!(system.contains("字段 question"));
        // history 与 question 两个字段分开，history 明确标注不是改写对象
        assert!(user.contains("【历史记录】"));
        assert!(user.contains("字段 history"));
        assert!(user.contains("不是改写对象"));
        assert!(user.contains("严禁复读"));
        assert!(user.contains("【当前输入】"));
        assert!(user.contains("字段 question"));
        assert!(user.contains("用户刚刚说的话"));
        assert!(user.contains("每个问题各写一行"));
        assert!(user.contains("那金额上限呢"));
        assert!(user.contains("补全成完整问题"));
        // 三类示例齐全（指代补全/多问题拆分/原样）——0.6B 靠示例复读而非读规则
        assert!(user.contains("报销金额的上限是多少？"));
        assert!(user.contains("讲讲报销流程和请假规定"));
        // 无历史时不带历史段
        let (_, user2) = query_rewrite_prompt("报销流程是什么", &[]);
        assert!(!user2.contains("【历史记录】"));
        assert!(user2.contains("原样写一遍"));
    }

    #[test]
    fn parse_rewritten_queries_splits_lines_and_dedupes() {
        assert_eq!(
            parse_rewritten_queries("公司的报销流程是什么\n请假规定有哪些"),
            vec![
                "公司的报销流程是什么".to_owned(),
                "请假规定有哪些".to_owned()
            ]
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
        assert_eq!(
            parse_rewritten_queries("## 标题行"),
            vec!["标题行".to_owned()]
        );
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
        let history = vec![
            message("user", "  你好  呀 "),
            message("assistant", "你好！"),
        ];
        let (system, user) = chat_prompt(&request(), &history);
        assert!(system.contains("翻翻"));
        assert!(user.contains("用户：你好 呀"));
        assert!(user.contains("翻翻：你好！"));
        assert!(user.contains("用户：项目如何优化召回率？"));
        // 无历史时不出现历史段
        let (_, user2) = chat_prompt(&request(), &[]);
        assert!(!user2.contains("对话历史"));
        assert!(user2.starts_with("【当前问题】"));
    }

    #[test]
    fn chat_prompt_has_technical_qa_rules() {
        // CASE 二（GENERAL 质量）：技术问答必须有结构纪律与反编造约束，
        // 且固定开场白仍被禁止；闲聊分支保持口语化短答
        let (system, _) = chat_prompt(&request(), &[]);
        assert!(
            system.contains("定义 → 核心机制 → 适用场景或区别"),
            "技术问答结构"
        );
        assert!(system.contains("不编造人名、年份、出处"), "反编造约束");
        assert!(system.contains("不要为了口语化牺牲准确性"), "准确性优先");
        assert!(
            system.contains("不要以「你好！我是翻翻」之类的固定开场白开头"),
            "禁固定开场"
        );
        assert!(system.contains("模型能力范围"), "能力限制要明说");
        assert!(system.contains("简短、自然、口语化"), "闲聊分支保留");
        // 模型必须看到「Transformer 是什么」这类问题属于知识问题
        assert!(system.contains("Transformer 是什么"));
        assert!(system.contains("RAG 和微调有什么区别"));
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
        let user_lines = user
            .lines()
            .filter(|line| line.starts_with("用户："))
            .count();
        let assistant_lines = user
            .lines()
            .filter(|line| line.starts_with("翻翻："))
            .count();
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
        // 5+5 历史段（【对话历史】标记，明确不是当前输入）+ 当前问题（【当前问题】标记）
        assert!(user.contains("【对话历史】"));
        assert!(user.contains("不是用户现在说的话"));
        assert!(user.contains("用户：你好"));
        assert!(user.contains("翻翻：你好！"));
        assert!(user.contains("【当前问题】"));
        assert!(user.contains("只根据这一句判断意图"));
        // JSON 输出约束与兜底句
        assert!(user.contains(r#"{"intent":"retrieval"}"#));
        assert!(user.contains(r#"{"intent":"chat"}"#));
        assert!(user.contains("宁可闲聊也绝不误检索"));
        assert!(user.contains("用户说：报销流程是什么"));
    }

    #[test]
    fn intent_routing_prompt_empty_history_has_no_history_section() {
        let (_, user) = intent_routing_prompt("今天天气怎么样", &[]);
        // 历史段头部只在有历史时出现（【对话历史】为该段独有的标记）
        assert!(!user.contains("【对话历史】"));
        assert!(!user.contains("延续上文"));
        assert!(user.contains("【当前问题】"));
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
        let draft =
            r#"{"claims":[{"text":"报销上限是5000元","citation_ids":["S1"]}],"refusal":null}"#;
        assert_eq!(extract_unverified_text(draft), "- 报销上限是5000元");
        // 带 [S\d+] 标记的正文被剥离
        let dirty =
            r#"{"claims":[{"text":"规定如下[S1][S2]","citation_ids":["S1"]}],"refusal":null}"#;
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
        assert_eq!(degraded.answer_mode, AnswerMode::Unverified);
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
