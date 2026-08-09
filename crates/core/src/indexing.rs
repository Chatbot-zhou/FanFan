use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{AppError, FileRecord, ScopeFilter, SourceLocator};

pub const INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseRequest {
    pub job_id: Uuid,
    pub file_id: Uuid,
    pub revision_id: Uuid,
    pub source_path: String,
    pub format: String,
    pub ocr_policy: String,
    pub language_hints: Vec<String>,
    pub max_pages: Option<u32>,
    pub parser_version: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParseOutcome {
    Parsed,
    Partial,
    Encrypted,
    Unsupported,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParseWarning {
    pub code: String,
    pub message: String,
    pub locator: Option<SourceLocator>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseMetrics {
    pub page_count: u64,
    pub node_count: u64,
    pub character_count: u64,
    pub ocr_page_count: u64,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentNode {
    pub node_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub ordinal: u64,
    pub node_type: String,
    pub text: Option<String>,
    pub table_data: Option<Value>,
    pub locator: SourceLocator,
    pub heading_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParseResult {
    pub revision_id: Uuid,
    pub status: ParseOutcome,
    pub parser_name: String,
    pub parser_version: String,
    pub nodes: Vec<DocumentNode>,
    pub warnings: Vec<ParseWarning>,
    pub metrics: ParseMetrics,
    pub error: Option<AppError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkRecord {
    pub chunk_id: Uuid,
    pub revision_id: Uuid,
    pub node_id: Uuid,
    pub ordinal: u64,
    pub text: String,
    pub normalized_text: String,
    pub token_count: u64,
    pub content_hash: String,
    pub language: String,
    pub locator: SourceLocator,
    pub embedding_model_id: Option<String>,
    pub embedding_status: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Filename,
    Fulltext,
    Semantic,
    Hybrid,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    Relevance,
    ModifiedDesc,
    NameAsc,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchRequest {
    pub query: String,
    pub scope: ScopeFilter,
    pub mode: SearchMode,
    pub sort: SearchSort,
    pub page_size: u32,
    pub cursor: Option<String>,
}

impl SearchRequest {
    pub fn validate(&self) -> Result<(), AppError> {
        let query = self.query.trim();
        if query.is_empty() || query.chars().count() > 500 {
            return Err(AppError::new(
                "SEARCH_QUERY_INVALID",
                "搜索内容需要保持在1到500个字符之间",
                false,
            ));
        }
        if !(10..=100).contains(&self.page_size) {
            return Err(AppError::new(
                "SEARCH_PAGE_SIZE_INVALID",
                "每页结果数量需要保持在10到100之间",
                false,
            ));
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.is_empty() || cursor.len() > 160 || !cursor.is_ascii())
        {
            return Err(AppError::new(
                "SEARCH_CURSOR_INVALID",
                "搜索分页游标格式无效，请重新搜索",
                false,
            ));
        }
        Ok(())
    }
}

pub(crate) fn encode_search_cursor(offset: usize, fingerprint: &str) -> String {
    format!("v1:{offset}:{fingerprint}")
}

pub(crate) fn decode_search_cursor(
    cursor: Option<&str>,
    expected_fingerprint: &str,
) -> Result<usize, AppError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    let mut parts = cursor.split(':');
    let version = parts.next();
    let offset = parts.next().and_then(|value| value.parse::<usize>().ok());
    let fingerprint = parts.next();
    if version != Some("v1")
        || parts.next().is_some()
        || offset.is_none_or(|value| value > 100_000)
        || fingerprint != Some(expected_fingerprint)
    {
        return Err(AppError::new(
            "SEARCH_CURSOR_INVALID",
            "资料索引或搜索条件已经变化，请重新搜索",
            false,
        ));
    }
    Ok(offset.expect("validated search offset"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchScore {
    pub filename: Option<f32>,
    pub fulltext: Option<f32>,
    pub semantic: Option<f32>,
    pub fused: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResult {
    pub file_id: Uuid,
    pub name: String,
    pub extension: String,
    #[serde(
        rename(serialize = "display_path"),
        alias = "display_path",
        serialize_with = "crate::serialize_display_path"
    )]
    pub path: String,
    pub modified_at: DateTime<Utc>,
    pub snippet: String,
    pub match_reasons: Vec<String>,
    pub locator: Option<SourceLocator>,
    pub revision_id: Option<Uuid>,
    pub scores: SearchScore,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchChannelState {
    Pending,
    Completed,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchChannels {
    pub filename: SearchChannelState,
    pub fulltext: SearchChannelState,
    pub semantic: SearchChannelState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchSession {
    pub search_id: Uuid,
    pub status: String,
    pub channels: SearchChannels,
    pub results: Vec<SearchResult>,
    pub next_cursor: Option<String>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilePreview {
    pub file: FileRecord,
    pub revision_id: Option<Uuid>,
    pub nodes: Vec<DocumentNode>,
    pub offset: u32,
    pub next_offset: Option<u32>,
    pub anchor_node_id: Option<Uuid>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingEmbeddingChunk {
    pub chunk_id: Uuid,
    pub file_id: Uuid,
    pub revision_id: Uuid,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkEmbeddingInput {
    pub chunk_id: Uuid,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticQuery<'a> {
    pub model_artifact_id: &'a str,
    pub vector: &'a [f32],
}

#[derive(Debug, Clone)]
pub(crate) struct RankedHit {
    pub file: FileRecord,
    pub revision_id: Option<Uuid>,
    pub snippet: String,
    pub locator: Option<SourceLocator>,
    pub reason: &'static str,
    pub channel_score: f32,
}

pub(crate) fn fuse_ranked_hits(
    channels: &[Vec<RankedHit>],
    sort: SearchSort,
    page_size: usize,
    started_at: Instant,
) -> SearchSession {
    let mut fused: HashMap<Uuid, SearchResult> = HashMap::new();
    for channel in channels {
        for (rank, hit) in channel.iter().enumerate() {
            let reciprocal_rank = 1.0 / (60.0 + rank as f32 + 1.0);
            let entry = fused
                .entry(hit.file.file_id)
                .or_insert_with(|| SearchResult {
                    file_id: hit.file.file_id,
                    name: hit.file.display_name.clone(),
                    extension: hit.file.extension.clone(),
                    path: hit.file.canonical_path.clone(),
                    modified_at: hit.file.fs_modified_at,
                    snippet: hit.snippet.clone(),
                    match_reasons: Vec::new(),
                    locator: hit.locator.clone(),
                    revision_id: hit.revision_id,
                    scores: SearchScore {
                        filename: None,
                        fulltext: None,
                        semantic: None,
                        fused: 0.0,
                    },
                });
            entry.scores.fused += reciprocal_rank;
            if !entry
                .match_reasons
                .iter()
                .any(|reason| reason == hit.reason)
            {
                entry.match_reasons.push(hit.reason.to_owned());
            }
            match hit.reason {
                "filename" | "path" => {
                    entry.scores.filename = Some(
                        entry
                            .scores
                            .filename
                            .unwrap_or_default()
                            .max(hit.channel_score),
                    );
                }
                "fulltext" => {
                    entry.scores.fulltext = Some(
                        entry
                            .scores
                            .fulltext
                            .unwrap_or_default()
                            .max(hit.channel_score),
                    );
                    if entry.locator.is_none() {
                        entry.locator = hit.locator.clone();
                        entry.snippet.clone_from(&hit.snippet);
                    }
                }
                "semantic" => {
                    entry.scores.semantic = Some(
                        entry
                            .scores
                            .semantic
                            .unwrap_or_default()
                            .max(hit.channel_score),
                    );
                    if entry.locator.is_none() {
                        entry.locator = hit.locator.clone();
                        entry.snippet.clone_from(&hit.snippet);
                    }
                }
                _ => {}
            }
        }
    }
    let mut results = fused.into_values().collect::<Vec<_>>();
    match sort {
        SearchSort::Relevance => results.sort_by(|left, right| {
            right
                .scores
                .fused
                .total_cmp(&left.scores.fused)
                .then_with(|| right.modified_at.cmp(&left.modified_at))
        }),
        SearchSort::ModifiedDesc => {
            results.sort_by_key(|result| std::cmp::Reverse(result.modified_at))
        }
        SearchSort::NameAsc => results.sort_by(|left, right| left.name.cmp(&right.name)),
    }
    results.truncate(page_size);
    SearchSession {
        search_id: Uuid::now_v7(),
        status: "completed".to_owned(),
        channels: SearchChannels {
            filename: SearchChannelState::Completed,
            fulltext: SearchChannelState::Completed,
            semantic: SearchChannelState::Unavailable,
        },
        results,
        next_cursor: None,
        elapsed_ms: started_at.elapsed().as_millis() as u64,
    }
}

pub fn chunks_from_nodes(result: &ParseResult) -> Vec<ChunkRecord> {
    let mut chunks = Vec::new();
    let mut ordinal = 0_u64;
    for node in &result.nodes {
        let source_text = node.text.clone().or_else(|| {
            node.table_data
                .as_ref()
                .and_then(|value| serde_json::to_string(value).ok())
        });
        let Some(source_text) = source_text.filter(|value| !value.trim().is_empty()) else {
            continue;
        };
        for text in split_text(&source_text, 450, 50) {
            ordinal += 1;
            let normalized_text = normalize_for_fts(&text);
            chunks.push(ChunkRecord {
                chunk_id: Uuid::now_v7(),
                revision_id: result.revision_id,
                node_id: node.node_id,
                ordinal,
                token_count: normalized_text.split_whitespace().count() as u64,
                content_hash: stable_content_hash(&text),
                language: detect_language(&text).to_owned(),
                text,
                normalized_text,
                locator: node.locator.clone(),
                embedding_model_id: None,
                embedding_status: "pending".to_owned(),
            });
        }
    }
    chunks
}

fn split_text(text: &str, target_chars: usize, overlap_chars: usize) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    if characters.len() <= target_chars {
        return vec![text.trim().to_owned()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let end = (start + target_chars).min(characters.len());
        let chunk = characters[start..end]
            .iter()
            .collect::<String>()
            .trim()
            .to_owned();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        if end == characters.len() {
            break;
        }
        start = end.saturating_sub(overlap_chars);
    }
    chunks
}

pub fn normalize_for_fts(text: &str) -> String {
    let mut tokens = Vec::new();
    let mut han_run = Vec::new();
    let mut latin_run = String::new();
    let flush_han = |run: &mut Vec<char>, tokens: &mut Vec<String>| {
        tokens.extend(run.iter().map(char::to_string));
        tokens.extend(run.windows(2).map(|pair| pair.iter().collect::<String>()));
        run.clear();
    };
    let flush_latin = |run: &mut String, tokens: &mut Vec<String>| {
        if !run.is_empty() {
            tokens.push(run.to_lowercase());
            run.clear();
        }
    };
    for character in text.chars() {
        if is_han(character) {
            flush_latin(&mut latin_run, &mut tokens);
            han_run.push(character);
        } else if character.is_alphanumeric() {
            flush_han(&mut han_run, &mut tokens);
            latin_run.push(character);
        } else {
            flush_han(&mut han_run, &mut tokens);
            flush_latin(&mut latin_run, &mut tokens);
        }
    }
    flush_han(&mut han_run, &mut tokens);
    flush_latin(&mut latin_run, &mut tokens);
    tokens.join(" ")
}

pub(crate) fn fts_query(text: &str) -> String {
    let normalized = normalize_for_fts(text);
    let mut tokens = normalized
        .split_whitespace()
        .filter(|token| token.chars().count() > 1 || text.chars().count() == 1)
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        tokens = normalized
            .split_whitespace()
            .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
            .collect();
    }
    tokens.join(" OR ")
}

fn is_han(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

fn detect_language(text: &str) -> &'static str {
    let han = text.chars().filter(|character| is_han(*character)).count();
    let latin = text
        .chars()
        .filter(|character| character.is_ascii_alphabetic())
        .count();
    match (han > 0, latin > 0) {
        (true, true) => "mixed",
        (true, false) => "zh",
        (false, true) => "en",
        _ => "unknown",
    }
}

fn stable_content_hash(text: &str) -> String {
    let mut hasher = std::hash::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourceKind;

    #[test]
    fn chinese_normalization_emits_characters_and_bigrams() {
        let normalized = normalize_for_fts("归航计划 RRF-60");
        assert!(normalized.contains("归航"));
        assert!(normalized.contains("计划"));
        assert!(normalized.contains("rrf"));
        assert!(fts_query("归航计划").contains("\"归航\" OR \"航计\""));
    }

    #[test]
    fn search_cursor_is_bound_to_the_same_index_snapshot() {
        let cursor = encode_search_cursor(30, "snapshot-a");
        assert_eq!(
            decode_search_cursor(Some(&cursor), "snapshot-a").expect("decode cursor"),
            30
        );
        assert_eq!(
            decode_search_cursor(Some(&cursor), "snapshot-b")
                .expect_err("reject stale cursor")
                .code,
            "SEARCH_CURSOR_INVALID"
        );
        assert_eq!(
            decode_search_cursor(Some("v1:100001:snapshot-a"), "snapshot-a")
                .expect_err("reject excessive offset")
                .code,
            "SEARCH_CURSOR_INVALID"
        );
    }

    #[test]
    fn chunks_keep_their_source_locator() {
        let result = ParseResult {
            revision_id: Uuid::now_v7(),
            status: ParseOutcome::Parsed,
            parser_name: "test".into(),
            parser_version: "1".into(),
            nodes: vec![DocumentNode {
                node_id: Uuid::now_v7(),
                parent_id: None,
                ordinal: 1,
                node_type: "paragraph".into(),
                text: Some("归航计划".repeat(150)),
                table_data: None,
                locator: SourceLocator {
                    kind: SourceKind::Docx,
                    paragraph_no: Some(3),
                    ..SourceLocator::default()
                },
                heading_path: vec!["检索".into()],
            }],
            warnings: vec![],
            metrics: ParseMetrics {
                page_count: 0,
                node_count: 1,
                character_count: 600,
                ocr_page_count: 0,
                elapsed_ms: 1,
            },
            error: None,
        };
        let chunks = chunks_from_nodes(&result);
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.locator.paragraph_no == Some(3))
        );
    }
}
