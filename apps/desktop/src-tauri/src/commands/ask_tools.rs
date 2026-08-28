//! 高层只读 Tool 的执行器（Phase 0+1，桌面编排层）。
//!
//! 现有能力（检索/摘要/抽取/比较）的编排逻辑在 `app_data.rs`；本模块先把
//! 现有链路尚未覆盖的 3 个高层只读 Tool 落地为可独立调用的执行器：
//!
//! - `library_overview`：知识库概览（按类型计数 + 最近文档），回答「我的
//!   知识库有什么」，不检索正文；
//! - `get_outline`：只取文档结构大纲（章节标题串），不做逐节模型摘要；
//! - `read_document`：直接读取短文档原文（受字符上限约束）。
//!
//! 数据来源全部复用 `CatalogService` 的只读方法（画像 / 节点 / 块），不
//! 暴露 Embedding/FTS/RRF/MMR 给决策层。执行器返回的引用只来自原始
//! chunk/节点，遵守「画像只用于定位，绝不成 Citation Evidence」的契约。
//!
//! Phase 0+1 阶段这些执行器尚未被默认问答链路调用（开关关闭时行为不变），
//! 由 Phase 2 的 Planner 接入；因此本模块整体标记为 `dead_code`，避免在
//! 桌面二进制里产生未使用告警。

use std::collections::HashMap;

use fanfan_core::ask::document_resolver::ResolverInput;
use fanfan_core::{
    build_document_sections, merge_tail_sections, resolve_documents, AppError, AskSessionContext,
    CatalogService, DocumentType, QueryPlan, SectionChunk, MAX_SECTIONS, MAX_SECTION_CHARS,
};
use uuid::Uuid;

/// 知识库概览结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LibraryOverview {
    /// 已画像的文档总数
    pub total_files: u64,
    /// 按文档类型分组的计数（无类型归入 unknown）
    pub by_type: Vec<TypeCount>,
    /// 最近更新的文档（仅定位用，不携带正文）
    pub recent: Vec<RecentFile>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct TypeCount {
    pub name: String,
    pub count: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RecentFile {
    pub file_id: String,
    pub file_name: String,
    pub title: String,
    pub updated_at: String,
}

/// 大纲中单个章节。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct OutlineSection {
    pub title: String,
    pub char_count: usize,
}

/// 文档大纲结果。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct DocumentOutline {
    pub file_id: String,
    pub sections: Vec<OutlineSection>,
    pub evidence: Vec<AgentEvidence>,
}

/// 直读文档结果（受 `max_chars` 上限约束）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ReadOutput {
    pub file_id: String,
    /// 按文档顺序拼接的正文（截断时长文本会显式截断，绝不隐藏降级）
    pub full_text: String,
    pub total_chars: usize,
    pub truncated: bool,
    /// 原始 chunk 引用（供生成/校验复用）
    pub evidence: Vec<AgentEvidence>,
}

/// 轻量证据引用（Phase 2 在注入生成前会转换为受限 RAG 的证据契约）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AgentEvidence {
    pub file_id: Uuid,
    pub node_id: Uuid,
    pub chunk_id: Uuid,
    /// 摘自原始块的真实原文（只读，不新增内容）
    pub excerpt: String,
}

/// 知识库概览：回答「我的知识库有什么」。
///
/// 只聚合画像元数据（文件数/类型/最近更新），不读取正文，不触发底层检索。
pub fn library_overview(catalog: &CatalogService) -> Result<LibraryOverview, AppError> {
    let profiles = catalog.list_document_profiles(None, 10_000)?;
    let total_files = profiles.len() as u64;

    // 按文档类型分组计数；无类型归入 unknown
    let mut counts: HashMap<Option<DocumentType>, u64> = HashMap::new();
    let mut recent_buf: Vec<(String, String, String, String)> = Vec::new();
    for (profile, file_name) in &profiles {
        *counts.entry(profile.document_type).or_insert(0) += 1;
        recent_buf.push((
            profile.file_id.to_string(),
            file_name.clone(),
            profile.title.clone(),
            profile.updated_at.to_rfc3339(),
        ));
    }
    // 最近更新排序取前 20（仅元数据定位）
    recent_buf.sort_by(|a, b| b.3.cmp(&a.3));
    let mut by_type: Vec<TypeCount> = counts
        .into_iter()
        .map(|(ty, count)| TypeCount {
            name: ty
                .map(|t| t.display_name().to_owned())
                .unwrap_or_else(|| "unknown".to_owned()),
            count,
        })
        .collect();
    by_type.sort_by(|a, b| b.count.cmp(&a.count));

    Ok(LibraryOverview {
        total_files,
        by_type,
        recent: recent_buf
            .into_iter()
            .take(20)
            .map(|(file_id, file_name, title, updated_at)| RecentFile {
                file_id,
                file_name,
                title,
                updated_at,
            })
            .collect(),
    })
}

