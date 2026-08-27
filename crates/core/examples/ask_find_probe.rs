//! 翻翻：诊断 DocumentFind 目标解析与定位（不修改任何代码，只读）。
//!
//! 目的：对指定 find 问句跑真实 query_parser → finalize_query_plan →
//! resolve_documents，打印完整 plan（含 reference/document_name/precise 等
//! 全部 target 字段）、resolver 候选及各自信号/分数，定位「文件名被塞进
//! content_query 而 target 只剩通用引导语」这类解析漂移。只读，不改数据。
//!
//! 用法:
//!   cargo run -p fanfan-core --example ask_find_probe
//! 环境变量:
//!   FANFAN_CHAT_MODEL   生成模型 tag（默认 qwen3.5:2b）
//!   FANFAN_DATA_DIR     真实 catalog 目录（默认 E:\Desktop\FanFan\DATA\FanFanData）
//!   FANFAN_MODEL_STORE  模型注册表目录（默认 E:\Desktop\FanFan\DATA\FanFanModelStore）
//!   PROBE_QUESTION      要诊断的问句（默认 r14 的问句）

use std::{collections::HashMap, env, path::PathBuf, sync::atomic::AtomicBool};

use fanfan_core::ask::document_resolver::ResolverInput;
use fanfan_core::ask::query_parser::{finalize_query_plan, parse_query_plan, query_parser_prompt, query_parser_schema};
use fanfan_core::generation::LocalGenerationRuntime;
use fanfan_core::{AppError, AskSessionContext, Availability, CatalogStore, ModelManager, ModelRole, OllamaClient, resolve_documents};

fn main() {
    if let Err(error) = run() {
        eprintln!("ask_find_probe 未完成: code={} message={}", error.code, error.message);
        if let Some(details) = error.details {
            eprintln!("  详情: {details}");
        }
        std::process::exit(1);
    }
}

