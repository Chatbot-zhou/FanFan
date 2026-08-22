//! 检索链路诊断工具：
//!   cargo run -p fanfan-core --example diagnose_search -- <db_path> --search <query> ...
//!   cargo run -p fanfan-core --example diagnose_search -- <db_path> --files <id1,id2,...>
//!   cargo run -p fanfan-core --example diagnose_search -- <db_path> --model-store <path> --hybrid <query> ...
//! 前两种分别运行 Filename / Fulltext 通道并打印命中；--hybrid 与评测同口径运行三通道
//! 混合检索（语义通道走本机 Ollama embedding），用于定位融合阶段的文件流失。
use std::path::PathBuf;

use fanfan_core::{
    Availability, CatalogStore, ModelManager, ModelRole, OllamaClient, ScopeFilter, SearchMode,
    SearchRequest, SearchSort, SemanticQuery,
};

/// 构造与评测等价的"全授权"scope。
fn all_authorized_scope() -> ScopeFilter {
    ScopeFilter {
        root_ids: Vec::new(),
        collection_ids: Vec::new(),
        file_ids: Vec::new(),
        extensions: Vec::new(),
        modified_from: None,
        modified_to: None,
        availability: Availability::Present,
    }
}

/// 运行单通道检索并打印命中列表。
fn run_channel(
    catalog: &CatalogStore,
    query: &str,
    mode: SearchMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = catalog.search(&SearchRequest {
        query: query.to_owned(),
        scope: all_authorized_scope(),
        mode,
        sort: SearchSort::Relevance,
        page_size: 15,
        cursor: None,
    })?;
    println!("  [{mode:?}] 命中 {} 条:", session.results.len());
    for (rank, result) in session.results.iter().enumerate() {
        println!(
            "    #{rank} {} | 原因={:?} | fused={:.4} | filename={:.3} fulltext={:.3}",
            result.name,
            result.match_reasons,
            result.scores.fused,
            result.scores.filename.unwrap_or(-1.0),
            result.scores.fulltext.unwrap_or(-1.0)
        );
    }
    Ok(())
}

/// 运行与评测同口径的 Hybrid 检索（Filename + Fulltext + 语义），打印最终排序。
fn run_hybrid(
    catalog: &CatalogStore,
    model_store: &PathBuf,
    query: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let manager = ModelManager::open_store(model_store)?;
    let embedding = manager
        .list_artifacts()?
        .into_iter()
        .find(|artifact| artifact.role == ModelRole::Embedding)
        .ok_or("注册表中没有 Embedding 模型")?;
    if embedding.format == fanfan_core::ModelFormat::Ollama {
        manager.ollama_embedding_ready(&embedding.model_id)?;
    }
    let embedding = manager
        .active_artifact(ModelRole::Embedding)?
        .ok_or("没有可用的 Embedding 模型")?;
    let ollama = OllamaClient::local();
    let prefixed = format!(
        "{}{}",
        embedding.query_prefix.as_deref().unwrap_or_default(),
        query
    );
    let (vectors, _dimension) = ollama.embed(&embedding.model_id, &[prefixed])?;
    let vector = vectors.into_iter().next().ok_or("embedding 返回空")?;
    let session = catalog.search_with_semantic(
        &SearchRequest {
            query: query.to_owned(),
            scope: all_authorized_scope(),
            mode: SearchMode::Hybrid,
            sort: SearchSort::Relevance,
            page_size: 15,
            cursor: None,
        },
        Some(SemanticQuery {
            model_artifact_id: &embedding.artifact_id.to_string(),
            vector: &vector,
        }),
    )?;
    println!("  [Hybrid] 命中 {} 条:", session.results.len());
    for (rank, result) in session.results.iter().enumerate() {
        println!(
            "    #{rank} {} | 原因={:?} | fused={:.4} | filename={:.3} fulltext={:.3} semantic={:.3}",
            result.name,
            result.match_reasons,
            result.scores.fused,
            result.scores.filename.unwrap_or(-1.0),
            result.scores.fulltext.unwrap_or(-1.0),
            result.scores.semantic.unwrap_or(-1.0)
        );
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let db_path = args.next().ok_or("缺少数据库路径参数")?;
    let catalog = CatalogStore::open(PathBuf::from(&db_path))?;
    let mut mode = String::new();
    let mut model_store: Option<PathBuf> = None;
    let mut targets = Vec::new();
    let mut next_is_model_store = false;
    for arg in args {
        if next_is_model_store {
            model_store = Some(PathBuf::from(&arg));
            next_is_model_store = false;
            continue;
        }
        if arg == "--search" || arg == "--files" || arg == "--hybrid" {
            mode = arg;
            continue;
        }
        if arg == "--model-store" {
            next_is_model_store = true;
            continue;
        }
        targets.push(arg);
    }
    if mode == "--hybrid" {
        let model_store = model_store.ok_or("--hybrid 需要 --model-store <path>")?;
        for query in targets {
            println!("== 查询: {query} ==");
            run_hybrid(&catalog, &model_store, &query)?;
        }
        return Ok(());
    }
    match mode.as_str() {
        "--files" => {
            let files = catalog.list_files()?;
            let wanted = targets
                .iter()
                .flat_map(|item| item.split(','))
                .map(|item| item.trim().to_owned())
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>();
            for file in files {
                let id = file.file_id.to_string();
                if wanted.iter().any(|w| id.starts_with(w)) {
                    println!(
                        "FILE {} | {} | parse={:?} | avail={:?} | size={}",
                        id,
                        file.display_name,
                        file.parse_status,
                        file.availability,
                        file.size_bytes
                    );
                }
            }
        }
        _ => {
            for query in targets {
                println!("== 查询: {query} ==");
                run_channel(&catalog, &query, SearchMode::Filename)?;
                run_channel(&catalog, &query, SearchMode::Fulltext)?;
            }
        }
    }
    Ok(())
}