/// 文档画像（标题/摘要/关键词/实体/类型/章节），供 Planner「先了解文档」。
pub fn get_document_profile(
    catalog: &CatalogService,
    file_id: Uuid,
) -> Result<Option<fanfan_core::DocumentProfile>, AppError> {
    catalog.get_document_profile(file_id)
}

/// 文档大纲：只取结构（章节标题 + 字符数），复用现有 `build_document_sections`
/// 的分组逻辑，但不做逐节模型摘要（比 DOCUMENT_SUMMARY 便宜，用于「大纲」）。
pub fn get_outline(catalog: &CatalogService, file_id: Uuid) -> Result<DocumentOutline, AppError> {
    let file_id_str = file_id.to_string();
    // 1. 分页读节点 heading 路径（上限 4000 防失控，与摘要管线同源）
    let mut nodes = Vec::<Uuid>::new();
    let mut headings: HashMap<Uuid, Vec<String>> = HashMap::new();
    let mut offset = 0usize;
    loop {
        let preview = catalog.file_preview_page(&file_id, offset, 200, None)?;
        if preview.revision_id.is_none() {
            break;
        }
        let batch_len = preview.nodes.len();
        for node in preview.nodes {
            headings.insert(node.node_id, node.heading_path);
            nodes.push(node.node_id);
        }
        offset = match preview.next_offset {
            Some(next) => next as usize,
            None => break,
        };
        if batch_len == 0 || nodes.len() > 4_000 {
            break;
        }
    }

    // 2. 当前修订全部 chunk（引用真实原文）
    let section_chunks = catalog
        .file_chunks(&file_id)?
        .into_iter()
        .map(|chunk| SectionChunk {
            chunk_id: chunk.chunk_id,
            node_id: chunk.node_id,
            revision_id: chunk.revision_id,
            ordinal: chunk.ordinal,
            text: chunk.text,
            locator: chunk.locator,
        })
        .collect::<Vec<_>>();

    let mut sections = build_document_sections(&section_chunks, &headings, MAX_SECTION_CHARS);
    merge_tail_sections(&mut sections, MAX_SECTIONS);

    let evidence = section_chunks
        .iter()
        .take(200)
        .map(|chunk| AgentEvidence {
            file_id,
            node_id: chunk.node_id,
            chunk_id: chunk.chunk_id,
            excerpt: trim_excerpt(&chunk.text),
        })
        .collect::<Vec<_>>();

    Ok(DocumentOutline {
        file_id: file_id_str,
        sections: sections
            .iter()
            .map(|section| OutlineSection {
                title: section.title.clone(),
                char_count: section.char_count(),
            })
            .collect(),
        evidence,
    })
}

