//! VLM 图片理解消费端:消费 ocr_pending 文件的 pending_understanding 资产队列,
//! 用本地多模态模型补齐图片文字提取与描述,提交后把文件从 ocr_pending 翻转为 parsed。
//!
//! 用法:
//!   cargo run --example vlm_consumer -- --limit 500
//!   cargo run --example vlm_consumer -- --limit 500 --dry-run
//!
//! 参数:
//!   --data-dir      数据库目录(默认 %APPDATA%\com.fanfan.desktop)
//!   --model-store   模型存储(默认 %LOCALAPPDATA%\FanFan\ModelStore\v1)
//!   --limit N       最多处理的资产数(默认 0 = 处理完队列)
//!   --dry-run       只统计队列,不领取不推理

use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Instant,
};

use fanfan_core::{
    AppError, CatalogStore, ChunkEmbeddingInput, EmbeddingRequest, ImageUnderstandingResult,
    InboxEventType, InboxQuery, LocalGenerationRuntime, ModelManager, ModelRole,
    PendingImageUnderstanding, TriageStatus, WorkerClient, WorkerRole,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("VLM消费未完成: code={}", error.code);
        if let Some(details) = error.details {
            eprintln!("technical={details}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), AppError> {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data_directory = argument_path("--data-dir").unwrap_or_else(default_data_directory);
    let model_store = argument_path("--model-store").unwrap_or_else(default_model_store);
    let limit = argument_value("--limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let dry_run = argument_present("--dry-run");

    let catalog = CatalogStore::open(data_directory.join("fanfan.db"))?;
    if argument_present("--requeue-failed") {
        let requeued = requeue_failed_files(&catalog)?;
        println!("已重试 {requeued} 个解析失败的文件（重新排队为 pending）");
        return Ok(());
    }
    if argument_present("--promote-only") {
        // 批量提升：仅把「资产已全部 ready 且无 pending/processing」的
        // ocr_pending 文件翻转为 parsed，不做任何推理。用于应用侧 vision
        // 循环消费完资产但不 promote 的场景兜底。
        let file_ids = catalog.list_ocr_pending_files()?;
        let mut promoted = 0_u64;
        for file_id in &file_ids {
            if catalog.promote_ocr_pending_file_when_assets_ready(file_id)? {
                promoted += 1;
            }
        }
        println!(
            "仅提升模式完成: ocr_pending 文件 {total} 个, 已提升 {promoted} 个（其余资产未就绪）",
            total = file_ids.len()
        );
        return Ok(());
    }
    if let Some(file_id) = argument_value("--retry-ocr") {
        let file_id = uuid::Uuid::parse_str(&file_id).map_err(|error| {
            AppError::new("VLM_CONSUMER_INVALID_FILE_ID", format!("file_id 无效: {error}"), false)
        })?;
        catalog.retry_ocr(&file_id)?;
        println!("已重试 OCR: file_id={file_id}（重新排队为 pending,等待解析侧car重渲页面）");
        return Ok(());
    }
    let manager = ModelManager::open_store(&model_store)?;
    if argument_present("--backfill-embeddings") {
        backfill_embeddings(&repository_root, &manager, &catalog)?;
        return Ok(());
    }
    let vision = manager
        .active_artifact(ModelRole::Vision)?
        .ok_or_else(|| {
            AppError::new(
                "VLM_CONSUMER_VISION_UNAVAILABLE",
                "本地评测需要已通过完整性检查的视觉语言模型",
                false,
            )
        })?;
    let projector = manager.vision_projector_path(&vision)?;
    let (total, ready, pending) = catalog.image_understanding_stats()?;
    println!(
        "VLM队列: vision={} 资产 total={} ready={} pending/processing={}",
        vision.artifact_id, total, ready, pending
    );
    if dry_run {
        return Ok(());
    }

    let mut runtime = select_generation_runtime(&repository_root, &data_directory)?;
    let context_size = vision
        .context_length
        .unwrap_or(4_096)
        .clamp(2_048, 8_192);
    let model_artifact_id = vision.artifact_id.to_string();
    let cancelled = AtomicBool::new(false);
    let started = Instant::now();
    let mut processed = 0_u64;
    let mut failed_assets = 0_u64;
    let mut promoted_files = 0_u64;

    loop {
        if limit != 0 && processed >= limit as u64 {
            println!("已达 --limit 上限,停止");
            break;
        }
        let Some(pending) = catalog.claim_pending_ocr_image_understanding(&model_artifact_id)?
        else {
            println!("队列已空,结束");
            break;
        };
        match process_one(&catalog, &mut runtime, &cancelled, &vision.local_path, &projector, context_size, &model_artifact_id, &pending) {
            Ok(()) => {
                processed += 1;
                if catalog.promote_ocr_pending_file_when_assets_ready(&pending.file_id)? {
                    promoted_files += 1;
                    println!(
                        "文件提升: file_id={} (第{promoted_files}个)",
                        pending.file_id
                    );
                }
            }
            Err(error) => {
                failed_assets += 1;
                let _ = catalog.fail_image_understanding(&pending.asset_id, &error);
                eprintln!(
                    "资产失败: asset_id={} code={} (累计{failed_assets})",
                    pending.asset_id, error.code
                );
                // 失败也算已决:如果文件其余资产都已 ready,仍可提升文件
                if catalog.promote_ocr_pending_file_when_assets_ready(&pending.file_id)? {
                    promoted_files += 1;
                    println!(
                        "文件提升: file_id={} (第{promoted_files}个)",
                        pending.file_id
                    );
                }
            }
        }
    }
    let (total_after, ready_after, pending_after) = catalog.image_understanding_stats()?;
    println!(
        "VLM消费完成: processed={processed} failed_assets={failed_assets} promoted_files={promoted_files} 用时={:.1}s",
        started.elapsed().as_secs_f64()
    );
    println!(
        "VLM队列(后): total={total_after} ready={ready_after} pending/processing={pending_after}"
    );
    runtime.stop();
    Ok(())
}

fn process_one(
    catalog: &CatalogStore,
    runtime: &mut LocalGenerationRuntime,
    cancelled: &AtomicBool,
    model_path: &str,
    projector: &Path,
    context_size: u32,
    model_artifact_id: &str,
    pending: &PendingImageUnderstanding,
) -> Result<(), AppError> {
    if !runtime.is_active() {
        runtime.activate_multimodal(model_path, projector.to_string_lossy().as_ref(), context_size, 4)?;
    }
    let text = runtime.describe_image_cancellable(
        "你是本地离线图片理解助手。",
        "请仔细观察这张图片,提取其中的全部可见文字;如果没有文字,则只描述图片内容。\
         以 JSON 输出(不要输出其他内容):\
         {\"summary\":\"一句话内容摘要\",\"visible_text\":\"图片中的全部文字,无文字则为空字符串\",\
         \"keywords\":[\"关键词1\",\"关键词2\"],\"entities\":[\"人名/机构/日期等,无则为空数组\"],\
         \"chart_summary\":\"如果是图表则描述其结构,否则为空字符串\"}",
        Path::new(&pending.cache_path),
        &pending.mime_type,
        1200,
        cancelled,
    )?;
    let parsed = parse_vision_json(&text)?;
    catalog.commit_image_understanding(&ImageUnderstandingResult {
        asset_id: pending.asset_id,
        revision_id: pending.revision_id,
        model_artifact_id: model_artifact_id.to_owned(),
        summary: parsed.summary,
        visible_text: parsed.visible_text,
        keywords: parsed.keywords,
        entities: parsed.entities,
        chart_summary: parsed.chart_summary,
        idempotency_key: pending.idempotency_key.clone(),
    })?;
    Ok(())
}

#[derive(Debug)]
struct VisionJson {
    summary: String,
    visible_text: Option<String>,
    keywords: Vec<String>,
    entities: Vec<String>,
    chart_summary: Option<String>,
}

fn parse_vision_json(text: &str) -> Result<VisionJson, AppError> {
    let trimmed = text.trim();
    let start = trimmed.find('{').ok_or_else(|| {
        AppError::new(
            "VLM_JSON_INVALID",
            "多模态模型响应缺少JSON对象",
            false,
        )
    })?;
    let end = trimmed.rfind('}').ok_or_else(|| {
        AppError::new(
            "VLM_JSON_INVALID",
            "多模态模型响应缺少JSON结束符",
            false,
        )
    })?;
    let value: serde_json::Value = serde_json::from_str(&trimmed[start..=end]).map_err(|error| {
        AppError::new("VLM_JSON_INVALID", format!("JSON解析失败: {error}"), false)
    })?;
    let summary = value
        .get("summary")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::new(
                "VLM_JSON_INVALID",
                "多模态模型响应缺少summary字段",
                false,
            )
        })?;
    let strings = |key: &str| -> Vec<String> {
        value
            .get(key)
            .and_then(|value| value.as_array())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|item| item.as_str().map(str::trim).filter(|s| !s.is_empty()))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let opt_string = |key: &str| -> Option<String> {
        value
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    Ok(VisionJson {
        summary: summary.to_owned(),
        visible_text: opt_string("visible_text"),
        keywords: strings("keywords"),
        entities: strings("entities"),
        chart_summary: opt_string("chart_summary"),
    })
}

/// 把 parse_failed 且 pending_retry 的收件箱项目全部重试(重新排队为 pending)。
fn requeue_failed_files(catalog: &CatalogStore) -> Result<u64, AppError> {
    let mut requeued = 0_u64;
    let mut cursor: Option<String> = None;
    loop {
        let page = catalog.query_inbox(&InboxQuery {
            status: TriageStatus::All,
            event_types: vec![InboxEventType::ParseFailed],
            root_ids: Vec::new(),
            date_from: None,
            date_to: None,
            cursor: cursor.clone(),
            page_size: 200,
        })?;
        for item in page.items {
            match catalog.retry_inbox_item(&item.inbox_id) {
                Ok(_) => requeued += 1,
                Err(error) => eprintln!(
                    "重试失败: inbox_id={} code={}",
                    item.inbox_id, error.code
                ),
            }
        }
        if page.has_more {
            cursor = page.next_cursor;
        } else {
            break;
        }
    }
    Ok(requeued)
}

/// 补嵌入回填:把 pending 的 chunk 全部编码并提交,再重建活动向量代,
/// 使 embedding_coverage 与 vector_coverage 回到 1.0(评测门禁口径)。
fn backfill_embeddings(
    repository_root: &Path,
    manager: &ModelManager,
    catalog: &CatalogStore,
) -> Result<(), AppError> {
    let embedding = manager
        .active_artifact(ModelRole::Embedding)?
        .ok_or_else(|| {
            AppError::new(
                "VLM_CONSUMER_EMBEDDING_UNAVAILABLE",
                "嵌入回填需要已通过完整性检查的Embedding模型",
                false,
            )
        })?;
    let dimension = embedding.embedding_dimension.ok_or_else(|| {
        AppError::new(
            "VLM_CONSUMER_EMBEDDING_DIMENSION_MISSING",
            "Embedding模型缺少向量维度信息",
            false,
        )
    })?;
    let tokenizer = Path::new(&embedding.local_path)
        .parent()
        .map(|parent| parent.join("tokenizer.json"))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            AppError::new(
                "VLM_CONSUMER_TOKENIZER_UNAVAILABLE",
                "Embedding模型缺少配套tokenizer",
                false,
            )
        })?;
    let worker_python = repository_root.join(".artifacts/packaging-venv/Scripts/python.exe");
    let packaged_worker = repository_root.join("target/debug/worker/fanfan-worker.exe");
    let worker = if worker_python.is_file() {
        WorkerClient::new(
            worker_python.into_os_string(),
            repository_root.join("services/worker"),
        )
    } else if packaged_worker.is_file() {
        WorkerClient::from_executable(packaged_worker)
    } else {
        WorkerClient::from_environment(repository_root.join("services/worker"))
    };
    let worker_onnx = worker.isolated().with_role(WorkerRole::Onnx);
    let model_artifact_id = embedding.artifact_id.to_string();
    let max_length = embedding.max_length.unwrap_or(512);
    let mut embedded = 0_u64;
    loop {
        let pending = catalog.list_pending_embedding_chunks(&model_artifact_id, 200)?;
        if pending.is_empty() {
            break;
        }
        let texts = pending
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>();
        let response = worker_onnx.encode_embeddings(&EmbeddingRequest {
            model_path: embedding.local_path.clone(),
            tokenizer_path: Some(tokenizer.to_string_lossy().into_owned()),
            texts,
            max_length,
            threads: 2,
        })?;
        if response.vectors.len() != pending.len() {
            return Err(AppError::new(
                "VLM_CONSUMER_EMBEDDING_COUNT_MISMATCH",
                "嵌入回填返回数量与请求不一致",
                true,
            ));
        }
        let inputs = pending
            .iter()
            .zip(response.vectors.iter())
            .map(|(chunk, vector)| ChunkEmbeddingInput {
                chunk_id: chunk.chunk_id,
                vector: vector.clone(),
            })
            .collect::<Vec<_>>();
        let committed = catalog.commit_chunk_embeddings(&model_artifact_id, dimension, &inputs)?;
        embedded += committed;
        println!("嵌入回填: 累计 {embedded} 个 chunk（本批 {committed}）");
    }
    let generation = catalog.rebuild_vector_generation(&model_artifact_id, dimension)?;
    println!(
        "已重建活动向量代: generation_id={} status={}",
        generation.generation_id, generation.status
    );
    println!("嵌入回填完成: 共嵌入 {embedded} 个 chunk");
    Ok(())
}

