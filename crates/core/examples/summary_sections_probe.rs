//! 探针：用生产库 r31 的真实 chunks 验证整文摘要分节（build_document_sections /
//! merge_tail_sections）在真实数据上能正常产出章节，供摘要管线数据路径核验。
//!
//! 运行：FANFAN_DATA_DIR=E:\Desktop\FanFan\DATA\FanFanData cargo run --example summary_sections_probe
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;

use fanfan_core::{
    MAX_SECTION_CHARS, MAX_SECTIONS, SectionChunk, build_document_sections, merge_tail_sections,
};

fn main() {
    if let Err(message) = run() {
        eprintln!("summary_sections_probe 失败: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let data_dir = env::var("FANFAN_DATA_DIR")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let catalog = fanfan_core::CatalogStore::open(PathBuf::from(&data_dir).join("fanfan.db"))
        .map_err(|error| error.message)?;

    // 定位 r31（2023 备考知识点集锦）
    let files = catalog
        .list_files()
        .map_err(|error| error.message)?;
    let r31 = files
        .into_iter()
        .find(|file| file.display_name.contains("备考知识点集锦"))
        .ok_or_else(|| "未找到 r31 文件（备考知识点集锦）".to_owned())?;
    println!(
        "== r31: {} revision={:?}",
        r31.display_name, r31.current_revision_id
    );

    // 整份文件 chunks（生产 run_document_summary_answer 同口径）
    let chunks = catalog
        .file_chunks(&r31.file_id)
        .map_err(|error| error.message)?;
    println!("chunks = {}", chunks.len());
    let section_chunks: Vec<SectionChunk> = chunks
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

    // 节点 heading 路径
    let mut node_heading_paths = HashMap::new();
    let mut offset = 0usize;
    loop {
        let preview = catalog
            .file_preview_page(&r31.file_id, offset, 200, None)
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
    println!("heading_path 节点 = {}（总节点含 page 等非正文节点）", node_heading_paths.len());

    // 分节 + 尾部合并（与生产同参数）
    let mut sections = build_document_sections(&section_chunks, &node_heading_paths, MAX_SECTION_CHARS);
    let merged = merge_tail_sections(&mut sections, MAX_SECTIONS);
    println!(
        "sections = {}（合并 {} 个尾部）",
        sections.len(),
        merged
    );
    let total_chars: usize = sections.iter().map(|s| s.text().chars().count()).sum();
    println!("总正文字符 = {}", total_chars);
    for (index, section) in sections.iter().enumerate() {
        let heading = section
            .title
            .trim()
            .to_owned()
            .trim()
            .to_string();
        let heading = if heading.is_empty() { "(无章节标题)".to_owned() } else { heading };
        let chars = section.text().chars().count();
        let has_locator = section.chunks.first().is_some();
        println!("  [{:>2}] {:<24} chars={:<6} chunk_ids={}", index + 1, truncate(&heading, 24), chars, has_locator);
    }
    Ok(())
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max {
        text
    } else {
        text.chars().take(max).collect::<String>() + "…"
    }
}
