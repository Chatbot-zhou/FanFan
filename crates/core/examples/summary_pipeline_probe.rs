//! 探针：镜像生产 `run_document_summary_answer` 的整文分层摘要管线。
//!
//! 目的：用生产库真实 chunk + 真实 Ollama 生成模型，对指定文件跑
//! 「章节分组 → 逐节摘要 → 聚合总览」，验证 DOCUMENT_SUMMARY 主链在
//! 真实数据上能产出合格摘要，并顺带核验 r31（2023 备考知识点集锦）的
//! 解析分类已升级为 DocumentSummary。
//!
//! 运行：
//!   FANFAN_DATA_DIR=E:\Desktop\FanFan\DATA\FanFanData \
//!   FANFAN_MODEL_STORE=E:\Desktop\FanFan\DATA\FanFanModelStore \
//!   FANFAN_PROBE_FILE=2023年数据库系统工程师备考知识点集锦 \
//!   cargo run --example summary_pipeline_probe
//!
//! 不做任何针对具体文件/关键词的特判：目标文件仅由环境变量/文件名片段
//! 定位，其余完全走通用摘要管线。
use std::{
    collections::HashMap,
    env, path::PathBuf,
    sync::atomic::AtomicBool,
};

use fanfan_core::ask::document_summary::{
    SectionChunk, SectionSummary, build_document_sections, digests_json,
    document_overview_prompt, document_summary_prompt, match_section_digests,
    merge_tail_sections, overview_schema, parse_overview, parse_section_summaries,
    section_summary_schema,
};
use fanfan_core::generation::LocalGenerationRuntime;
use fanfan_core::{CatalogStore, MAX_SECTION_CHARS, MAX_SECTIONS};
use serde_json::json;

const CHAT_MODEL: &str = "qwen3.5:2b";
const SUMMARY_BATCH_CHARS: usize = 3_500;
const SUMMARY_SECTION_CAP_CHARS: usize = 1_200;
const SUMMARY_FALLBACK_CHARS: usize = 220;

fn main() {
    if let Err(message) = run() {
        eprintln!("summary_pipeline_probe 失败: {message}");
        std::process::exit(1);
    }
}

