//! 临时诊断工具：只读列出 catalog 文档清单并按需采样 chunk 原文，
//! 用于确立下一轮人工评价题的 Ground Truth（不改库、不写数据）。
//!
//! 环境变量：
//!   FANFAN_DATA_DIR      catalog db 目录（默认 E:\Desktop\FanFan\DATA\FanFanData）
//!   DUMP_SAMPLE         非空时对每份文档采样头/中/尾 chunk 各若干字
//!   DUMP_NAME_FILTER    仅输出 display_name 含该子串的文档（可选）

use std::{collections::HashSet, env, path::PathBuf};

use fanfan_core::{Availability, CatalogStore};

fn main() {
    let data_dir = env::var("FANFAN_DATA_DIR")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let sample = env::var("DUMP_SAMPLE").map(|v| v.trim() != "").unwrap_or(false);
    let filter = env::var("DUMP_NAME_FILTER").ok().map(|v| v.trim().to_string());

    let catalog = match CatalogStore::open(PathBuf::from(&data_dir).join("fanfan.db")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("open catalog failed: {}: {}", e.code, e.message);
            std::process::exit(1);
        }
    };
    let files = match catalog.list_files() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("list_files failed: {}: {}", e.code, e.message);
            std::process::exit(1);
        }
    };

    let mut by_name = Vec::new();
    for file in files {
        if file.availability != Availability::Present {
            continue;
        }
        if let Some(f) = &filter {
            if !file.display_name.contains(f.as_str()) {
                continue;
            }
        }
        let status = format!("{:?}", file.parse_status);
        let rev = file.current_revision_id;
        by_name.push((file.file_id, file.display_name, status, rev));
    }
    println!("== documents({}) ==", by_name.len());
    for (fid, name, status, rev) in &by_name {
        let nchunk = match rev {
            Some(rev) => catalog.file_chunks(fid).map(|c| c.len() as u64).unwrap_or(0),
            None => 0,
        };
        println!("{name:80} | status={status} | rev={rev:?} | chunks={nchunk}");
    }

    if !sample {
        return;
    }
    for (fid, name, _, rev) in &by_name {
        let Some(rev) = rev else { continue };
        let Ok(chunks) = catalog.file_chunks(fid) else { continue };
        if chunks.is_empty() {
            continue;
        }
        println!("\n===== FILE: {name} ({} chunks) =====", chunks.len());
        let total = chunks.len();
        let mut picks: HashSet<usize> = HashSet::new();
        picks.insert(0);
        if total > 1 {
            picks.insert(total - 1);
        }
        for i in 1..8 {
            if i < total {
                picks.insert((total * i) / 9);
            }
        }
        let mut sorted: Vec<usize> = picks.into_iter().filter(|&i| i < total).collect();
        sorted.sort_unstable();
        for ordinal in sorted {
            let text: String = chunks[ordinal]
                .text
                .chars()
                .take(500)
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect();
            println!("--- chunk[{ordinal}] ---\n{text}");
        }
    }
}