fn select_generation_runtime(
    repository_root: &Path,
    data_directory: &Path,
) -> Result<LocalGenerationRuntime, AppError> {
    if let Some(explicit) = argument_path("--llama-runtime") {
        if explicit.is_file() {
            return Ok(LocalGenerationRuntime::new(explicit));
        }
        return Err(AppError::new(
            "VLM_CONSUMER_RUNTIME_UNAVAILABLE",
            "指定的llama.cpp消费运行时不存在",
            false,
        ));
    }
    let roots = [
        data_directory.join("runtime"),
        repository_root.join(".artifacts/runtime"),
    ];
    let cpu = roots
        .iter()
        .map(|root| root.join("llama/llama-server.exe"))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            AppError::new(
                "VLM_CONSUMER_RUNTIME_UNAVAILABLE",
                "缺少llama.cpp CPU运行时",
                false,
            )
        })?;
    for candidate in roots.iter().flat_map(|root| {
        [
            root.join("llama-cuda/llama-server.exe"),
            root.join("llama-vulkan/llama-server.exe"),
        ]
    }) {
        if !candidate.is_file() {
            continue;
        }
        let mut probe = LocalGenerationRuntime::new(candidate.clone());
        let capability = probe.probe_capability();
        if capability.gpu_available {
            return Ok(LocalGenerationRuntime::new_with_fallback_and_capability(
                candidate, cpu, capability,
            ));
        }
    }
    Ok(LocalGenerationRuntime::new(cpu))
}

fn argument_present(name: &str) -> bool {
    std::env::args().any(|arg| arg == name)
}

fn argument_value(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next();
        }
    }
    None
}

fn argument_path(name: &str) -> Option<PathBuf> {
    argument_value(name).map(PathBuf::from)
}

fn default_data_directory() -> PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("com.fanfan.desktop")
}

fn default_model_store() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    PathBuf::from(base).join("FanFan/ModelStore/v1")
}