/// 主流程：激活生成运行时 → 定位目标文件 → 跑分层摘要管线 → 打印结果。
fn run() -> Result<(), String> {
    let data_dir = env::var("FANFAN_DATA_DIR")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let file_hint = env::var("FANFAN_PROBE_FILE")
        .unwrap_or_else(|_| "备考知识点集锦".to_owned());

    // 激活真实生成运行时（与 real50_chain 同口径）。
    let mut runtime = LocalGenerationRuntime::new();
    let threads = (std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(4)
        / 2)
        .clamp(1, 4);
    runtime
        .activate(CHAT_MODEL, 4096, threads)
        .map_err(|error| error.message)?;
    let cancelled = AtomicBool::new(false);

    let catalog = CatalogStore::open(PathBuf::from(&data_dir).join("fanfan.db"))
        .map_err(|error| error.message)?;

    // 定位目标文件（文件名片段，通用；不针对具体文件做特判）。
    let files = catalog.list_files().map_err(|error| error.message)?;
    let target = files
        .into_iter()
        .find(|file| file.display_name.contains(&file_hint))
        .ok_or_else(|| format!("未找到文件名含「{file_hint}」的文件"))?;
    let file_id = target.file_id;
    println!(
        "== 目标: {} (file_id={})",
        target.display_name, file_id
    );

    // 1. 整份文档结构：document_nodes 分页读取（生产同口径：200/批，上限 4000）。
    let mut node_heading_paths = HashMap::new();
    let mut offset = 0usize;
    loop {
        let preview = catalog
            .file_preview_page(&file_id, offset, 200, None)
            .map_err(|error| error.message)?;
        let batch_len = preview.nodes.len();
        for node in preview.nodes {
            node_heading_paths.insert(node.node_id, node.heading_path);
        }
        offset = match preview.next_offset {
            Some(next) => next as usize,
            None => break,
        };
        if batch_len == 0 {
            break;
        }
    }
    println!("heading_path 节点 = {}", node_heading_paths.len());

    // 2. 当前修订全部 chunk（摘要证据：真实 chunk 原文）。
    let section_chunks: Vec<SectionChunk> = catalog
        .file_chunks(&file_id)
        .map_err(|error| error.message)?
        .into_iter()
        .map(|chunk| SectionChunk {
            chunk_id: chunk.chunk_id,
            node_id: chunk.node_id,
            revision_id: chunk.revision_id,
            ordinal: chunk.ordinal,
            text: chunk.text,
            locator: chunk.locator,
        })
        .collect();
    if section_chunks.is_empty() {
        return Err("该文件没有可概括的 chunk（可能仍在解析或为纯图片/扫描件）".into());
    }
    println!("chunks = {}", section_chunks.len());

    // 3. 章节分组 + 尾部合并（生产同参数）。
    let mut sections =
        build_document_sections(&section_chunks, &node_heading_paths, MAX_SECTION_CHARS);
    merge_tail_sections(&mut sections, MAX_SECTIONS);
    println!("sections = {}", sections.len());

    // 4. 分批逐节摘要（生产同口径：批次字符预算 + 截断 + 确定性回退）。
    let mut digests = Vec::<SectionSummary>::with_capacity(sections.len());
    let mut batch_fallbacks = 0_usize;
    let mut index = 0usize;
    while index < sections.len() {
        let mut batch = Vec::new();
        let mut batch_chars = 0usize;
        while index < sections.len() && (batch.is_empty() || batch_chars < SUMMARY_BATCH_CHARS) {
            let section = &sections[index];
            let compacted = compact_for_prompt(&section.text(), SUMMARY_SECTION_CAP_CHARS);
            if !batch.is_empty()
                && batch_chars.saturating_add(compacted.len()) > SUMMARY_BATCH_CHARS
            {
                break;
            }
            batch_chars = batch_chars.saturating_add(compacted.len());
            batch.push((index, section, compacted));
            index += 1;
        }
        let payload = batch
            .iter()
            .map(|(_, section, compacted)| {
                json!({
                    "title": section.title,
                    "content": compacted,
                })
            })
            .collect::<Vec<_>>();
        let (system, user) =
            document_summary_prompt(&target.display_name, None, &json!(&payload).to_string());
        let parsed = match runtime.complete_json_cancellable(
            &system,
            &user,
            640,
            &section_summary_schema(),
            &cancelled,
        ) {
            Ok(raw) => parse_section_summaries(&raw),
            Err(_) => Vec::new(),
        };
        // 标题匹配 + 位置补齐混合对齐（生产同口径，见 match_section_digests）。
        let batch_start = batch.first().map(|(first, _, _)| *first).unwrap_or(index);
        let (batch_digests, batch_fallback_count) =
            match_section_digests(&sections[batch_start..index], parsed, SUMMARY_FALLBACK_CHARS);
        if batch_fallback_count > 0 {
            batch_fallbacks += 1;
        }
        digests.extend(batch_digests);
    }
    println!("逐节摘要完成: digests={} batch_fallbacks={}", digests.len(), batch_fallbacks);

    // 5. 总览聚合（最后一层）。
    let payload = digests_json(&digests);
    let (system, user) =
        document_overview_prompt(&target.display_name, None, &payload.to_string());
    let overview = runtime
        .complete_json_cancellable(&system, &user, 512, &overview_schema(), &cancelled)
        .ok()
        .and_then(|raw| parse_overview(&raw))
        .unwrap_or_else(|| {
            let structure = digests
                .iter()
                .map(|digest| fanfan_core::StructureEntry {
                    title: digest.title.clone(),
                    key_points: digest.key_points.clone(),
                })
                .collect();
            fanfan_core::DocumentOverview {
                overview: String::new(),
                overall_summary: digests
                    .iter()
                    .map(|digest| digest.summary.as_str())
                    .collect::<Vec<_>>()
                    .join("；"),
                structure,
            }
        });

    // 6. 打印结果。
    println!("\n======== 文档总览 ========");
    println!("【总览】{}", overview.overview);
    println!("【总体摘要】{}", overview.overall_summary);
    println!("\n== 章节结构 ==");
    for entry in &overview.structure {
        println!("  - {}", truncate(&entry.title, 40));
        for point in &entry.key_points {
            println!("      · {}", truncate(point, 90));
        }
    }
    println!("\n== 逐节摘要 ==");
    for (section, digest) in sections.iter().zip(digests.iter()) {
        println!("--- {}", section.title);
        println!("    {}", truncate(&digest.summary, 220));
    }
    Ok(())
}

/// 与生产同口径的截断：超限保留前缀并追加省略号。
fn compact_for_prompt(value: &str, limit: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= limit {
        value.to_owned()
    } else {
        let mut out: String = value.chars().take(limit).collect();
        out.push('…');
        out
    }
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        text
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}
