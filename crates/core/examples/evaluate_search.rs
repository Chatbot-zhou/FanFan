use std::{fs, path::PathBuf};

use remin_core::{
    Availability, CatalogService, ParseMetrics, ParseOutcome, ParseRequest, ParseResult,
    RootSource, ScopeFilter, SearchMode, SearchRequest, SearchSort, WorkerClient,
};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct ExpectedHit {
    file: String,
}

#[derive(Debug, Deserialize)]
struct SearchCase {
    case_id: String,
    query: String,
    expected_any: Vec<ExpectedHit>,
    #[serde(default)]
    must_not_return: Vec<String>,
    #[serde(default)]
    requires_ocr: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let corpus_root = repository_root.join("tests/fixtures/corpus");
    let data_directory = tempfile::tempdir()?;
    let catalog = CatalogService::open(data_directory.path().to_path_buf())?;
    let root = catalog.register_folder("测试语料", corpus_root, RootSource::UserFolder)?;
    let scan = catalog.prepare_scan(&root.root_id, "search_baseline")?;
    if scan.should_start {
        catalog.execute_scan(root.root_id, scan.job.job_id)?;
    }

    let worker = WorkerClient::from_environment(repository_root.join("services/worker"));
    loop {
        let pending = catalog.list_pending_parse_files(32)?;
        if pending.is_empty() {
            break;
        }
        for file in pending {
            let Some(revision_id) = file.current_revision_id else {
                continue;
            };
            catalog.mark_file_parsing(&file.file_id, &revision_id)?;
            let request = ParseRequest {
                job_id: Uuid::now_v7(),
                file_id: file.file_id,
                revision_id,
                source_path: file.canonical_path.clone(),
                format: file.extension.clone(),
                ocr_policy: "auto".into(),
                language_hints: vec!["zh".into()],
                max_pages: None,
                asset_cache_dir: None,
                parser_version: "0.1.0".into(),
            };
            let result = worker
                .parse_document(&request)
                .unwrap_or_else(|error| ParseResult {
                    revision_id,
                    status: ParseOutcome::Failed,
                    parser_name: "none".into(),
                    parser_version: request.parser_version,
                    nodes: vec![],
                    image_assets: vec![],
                    warnings: vec![],
                    metrics: ParseMetrics {
                        page_count: 0,
                        node_count: 0,
                        character_count: 0,
                        ocr_page_count: 0,
                        elapsed_ms: 0,
                    },
                    error: Some(error),
                });
            catalog.commit_parse_result(&file.file_id, &result)?;
        }
    }

    let baselines = fs::read_to_string(repository_root.join("tests/baselines/search.jsonl"))?;
    let mut checked = 0_u64;
    let mut passed = 0_u64;
    for line in baselines.lines().filter(|line| !line.trim().is_empty()) {
        let case: SearchCase = serde_json::from_str(line)?;
        if case.requires_ocr {
            continue;
        }
        checked += 1;
        let session = catalog.search(&SearchRequest {
            query: case.query.clone(),
            scope: ScopeFilter {
                knowledge_space_ids: vec![],
                root_ids: vec![root.root_id],
                collection_ids: vec![],
                file_ids: vec![],
                extensions: vec![],
                modified_from: None,
                modified_to: None,
                availability: Availability::Present,
            },
            mode: SearchMode::Hybrid,
            sort: SearchSort::Relevance,
            page_size: 30,
            cursor: None,
        })?;
        let returned = session
            .results
            .iter()
            .map(|result| result.name.as_str())
            .collect::<Vec<_>>();
        let expected_found = case
            .expected_any
            .iter()
            .any(|expected| returned.contains(&expected.file.as_str()));
        let forbidden_found = case
            .must_not_return
            .iter()
            .any(|forbidden| returned.contains(&forbidden.as_str()));
        if !expected_found || forbidden_found {
            return Err(format!(
                "{}未通过: query={} returned={returned:?}",
                case.case_id, case.query
            )
            .into());
        }
        passed += 1;
    }
    if checked == 0 || passed != checked {
        return Err("没有完成可评测的搜索用例".into());
    }
    println!("搜索基线通过: passed={passed}, checked={checked}, OCR用例等待模型后单独验收");
    Ok(())
}