/// 直接读取短文档原文（受 `max_chars` 上限约束）。
///
/// 短文件直读的语义：当文档短、回答依赖整段原文细节时，直接用只读 chunk
/// 拼接；超长则显式截断并置 `truncated=true`，不静默截断冒充完整。
pub fn read_document(
    catalog: &CatalogService,
    file_id: Uuid,
    max_chars: usize,
) -> Result<ReadOutput, AppError> {
    let mut chunks = catalog.file_chunks(&file_id)?;
    // 按 ordinal 稳定排序，保证直读顺序与文档一致
    chunks.sort_by_key(|chunk| chunk.ordinal);

    let mut full_text = String::new();
    let mut evidence: Vec<AgentEvidence> = Vec::new();
    let mut truncated = false;
    for chunk in &chunks {
        let excerpt = trim_excerpt(&chunk.text);
        evidence.push(AgentEvidence {
            file_id,
            node_id: chunk.node_id,
            chunk_id: chunk.chunk_id,
            excerpt: excerpt.clone(),
        });
        if full_text.chars().count() < max_chars {
            full_text.push_str(&excerpt);
            full_text.push('\n');
        } else {
            truncated = true;
        }
    }
    if full_text.chars().count() > max_chars {
        let trimmed: String = full_text.chars().take(max_chars).collect();
        full_text = trimmed;
        truncated = true;
    }
    let total_chars = chunks.iter().map(|c| c.text.chars().count()).sum();

    Ok(ReadOutput {
        file_id: file_id.to_string(),
        full_text,
        total_chars,
        truncated,
        evidence,
    })
}

/// 找文件结果：定位到的候选文件元数据清单。
///
/// 仅返回画像定位元数据（file_id / 文件显示名 / 标题），不携带正文，
/// 不触发底层检索；与 legacy find 同源复用 `resolve_documents` 定位逻辑，
/// 但以显式高层 Tool（`search_files`）形态暴露给 Planner。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SearchFilesOutput {
    /// 是否定位到候选文件（false 表示未命中/目标为空）。
    pub found: bool,
    /// 候选文件清单（按 resolver 置信度排序；可能多份同名内容族）。
    pub candidates: Vec<SearchFile>,
}

/// 单个候选文件的定位元数据。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SearchFile {
    pub file_id: String,
    pub file_name: String,
    pub title: String,
}

/// 找文件：按文件名 / 标题 / 类型定位候选文件（回答「在哪份文件里」）。
///
/// 复用 Document Resolver 定位逻辑：解析 `QueryPlan.target` 锁定白名单，
/// 再映射回文件显示名与标题。只在 `found` 为真时返回候选，未命中不强行
/// 扩大 scope（避免 Wrong Scope）。
pub fn search_files(catalog: &CatalogService, plan: &QueryPlan) -> Result<SearchFilesOutput, AppError> {
    let profiles = catalog.list_document_profiles(None, 10_000)?;
    let mut file_names: HashMap<Uuid, String> = HashMap::with_capacity(profiles.len());
    let mut titles: HashMap<Uuid, String> = HashMap::with_capacity(profiles.len());
    let profile_vec = profiles
        .iter()
        .map(|(profile, name)| {
            file_names.insert(profile.file_id, name.clone());
            titles.insert(profile.file_id, profile.title.clone());
            profile.clone()
        })
        .collect::<Vec<_>>();

    let session = AskSessionContext::default();
    let input = ResolverInput::new(plan, &session, profile_vec, file_names.clone());
    let resolution = resolve_documents(&input);

    // 命中白名单为主，未命中回退候选（与 legacy find 同口径）
    let mut ids: Vec<Uuid> = resolution.resolved_file_ids;
    if ids.is_empty() {
        ids = resolution.candidates.iter().map(|c| c.file_id).collect();
    }
    let mut candidates = Vec::with_capacity(ids.len());
    let mut seen = std::collections::HashSet::new();
    for id in ids {
        if seen.insert(id) {
            if let Some(name) = file_names.get(&id) {
                candidates.push(SearchFile {
                    file_id: id.to_string(),
                    file_name: name.clone(),
                    title: titles.get(&id).cloned().unwrap_or_default(),
                });
            }
        }
    }

    Ok(SearchFilesOutput {
        found: !candidates.is_empty(),
        candidates,
    })
}

/// 统一的短文本摘取（用于证据 excerpt；超长截断并补省略标记）。
fn trim_excerpt(text: &str) -> String {
    const EXCERPT_CAP: usize = 200;
    let trimmed = text.trim();
    if trimmed.chars().count() <= EXCERPT_CAP {
        trimmed.to_owned()
    } else {
        let head: String = trimmed.chars().take(EXCERPT_CAP).collect();
        format!("{head}…")
    }
}