/// 主流程：逐问句跑 parser → resolver，打印诊断信息。
fn run() -> Result<(), AppError> {
    let chat_model = env::var("FANFAN_CHAT_MODEL").unwrap_or_else(|_| "qwen3.5:2b".to_owned());
    let data_dir =
        env::var("FANFAN_DATA_DIR").unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let model_store = env::var("FANFAN_MODEL_STORE")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanModelStore".to_owned());
    let question = env::var("PROBE_QUESTION").unwrap_or_else(|_| {
        "2019年数据库下午的真题文件是哪个".to_owned()
    });

    let catalog = CatalogStore::open(PathBuf::from(&data_dir).join("fanfan.db"))?;
    let mut file_names = HashMap::new();
    for file in catalog.list_files()? {
        if file.availability == Availability::Present {
            file_names.insert(file.file_id, file.display_name);
        }
    }
    let mut runtime = LocalGenerationRuntime::new();
    let threads = (std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4) / 2)
        .clamp(1, 4);
    runtime.activate(chat_model.as_str(), 4096, threads)?;
    let cancelled = AtomicBool::new(false);

    // 1) Query Parser（生产同口径：最多 2 次，解析失败走 None）。
    let mut plan = None;
    for _ in 0..2 {
        let (system, user) = query_parser_prompt(&question, &[]);
        match runtime.complete_json_cancellable(&system, &user, 320, &query_parser_schema(), &cancelled)
        {
            Ok(raw) => {
                println!("[parser raw] {raw}");
                if let Some(parsed) = parse_query_plan(&raw)
                    .and_then(|p| finalize_query_plan(p, &question, &[]))
                {
                    plan = Some(parsed);
                    break;
                }
            }
            Err(_) => {}
        }
    }
    let Some(plan) = plan else {
        println!("[plan] parse_failed");
        return Ok(());
    };
    println!(
        "[plan] intent={} op={} requires_doc_resolution={}",
        plan.intent.as_str(),
        plan.operation.as_str(),
        plan.requires_document_resolution
    );
    println!(
        "[plan.target] reference={:?} document_name={:?} precise={} document_type={:?} entity_name={:?} owner={:?}",
        plan.target.reference,
        plan.target.document_name,
        plan.target.precise_named_document,
        plan.target.document_type,
        plan.target.entity_name,
        plan.target.owner
    );
    println!("[plan.content_query] {:?}", plan.content_query);

    // 2) Resolver（与 harness 同口径：不带向量）。
    let profiles_with_names = catalog.list_document_profiles(None, 2000)?;
    let profiles = profiles_with_names
        .into_iter()
        .map(|(profile, _)| profile)
        .collect::<Vec<_>>();
    let session = AskSessionContext::default();
    let input = ResolverInput::new(&plan, &session, profiles.clone(), file_names.clone());
    let resolution = resolve_documents(&input);
    println!(
        "[resolution] status={:?} resolved={} reason={:?}",
        resolution.status,
        resolution.resolved_file_ids.len(),
        resolution.fallback_reason
    );
    for candidate in resolution.candidates.iter().take(8) {
        println!(
            "  candidate score={:.3} signals={:?} name={}",
            candidate.score,
            candidate.signals,
            file_names.get(&candidate.file_id).cloned().unwrap_or_default()
        );
    }
    for id in resolution.resolved_file_ids.iter().take(5) {
        println!("  resolved → {}", file_names.get(id).cloned().unwrap_or_default());
    }

    // 2b) COMPARE 诊断：解析 secondary_target，确认两侧是否命中不同文件。
    if let Some(secondary) = plan.secondary_target.as_ref() {
        println!(
            "[secondary_target] reference={:?} document_name={:?} document_type={:?}",
            secondary.reference,
            secondary.document_name,
            secondary.document_type
        );
        let mut secondary_plan = plan.clone();
        secondary_plan.target = secondary.clone();
        let input = ResolverInput::new(&secondary_plan, &session, profiles.clone(), file_names.clone());
        let sec = resolve_documents(&input);
        println!(
            "[secondary resolution] status={:?} resolved={}",
            sec.status,
            sec.resolved_file_ids.len()
        );
        for candidate in sec.candidates.iter().take(6) {
            println!(
                "  candidate score={:.3} signals={:?} name={}",
                candidate.score,
                candidate.signals,
                file_names.get(&candidate.file_id).cloned().unwrap_or_default()
            );
        }
        for id in sec.resolved_file_ids.iter().take(3) {
            println!("  secondary resolved → {}", file_names.get(id).cloned().unwrap_or_default());
        }
        let primary_first = resolution.resolved_file_ids.first().copied().or(resolution.candidates.first().map(|c| c.file_id));
        let secondary_first = sec.resolved_file_ids.first().copied().or(sec.candidates.first().map(|c| c.file_id));
        match (primary_first, secondary_first) {
            (Some(a), Some(b)) if a != b => println!("[compare verdict] 两侧已锁定不同文件 A={} B={}", file_names.get(&a).cloned().unwrap_or_default(), file_names.get(&b).cloned().unwrap_or_default()),
            (Some(a), Some(b)) => println!("[compare verdict] 两侧解析到同一文件 {}", file_names.get(&a).cloned().unwrap_or_default()),
            _ => println!("[compare verdict] 未同时锁定两侧文件"),
        }
    }

    // 3) 语义通道对照（生产会走 with_vectors）：嵌入 resolver_reference。
    let manager = ModelManager::open_store(PathBuf::from(&model_store))?;
    if let Some(active) = manager.active_artifact(ModelRole::Embedding)? {
        let ollama = OllamaClient::local();
        let resolver_reference = plan
            .target
            .reference
            .clone()
            .or_else(|| plan.target.document_name.clone())
            .unwrap_or_else(|| question.trim().to_owned());
        let prefixed = format!(
            "{}{}",
            active.query_prefix.as_deref().unwrap_or_default(),
            resolver_reference
        );
        if let Ok((vectors, _)) = ollama.embed(&active.model_id, &[prefixed]) {
            if let Some(vector) = vectors.into_iter().next() {
                let profile_ids = profiles.iter().map(|p| p.file_id).collect::<Vec<_>>();
                let profile_vectors = catalog.profile_vectors(&profile_ids).unwrap_or_default();
                let input = ResolverInput::new(&plan, &session, profiles, file_names)
                    .with_vectors(Some(vector), profile_vectors);
                let resolution = resolve_documents(&input);
                println!(
                    "[resolution+semantic] status={:?} resolved={} reason={:?}",
                    resolution.status,
                    resolution.resolved_file_ids.len(),
                    resolution.fallback_reason
                );
                for candidate in resolution.candidates.iter().take(8) {
                    println!(
                        "  candidate score={:.3} signals={:?} name={}",
                        candidate.score,
                        candidate.signals,
                        "见上方映射（向量版本未保留 name 映射）"
                    );
                }
                for id in resolution.resolved_file_ids.iter().take(5) {
                    println!("  resolved → {}", catalog.file_preview(id, 1).map(|p| p.file.display_name).unwrap_or_default());
                }
            }
        }
    }
    Ok(())
}
