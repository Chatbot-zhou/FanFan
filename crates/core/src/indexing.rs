use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{AppError, FileRecord, ScopeFilter, SourceLocator};

pub const INDEX_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OcrRuntimeConfig {
    pub model_path: String,
    pub det_model_path: String,
    pub cls_model_path: String,
    pub dictionary_path: String,
    pub threads: u32,
    /// OCR 版本形态：`PPOCRV5` / `PPOCRV6`；缺省按 PPOCRV5 处理。
    #[serde(default = "default_ocr_version")]
    pub ocr_version: String,
}

/// OCR 运行时配置的缺省版本（兼容未携带该字段的旧数据）。
fn default_ocr_version() -> String {
    "PPOCRV5".to_owned()
}

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
    #[serde(default)]
    pub asset_cache_dir: Option<String>,
    #[serde(default)]
    pub ocr_runtime: Option<OcrRuntimeConfig>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OcrAttempt {
    pub engine: String,
    pub model_version: Option<String>,
    pub status: String,
    pub page_no: Option<u32>,
    pub confidence: Option<f32>,
    pub fallback_reason: Option<String>,
    pub elapsed_ms: u64,
    pub error: Option<AppError>,
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
pub struct ImageAsset {
    pub asset_id: Uuid,
    pub revision_id: Uuid,
    pub asset_kind: String,
    #[serde(default, skip_serializing)]
    pub cache_path: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub locator: SourceLocator,
    pub ocr_text: Option<String>,
    #[serde(default)]
    pub ocr_confidence: Option<f32>,
    #[serde(default)]
    pub ocr_engine: Option<String>,
    pub description: Option<String>,
    pub vision_model_id: Option<String>,
    #[serde(default)]
    pub vision_route_reason: Option<String>,
    pub status: String,
    #[serde(default)]
    pub error: Option<AppError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingImageOcr {
    pub asset_id: Uuid,
    pub file_id: Uuid,
    pub revision_id: Uuid,
    #[serde(skip_serializing)]
    pub cache_path: String,
    pub mime_type: String,
    pub asset_kind: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub locator: SourceLocator,
    pub attempt_count: u32,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageOcrResult {
    pub asset_id: Uuid,
    pub revision_id: Uuid,
    pub model_artifact_id: String,
    #[serde(default)]
    pub ocr_text: Option<String>,
    #[serde(default)]
    pub confidence: Option<f32>,
    pub engine: String,
    #[serde(default)]
    pub model_version: Option<String>,
    pub vision_required: bool,
    pub route_reason: String,
    #[serde(default)]
    pub attempts: Vec<OcrAttempt>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PendingImageUnderstanding {
    pub asset_id: Uuid,
    pub file_id: Uuid,
    pub revision_id: Uuid,
    #[serde(skip_serializing)]
    pub cache_path: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub locator: SourceLocator,
    pub ocr_text: Option<String>,
    pub attempt_count: u32,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImageUnderstandingResult {
    pub asset_id: Uuid,
    pub revision_id: Uuid,
    pub model_artifact_id: String,
    pub summary: String,
    #[serde(default)]
    pub visible_text: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub entities: Vec<String>,
    #[serde(default)]
    pub chart_summary: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ParseResult {
    pub revision_id: Uuid,
    pub status: ParseOutcome,
    pub parser_name: String,
    pub parser_version: String,
    pub nodes: Vec<DocumentNode>,
    #[serde(default)]
    pub image_assets: Vec<ImageAsset>,
    #[serde(default)]
    pub ocr_attempts: Vec<OcrAttempt>,
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

pub type ContentChunk = ChunkRecord;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Filename,
    Fulltext,
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
    #[serde(default)]
    pub image_asset_id: Option<Uuid>,
    pub scores: SearchScore,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalCandidate {
    pub file_id: Uuid,
    pub chunk_id: Option<Uuid>,
    pub revision_id: Option<Uuid>,
    #[serde(default)]
    pub image_asset_id: Option<Uuid>,
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
    pub locator: Option<SourceLocator>,
    pub match_reasons: Vec<String>,
    pub scores: SearchScore,
    pub rank: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalTrace {
    pub retrieval_id: Uuid,
    pub channels: SearchChannels,
    pub candidates: Vec<RetrievalCandidate>,
    pub elapsed_ms: u64,
}

/// Search may surface weak nearest neighbours so the user can continue to
/// explore, but strict RAG must not promote every ANN result to evidence. The
/// persisted semantic score is cosine similarity mapped from `[-1, 1]` to
/// `[0, 1]`; 0.70 therefore represents an original cosine similarity of 0.40.
/// Lexical hits use the normalized BM25 score produced by `search_fulltext`.
///
/// `fts_query` expands tokens with OR, so a query that shares a common word
/// with many chunks (or a gibberish string that happens to split into common
/// tokens) gets large BM25 hits from weak single-token matches. Those weak
/// matches must not become evidence: fulltext-only evidence requires the
/// semantic channel to also support the candidate (>= 0.60, cosine >= 0.20),
/// while the semantic channel alone still promotes strong neighbours (>= 0.70,
/// cosine >= 0.40).
pub(crate) const RAG_MIN_SEMANTIC_SCORE: f32 = 0.70;
/// 融合排序时每个文件最多参与 RRF 分数累加的内容 chunk 数。
const MAX_FUSED_CHUNKS_PER_FILE: usize = 3;
/// 文件名/路径命中（「找文件」类查询的最强信号）在文件级排序时的保底加分，
/// 保证语义噪声堆叠的大文档不会把真正目标文件挤出前列。
const FILENAME_MATCH_BOOST: f32 = 2.0;
/// MMR 去重参与候选中按融合分截断到 limit 的倍数：原实现对全部候选做
/// O(N^2) 的 token 冗余比较，长文本语料下单次搜索可达数十秒（实测 41s），
/// 截断只排除低分候选，不影响高分去重结果。
const MMR_CANDIDATE_FACTOR: usize = 3;
/// token_jaccard 每次比较参与计算的 token 上限：MMR 冗余度只关心内容
/// 是否重复，截断超长 chunk 的尾部 token 可把单次比较成本降为常数。
const MMR_TOKEN_JACCARD_MAX_TOKENS: usize = 256;
pub(crate) const RAG_MIN_SEMANTIC_SUPPORT: f32 = 0.60;
pub(crate) const RAG_MIN_FULLTEXT_SCORE: f32 = 0.05;

/// 查询级整体相关性门槛：乱码/无关查询的 BGE 向量落在模型先验方向上，
/// 语义 top-1 虚高。2026-08-14 用 eval/calibrate_query_gate.py 对
/// qa-fixtures 38 道正例 + 13 道寒暄/乱码负例做全量 197,601 块余弦扫描标定：
/// - 正例 top-1 分布 0.784~0.912，负例 0.768~0.803，二者重叠，margin
///   无区分力（正负例均 ≤0.032）——因此只保留纯 top-1 门槛 0.80；
/// - 该门槛放行 35/38 正例（3 道困难查询 top-1 0.784~0.797 且 fulltext
///   也弱，无法救），拒绝 12/13 负例（唯一误放的「你好」0.803 在路由层
///   被 LLM 直路由判为闲聊，到不了检索）；
/// - 0.87（含 margin 分支）在此标定集上误杀 34/38 正例，已废弃。
///
/// fulltext 通道单独不设低门槛放行——部分 token 命中（bm25 中等负数）在
/// 乱码查询上也会出现（实测 0.09-0.22），只有罕见词全部命中的极强全文
/// 证据（>=0.30）才可作为查询相关的唯一依据（正例实测最高 0.171，正常
/// 查询不会触发该分支）。
pub(crate) const RAG_QUERY_RELEVANT_SEMANTIC: f32 = 0.80;
pub(crate) const RAG_QUERY_RELEVANT_FULLTEXT: f32 = 0.30;

/// 查询级证据存在性判断：候选里最高的语义分（或极高的全文命中）达到
/// 门槛才认为库里有与查询相关的内容。
pub(crate) fn query_has_relevant_evidence(candidates: &[RetrievalCandidate]) -> bool {
    let mut top_semantic = f32::NEG_INFINITY;
    let mut top_fulltext = f32::NEG_INFINITY;
    for candidate in candidates {
        if let Some(score) = candidate.scores.semantic {
            top_semantic = top_semantic.max(score);
        }
        if let Some(score) = candidate.scores.fulltext {
            top_fulltext = top_fulltext.max(score);
        }
    }
    top_semantic >= RAG_QUERY_RELEVANT_SEMANTIC || top_fulltext >= RAG_QUERY_RELEVANT_FULLTEXT
}

/// Whether this candidate may serve as RAG evidence, given whether the
/// semantic engine is available for this query (`semantic_available`). Without
/// a semantic engine the fulltext channel is the only retrieval path, so the
/// plain fulltext floor applies.
pub(crate) fn candidate_is_relevant_rag_evidence(
    candidate: &RetrievalCandidate,
    semantic_available: bool,
) -> bool {
    if !semantic_available {
        return candidate
            .scores
            .fulltext
            .is_some_and(|score| score >= RAG_MIN_FULLTEXT_SCORE);
    }
    candidate
        .scores
        .semantic
        .is_some_and(|score| score >= RAG_MIN_SEMANTIC_SCORE)
        || (candidate
            .scores
            .semantic
            .is_some_and(|score| score >= RAG_MIN_SEMANTIC_SUPPORT)
            && candidate
                .scores
                .fulltext
                .is_some_and(|score| score >= RAG_MIN_FULLTEXT_SCORE))
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
    #[serde(default)]
    pub image_assets: Vec<ImageAsset>,
    #[serde(default)]
    pub ocr_attempts: Vec<OcrAttempt>,
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
    pub chunk_id: Option<Uuid>,
    pub revision_id: Option<Uuid>,
    pub image_asset_id: Option<Uuid>,
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
    let candidates =
        fuse_retrieval_candidates(channels, page_size.saturating_mul(4).max(page_size));
    let mut by_file = HashMap::<Uuid, SearchResult>::new();
    for candidate in candidates {
        by_file
            .entry(candidate.file_id)
            .or_insert_with(|| SearchResult {
                file_id: candidate.file_id,
                name: candidate.name,
                extension: candidate.extension,
                path: candidate.path,
                modified_at: candidate.modified_at,
                snippet: candidate.snippet,
                match_reasons: candidate.match_reasons,
                locator: candidate.locator,
                revision_id: candidate.revision_id,
                image_asset_id: candidate.image_asset_id,
                scores: candidate.scores,
            });
    }
    let mut results = by_file.into_values().collect::<Vec<_>>();
    for result in &mut results {
        if result
            .match_reasons
            .iter()
            .any(|reason| matches!(reason.as_str(), "filename" | "path"))
        {
            result.scores.fused += FILENAME_MATCH_BOOST;
        }
    }
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

pub(crate) fn fuse_retrieval_candidates(
    channels: &[Vec<RankedHit>],
    limit: usize,
) -> Vec<RetrievalCandidate> {
    let mut fused: HashMap<(Uuid, Option<Uuid>), RetrievalCandidate> = HashMap::new();
    // 每文件最多让 MAX_FUSED_CHUNKS_PER_FILE 个内容 chunk 参与 RRF 累加：
    // 防止大文档（论文/报告）靠 chunk 数量刷分霸榜，单次强命中的小文件被淹没。
    let mut per_file_chunk_count = HashMap::<Uuid, usize>::new();
    for channel in channels {
        for (rank, hit) in channel.iter().enumerate() {
            let weight = match hit.reason {
                "filename" | "path" => 1.3,
                "semantic" => 1.15,
                "fulltext" => 1.0,
                _ => 0.7,
            };
            let reciprocal_rank = weight / (60.0 + rank as f32 + 1.0);
            let fused_key = (hit.file.file_id, hit.chunk_id);
            let is_new_chunk = !fused.contains_key(&fused_key);
            let mut accumulate_rank = true;
            if is_new_chunk && hit.chunk_id.is_some() {
                let count = per_file_chunk_count.entry(hit.file.file_id).or_insert(0);
                if *count >= MAX_FUSED_CHUNKS_PER_FILE {
                    accumulate_rank = false;
                } else {
                    *count += 1;
                }
            }
            let entry = fused
                .entry(fused_key)
                .or_insert_with(|| RetrievalCandidate {
                    file_id: hit.file.file_id,
                    chunk_id: hit.chunk_id,
                    revision_id: hit.revision_id,
                    image_asset_id: hit.image_asset_id,
                    name: hit.file.display_name.clone(),
                    extension: hit.file.extension.clone(),
                    path: hit.file.canonical_path.clone(),
                    modified_at: hit.file.fs_modified_at,
                    snippet: hit.snippet.clone(),
                    match_reasons: Vec::new(),
                    locator: hit.locator.clone(),
                    scores: SearchScore {
                        filename: None,
                        fulltext: None,
                        semantic: None,
                        fused: 0.0,
                    },
                    rank: 0,
                });
            if accumulate_rank {
                entry.scores.fused += reciprocal_rank;
            }
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
                    if entry.locator.is_none() || entry.chunk_id.is_none() {
                        entry.chunk_id = hit.chunk_id;
                        entry.image_asset_id = hit.image_asset_id;
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
                    if entry.locator.is_none() || entry.chunk_id.is_none() {
                        entry.chunk_id = hit.chunk_id;
                        entry.image_asset_id = hit.image_asset_id;
                        entry.locator = hit.locator.clone();
                        entry.snippet.clone_from(&hit.snippet);
                    }
                }
                _ => {}
            }
        }
    }
    let mut remaining = fused.into_values().collect::<Vec<_>>();
    remaining.sort_by(|left, right| right.scores.fused.total_cmp(&left.scores.fused));
    // MMR 去重对每个未选候选 × 每个已选候选做 token 冗余比较（O(N^2)）。
    // 只保留融合分最高的 limit × MMR_CANDIDATE_FACTOR 个候选参与去重：
    // 低分候选对最终 Top-limit 结果影响可忽略，却占去重比较的主体。
    remaining.truncate(
        remaining
            .len()
            .min(limit.saturating_mul(MMR_CANDIDATE_FACTOR)),
    );
    let max_relevance = remaining
        .first()
        .map(|candidate| candidate.scores.fused)
        .unwrap_or(1.0)
        .max(f32::EPSILON);
    // 预计算每个候选的 token 集合：原实现对同一 snippet 在每轮比较中反复
    // 重新 token 化，长文本场景成为单次搜索的主要耗时（实测 41s）。
    let mut token_sets = remaining
        .iter()
        .map(|candidate| snippet_token_set(&candidate.snippet))
        .collect::<Vec<_>>();
    let mut selected = Vec::new();
    // 每个候选相对「已选集合」的当前最大冗余度（增量维护）。原实现对每轮
    // 全部候选 × 全部已选重新做 token Jaccard（O(R×S²)，372×124² ≈ 286 万次），
    // 实测单次搜索的 fusion 阶段耗时 7~13 秒。这里缓存每个候选已累计的最大
    // 冗余度，每加入一个已选结果只对剩余候选各算一次 jaccard 并取 max
    // （结果与全量重算完全等价），总比较次数降为 O(R×S) ≈ 4.6 万次。
    let mut redundancy_cache = vec![0.0_f32; remaining.len()];
    while !remaining.is_empty() && selected.len() < limit {
        let (best_index, _) = remaining
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
                let relevance = candidate.scores.fused / max_relevance;
                (index, relevance * 0.78 - redundancy_cache[index] * 0.22)
            })
            .max_by(|left, right| left.1.total_cmp(&right.1))
            .expect("remaining candidates are not empty");
        let mut candidate = remaining.swap_remove(best_index);
        let candidate_tokens = token_sets.swap_remove(best_index);
        redundancy_cache.swap_remove(best_index);
        // 增量更新：新选中的候选对每个剩余候选的冗余度，只与新加入的已选
        // 结果比较一次，累积取最大值（同文件候选保底 0.2 与全量重算一致）。
        for (index, remaining_candidate) in remaining.iter().enumerate() {
            let lexical = token_set_jaccard(&token_sets[index], &candidate_tokens);
            let redundancy = if remaining_candidate.file_id == candidate.file_id {
                lexical.max(0.2)
            } else {
                lexical
            };
            if redundancy > redundancy_cache[index] {
                redundancy_cache[index] = redundancy;
            }
        }
        candidate.rank = selected.len() as u32 + 1;
        selected.push(candidate);
    }
    selected
}

/// 把 snippet 切成小写 token 集合，最多取 MMR_TOKEN_JACCARD_MAX_TOKENS 个：
/// 冗余度比较只关心「内容是否重复」，截断尾部可避免超长 chunk 拖慢比较。
fn snippet_token_set(snippet: &str) -> std::collections::HashSet<String> {
    snippet
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_ascii_lowercase)
        .take(MMR_TOKEN_JACCARD_MAX_TOKENS)
        .collect()
}

/// 基于预计算 token 集合的 Jaccard 相似度（MMR 冗余度信号）。
fn token_set_jaccard(
    left: &std::collections::HashSet<String>,
    right: &std::collections::HashSet<String>,
) -> f32 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(right).count() as f32;
    let union = left.union(right).count() as f32;
    intersection / union
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
        for text in split_text(&source_text, 384, 480, 64) {
            ordinal += 1;
            let normalized_text = normalize_for_fts(&text);
            chunks.push(ChunkRecord {
                chunk_id: Uuid::now_v7(),
                revision_id: result.revision_id,
                node_id: node.node_id,
                ordinal,
                token_count: estimated_token_count(&text),
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

fn split_text(
    text: &str,
    target_tokens: usize,
    max_tokens: usize,
    overlap_tokens: usize,
) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    if estimated_token_count(text) <= max_tokens as u64 {
        return vec![text.trim().to_owned()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let mut end = start;
        let mut weight = 0.0_f32;
        let mut preferred_end = None;
        while end < characters.len() && weight.ceil() < max_tokens as f32 {
            let next_weight = weight + token_weight(characters[end]);
            if next_weight.ceil() > max_tokens as f32 && end > start {
                break;
            }
            weight = next_weight;
            end += 1;
            if weight.ceil() >= target_tokens as f32 && is_chunk_boundary(characters[end - 1]) {
                preferred_end = Some(end);
                break;
            }
        }
        let end = preferred_end.unwrap_or(end).max(start + 1);
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
        let mut next_start = end;
        let mut overlap = 0.0_f32;
        while next_start > start && overlap.ceil() < overlap_tokens as f32 {
            next_start -= 1;
            overlap += token_weight(characters[next_start]);
        }
        start = next_start.max(start + 1);
    }
    chunks
}

fn estimated_token_count(text: &str) -> u64 {
    text.chars().map(token_weight).sum::<f32>().ceil().max(1.0) as u64
}

/// 按切块同一套估算权重截取前 `max_tokens` token 的文本。
/// 用于相邻块上下文：邻居块全文可能接近 480 token，注入答案前先压到
/// 固定上限，避免把生成上下文窗口撑爆。
pub(crate) fn cap_by_estimated_tokens(text: &str, max_tokens: u64) -> String {
    let mut weight = 0.0_f32;
    let mut length = 0;
    for (index, character) in text.chars().enumerate() {
        let next = weight + token_weight(character);
        if next.ceil() > max_tokens as f32 {
            break;
        }
        weight = next;
        length = index + 1;
    }
    text.chars().take(length).collect()
}

fn token_weight(value: char) -> f32 {
    if value.is_whitespace() {
        0.0
    } else if value.is_ascii_alphanumeric() {
        0.25
    } else {
        1.0
    }
}

fn is_chunk_boundary(value: char) -> bool {
    matches!(
        value,
        '\n' | '\r' | '。' | '！' | '？' | '；' | '.' | '!' | '?' | ';'
    )
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
    use crate::{Availability, ParseStatus, SourceKind};

    fn ranked_hit(
        file_id: Uuid,
        chunk_id: Uuid,
        reason: &'static str,
        score: f32,
        snippet: &str,
    ) -> RankedHit {
        let now = Utc::now();
        RankedHit {
            file: FileRecord {
                file_id,
                volume_id: "volume".into(),
                canonical_path: format!("资料\\{file_id}.txt"),
                display_name: format!("{file_id}.txt"),
                extension: "txt".into(),
                mime_type: "text/plain".into(),
                size_bytes: 1,
                fs_created_at: Some(now),
                fs_modified_at: now,
                windows_file_id: None,
                content_sha256: None,
                availability: Availability::Present,
                current_revision_id: Some(Uuid::now_v7()),
                parse_status: ParseStatus::Parsed,
                first_seen_at: now,
                last_seen_at: now,
            },
            chunk_id: Some(chunk_id),
            revision_id: Some(Uuid::now_v7()),
            image_asset_id: None,
            snippet: snippet.into(),
            locator: Some(SourceLocator::default()),
            reason,
            channel_score: score,
        }
    }

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
    fn strict_rag_rejects_weak_semantic_only_neighbours() {
        let weak = fuse_retrieval_candidates(
            &[vec![ranked_hit(
                Uuid::now_v7(),
                Uuid::now_v7(),
                "semantic",
                0.64,
                "unrelated nearest neighbour",
            )]],
            10,
        )
        .remove(0);
        assert!(!candidate_is_relevant_rag_evidence(&weak, true));

        let strong = fuse_retrieval_candidates(
            &[vec![ranked_hit(
                Uuid::now_v7(),
                Uuid::now_v7(),
                "semantic",
                0.82,
                "semantically relevant evidence",
            )]],
            10,
        )
        .remove(0);
        assert!(candidate_is_relevant_rag_evidence(&strong, true));
    }

    #[test]
    fn strict_rag_keeps_lexical_evidence_with_semantic_support() {
        // A fulltext hit that the semantic channel also supports is evidence.
        let file_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();
        let supported = fuse_retrieval_candidates(
            &[
                vec![ranked_hit(
                    file_id,
                    chunk_id,
                    "semantic",
                    0.65,
                    "shared chunk",
                )],
                vec![ranked_hit(
                    file_id,
                    chunk_id,
                    "fulltext",
                    0.25,
                    "exact phrase from the document",
                )],
            ],
            10,
        )
        .remove(0);
        assert!(candidate_is_relevant_rag_evidence(&supported, true));

        // Without a semantic engine the fulltext channel is the only
        // retrieval path, so the plain floor applies.
        assert!(candidate_is_relevant_rag_evidence(&supported, false));

        // Semantic support below the floor: the lexical hit is too weak to
        // stand on its own.
        let unsupported = fuse_retrieval_candidates(
            &[
                vec![ranked_hit(
                    file_id,
                    chunk_id,
                    "semantic",
                    0.55,
                    "shared chunk",
                )],
                vec![ranked_hit(
                    file_id,
                    chunk_id,
                    "fulltext",
                    0.25,
                    "exact phrase from the document",
                )],
            ],
            10,
        )
        .remove(0);
        assert!(!candidate_is_relevant_rag_evidence(&unsupported, true));

        // A pure lexical hit -- e.g. a chunk that only matched an OR-expanded
        // common token -- carries no semantic score and is not evidence.
        let lexical_only = fuse_retrieval_candidates(
            &[vec![ranked_hit(
                Uuid::now_v7(),
                Uuid::now_v7(),
                "fulltext",
                0.25,
                "exact phrase from the document",
            )]],
            10,
        )
        .remove(0);
        assert!(!candidate_is_relevant_rag_evidence(&lexical_only, true));
    }

    #[test]
    fn query_level_relevance_gate_rejects_gibberish() {
        // 真实查询的语义 top-1 明显领先（实测 0.90+）：强语义命中放行。
        let strong = fuse_retrieval_candidates(
            &[vec![
                ranked_hit(
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    "semantic",
                    0.9018,
                    "产品定位",
                ),
                ranked_hit(
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    "semantic",
                    0.8889,
                    "产品规划",
                ),
            ]],
            10,
        );
        assert!(query_has_relevant_evidence(&strong));

        // 乱码查询：真实标定（calibrate_query_gate.py）中乱码/闲聊的 top-1
        // 在 0.768~0.803，与正例重叠但都低于 0.80 门槛——必须拒绝。
        let gibberish = fuse_retrieval_candidates(
            &[vec![
                ranked_hit(
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    "semantic",
                    0.7752,
                    "重命名.png",
                ),
                ranked_hit(
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    "semantic",
                    0.7713,
                    "查看属性.png",
                ),
                ranked_hit(Uuid::now_v7(), Uuid::now_v7(), "semantic", 0.7690, "逐字稿"),
            ]],
            10,
        );
        assert!(!query_has_relevant_evidence(&gibberish));

        // 中语义 0.80 整（真实正例最低档）：视为相关。
        let leading = fuse_retrieval_candidates(
            &[vec![
                ranked_hit(Uuid::now_v7(), Uuid::now_v7(), "semantic", 0.80, "报销流程"),
                ranked_hit(Uuid::now_v7(), Uuid::now_v7(), "semantic", 0.60, "其他文档"),
            ]],
            10,
        );
        assert!(query_has_relevant_evidence(&leading));

        // 极强全文命中（罕见词全部命中，>= 0.30）单独放行。
        let exact_phrase = fuse_retrieval_candidates(
            &[vec![ranked_hit(
                Uuid::now_v7(),
                Uuid::now_v7(),
                "fulltext",
                0.35,
                "全库唯一精确命中",
            )]],
            10,
        );
        assert!(query_has_relevant_evidence(&exact_phrase));

        // 弱语义（0.79 < 门槛）+ 部分 token 弱全文（0.22 < 0.30）：拒绝。
        let weak_lexical = fuse_retrieval_candidates(
            &[vec![
                ranked_hit(
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    "semantic",
                    0.79,
                    "共享 chunk",
                ),
                ranked_hit(
                    Uuid::now_v7(),
                    Uuid::now_v7(),
                    "fulltext",
                    0.221,
                    "movies.csv 部分 token",
                ),
            ]],
            10,
        );
        assert!(!query_has_relevant_evidence(&weak_lexical));

        // 空候选：无证据。
        assert!(!query_has_relevant_evidence(&[]));
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
            image_assets: vec![],
            ocr_attempts: vec![],
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
        assert!(chunks.iter().all(|chunk| chunk.token_count <= 480));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.locator.paragraph_no == Some(3))
        );
    }

    #[test]
    fn hybrid_fusion_keeps_chunks_until_mmr_and_merges_the_same_chunk_channels() {
        let first_file = Uuid::now_v7();
        let second_file = Uuid::now_v7();
        let shared_chunk = Uuid::now_v7();
        let sibling_chunk = Uuid::now_v7();
        let other_chunk = Uuid::now_v7();
        let fulltext = vec![
            ranked_hit(
                first_file,
                shared_chunk,
                "fulltext",
                0.9,
                "项目计划包含检索优化",
            ),
            ranked_hit(
                first_file,
                sibling_chunk,
                "fulltext",
                0.8,
                "项目风险与恢复检查点",
            ),
        ];
        let semantic = vec![
            ranked_hit(
                first_file,
                shared_chunk,
                "semantic",
                0.95,
                "项目计划包含检索优化",
            ),
            ranked_hit(
                second_file,
                other_chunk,
                "semantic",
                0.7,
                "数据库锁竞争处理",
            ),
        ];

        let candidates = fuse_retrieval_candidates(&[fulltext, semantic], 10);

        assert_eq!(candidates.len(), 3);
        let shared = candidates
            .iter()
            .find(|candidate| candidate.chunk_id == Some(shared_chunk))
            .expect("shared chunk");
        assert!(
            shared
                .match_reasons
                .iter()
                .any(|reason| reason == "fulltext")
        );
        assert!(
            shared
                .match_reasons
                .iter()
                .any(|reason| reason == "semantic")
        );
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.file_id == first_file)
                .count(),
            2
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.rank)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }
}
